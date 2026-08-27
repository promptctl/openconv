//! The HTTP surface Happy's server calls, shaped to match `api.elevenlabs.io/v1/convai`
//! closely enough that pointing Happy at this host is a base-URL change and nothing
//! else.

use crate::config::XiApiKey;
use crate::conversation::ConversationId;
use crate::livekit::{ConversationToken, LiveKitError};
use crate::record::{now_unix_secs, AgentId, ConversationEvent, ConversationRecord, HappyUserId};
use crate::state::AppState;
use crate::store::LogError;
use crate::usage::{self, ConversationPage, UsageQuery};
use crate::webhook::WebhookRejected;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::{request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// The header ElevenLabs authenticates with, and therefore the one Happy sends.
const API_KEY_HEADER: &str = "xi-api-key";

/// Everything Happy's server and the SFU call. Joined with the browser client's routes
/// by [`crate::app::router`], which is the only place that knows both exist.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/convai/conversation/token", get(conversation_token))
        .route("/v1/convai/conversations", get(conversations))
        // Authenticated by LiveKit's own signature over the body rather than by
        // `xi-api-key`, because LiveKit is the caller here and knows nothing about ours.
        .route("/livekit/webhook", post(livekit_webhook))
        // Unauthenticated on purpose: a liveness probe that needs a credential tells
        // you the credential is good, not that the service is up.
        .route("/health", get(|| async { "ok" }))
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
    state.log.append(&ConversationEvent::Started(record.clone())).await?;
    let token = state.livekit.mint_participant_token(&record)?;

    // Before the caller is told the room is ready, so the agent is on its way in rather
    // than starting only once the user has already joined an empty room. It joins in
    // the background: the agent's own failures belong in the logs, not in a response
    // about whether a token could be minted.
    openconv_agent::spawn(openconv_agent::Assignment {
        url: state.livekit.signaling_url(),
        token: state.livekit.mint_agent_token(&record.conversation_id)?.as_str().to_owned(),
        conversation_id: record.conversation_id.as_str().to_owned(),
    }, state.agents.clone());

    tracing::info!(
        conversation = %record.conversation_id,
        agent = %record.agent_id,
        user = record.happy_user.as_ref().map(|u| u.as_str()).unwrap_or("<byo>"),
        "conversation opened"
    );

    Ok(Json(TokenResponse { token }))
}

/// `GET /v1/convai/conversations?user_id=...&created_after=...&page_size=...`
#[derive(Debug, Deserialize)]
pub struct ConversationsRequest {
    #[serde(default)]
    user_id: Option<HappyUserId>,
    /// Left as a string here because callers disagree about its format; see
    /// [`parse_created_after`].
    #[serde(default)]
    created_after: Option<String>,
    #[serde(default)]
    page_size: Option<usize>,
}

/// Reads `created_after` in either form its callers send.
///
/// ElevenLabs documents unix seconds, but Happy sends `new Date(...).toISOString()` —
/// an ISO-8601 string — and Happy is the caller that has to work. Accepting both is
/// liberality at the outermost edge, and it ends here: the rest of the service sees
/// one integer and never learns there was a choice.
///
/// An unparseable value is an error rather than a silently ignored filter. Dropping it
/// would widen the window to all of history and quietly under-report nobody's usage
/// while over-reporting everyone's.
fn parse_created_after(raw: &str) -> Result<i64, ApiError> {
    use time::format_description::well_known::Rfc3339;

    raw.parse::<i64>()
        .ok()
        .or_else(|| {
            time::OffsetDateTime::parse(raw, &Rfc3339).ok().map(|at| at.unix_timestamp())
        })
        .ok_or_else(|| ApiError::BadCreatedAfter(raw.to_owned()))
}

/// Serves the usage history Happy sums to decide whether a user may start a call.
async fn conversations(
    _: Authenticated,
    State(state): State<AppState>,
    Query(request): Query<ConversationsRequest>,
) -> Result<Json<ConversationPage>, ApiError> {
    let created_after_unix_secs =
        request.created_after.as_deref().map(parse_created_after).transpose()?;

    let events = state.log.read_all().await?;

    Ok(Json(usage::conversations(
        &events,
        &UsageQuery {
            user_id: request.user_id,
            created_after_unix_secs,
            page_size: request.page_size,
        },
        now_unix_secs(),
    )))
}

/// Receives LiveKit's room lifecycle notifications, and records the ends of calls.
///
/// Takes the body as a `String` rather than parsed JSON because the signature covers
/// the exact bytes sent: re-serializing a parsed value would change them, and the
/// verification would fail for reasons that have nothing to do with authenticity.
async fn livekit_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError::UnsignedWebhook)?;

    let Some(event) = state.webhooks.interpret(&body, auth)? else {
        // A kind of event this service does not record. Acknowledged so LiveKit stops
        // retrying it.
        return Ok(StatusCode::NO_CONTENT);
    };

    state.log.append(&event).await?;

    tracing::info!(conversation = %event.conversation_id(), "conversation ended");

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug)]
pub enum ApiError {
    Unauthenticated,
    BadCreatedAfter(String),
    UnsignedWebhook,
    Webhook(WebhookRejected),
    LiveKit(LiveKitError),
    Log(LogError),
}

impl From<WebhookRejected> for ApiError {
    fn from(error: WebhookRejected) -> Self {
        Self::Webhook(error)
    }
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
            Self::BadCreatedAfter(value) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_created_after",
                format!(
                    "created_after={value:?} is neither unix seconds nor an ISO-8601 timestamp"
                ),
            ),
            // The webhook arms answer LiveKit, not a user. Both are 401 so a
            // misconfigured webhook key shows up in the SFU's delivery log as a
            // rejection rather than as a success that recorded nothing.
            Self::UnsignedWebhook => (
                StatusCode::UNAUTHORIZED,
                "unsigned_webhook",
                "webhook delivery carried no Authorization header".to_owned(),
            ),
            Self::Webhook(error) => {
                tracing::warn!(%error, "rejected a webhook delivery");
                (
                    StatusCode::UNAUTHORIZED,
                    "invalid_webhook",
                    "webhook delivery was not accepted".to_owned(),
                )
            }
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
