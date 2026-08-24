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
pub mod clause;
pub mod control;
pub mod endpoint;
pub mod listen;
pub mod llm;
pub mod resample;
pub mod session;
pub mod speak;
pub mod transcribe;
pub mod tts;

use audio::Voice;
use control::{ControlChannel, PublishFailed, Unannounced};
use futures_util::stream;
use listen::Speech;
use livekit::track::RemoteTrack;
use livekit::{Room, RoomEvent, RoomOptions};
use openconv_protocol::{
    AgentResponseEvent, AudioFormat, ClientEvent, ConversationInitiationMetadataEvent,
    ServerEvent, TentativeUserTranscriptionEvent, UserTranscriptionEvent, VadScoreEvent,
};
use speak::{Spoken, Synthesizer};
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;
use llm::{Llm, Reply, Turn};
use session::SessionConfig;
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
    pub llm: Arc<dyn Llm>,
    /// Turns the model's words into audio. Shared rather than per-conversation because
    /// it is a client for a service, and one connection pool serves every call.
    pub tts: Arc<dyn Synthesizer>,
    /// Used when the client sends no system prompt override of its own.
    pub default_prompt: Arc<str>,
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

    // The conversation as the model will see it. Held here rather than in the LLM
    // client because the client answers one question at a time and holds no session —
    // making it stateless is what lets a second conversation share it.
    let mut history: Vec<Turn> = Vec::new();

    // Settled once, when the client tells us what this conversation is. Until then the
    // default prompt stands, which matters only if the caller speaks before their SDK
    // has sent its configuration.
    let mut config = SessionConfig::settle(&services.default_prompt, Default::default());

    // A configured greeting that arrived before there was anyone to say it to. See the
    // `DataReceived` arm below — the client's configuration normally lands first.
    let mut greeting_to_say: Option<String> = None;

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            Some(said) = speech.recv() => {
                // A tentative transcript is published and nothing more. Only a settled
                // one drives a turn: partials change under you, and answering one means
                // answering half a sentence — out loud, to someone still speaking it.
                let settled = matches!(said, Speech::Final(_));
                let text = said.text().to_owned();

                publish_transcript(control.as_ref(), &assignment, said).await?;

                if settled {
                    take_turn(
                        &voice,
                        control.as_ref(),
                        &services,
                        &config,
                        &mut history,
                        &assignment,
                        text,
                    )
                    .await?;
                }
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

                // The channel exists now, so a greeting that was waiting on it can go.
                if let Some(greeting) = greeting_to_say.take() {
                    say(&voice, control.as_ref(), &services, &config, &assignment, fixed(greeting))
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

            // The client's own control messages. The one that matters here is the
            // conversation configuration: the system prompt override, the first
            // message, and the dynamic variables that connect this agent to the coding
            // session it is driving.
            RoomEvent::DataReceived { payload, .. } => {
                let Some(client_event) = decode_client_event(&payload, &assignment) else {
                    continue;
                };

                let ClientEvent::ConversationInitiation(client_data) = client_event else {
                    // Every other client message belongs to a later ticket. Logged at
                    // debug so an unhandled one is findable rather than invisible.
                    tracing::debug!(
                        conversation = %assignment.conversation_id,
                        "client message not handled yet"
                    );
                    continue;
                };

                config = SessionConfig::settle(&services.default_prompt, *client_data);
                tracing::info!(
                    conversation = %assignment.conversation_id,
                    prompt_chars = config.system_prompt.len(),
                    has_first_message = config.first_message.is_some(),
                    "conversation configured by the client"
                );

                // Said before the caller says anything, which is the whole point of a
                // first message — it opens the conversation rather than answering it.
                //
                // The client routinely sends its configuration *before* the agent has
                // announced, so this greeting usually has nowhere to go yet. It waits
                // rather than being dropped: a first message that never arrives leaves
                // the caller listening to silence wondering whether anything is there.
                if let Some(greeting) = config.first_message.clone() {
                    history.push(Turn::Agent(greeting.clone()));
                    match control.is_some() {
                        true => {
                            say(
                                &voice,
                                control.as_ref(),
                                &services,
                                &config,
                                &assignment,
                                fixed(greeting),
                            )
                            .await?;
                        }
                        false => greeting_to_say = Some(greeting),
                    }
                }
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

/// Reads one control message from the client.
///
/// Returns `None` for anything unreadable rather than ending the conversation. The data
/// channel is shared, and a message this agent cannot parse is far more likely to be
/// something it was never meant to read than a reason to hang up — but it is logged,
/// because a client message being silently ignored is exactly how a configuration
/// override goes missing.
fn decode_client_event(payload: &[u8], assignment: &Assignment) -> Option<ClientEvent> {
    match serde_json::from_slice(payload) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(
                conversation = %assignment.conversation_id,
                %error,
                raw = %String::from_utf8_lossy(payload).chars().take(200).collect::<String>(),
                "could not read a client control message"
            );
            None
        }
    }
}

/// Answers the caller.
///
/// The whole turn: record what was said, ask the model, speak what comes back as it is
/// written, and record it so the next turn has it.
///
/// A turn that fails is said out loud in the logs and then dropped. The alternative —
/// ending the conversation — would hang up on someone mid-sentence over one bad
/// response, when the next thing they say may well work.
async fn take_turn(
    voice: &Voice,
    control: Option<&ControlChannel>,
    services: &Services,
    config: &SessionConfig,
    history: &mut Vec<Turn>,
    assignment: &Assignment,
    said: String,
) -> Result<(), AgentError> {
    history.push(Turn::Caller(said));

    let started = std::time::Instant::now();
    // Borrows `history` for as long as the reply is being read, which is why the
    // agent's own turn is recorded after `say` returns rather than before.
    let reply = services.llm.respond(&config.system_prompt, history);
    let answered = say(voice, control, services, config, assignment, reply).await?;

    tracing::info!(
        conversation = %assignment.conversation_id,
        took_ms = started.elapsed().as_millis(),
        "answered"
    );

    history.extend(answered.map(Turn::Agent));
    Ok(())
}

/// Says a reply out loud and publishes the words, returning what was said.
///
/// The one path for everything the agent says. A configured greeting is a reply of a
/// single fixed piece, so it goes through the same clause splitting, the same synthesis
/// and the same ordering as a model's answer — rather than a second, quieter path
/// beside them that nobody exercises until a first message is configured.
///
/// The transcript is published as soon as the words are known, before the audio has
/// finished going out. Waiting would show the caller's app the agent's message several
/// seconds after they heard it spoken.
async fn say(
    voice: &Voice,
    control: Option<&ControlChannel>,
    services: &Services,
    config: &SessionConfig,
    assignment: &Assignment,
    reply: Reply<'_>,
) -> Result<Option<String>, AgentError> {
    let (spoken, speaking) = speak::speak(
        voice,
        services.tts.clone(),
        config.voice_id.clone(),
        reply,
    )
    .await;

    let text = match spoken {
        Spoken::Nothing(error) => {
            tracing::error!(
                conversation = %assignment.conversation_id,
                %error,
                "could not answer the caller"
            );
            None
        }
        Spoken::Said { text, cut_short } => {
            // Words already going out, with the rest of the sentence missing. Worth an
            // error even though the turn is not abandoned: the caller hears a reply
            // that stops mid-thought and nothing else would explain why.
            if let Some(error) = cut_short {
                tracing::error!(
                    conversation = %assignment.conversation_id,
                    %error,
                    "the reply was cut short partway through"
                );
            }
            publish_response(control, &text).await?;
            Some(text)
        }
    };

    // Held until the audio has all been queued, so the next turn cannot start speaking
    // over this one.
    speaking.finish().await;
    Ok(text)
}

/// Sends the agent's words to the client, which renders them beside the caller's own.
async fn publish_response(
    control: Option<&ControlChannel>,
    text: &str,
) -> Result<(), AgentError> {
    let Some(channel) = control else {
        tracing::warn!(text, "had something to say before the conversation was announced");
        return Ok(());
    };

    channel
        .publish(&ServerEvent::AgentResponse {
            agent_response_event: AgentResponseEvent {
                agent_response: text.to_owned(),
                event_id: channel.next_event_id(),
            },
        })
        .await?;
    Ok(())
}

/// A fixed line, in the shape a reply arrives in.
///
/// What lets a configured greeting reuse the whole speech path rather than needing one
/// of its own.
fn fixed(text: String) -> Reply<'static> {
    Box::pin(stream::once(async move { Ok(text) }))
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
