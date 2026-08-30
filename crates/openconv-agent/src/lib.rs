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
pub mod tools;
pub mod transcribe;
pub mod tts;
pub mod vad;

use audio::Voice;
use control::{ControlChannel, PublishFailed, Unannounced};
use futures_util::stream;
use listen::{Noticed, Speech};
use livekit::track::RemoteTrack;
use livekit::{Room, RoomEvent, RoomOptions};
use openconv_protocol::{
    AgentResponseEvent, AudioFormat, ClientEvent, ClientToolCall,
    ConversationInitiationMetadataEvent, InterruptionEvent, ServerEvent,
    TentativeUserTranscriptionEvent, UserTranscriptionEvent, VadScoreEvent,
};
use speak::{Made, Spoken, Stopped, Synthesizer};
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use llm::{Llm, Piece, Reply, Turn};
use session::SessionConfig;
use tools::{Pending, Run, Then, ToolCall, ToolResult};
use transcribe::Transcriber;
use vad::{Score, SpeechDetector, VadUnavailable};

/// How long the agent waits for the app to answer a client tool call.
///
/// Generous on purpose. `sendMessageToSession` reaches a coding agent and Happy's own
/// prompt warns that it "may take a long time to return", so a tight bound here would
/// abandon calls that were going to succeed. Bounded all the same: a call that never
/// comes back would otherwise hold the turn open for the rest of the conversation, and
/// the caller would hear nothing while it did.
const TOOL_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How many times one turn may call tools and be asked again.
///
/// A turn that calls a tool is asked once more so it can say what came back, and that
/// answer may itself call another tool — a permission approved, then a message sent. The
/// ceiling exists because nothing else bounds it: a model that keeps calling would keep
/// being asked, and every pass costs a request to the model and another round trip
/// through the app. Unbounded, that is a caller sitting in silence while their coding
/// session receives the same message over and over.
const MOST_PASSES: usize = 6;

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
    // Kept past the announcement because everything the conversation says goes out
    // through it: transcripts, agent responses, scores, interruptions. Shared rather
    // than owned outright because the agent's turn runs in its own task and publishes
    // its own words from there.
    let mut control: Option<Arc<ControlChannel>> = None;

    // What the listener noticed, from a separate task: the caller's track is subscribed
    // before the control channel exists, and neither speech detection nor transcription
    // may run on this loop. The loop below is the only thing that publishes ids, so
    // ordering stays in one place.
    let (noticed, mut listening) = mpsc::channel::<Noticed>(64);

    // Ends the listener when this conversation does.
    //
    // Dropping the receiver above is not enough to end it: a listener parked on its audio
    // stream is waiting on frames, not on a send, and that stream never finishes by itself
    // — see [`listen::listen`]. Left unsaid, every call leaves a task, a native audio sink
    // and a handle on the caller's track behind for the life of the process.
    //
    // A drop guard rather than a `cancel()` at the bottom of this function: every publish
    // below returns early on failure, and cleanup that only runs on the happy path is
    // cleanup that eventually does not run.
    //
    // [LAW:no-ambient-temporal-coupling] the listener's lifetime has an owner: this scope.
    let hung_up = CancellationToken::new();
    let _ends_with_this_conversation = hung_up.clone().drop_guard();

    // The agent's own turn, reporting back from the task it runs in.
    let (from_turn, mut turn_events) = mpsc::channel::<FromTurn>(4);

    // Everything a turn is spoken through, settled once. `pending` is the seam between
    // this loop, which is the only thing that reads the data channel, and the turn task,
    // which is the only thing that waits for a tool to be answered.
    let stage = Stage {
        voice: voice.clone(),
        services: services.clone(),
        conversation: assignment.conversation_id.clone(),
        pending: Arc::new(Pending::default()),
        from_turn,
    };

    // Whether the agent is currently answering, and how to stop it. `None` is an agent
    // with nothing to interrupt; holding the token rather than a bare flag is what makes
    // barge-in one call rather than a hunt for whatever happens to be speaking.
    let mut answering: Option<CancellationToken> = None;

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

            // Nothing in this arm blocks. That is the whole reason the agent's turn runs
            // elsewhere: a loop sitting inside its own answer cannot hear the caller
            // interrupt it, and an agent that talks over the person on the line is the
            // most noticeable failure a voice product has.
            Some(noticed) = listening.recv() => {
                match noticed {
                    Noticed::Speaking(score) => {
                        publish_score(control.as_deref(), score).await?;
                    }

                    Noticed::Started => {
                        if stop_answering(&mut answering, control.as_deref()).await? {
                            tracing::info!(
                                conversation = %assignment.conversation_id,
                                "the caller spoke over the agent; stopping"
                            );
                        }
                    }

                    // A tentative transcript is published and nothing more. Only a
                    // settled one drives a turn: partials change under you, and
                    // answering one means answering half a sentence — out loud, to
                    // someone still speaking it.
                    Noticed::Said(said) => {
                        let settled = matches!(said, Speech::Final(_));
                        let text = said.text().to_owned();

                        publish_transcript(control.as_deref(), &assignment, said).await?;

                        if settled {
                            history.push(Turn::Caller(text));
                            answering = Some(start_turn(
                                &stage,
                                control.clone(),
                                &config,
                                Says::Answer(history.clone()),
                            ));
                        }
                    }
                }
                continue;
            }

            Some(from_turn) = turn_events.recv() => {
                match from_turn {
                    // Recorded when the words are known rather than when the audio has
                    // finished, so an answer the caller cuts off is still remembered as
                    // far as it got.
                    FromTurn::Said(text) => history.push(Turn::Agent(text)),
                    // Both halves at once, so the canonical history can never hold a
                    // call the API will reject for having no answer.
                    FromTurn::Used { calls, results } => {
                        history.push(Turn::Used { calls, results });
                    }
                    FromTurn::Ended => answering = None,
                }
                continue;
            }
        };

        match event {
            // Deliberately not `ParticipantConnected`, which the SDK documents as
            // firing *before* the participant can receive data messages. Announcing
            // there races the client's data channel and loses often enough to matter.
            RoomEvent::ParticipantActive(participant) => {
                let Some(announcement) = unannounced.take() else { continue };

                tracing::info!(
                    conversation = %assignment.conversation_id,
                    participant = %participant.identity(),
                    "caller is listening, announcing the conversation"
                );

                control = Some(Arc::new(
                    announcement
                        .announce(ConversationInitiationMetadataEvent {
                            conversation_id: assignment.conversation_id.clone(),
                            agent_output_audio_format: AudioFormat::Pcm48000,
                            user_input_audio_format: AudioFormat::Pcm48000,
                        })
                        .await?,
                ));

                // The channel exists now, so a greeting that was waiting on it can go.
                if let Some(greeting) = greeting_to_say.take() {
                    answering =
                        Some(start_turn(&stage, control.clone(), &config, Says::Line(greeting)));
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

            // The caller's microphone. Listening runs in its own task because speech
            // detection and transcription are CPU-bound and would otherwise stall this
            // loop — and with it the announcement, the disconnect handling, and every
            // other conversation's audio pump sharing the runtime.
            RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), participant, .. } => {
                // Attached before anything else in this arm, and before the log line that
                // announces it, because this call is where the agent stops being deaf:
                // everything published earlier is discarded by libwebrtc and nothing
                // buffers it. Attaching inside the spawned task left that moment to the
                // scheduler, and the caller's opening words in the gap.
                let ear = listen::attach(&track);

                tracing::info!(
                    conversation = %assignment.conversation_id,
                    participant = %participant.identity(),
                    "listening to the caller"
                );

                // Built here rather than inside the listener so that a model that will
                // not load ends the conversation loudly. An agent that cannot tell
                // speech from silence never decides an utterance ended, so it never
                // answers — which from the caller's side is indistinguishable from a
                // dead line, with nothing in the logs to say otherwise.
                // Wrapped in a span rather than handed the conversation id, because the id
                // is not a fact about an ear or a stretch of audio — it would be a field on
                // two types that exist to describe sound, carried only so a log line could
                // print it, and added again to the next type in that chain. A span costs
                // one wrapper here, where the id already lives, and every event emitted
                // anywhere inside the listener inherits it: the arrival lines, the closing
                // account, and the per-window scores at trace.
                //
                // [LAW:one-source-of-truth] the conversation id stays on `Assignment`.
                tokio::spawn(
                    listen::listen(
                        ear,
                        SpeechDetector::new()?,
                        services.transcriber.clone(),
                        noticed.clone(),
                        hung_up.clone(),
                    )
                    .instrument(tracing::info_span!(
                        "listening",
                        conversation = %assignment.conversation_id,
                    )),
                );
            }

            // The client's own control messages. The one that matters here is the
            // conversation configuration: the system prompt override, the first
            // message, and the dynamic variables that connect this agent to the coding
            // session it is driving.
            RoomEvent::DataReceived { payload, .. } => {
                let Some(client_event) = decode_client_event(&payload, &assignment) else {
                    continue;
                };

                let client_data = match client_event {
                    ClientEvent::ConversationInitiation(client_data) => client_data,

                    // The app has finished running a tool the agent asked for. Handed
                    // straight to whoever is waiting on it — this loop must not block on
                    // a tool, which is exactly why the turn does the waiting.
                    ClientEvent::ClientToolResult { tool_call_id, result, is_error } => {
                        tracing::info!(
                            conversation = %assignment.conversation_id,
                            %tool_call_id,
                            is_error,
                            "the app answered a tool call"
                        );
                        stage.pending.deliver(ToolResult {
                            id: tool_call_id,
                            content: result,
                            is_error,
                        });
                        continue;
                    }

                    // Context to absorb, and nothing more. The app pushes these
                    // continuously — new coding-agent messages, session focus changes,
                    // sessions coming and going — and the agent is meant to know them
                    // without remarking on them.
                    //
                    // Silence here is structural rather than something the model is asked
                    // for: no turn is started, so there is no reply to suppress. An agent
                    // that answered instead would talk over its own caller every time
                    // their session emitted anything.
                    ClientEvent::ContextualUpdate { text } => {
                        tracing::debug!(
                            conversation = %assignment.conversation_id,
                            chars = text.len(),
                            "absorbed context from the app"
                        );
                        history.push(Turn::Context(text));
                        continue;
                    }

                    // A typed turn, which the app sends when a prompt it had queued
                    // flushes. Handled exactly as a settled transcript is, because it is
                    // the same event arriving by a different road: the caller has taken
                    // the turn and is owed an answer out loud.
                    ClientEvent::UserMessage { text } => {
                        match text.filter(|text| !text.trim().is_empty()) {
                            // Nothing was said, so there is nothing to answer. Said out
                            // loud because it is drift rather than routine: the SDK marks
                            // the field optional but never omits it, so an empty one is a
                            // client this agent has stopped understanding.
                            None => tracing::warn!(
                                conversation = %assignment.conversation_id,
                                "the app sent a user_message carrying no text"
                            ),
                            Some(text) => {
                                // Superseding whatever the agent was mid-sentence about,
                                // for the same reason speaking over it does: two turns
                                // talking at once is the one thing a caller cannot listen
                                // through. The app queues prompts until the room is quiet,
                                // so this is the race it loses rather than the normal path.
                                if stop_answering(&mut answering, control.as_deref()).await? {
                                    tracing::info!(
                                        conversation = %assignment.conversation_id,
                                        "a typed message arrived mid-answer; stopping"
                                    );
                                }

                                tracing::info!(
                                    conversation = %assignment.conversation_id,
                                    chars = text.len(),
                                    "the app sent a message to answer"
                                );
                                history.push(Turn::Caller(text));
                                answering = Some(start_turn(
                                    &stage,
                                    control.clone(),
                                    &config,
                                    Says::Answer(history.clone()),
                                ));
                            }
                        }
                        continue;
                    }

                    // Every other client message belongs to a later ticket. Logged at
                    // debug so an unhandled one is findable rather than invisible.
                    _ => {
                        tracing::debug!(
                            conversation = %assignment.conversation_id,
                            "client message not handled yet"
                        );
                        continue;
                    }
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
                //
                // Not recorded in `history` here: the turn reports what it said the
                // moment it has said it, and recording it in both places would give the
                // model the greeting twice.
                if let Some(greeting) = config.first_message.clone() {
                    match control.is_some() {
                        true => {
                            answering = Some(start_turn(
                                &stage,
                                control.clone(),
                                &config,
                                Says::Line(greeting),
                            ));
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

    // A caller who left mid-answer is still owed nothing, and the answer costs money to
    // finish: the model keeps writing and every clause of it keeps being synthesized for
    // a room with nobody in it. Ending the turn is the same act as barge-in, because it
    // is the same fact — nobody is listening to the rest of this.
    if let Some(turn) = answering.take() {
        turn.cancel();
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
            // The error and the size, not the payload. A message that failed to parse is
            // the one most likely to be a half-formed prompt override, and this log is on
            // by default. Nothing diagnostic is lost: serde names the field it did not
            // expect and where it found it, which is what says a client has drifted.
            tracing::warn!(
                conversation = %assignment.conversation_id,
                %error,
                chars = payload.len(),
                "could not read a client control message"
            );
            None
        }
    }
}

/// What the agent is about to say.
///
/// The two ways a turn begins, as a value rather than as two functions: a configured
/// greeting and a model's answer differ only in where the words come from, and they go
/// through the same clause splitting, the same synthesis and the same ordering after
/// that. A second, quieter speech path beside the first is one nobody exercises until a
/// first message is configured, which is when it breaks.
enum Says {
    /// A line settled in advance — the client's configured first message.
    Line(String),
    /// Whatever the model makes of the conversation so far.
    ///
    /// Carries its own copy of the history because it is read inside a task that
    /// outlives this call. The conversation loop stays the one owner of the real one.
    Answer(Vec<Turn>),
}

/// What the agent's turn reports back to the conversation.
///
/// The conversation loop owns history; the turn runs elsewhere and can only tell it
/// what happened. Every variant here is one thing to append.
#[derive(Debug)]
enum FromTurn {
    /// What the model wrote, as soon as it had written it.
    Said(String),
    /// Tools the model asked for and what they returned — never one without the other.
    ///
    /// Reported once the results are in hand rather than when the calls were made, so
    /// history cannot come to hold a `tool_use` with nothing answering it. The API
    /// rejects the whole conversation in that state, so a turn cut off mid-call would
    /// otherwise break every turn after it.
    Used { calls: Vec<ToolCall>, results: Vec<ToolResult> },
    /// The turn is over: every clause queued, or the caller cut it off.
    Ended,
}

/// What every turn in one conversation is spoken through.
///
/// The mouth, the models, the room's name and the two channels a turn reports back on
/// are all settled when the agent joins and never change after. Bundling them is what
/// keeps the difference between the four ways a turn begins down to the two things that
/// actually differ — what is being said, and the configuration in force when it is.
struct Stage {
    voice: Voice,
    services: Arc<Services>,
    conversation: String,
    /// Client tool calls in flight, shared with the loop that reads their answers.
    pending: Arc<Pending>,
    from_turn: mpsc::Sender<FromTurn>,
}

/// Starts the agent's turn and hands back the token that stops it.
///
/// Spawned rather than awaited, because a conversation loop sitting inside its own
/// answer cannot hear the caller interrupt it. Everything the turn needs is owned or
/// cloned for the same reason: it outlives this call.
///
/// A turn that fails is said out loud in the logs and then dropped. The alternative —
/// ending the conversation — would hang up on someone mid-sentence over one bad
/// response, when the next thing they say may well work.
fn start_turn(
    stage: &Stage,
    control: Option<Arc<ControlChannel>>,
    config: &SessionConfig,
    says: Says,
) -> CancellationToken {
    let interrupted = CancellationToken::new();

    tokio::spawn({
        let interrupted = interrupted.clone();
        let voice = stage.voice.clone();
        let services = stage.services.clone();
        let pending = stage.pending.clone();
        let from_turn = stage.from_turn.clone();
        let config = config.clone();
        let conversation = stage.conversation.clone();

        async move {
            let started = std::time::Instant::now();

            // The turn's own copy of the conversation, which it extends as it goes. The
            // loop below owns the real one and is told about every addition — a turn
            // that ran two tools has to ask the model again with their results in hand,
            // and it cannot reach into history to put them there.
            let (mut history, mut opening) = match says {
                Says::Answer(history) => (history, None),
                Says::Line(text) => (Vec::new(), Some(text)),
            };

            // One pass per thing the model has to say. Most turns take one; a turn that
            // calls a tool takes another to say what came back, which is the whole
            // point — "sent" is only worth saying once the message actually went.
            for pass in 1..=MOST_PASSES {
                let (spoken, speaking) = {
                    let reply: Reply<'_> = match opening.take() {
                        // A configured greeting has no conversation behind it and
                        // nothing to call, so it never reaches the model at all.
                        Some(text) => fixed(text),
                        None => services.llm.respond(
                            &config.system_prompt,
                            &history,
                            tools::declarations(),
                        ),
                    };

                    speak::speak(
                        &voice,
                        services.tts.clone(),
                        config.voice_id.clone(),
                        interrupted.clone(),
                        reply,
                    )
                    .await
                };

                let Some(made) = made_of(spoken, &conversation) else { break };

                // Reported the moment the words are known, before the audio has finished
                // going out. Waiting would show the caller's app the agent's message
                // several seconds after they heard it spoken — and would lose it
                // entirely when they interrupt.
                if let Some(text) = made.text {
                    if let Err(error) = publish_response(control.as_deref(), &text).await {
                        // A cancelled turn publishing into a room that is closing is
                        // what hanging up mid-answer looks like from here, and it is not
                        // a fault. The state is carried rather than guessed at, so the
                        // same message at the same level does not mean two different
                        // things.
                        tracing::warn!(
                            conversation = %conversation,
                            %error,
                            interrupted = interrupted.is_cancelled(),
                            "could not publish the agent's reply"
                        );
                    }
                    history.push(Turn::Agent(text.clone()));
                    let _ = from_turn.send(FromTurn::Said(text)).await;
                }

                // Held until this pass's audio has all been queued — or until the caller
                // talks over it — so the next pass cannot start speaking on top of it.
                speaking.finish().await;

                if made.calls.is_empty() {
                    break;
                }

                // Deliberately not cancelled with the rest of the turn. By the time a
                // call is out, the app has been asked to do the thing — a message has
                // gone to a coding session, a permission has been answered — and
                // stopping the wait cannot un-do it. Dropping the answer would leave the
                // model believing the tool never ran, so it would call it again: the
                // caller's session gets the same message twice for having interrupted.
                let (results, then) =
                    run_tools(&pending, control.as_deref(), &made.calls, &conversation).await;

                history.push(Turn::Used { calls: made.calls.clone(), results: results.clone() });
                let _ = from_turn
                    .send(FromTurn::Used { calls: made.calls, results })
                    .await;

                // `Stop` is `skip_turn`: the model has decided this turn was not the
                // agent's to take, so there is nothing further to say. Interruption ends
                // the turn here too — the results are recorded, and the caller who talked
                // over the agent is not owed a reply to a question they moved on from.
                if then == Then::Stop || interrupted.is_cancelled() {
                    break;
                }

                // Loud, because it is not a thing that should happen: the model is
                // calling tools without ever settling on something to say, and the
                // caller has been listening to nothing while it did.
                if pass == MOST_PASSES {
                    tracing::error!(
                        conversation = %conversation,
                        passes = MOST_PASSES,
                        "the model kept calling tools; giving up on this turn"
                    );
                }
            }

            tracing::info!(
                conversation = %conversation,
                took_ms = started.elapsed().as_millis(),
                interrupted = interrupted.is_cancelled(),
                "turn over"
            );

            let _ = from_turn.send(FromTurn::Ended).await;
        }
    });

    interrupted
}

/// Runs every tool the model asked for, and says whether the turn goes on.
///
/// The calls go out together rather than one after another: the model may ask for
/// several at once, and `sendMessageToSession` is slow enough that running two in
/// sequence would be heard as a pause twice as long as it needs to be. Results come
/// back in the order they were asked for, which is the order the API pairs them in.
async fn run_tools(
    pending: &Pending,
    control: Option<&ControlChannel>,
    calls: &[ToolCall],
    conversation: &str,
) -> (Vec<ToolResult>, Then) {
    let running = calls
        .iter()
        .map(|call| run_one(pending, control, call, conversation));
    let ran: Vec<(ToolResult, Then)> = futures_util::future::join_all(running).await;

    // One tool that ends the turn ends it. There is no sensible way to both fall silent
    // and go on talking, and `skip_turn` alongside anything else is the model hedging.
    let then = match ran.iter().any(|(_, then)| *then == Then::Stop) {
        true => Then::Stop,
        false => Then::Answer,
    };

    (ran.into_iter().map(|(result, _)| result).collect(), then)
}

/// Runs one tool wherever it runs.
async fn run_one(
    pending: &Pending,
    control: Option<&ControlChannel>,
    call: &ToolCall,
    conversation: &str,
) -> (ToolResult, Then) {
    // A name this agent does not have is the model inventing a tool, or a prompt naming
    // one that was never declared. Told rather than dropped: a call that vanishes leaves
    // the model waiting on an answer that is never coming.
    let Some(tool) = tools::named(&call.name) else {
        tracing::warn!(conversation, tool = %call.name, "the model asked for a tool that does not exist");
        return (
            ToolResult {
                id: call.id.clone(),
                content: format!("There is no tool called {}.", call.name),
                is_error: true,
            },
            Then::Answer,
        );
    };

    let content = match tool.run {
        Run::Here(answer) => {
            tracing::info!(conversation, tool = %tool.name, "answered without the client");
            Ok(answer.to_owned())
        }
        Run::OnTheClient => ask_the_client(pending, control, call, conversation).await,
    };

    let result = match content {
        Ok(content) => ToolResult { id: call.id.clone(), content, is_error: false },
        Err(why) => ToolResult { id: call.id.clone(), content: why, is_error: true },
    };

    (result, tool.then)
}

/// Publishes one tool call and waits for the app to answer it.
///
/// Interest is registered before the call goes out, because the app can answer faster
/// than this task is next scheduled and an answer nobody is waiting for is one that is
/// lost.
async fn ask_the_client(
    pending: &Pending,
    control: Option<&ControlChannel>,
    call: &ToolCall,
    conversation: &str,
) -> Result<String, String> {
    let Some(channel) = control else {
        // Nothing has been announced, so there is nobody to run it. Only reachable if
        // the model calls a tool before the caller has joined, which the greeting path
        // makes possible.
        return Err("The app is not connected, so this could not be run.".to_owned());
    };

    let awaited = pending.expect(&call.id);

    let published = channel
        .publish(&ServerEvent::ClientToolCall {
            client_tool_call: ClientToolCall {
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                parameters: call.input.clone(),
                event_id: channel.next_event_id(),
            },
        })
        .await;

    if let Err(error) = published {
        pending.give_up(&call.id);
        tracing::error!(conversation, tool = %call.name, %error, "could not send a tool call to the app");
        return Err(format!("This could not be sent to the app: {error}"));
    }

    tracing::info!(conversation, tool = %call.name, tool_call_id = %call.id, "asked the app to run a tool");

    match tokio::time::timeout(TOOL_ANSWER_TIMEOUT, awaited).await {
        Ok(Ok(result)) => match result.is_error {
            true => Err(result.content),
            false => Ok(result.content),
        },
        // The conversation ended underneath the wait.
        Ok(Err(_)) => {
            tracing::warn!(conversation, tool = %call.name, "the conversation ended before the app answered");
            Err("The conversation ended before this finished.".to_owned())
        }
        Err(_) => {
            pending.give_up(&call.id);
            tracing::error!(
                conversation,
                tool = %call.name,
                seconds = TOOL_ANSWER_TIMEOUT.as_secs(),
                "the app never answered a tool call"
            );
            Err("The app did not answer in time, so this may not have run.".to_owned())
        }
    }
}

/// What one pass of the model produced, with anything that went wrong said out loud.
///
/// A reply that broke and a reply the caller talked over both end early, and only one of
/// them is a fault. Logging them alike is how the fault gets lost: barge-in is routine
/// in a working conversation, so an error level shared with it stops meaning anything.
fn made_of(spoken: Spoken, conversation: &str) -> Option<Made> {
    match spoken {
        Spoken::Nothing(Stopped::Interrupted) => {
            tracing::info!(conversation, "the caller cut in before the agent had an answer");
            None
        }
        Spoken::Nothing(Stopped::Failed(error)) => {
            tracing::error!(conversation, %error, "could not answer the caller");
            None
        }
        Spoken::Did(made) => {
            match &made.cut_short {
                None | Some(Stopped::Interrupted) => {}
                // Words already going out, with the rest of the sentence missing. Worth
                // an error even though the turn is not abandoned: the caller hears a
                // reply that stops mid-thought and nothing else would explain why.
                Some(Stopped::Failed(error)) => {
                    tracing::error!(conversation, %error, "the reply was cut short partway through");
                }
            }
            Some(made)
        }
    }
}

/// Tells the client how likely the caller is speaking, which drives its microphone
/// indicator.
///
/// Sent on the cadence [`vad::Reporter`] sets rather than on every frame — the app
/// debounces over 300 ms before it acts on one, so a hundred a second would be a hundred
/// data messages a second to make the same decision.
async fn publish_score(
    control: Option<&ControlChannel>,
    score: Score,
) -> Result<(), AgentError> {
    // Scores start the moment the track is subscribed, which is before the caller can
    // receive data. There is nothing to say about a level nobody can see, and unlike a
    // dropped transcript nothing is lost by it — another arrives in a tenth of a second.
    let Some(channel) = control else { return Ok(()) };

    channel
        .publish(&ServerEvent::VadScore {
            vad_score_event: VadScoreEvent { vad_score: score.as_f64() },
        })
        .await?;
    Ok(())
}

/// Stops whatever the agent is saying, because the caller has taken the turn.
///
/// Cancelling and telling the client are one act rather than two: the token stops the
/// agent sending, and only the interruption event makes the client drop the audio it has
/// already buffered. A caller who does one without the other keeps hearing a reply the
/// agent abandoned seconds ago.
///
/// Reports whether there was anything to stop, so the two ways a turn gets taken — spoken
/// over, or typed over — can say which one happened without each keeping its own idea of
/// what stopping involves.
async fn stop_answering(
    answering: &mut Option<CancellationToken>,
    control: Option<&ControlChannel>,
) -> Result<bool, AgentError> {
    match answering.take() {
        // The caller is opening a turn, not cutting one off.
        None => Ok(false),
        Some(turn) => {
            turn.cancel();
            publish_interruption(control).await?;
            Ok(true)
        }
    }
}

/// Tells the client its agent has been cut off.
///
/// The client drops the audio it has buffered when it sees this. Without it the caller
/// keeps hearing the abandoned reply out of their own speaker for as long as their
/// buffer holds, however promptly the agent stopped sending.
async fn publish_interruption(control: Option<&ControlChannel>) -> Result<(), AgentError> {
    let Some(channel) = control else {
        tracing::warn!("interrupted a reply before the conversation was announced");
        return Ok(());
    };

    channel
        .publish(&ServerEvent::Interruption {
            interruption_event: InterruptionEvent { event_id: channel.next_event_id() },
        })
        .await?;
    Ok(())
}

/// Sends the agent's words to the client, which renders them beside the caller's own.
async fn publish_response(
    control: Option<&ControlChannel>,
    text: &str,
) -> Result<(), AgentError> {
    let Some(channel) = control else {
        // Length, not words — see `publish_transcript` for why.
        tracing::warn!(
            chars = text.len(),
            "had something to say before the conversation was announced"
        );
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
    Box::pin(stream::once(async move { Ok(Piece::Say(text)) }))
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
        // The length and not the words. What is worth knowing here is that a transcript
        // vanished, and the caller may be discussing anything at all in their coding
        // session — the rest of this module logs `chars` for exactly that reason, and
        // these two were the exceptions until the agent's logs were on by default.
        tracing::warn!(
            conversation = %assignment.conversation_id,
            chars = said.text().len(),
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
    Vad(VadUnavailable),
}

impl From<PublishFailed> for AgentError {
    fn from(error: PublishFailed) -> Self {
        Self::Publish(error)
    }
}

impl From<VadUnavailable> for AgentError {
    fn from(error: VadUnavailable) -> Self {
        Self::Vad(error)
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
            Self::Vad(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AgentError {}
