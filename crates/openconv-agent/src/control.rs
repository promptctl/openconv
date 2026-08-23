//! Publishing control events, with the client's ordering rule made structural.
//!
//! # The rule
//!
//! The ElevenLabs client resolves its connect promise from a one-shot listener on the
//! first data message it receives (`dist/lib.modern.js` in `@elevenlabs/client`):
//!
//! ```text
//! addEventListener("message", e => { const t = JSON.parse(e.data);
//!   d(t) && ("conversation_initiation_metadata" === t.type
//!     ? n(t.conversation_initiation_metadata_event)
//!     : console.warn("First received message is not conversation metadata.")) },
//!   {once: true})
//! ```
//!
//! If the first message is anything else, the listener fires once, logs a warning to a
//! console nobody is reading, and the promise is **never resolved**. `startSession()`
//! hangs forever: no error, no timeout, and a user sitting in a room waiting for an
//! agent that is connected and publishing happily. A single `vad_score` sent one line
//! too early does this.
//!
//! # Why it is a type and not a comment
//!
//! An ordering rule kept as a convention is violated by the next person who adds a line
//! above it, and this particular violation is invisible in every log on our side. So
//! the phase is a type: [`Unannounced`] has no way to publish anything, and the only
//! way to obtain a [`ControlChannel`] — which can publish anything — is
//! [`Unannounced::announce`], which consumes the [`Unannounced`] to produce it. There
//! is no ordering to remember because there is no other order available.

use livekit::{DataPacket, Room};
use openconv_protocol::{ConversationInitiationMetadataEvent, ServerEvent};
use std::fmt;
use std::sync::Arc;

/// A room the agent has joined but has not yet announced itself in.
///
/// Deliberately carries no `publish`. See the module docs.
pub struct Unannounced {
    room: Arc<Room>,
}

impl Unannounced {
    pub(crate) fn new(room: Arc<Room>) -> Self {
        Self { room }
    }

    /// Announces the conversation, opening the channel for everything else.
    ///
    /// Consumes `self`, so the metadata cannot be sent twice and nothing can be sent
    /// before it.
    pub async fn announce(
        self,
        metadata: ConversationInitiationMetadataEvent,
    ) -> Result<ControlChannel, PublishFailed> {
        let channel = ControlChannel { room: self.room };
        channel
            .publish(&ServerEvent::ConversationMetadata {
                conversation_initiation_metadata_event: metadata,
            })
            .await?;
        Ok(channel)
    }
}

/// The agent's side of the control channel, open for business.
///
/// Obtainable only from [`Unannounced::announce`]; the field is private and there is no
/// other constructor, which is what makes the ordering rule hold by construction rather
/// than by review.
pub struct ControlChannel {
    room: Arc<Room>,
}

impl ControlChannel {
    /// Publishes one event as UTF-8 JSON on the reliable data channel.
    ///
    /// One `publishData` call carries exactly one serialized event, with no envelope
    /// and no topic — the shape the client's `RoomEvent.DataReceived` handler decodes.
    /// Reliable rather than lossy because these are transcripts, tool calls and turn
    /// boundaries: a dropped one desynchronizes the conversation rather than degrading
    /// it.
    pub async fn publish(&self, event: &ServerEvent) -> Result<(), PublishFailed> {
        let payload = serde_json::to_vec(event).map_err(PublishFailed::Serialize)?;

        self.room
            .local_participant()
            .publish_data(DataPacket { payload, reliable: true, ..Default::default() })
            .await
            .map_err(|error| PublishFailed::Room(error.to_string()))
    }
}

#[derive(Debug)]
pub enum PublishFailed {
    Serialize(serde_json::Error),
    Room(String),
}

impl fmt::Display for PublishFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(f, "could not serialize a control event: {error}"),
            Self::Room(error) => write!(f, "could not publish a control event: {error}"),
        }
    }
}

impl std::error::Error for PublishFailed {}

#[cfg(test)]
mod tests {
    use openconv_protocol::{AudioFormat, ConversationInitiationMetadataEvent, ServerEvent};

    /// The bytes the client's one-shot listener has to recognise. Asserted on the
    /// serialization rather than on a round trip, because a round trip agrees with
    /// itself no matter what the field is called.
    #[test]
    fn the_announcement_serializes_as_the_client_expects() {
        let event = ServerEvent::ConversationMetadata {
            conversation_initiation_metadata_event: ConversationInitiationMetadataEvent {
                conversation_id: "conv_abc123".to_owned(),
                agent_output_audio_format: AudioFormat::Pcm48000,
                user_input_audio_format: AudioFormat::Pcm48000,
            },
        };

        let json: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();

        // `type` is what the listener switches on; anything else leaves the client
        // hanging forever.
        assert_eq!(json["type"], "conversation_initiation_metadata");
        assert_eq!(
            json["conversation_initiation_metadata_event"]["conversation_id"],
            "conv_abc123"
        );
        assert_eq!(
            json["conversation_initiation_metadata_event"]["agent_output_audio_format"],
            "pcm_48000"
        );
    }
}
