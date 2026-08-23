//! The edge where this service talks to LiveKit: creating rooms and signing the JWTs
//! that admit participants to them.
//!
//! Grant and claim names are taken from `livekit_api`'s own types rather than
//! transcribed into a hand-built JWT, so the claims stay a derived copy of LiveKit's
//! map of them instead of a second one that can drift as the server evolves.

use crate::config::Config;
use crate::conversation::ConversationId;
use crate::record::ConversationRecord;
use livekit_api::access_token::{AccessToken, AccessTokenError, VideoGrants};
use livekit_api::services::room::{CreateRoomOptions, RoomClient};
use livekit_api::services::ServiceError;
use std::fmt;
use std::time::Duration;

/// How long a room with nobody in it survives before LiveKit reaps it.
///
/// This is the window between us creating the room and the caller's SDK joining it —
/// a user granting microphone permission, a cold app start. Five minutes is generous
/// for that and still short enough that a token minted and never used does not leave
/// an empty room lying around.
const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a minted token stays valid.
///
/// Longer than it takes to join, because LiveKit re-presents the same token on every
/// reconnect: a token that expires mid-call turns a recoverable network blip into a
/// dropped conversation. Six hours covers the five-hour ceiling Happy enforces on a
/// single user's usage, so no conversation can outlive its token.
const TOKEN_TTL: Duration = Duration::from_secs(6 * 3600);

/// The far side of LiveKit, as this service uses it.
pub struct LiveKit {
    rooms: RoomClient,
    url: String,
    api_key: String,
    api_secret: String,
}

impl LiveKit {
    pub fn new(config: &Config) -> Self {
        Self {
            rooms: RoomClient::with_api_key(
                &config.livekit_url,
                &config.livekit_api_key,
                &config.livekit_api_secret,
            ),
            url: config.livekit_url.clone(),
            api_key: config.livekit_api_key.clone(),
            api_secret: config.livekit_api_secret.clone(),
        }
    }

    /// Creates the room for a conversation, carrying the record as room metadata.
    ///
    /// This call is not optional. The deployment runs with `room.auto_create` off, so
    /// a client presenting a token for a room that was never created is refused at the
    /// door — which is the point. The alternative, auto-creating on join, would put the
    /// caller in an empty room with no agent and no error, and a silent failure at this
    /// step is precisely what the rest of this crate is arranged to prevent.
    pub async fn create_room(&self, record: &ConversationRecord) -> Result<(), LiveKitError> {
        let metadata = serde_json::to_string(record).map_err(LiveKitError::Metadata)?;

        self.rooms
            .create_room(
                record.conversation_id.as_str(),
                CreateRoomOptions {
                    empty_timeout: EMPTY_ROOM_TIMEOUT.as_secs() as u32,
                    metadata,
                    // One human and one agent. Capping it means a leaked token cannot
                    // be used to bring an audience into somebody's conversation.
                    max_participants: 2,
                    ..Default::default()
                },
            )
            .await
            .map_err(LiveKitError::CreateRoom)?;

        Ok(())
    }

    /// Signs the JWT the caller hands to the ElevenLabs SDK.
    ///
    /// The room claim is the conversation ID itself, which is what makes the ID
    /// recoverable by both consumers: Happy reads `video.room` out of this token, and
    /// the SDK reads the name of the room it joins. They are the same string.
    pub fn mint_participant_token(
        &self,
        record: &ConversationRecord,
    ) -> Result<ConversationToken, LiveKitError> {
        // The display name carries Happy's user ID when there is one. The BYO path
        // supplies no `participant_name`, and an empty name is the honest rendering of
        // that rather than a placeholder that would later look like a real user.
        self.mint_token(
            &record.conversation_id,
            &participant_identity(&record.conversation_id),
            record.happy_user.as_ref().map(|user| user.as_str()),
        )
    }

    /// Signs the agent's own JWT for the same room.
    ///
    /// Identical grants to the human's: the agent publishes speech, subscribes to the
    /// user's microphone, and both sends and receives control events. The two differ
    /// only in who they say they are, which is why they are one function.
    pub fn mint_agent_token(
        &self,
        conversation: &ConversationId,
    ) -> Result<ConversationToken, LiveKitError> {
        self.mint_token(conversation, &agent_identity(conversation), Some(AGENT_NAME))
    }

    fn mint_token(
        &self,
        conversation: &ConversationId,
        identity: &str,
        name: Option<&str>,
    ) -> Result<ConversationToken, LiveKitError> {
        let mut token = AccessToken::with_api_key(&self.api_key, &self.api_secret)
            .with_ttl(TOKEN_TTL)
            .with_identity(identity)
            .with_grants(VideoGrants {
                room_join: true,
                room: conversation.as_str().to_owned(),
                // Publish audio, subscribe to the other side's audio, and both send and
                // receive control messages on the data channel — everything either
                // party does once it is in the room, and nothing more.
                can_publish: true,
                can_subscribe: true,
                can_publish_data: true,
                ..Default::default()
            });

        if let Some(name) = name {
            token = token.with_name(name);
        }

        token.to_jwt().map(ConversationToken).map_err(LiveKitError::MintToken)
    }

    /// The signaling URL the agent dials.
    ///
    /// Derived from the one configured origin rather than configured separately, so the
    /// REST calls and the agent can never end up pointed at different deployments.
    pub fn signaling_url(&self) -> String {
        self.url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1)
    }
}

/// What the client shows for the agent, and how it appears in the room.
const AGENT_NAME: &str = "agent";

/// The identity LiveKit knows the human participant by.
///
/// Derived from the conversation ID rather than from `participant_name`, because a
/// room hosts exactly one human and LiveKit disconnects an existing participant when a
/// second one joins with the same identity. Keying on the user would mean a user's
/// second conversation silently killed their first.
fn participant_identity(conversation: &ConversationId) -> String {
    format!("user_{conversation}")
}

/// The identity the agent joins under.
///
/// Distinct from the human's, so the two never collide and so either side can tell who
/// published a track without consulting anything else.
fn agent_identity(conversation: &ConversationId) -> String {
    format!("agent_{conversation}")
}

/// A signed LiveKit JWT, ready to hand to the client SDK.
#[derive(Clone, serde::Serialize)]
#[serde(transparent)]
pub struct ConversationToken(String);

impl ConversationToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Redacted, because this value admits its bearer to a live conversation.
impl fmt::Debug for ConversationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConversationToken(<redacted>)")
    }
}

#[derive(Debug)]
pub enum LiveKitError {
    CreateRoom(ServiceError),
    MintToken(AccessTokenError),
    Metadata(serde_json::Error),
}

/// Renders an error together with everything underneath it.
///
/// `ServiceError` and `reqwest::Error` both collapse to "error sending request for url
/// (...)", which reads the same whether the host is unreachable, the TLS handshake
/// failed, the request timed out, or the process was denied a socket. Those call for
/// four different responses, so the causes go in the message.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        // Each layer tends to restate the one below it verbatim; only the layers that
        // add something are worth the width.
        if !rendered.contains(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        source = cause.source();
    }
    rendered
}

impl fmt::Display for LiveKitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateRoom(error) => {
                write!(f, "LiveKit refused to create the room: {}", with_causes(error))
            }
            Self::MintToken(error) => write!(f, "could not sign the participant token: {error}"),
            Self::Metadata(error) => write!(f, "could not serialize the room metadata: {error}"),
        }
    }
}

impl std::error::Error for LiveKitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::XiApiKey;
    use crate::record::{AgentId, HappyUserId};
    use livekit_api::access_token::TokenVerifier;

    fn test_config() -> Config {
        Config {
            livekit_url: "https://livekit.example".to_owned(),
            livekit_api_key: "openconv".to_owned(),
            livekit_api_secret: "secret-secret-secret-secret-secret".to_owned(),
            xi_api_key: XiApiKey::new("sk-test"),
            bind: "127.0.0.1:0".parse().unwrap(),
            conversation_log: "conversations.jsonl".into(),
            whisper_model: "ggml-base.en.bin".into(),
        }
    }

    fn record_with_user(user: Option<HappyUserId>) -> ConversationRecord {
        ConversationRecord::start(
            ConversationId::generate(),
            AgentId::new("agent_happy"),
            user,
            1_700_000_000,
        )
    }

    /// The contract Happy depends on, asserted the way Happy asserts it: decode the
    /// token, read `video.room`, recover the conversation ID from it.
    #[test]
    fn the_room_claim_carries_the_conversation_id() {
        let livekit = LiveKit::new(&test_config());
        let record = record_with_user(Some(HappyUserId::new("u_deadbeef")));
        let token = livekit.mint_participant_token(&record).unwrap();

        let claims = TokenVerifier::with_api_key("openconv", "secret-secret-secret-secret-secret")
            .verify(token.as_str())
            .expect("the deployment verifies tokens with these same credentials");

        assert_eq!(claims.video.room, record.conversation_id.as_str());
        assert_eq!(
            ConversationId::parse(&claims.video.room).unwrap(),
            record.conversation_id
        );
    }

    #[test]
    fn the_grants_admit_the_bearer_to_that_room_only() {
        let livekit = LiveKit::new(&test_config());
        let record = record_with_user(None);
        let token = livekit.mint_participant_token(&record).unwrap();

        let claims = TokenVerifier::with_api_key("openconv", "secret-secret-secret-secret-secret")
            .verify(token.as_str())
            .unwrap();

        assert!(claims.video.room_join, "bearer cannot join the room it was minted for");
        assert!(claims.video.can_publish, "bearer cannot publish a microphone track");
        assert!(claims.video.can_subscribe, "bearer cannot hear the agent");
        assert!(claims.video.can_publish_data, "bearer cannot send control messages");
        assert!(!claims.video.room_create, "a participant token grants room creation");
        assert!(!claims.video.room_admin, "a participant token grants room administration");
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let livekit = LiveKit::new(&test_config());
        let token = livekit.mint_participant_token(&record_with_user(None)).unwrap();

        assert!(
            TokenVerifier::with_api_key("openconv", "a-different-secret-entirely")
                .verify(token.as_str())
                .is_err()
        );
    }

    #[test]
    fn each_conversation_gets_its_own_participant_identity() {
        let first = participant_identity(&ConversationId::generate());
        let second = participant_identity(&ConversationId::generate());
        assert_ne!(first, second);
    }
}
