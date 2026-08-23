//! The voice agent: the participant waiting on the far side of a conversation room.
//!
//! # How it gets into a room
//!
//! One process, one task per room. The token endpoint creates a room and spawns an
//! agent for it; the agent joins with its own credentials and stays until the room
//! closes.
//!
//! LiveKit's explicit dispatch is the obvious alternative and is not available to us:
//! `CreateDispatch` targets a worker registered under an `agent_name`, and the worker
//! framework that does that registration is Python and Node only. Rewriting it to put a
//! dispatch protocol between two halves of the same binary would buy nothing today.
//!
//! What that choice costs is bounded, because the seam is narrow: an agent is a
//! function of a URL, a token, and the conversation it is serving ([`run`]). Moving
//! agents into their own process later means giving them another way to receive those
//! three things, not changing what an agent is.
//!
//! # What it holds
//!
//! No credentials. The server mints the agent's token and passes it in, which is what
//! keeps this crate from depending on the server that spawns it — and keeps the
//! LiveKit API secret in one place rather than two.

pub mod audio;
pub mod control;

use audio::Voice;
use control::{PublishFailed, Unannounced};
use livekit::{Room, RoomEvent, RoomOptions};
use openconv_protocol::{
    AudioFormat, ConversationInitiationMetadataEvent, ServerEvent, VadScoreEvent,
};
use std::fmt;
use std::sync::Arc;

/// Everything an agent needs to serve one conversation.
///
/// A plain value, so an agent can be started from a spawned task today and from a
/// dispatch message tomorrow without either one learning anything new.
#[derive(Clone, Debug)]
pub struct Assignment {
    /// The LiveKit signaling URL, `wss://…`.
    pub url: String,
    /// The agent's own participant token, minted by the server.
    pub token: String,
    /// The conversation this room is, which the client expects echoed back in the
    /// announcement.
    pub conversation_id: String,
}

/// Joins the room and serves the conversation until it ends.
pub async fn run(assignment: Assignment) -> Result<(), AgentError> {
    let (room, mut events) = Room::connect(&assignment.url, &assignment.token, RoomOptions::default())
        .await
        .map_err(|error| AgentError::Join(error.to_string()))?;
    let room = Arc::new(room);

    tracing::info!(conversation = %assignment.conversation_id, "agent joined");

    // Before anything else reaches the client. See `control`'s module docs: the client
    // hangs forever if this is not the first message, and the type system is what stops
    // a future edit from putting something above it.
    let control = Unannounced::new(room.clone())
        .announce(ConversationInitiationMetadataEvent {
            conversation_id: assignment.conversation_id.clone(),
            agent_output_audio_format: AudioFormat::Pcm48000,
            user_input_audio_format: AudioFormat::Pcm48000,
        })
        .await?;

    let voice = Voice::publish(&room).await?;
    tokio::spawn(voice.clone().run());

    // Scaffolding that ticket .8 removes: proves the track carries audio to the client
    // before any speech pipeline exists to put words on it.
    voice.enqueue(&audio::tone(440.0, 250, 0.2));

    // Likewise a placeholder for ticket .6, which computes these from the user's audio.
    // Sent after the announcement, which is the only ordering the client cares about.
    control
        .publish(&ServerEvent::VadScore { vad_score_event: VadScoreEvent { vad_score: 0.0 } })
        .await?;

    // The room is the agent's lifetime. Every event flows through here so that
    // disconnection has one place it is noticed rather than a timeout somewhere.
    while let Some(event) = events.recv().await {
        match event {
            RoomEvent::ParticipantDisconnected(participant) => {
                tracing::info!(
                    conversation = %assignment.conversation_id,
                    participant = %participant.identity(),
                    "participant left, ending conversation"
                );
                break;
            }
            RoomEvent::Disconnected { reason } => {
                tracing::info!(
                    conversation = %assignment.conversation_id,
                    ?reason,
                    "agent disconnected"
                );
                break;
            }
            _ => {}
        }
    }

    // Leaving explicitly rather than dropping the room: it closes the room promptly for
    // the SFU, which is what turns into the `room_finished` webhook the usage endpoint
    // bills from.
    let _ = room.close().await;

    Ok(())
}

/// Starts an agent in the background, logging rather than returning its outcome.
///
/// The caller is an HTTP handler that must answer its own request; an agent that fails
/// to join has to be visible in the logs, not in a response the client already received.
pub fn spawn(assignment: Assignment) {
    tokio::spawn(async move {
        let conversation = assignment.conversation_id.clone();
        if let Err(error) = run(assignment).await {
            tracing::error!(%conversation, %error, "agent failed");
        }
    });
}

#[derive(Debug)]
pub enum AgentError {
    Join(String),
    Publish(PublishFailed),
    Voice(audio::VoiceError),
}

impl From<PublishFailed> for AgentError {
    fn from(error: PublishFailed) -> Self {
        Self::Publish(error)
    }
}

impl From<audio::VoiceError> for AgentError {
    fn from(error: audio::VoiceError) -> Self {
        Self::Voice(error)
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Join(error) => write!(f, "agent could not join the room: {error}"),
            Self::Publish(error) => write!(f, "{error}"),
            Self::Voice(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AgentError {}
