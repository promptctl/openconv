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
pub mod endpoint;
pub mod listen;
pub mod transcribe;

use audio::Voice;
use control::{ControlChannel, PublishFailed, Unannounced};
use listen::Speech;
use livekit::track::RemoteTrack;
use livekit::{Room, RoomEvent, RoomOptions};
use openconv_protocol::{
    AudioFormat, ConversationInitiationMetadataEvent, ServerEvent,
    TentativeUserTranscriptionEvent, UserTranscriptionEvent, VadScoreEvent,
};
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;
use transcribe::Transcriber;

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

/// What every conversation in the process shares.
///
/// Kept apart from [`Assignment`] because the two have opposite lifetimes and opposite
/// costs: an assignment is three small strings describing one call, while these are
/// heavyweight handles — a speech model is hundreds of megabytes — that are loaded once
/// and used by every call. Folding them together would either copy a model per
/// conversation or make the per-call value impossible to construct in a test.
pub struct Services {
    pub transcriber: Arc<Transcriber>,
}

/// Joins the room and serves the conversation until it ends.
pub async fn run(assignment: Assignment, services: Arc<Services>) -> Result<(), AgentError> {
    let (room, mut events) = Room::connect(&assignment.url, &assignment.token, RoomOptions::default())
        .await
        .map_err(|error| AgentError::Join(error.to_string()))?;
    let room = Arc::new(room);

    tracing::info!(conversation = %assignment.conversation_id, "agent joined");

    // Published before anyone is here to hear it, which is fine and is the point: a
    // track exists to be subscribed to, and the caller subscribes on the way in.
    let voice = Voice::publish(&room).await?;
    tokio::spawn(voice.clone().run());

    // Nothing has been said yet, because there is nobody to say it to.
    //
    // The agent is dispatched when the token is minted, so it is normally in the room
    // *first* — and the data channel does not replay. Anything published into an empty
    // room is gone, and the client's one-shot listener then never fires, hanging
    // `startSession()` exactly as sending the wrong message first would. Both failures
    // look identical from outside and neither logs anything on our side.
    //
    // So the announcement waits for an audience, and `Unannounced` is what carries
    // "nothing has been published yet" as state rather than as an assumption.
    let mut unannounced = Some(Unannounced::new(room.clone()));
    // Kept past the announcement because every later ticket publishes through it —
    // transcripts, agent responses, tool calls. Today only the VAD placeholder does.
    let mut control: Option<ControlChannel> = None;

    // Transcripts arrive from a separate task, because the caller's track is subscribed
    // before the control channel exists and inference must not run on this loop. The
    // loop below is the only thing that publishes, so ids and ordering stay in one place.
    let (heard, mut speech) = mpsc::channel::<Speech>(32);

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            Some(said) = speech.recv() => {
                publish_transcript(control.as_ref(), &assignment, said).await?;
                continue;
            }
        };

        match event {
            // Deliberately not `ParticipantConnected`, which the SDK documents as
            // firing *before* the participant can receive data messages. Announcing
            // there races the client's data channel and loses often enough to matter.
            RoomEvent::ParticipantActive(participant) => {
                let Some(pending) = unannounced.take() else { continue };

                tracing::info!(
                    conversation = %assignment.conversation_id,
                    participant = %participant.identity(),
                    "caller is listening, announcing the conversation"
                );

                control = Some(
                    pending
                        .announce(ConversationInitiationMetadataEvent {
                            conversation_id: assignment.conversation_id.clone(),
                            agent_output_audio_format: AudioFormat::Pcm48000,
                            user_input_audio_format: AudioFormat::Pcm48000,
                        })
                        .await?,
                );

                // A placeholder for ticket .6, which computes these from the caller's
                // audio. Sent after the announcement, which is the ordering that
                // matters to the client.
                if let Some(channel) = &control {
                    channel
                        .publish(&ServerEvent::VadScore {
                            vad_score_event: VadScoreEvent { vad_score: 0.0 },
                        })
                        .await?;
                }
            }

            // Someone has subscribed to the agent's track, so audio published now will
            // actually be heard. Waiting for this rather than speaking on join is the
            // same lesson as the announcement: joining and listening are different
            // moments, and only one of them is worth talking into.
            RoomEvent::LocalTrackSubscribed { .. } => {
                tracing::info!(
                    conversation = %assignment.conversation_id,
                    "caller subscribed to the agent's audio"
                );
                // Scaffolding that ticket .8 removes: proves the track carries audio
                // end to end before any speech pipeline exists to put words on it.
                voice.enqueue(&audio::tone(440.0, 400, 0.2));
            }

            // The caller's microphone. Listening runs in its own task because
            // transcription is CPU-bound and would otherwise stall this loop — and with
            // it the announcement, the disconnect handling, and every other
            // conversation's audio pump sharing the runtime.
            RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), participant, .. } => {
                tracing::info!(
                    conversation = %assignment.conversation_id,
                    participant = %participant.identity(),
                    "listening to the caller"
                );
                tokio::spawn(listen::listen(
                    track,
                    services.transcriber.clone(),
                    heard.clone(),
                ));
            }

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

    // An agent that never announced is one nobody ever joined — a token minted and not
    // used, or a caller who could not reach the SFU. It bills as a conversation either
    // way, so it is worth being able to tell the two apart afterwards.
    if control.is_none() {
        tracing::warn!(
            conversation = %assignment.conversation_id,
            "conversation ended without a caller ever joining"
        );
    }

    // Leaving explicitly rather than dropping the room: it closes the room promptly for
    // the SFU, which is what turns into the `room_finished` webhook the usage endpoint
    // bills from.
    let _ = room.close().await;

    Ok(())
}

/// Sends one transcript to the client.
///
/// The tentative/final distinction survives all the way to the wire as two different
/// message types, because the app renders them differently — settled text replaces the
/// provisional line rather than appending to it.
async fn publish_transcript(
    control: Option<&ControlChannel>,
    assignment: &Assignment,
    said: Speech,
) -> Result<(), AgentError> {
    // Speech can be transcribed before the caller is ready to receive data, since the
    // track is subscribed first. Dropping it is right — there is nowhere to send it —
    // but it is said out loud, because a transcript vanishing is otherwise invisible.
    let Some(channel) = control else {
        tracing::warn!(
            conversation = %assignment.conversation_id,
            text = said.text(),
            "heard speech before the conversation was announced; dropping it"
        );
        return Ok(());
    };

    let event_id = channel.next_event_id();
    let event = match said {
        Speech::Tentative(user_transcript) => ServerEvent::TentativeUserTranscript {
            tentative_user_transcription_event: TentativeUserTranscriptionEvent {
                user_transcript,
                event_id,
            },
        },
        Speech::Final(user_transcript) => ServerEvent::UserTranscript {
            user_transcription_event: UserTranscriptionEvent { user_transcript, event_id },
        },
    };

    channel.publish(&event).await?;
    Ok(())
}

/// Starts an agent in the background, logging rather than returning its outcome.
///
/// The caller is an HTTP handler that must answer its own request; an agent that fails
/// to join has to be visible in the logs, not in a response the client already received.
pub fn spawn(assignment: Assignment, services: Arc<Services>) {
    tokio::spawn(async move {
        let conversation = assignment.conversation_id.clone();
        if let Err(error) = run(assignment, services).await {
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
