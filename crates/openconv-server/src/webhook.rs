//! Where LiveKit tells us a conversation ended.
//!
//! The end of a call is observed rather than reported. The agent could send a duration
//! when it finishes, but an agent that crashes mid-call — the case that matters most —
//! sends nothing, and a conversation missing an end reads to Happy as free usage. The
//! SFU sees the room close either way, so that is who we ask.

use crate::conversation::{ConversationId, MalformedConversationId};
use crate::record::ConversationEvent;
use livekit_api::webhooks::{WebhookError, WebhookReceiver};
use std::fmt;

/// The event LiveKit sends when the last participant leaves and the room closes.
const ROOM_FINISHED: &str = "room_finished";

/// Verifies webhook deliveries and turns the ones we care about into log events.
pub struct Webhooks {
    receiver: WebhookReceiver,
}

impl Webhooks {
    pub fn new(api_key: &str, api_secret: &str) -> Self {
        Self {
            receiver: WebhookReceiver::new(
                livekit_api::access_token::TokenVerifier::with_api_key(api_key, api_secret),
            ),
        }
    }

    /// Authenticates a delivery and returns the event it implies, if any.
    ///
    /// `Ok(None)` is not a failure and not an empty answer standing in for one: LiveKit
    /// sends a dozen event kinds down this one hook — participants joining, tracks
    /// published — and every kind except `room_finished` is genuinely nothing for this
    /// service to record. A delivery that fails to authenticate returns `Err` and never
    /// reaches this distinction.
    pub fn interpret(
        &self,
        body: &str,
        auth_token: &str,
    ) -> Result<Option<ConversationEvent>, WebhookRejected> {
        let event = self.receiver.receive(body, auth_token).map_err(WebhookRejected::Unverified)?;

        if event.event != ROOM_FINISHED {
            return Ok(None);
        }

        // A room_finished with no room is malformed rather than uninteresting, so it is
        // an error: silently dropping it would lose a call's duration for good.
        let room = event.room.as_ref().ok_or(WebhookRejected::NoRoom)?;

        // Every room this service creates is named for its conversation. A room that is
        // not — someone else sharing the deployment, a leftover from another tool — is
        // not ours to bill, and parsing is what tells the two apart.
        let conversation_id =
            ConversationId::parse(&room.name).map_err(WebhookRejected::ForeignRoom)?;

        Ok(Some(ConversationEvent::Finished {
            conversation_id,
            ended_at_unix_secs: event.created_at,
        }))
    }
}

#[derive(Debug)]
pub enum WebhookRejected {
    Unverified(WebhookError),
    NoRoom,
    ForeignRoom(MalformedConversationId),
}

impl fmt::Display for WebhookRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unverified(error) => write!(f, "webhook did not verify: {error}"),
            Self::NoRoom => write!(f, "room_finished carried no room"),
            Self::ForeignRoom(error) => write!(f, "not a conversation room: {error}"),
        }
    }
}

impl std::error::Error for WebhookRejected {}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use livekit_api::access_token::{AccessToken, VideoGrants};
    use sha2::{Digest, Sha256};

    const API_KEY: &str = "openconv";
    const API_SECRET: &str = "secret-secret-secret-secret-secret";

    /// Signs a delivery the way LiveKit does: a token whose `sha256` claim is the
    /// digest of the body, so body and signature cannot be mixed and matched.
    fn deliver(body: &str) -> String {
        let digest = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        AccessToken::with_api_key(API_KEY, API_SECRET)
            .with_grants(VideoGrants::default())
            .with_sha256(&digest)
            .to_jwt()
            .unwrap()
    }

    fn room_finished(room_name: &str, ended_at: i64) -> String {
        format!(
            r#"{{"event":"room_finished","room":{{"name":"{room_name}"}},"id":"ev_1","createdAt":{ended_at}}}"#
        )
    }

    fn webhooks() -> Webhooks {
        Webhooks::new(API_KEY, API_SECRET)
    }

    #[test]
    fn a_finished_room_becomes_a_finished_conversation() {
        let body = room_finished("conv_abc123", 1_700_000_060);
        let event = webhooks().interpret(&body, &deliver(&body)).unwrap();

        assert_eq!(
            event,
            Some(ConversationEvent::Finished {
                conversation_id: ConversationId::parse("conv_abc123").unwrap(),
                ended_at_unix_secs: 1_700_000_060,
            })
        );
    }

    #[test]
    fn other_event_kinds_are_nothing_to_record() {
        let body = r#"{"event":"participant_joined","room":{"name":"conv_abc123"},"id":"ev_2"}"#;
        assert_eq!(webhooks().interpret(body, &deliver(body)).unwrap(), None);
    }

    #[test]
    fn a_room_this_service_did_not_name_is_not_ours_to_bill() {
        let body = room_finished("some-other-tools-room", 1_700_000_060);
        assert!(matches!(
            webhooks().interpret(&body, &deliver(&body)),
            Err(WebhookRejected::ForeignRoom(_))
        ));
    }

    /// The point of verifying at all: anyone who can reach this endpoint must not be
    /// able to end conversations they do not own.
    #[test]
    fn a_delivery_signed_with_another_secret_is_refused() {
        let body = room_finished("conv_abc123", 1_700_000_060);
        let forged = AccessToken::with_api_key(API_KEY, "not-the-real-secret")
            .with_grants(VideoGrants::default())
            .with_sha256(
                &base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)),
            )
            .to_jwt()
            .unwrap();

        assert!(matches!(
            webhooks().interpret(&body, &forged),
            Err(WebhookRejected::Unverified(_))
        ));
    }

    /// A valid signature over a *different* body must not authenticate this one.
    #[test]
    fn a_body_swapped_under_a_valid_signature_is_refused() {
        let signed = room_finished("conv_abc123", 1_700_000_060);
        let swapped = room_finished("conv_zzz999", 1_700_009_999);

        assert!(matches!(
            webhooks().interpret(&swapped, &deliver(&signed)),
            Err(WebhookRejected::Unverified(_))
        ));
    }
}
