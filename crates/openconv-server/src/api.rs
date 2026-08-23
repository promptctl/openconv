//! The HTTP surface Happy's server calls, shaped to match `api.elevenlabs.io/v1/convai`
//! closely enough that pointing Happy at this host is a base-URL change and nothing
//! else.

use crate::config::{Config, XiApiKey};
use crate::conversation::ConversationId;
use crate::livekit::{ConversationToken, LiveKit, LiveKitError};
use crate::record::{now_unix_secs, AgentId, ConversationRecord, HappyUserId};
use crate::store::{ConversationLog, LogError};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The header ElevenLabs authenticates with, and therefore the one Happy sends.
const API_KEY_HEADER: &str = "xi-api-key";

#[derive(Clone)]
pub struct AppState {
    pub livekit: Arc<LiveKit>,
    pub log: Arc<ConversationLog>,
    pub xi_api_key: XiApiKey,
}

impl AppState {
    pub fn new(config: &Config, livekit: LiveKit, log: ConversationLog) -> Self {
        Self {
            livekit: Arc::new(livekit),
            log: Arc::new(log),
            xi_api_key: config.xi_api_key.clone(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/convai/conversation/token", get(conversation_token))
        // Unauthenticated on purpose: a liveness probe that needs a credential tells
        // you the credential is good, not that the service is up.
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

/// Proof that the request carried the right `xi-api-key`.
///
/// The value cannot be constructed except by presenting the key, so a handler that
/// takes one has already been authenticated and no handler can forget to check. That
/// makes this extractor the single place the credential is verified.
pub struct Authenticated;

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(API_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(ApiError::Unauthenticated)?;

        (XiApiKey::new(presented) == state.xi_api_key)
            .then_some(Self)
            .ok_or(ApiError::Unauthenticated)
    }
}

/// `GET /v1/convai/conversation/token?agent_id=...&participant_name=...`
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    agent_id: AgentId,
    /// Happy's user ID on the metered path. Absent on the bring-your-own-key path,
    /// which mints a token the caller pays their own provider for and which no usage
    /// is gated on.
    #[serde(default)]
    participant_name: Option<HappyUserId>,
}

/// The response body. ElevenLabs returns exactly this one field, and Happy destructures
/// it as `{ token }` before decoding the JWT itself.
///
/// Holds the [`ConversationToken`] rather than unwrapping it to a `String`, so the
/// credential keeps its redacting [`Debug`] all the way to the wire — the point at
/// which a response body is most likely to end up in a log line.
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    token: ConversationToken,
}

/// Creates a conversation and returns the JWT that admits its caller.
///
/// The order of the three steps is the contract: the room exists before a token for it
/// is signed, and the conversation is recorded before the token that starts it leaves
/// the process. Any other order can hand out a token for a room that does not exist or
/// for a call that will never be billed.
async fn conversation_token(
    _: Authenticated,
    State(state): State<AppState>,
    Query(request): Query<TokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let record = ConversationRecord::start(
        ConversationId::generate(),
        request.agent_id,
        request.participant_name,
        now_unix_secs(),
    );

    state.livekit.create_room(&record).await?;
    state.log.append(&record).await?;
    let token = state.livekit.mint_participant_token(&record)?;

    tracing::info!(
        conversation = %record.conversation_id,
        agent = %record.agent_id,
        user = record.happy_user.as_ref().map(|u| u.as_str()).unwrap_or("<byo>"),
        "conversation opened"
    );

    Ok(Json(TokenResponse { token }))
}

#[derive(Debug)]
pub enum ApiError {
    Unauthenticated,
    LiveKit(LiveKitError),
    Log(LogError),
}

impl From<LiveKitError> for ApiError {
    fn from(error: LiveKitError) -> Self {
        Self::LiveKit(error)
    }
}

impl From<LogError> for ApiError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// ElevenLabs' error envelope, which is what a client written against their API will
/// try to read when something goes wrong.
#[derive(Serialize)]
struct ErrorBody {
    detail: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    status: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, status, message) = match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                format!("missing or incorrect {API_KEY_HEADER} header"),
            ),
            // The caller did nothing wrong in either remaining case, so both are 5xx —
            // and both are logged here with their full cause, because the body
            // deliberately does not carry it.
            Self::LiveKit(error) => {
                tracing::error!(%error, "could not open the conversation");
                (
                    StatusCode::BAD_GATEWAY,
                    "livekit_unavailable",
                    "could not create the conversation room".to_owned(),
                )
            }
            Self::Log(error) => {
                tracing::error!(%error, "could not record the conversation");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "conversation_log_unavailable",
                    "could not record the conversation".to_owned(),
                )
            }
        };

        (code, Json(ErrorBody { detail: ErrorDetail { status, message } })).into_response()
    }
}
