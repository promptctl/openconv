//! Saying a reply out loud while it is still being written.
//!
//! Three things have to be true at once, and they pull against each other:
//!
//! 1. **Start early.** The first clause goes to synthesis as soon as the model has
//!    written it, not when the reply is finished. That is the second of silence this
//!    module exists to remove.
//! 2. **Overlap.** Clauses are in flight together rather than one after another.
//!
//!    Read the reason carefully, because it has changed and the old one inverts the
//!    design. Against elvenreader-server synthesis cost a few seconds of *fixed*
//!    overhead per request, so cutting a reply into three clauses tripled that overhead
//!    and overlapping was what paid it back. Against elvenspeak the cost is proportional
//!    and small — see [`crate::tts`] for the measurement — so serial synthesis would
//!    already keep well ahead of playback, and overlap no longer buys throughput.
//!
//!    It is kept for what it still buys: the burst at the start of a reply, where
//!    several short clauses land at once and the caller is waiting on the first sound.
//!    That moment is the one still worth optimizing, because it is the only one that is
//!    close — the first clause's audio lands within a couple of tenths either side of
//!    the model finishing. See [`IN_FLIGHT`], now a bound rather than a throughput knob.
//! 3. **Stay in order.** Overlapping requests do not finish in the order they were
//!    sent, and a reply whose sentences arrive shuffled is worse than a slow one.
//!
//! Ordering is the one that cannot be left to chance, so it is owned rather than
//! observed: clauses are handed to a single drain in dispatch order, and the drain waits
//! for each in turn before queueing the next onto the track. Nothing depends on which
//! request happens to come back first.
//!
//! # Why the text comes back before the audio has gone out
//!
//! [`speak`] returns as soon as the model has finished writing, handing back a
//! [`Speaking`] for audio still on its way. The caller publishes the transcript at that
//! moment — the app would otherwise show the agent's words several seconds after the
//! caller heard them — and awaits the [`Speaking`] before starting another turn, which
//! is what keeps two replies from talking over each other.
//!
//! # Being talked over
//!
//! A caller who starts speaking has decided not to hear the rest of this reply, and the
//! audio already queued has to go. The drain is what does it, rather than whoever
//! noticed the interruption: the drain is the only writer to the track's queue, so it is
//! the only thing that can throw the queue away without racing another write into it.
//! Cancellation reaches it as a value — the token handed to [`speak`] — for the same
//! reason.

use crate::audio::Voice;
use crate::clause::Clauses;
use crate::llm::{LlmError, Piece, Reply};
use crate::tools::ToolCall;
use crate::tts::TtsError;
use openconv_protocol::Language;
use futures_util::{Stream, StreamExt};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How many clauses may be synthesized at once.
///
/// This is a ceiling, not a target. Against a local engine that synthesizes far faster
/// than the caller can listen, the queue rarely reaches it; what the number actually
/// does is stop one caller opening a dozen connections to a text-to-speech server shared
/// with every other conversation.
///
/// Raising it does not make a reply start sooner — that is decided by when the first
/// clause is written, not by how many follow it — so tune it against the server's
/// concurrency, not against latency.
const IN_FLIGHT: usize = 4;

/// How much of a clause may be decoded ahead of the one being spoken.
///
/// Chunks are about a fifth of a second, so this is a few seconds of run-up — enough
/// that a clause waiting its turn arrives fully formed, and bounded so a reply cannot
/// hold minutes of audio in memory while the first sentence plays.
const READ_AHEAD: usize = 32;

/// Something that turns text into samples for the agent's track.
///
/// A trait for the same reason [`crate::llm::Llm`] is one: it makes the engine a value.
/// Here it also makes the ordering guarantee above testable, which it otherwise would
/// not be — the failure it prevents only shows up when requests finish out of order.
pub trait Synthesizer: Send + Sync + 'static {
    /// Owned arguments so the returned stream outlives this call and can be spawned.
    fn speak(&self, voicing: Voicing, text: String) -> Speech;
}

/// Which voice speaks, which engine speaks it, and which language it is speaking.
///
/// One value rather than three arguments, because these travel together through five
/// signatures and two of them are both `Option<String>`: as separate parameters nothing
/// but argument order tells those two apart, and swapping them compiles. Named, that
/// mistake stops being expressible.
///
/// All three are the client's, carried untranslated. ElevenLabs models them as
/// independent axes, and the text-to-speech server owns what any of them means —
/// including which it refuses. None is resolved here; a table on this side would be a
/// second answer to a question that server already answers.
///
/// The language is this crate's own closed union rather than a string, unlike the two
/// ids beside it, and the difference is the direction each one faces. A voice or engine
/// id is the *server's* vocabulary — an open set this crate must not have an opinion
/// about. A language is the client's, out of the published list [`Language`] already is,
/// so keeping it typed to the moment it is serialized means the spelling on the wire has
/// exactly one source: that enum's own serde renames, where `pt-br` lives.
///
/// `None` is "the client asked for no particular one", which is not the same as asking
/// for a default: the default belongs to whoever is serving, so it is applied there and
/// not invented here. That matters most for the language, because there is a plausible
/// default to invent — every conversation before this field existed was in English —
/// and inventing it would send `en` on behalf of an agent that configured nothing,
/// pinning to English the deployments that today let the server decide.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Voicing {
    pub voice_id: Option<String>,
    pub model_id: Option<String>,
    pub language: Option<Language>,
}

/// One clause's audio, arriving as it is decoded rather than all at the end.
///
/// A stream rather than a `Vec` because more than a second of every synthesis request
/// is spent receiving audio that could already be playing — see [`crate::tts`].
pub type Speech = Pin<Box<dyn Stream<Item = Result<Vec<i16>, TtsError>> + Send>>;

/// Everything a clause decodes to, gathered into one piece.
///
/// The pipeline never does this — queueing each stretch as it arrives is the point. It
/// is for callers that want the audio as a thing rather than as it happens: writing it
/// to a file, measuring it in a test.
pub async fn collect(mut speech: Speech) -> Result<Vec<i16>, TtsError> {
    let mut samples = Vec::new();
    while let Some(chunk) = speech.next().await {
        samples.extend(chunk?);
    }
    Ok(samples)
}

/// Why a reply ended before the model had finished writing it.
///
/// Two different facts that a bare error would collapse into one: a reply that broke is
/// a fault worth an alarm, and a reply the caller talked over is the product working.
/// Reported at the same level, the first becomes invisible in a log full of the second.
#[derive(Debug)]
pub enum Stopped {
    /// The reply stream broke, or the model had nothing to say.
    Failed(LlmError),
    /// The caller started talking, so the rest was never going to be heard.
    Interrupted,
}

/// What the model produced.
///
/// Two variants rather than a bag of maybes: a turn either produced something — words,
/// tool calls, or both — or it produced a reason it did not. Splitting those apart is
/// what stops "nothing happened" and "nothing happened *because*" from being the same
/// value, which is how a broken turn comes to look like a quiet one.
#[derive(Debug)]
pub enum Spoken {
    /// Words, calls, or both, with the reason it stopped if it stopped early.
    Did(Made),
    /// The turn yielded neither words nor calls, and why.
    Nothing(Stopped),
}

/// Everything one pass of the model produced.
///
/// At least one of `text` and `calls` is always present: a pass that produced neither
/// is [`Spoken::Nothing`] instead. That is settled once, where [`speak`] returns, and
/// nothing downstream re-checks it.
#[derive(Debug)]
pub struct Made {
    /// The words said out loud, absent when the model only asked for tools.
    ///
    /// Optional rather than empty-string, because a turn that spoke nothing and a turn
    /// that spoke an empty string mean different things to the caller's transcript —
    /// one shows no bubble at all, the other shows a blank one.
    pub text: Option<String>,
    /// The tools the model asked for, in the order it asked for them.
    pub calls: Vec<ToolCall>,
    /// Why it ended before the model had finished, if it did.
    pub cut_short: Option<Stopped>,
}

/// Audio still on its way out.
///
/// Held by whoever started the turn, and awaited before the next one begins. Dropping it
/// abandons the *wait*, not the speech — the audio keeps going out, which is why the
/// caller is expected to await it rather than let it fall out of scope.
#[must_use = "a dropped Speaking lets the next turn talk over this one"]
#[derive(Debug)]
pub struct Speaking {
    drain: JoinHandle<()>,
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(error) => write!(f, "{error}"),
            Self::Interrupted => write!(f, "the caller started talking"),
        }
    }
}

impl Speaking {
    /// Waits until every clause of this reply has been queued onto the track.
    pub async fn finish(self) {
        // The drain only ends by running out of clauses. A join error is the runtime
        // shutting down underneath it, which the conversation is ending for anyway.
        let _ = self.drain.await;
    }
}

/// Speaks a reply as it is written.
///
/// Cancelling `interrupted` stops the audio and drops whatever of this reply had been
/// queued and not yet heard.
///
/// Cancellation always beats the model, never sometimes: a token already cancelled when
/// `speak` is called is a turn that never starts, and comes back as
/// [`Spoken::Nothing`] carrying [`Stopped::Interrupted`], never as a reply nobody was
/// ever going to hear.
pub async fn speak(
    voice: &Voice,
    synthesizer: Arc<dyn Synthesizer>,
    voicing: Voicing,
    interrupted: CancellationToken,
    mut reply: Reply<'_>,
) -> (Spoken, Speaking) {
    // What the caller is actually waiting through, split where it can be acted on. The
    // endpointer's silence window is measurable from the listener's own log; these two
    // are not, and without them "the agent takes too long to answer" cannot be told from
    // "the model takes too long to start" or "synthesis takes too long to sound".
    let started = Instant::now();

    let (dispatch, mut pending) =
        mpsc::channel::<mpsc::Receiver<Result<Vec<i16>, TtsError>>>(IN_FLIGHT);

    // The single owner of what the caller hears, in the order they hear it, and the
    // single writer to the queue behind it — which is what lets it also be the thing
    // that throws the queue away when the caller talks over the reply.
    let drain = tokio::spawn({
        let voice = voice.clone();
        let interrupted = interrupted.clone();
        async move {
            // `biased`, so cancellation is read before the queue rather than alongside
            // it. Both are ready together whenever the caller cuts in as the last clause
            // lands, and there the caller's decision is the one that counts.
            tokio::select! {
                biased;
                _ = interrupted.cancelled() => voice.silence(),
                _ = queue_in_order(&voice, &mut pending, started) => {}
            }
        }
    });

    let mut clauses = Clauses::new();
    let mut said = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut cut_short: Option<Stopped> = None;

    loop {
        // Reading stops the moment the caller cuts in. The words already written stay
        // in `said` and are still reported: the caller heard some of them, and a
        // transcript that omits what was spoken is worse than one that includes it.
        //
        // `biased`, so cancellation is checked before the next piece rather than
        // alongside it. The two are ready in the same poll whenever the caller cut in
        // while a piece was already waiting — and, for a token cancelled before `speak`
        // was called, on the first pass of every such turn. An unbiased select picks
        // between ready branches at random, which would leave "the caller is talking"
        // meaning *stop* on some runs and *read one more piece* on others. Biased, it
        // means stop, always.
        let piece = tokio::select! {
            biased;
            _ = interrupted.cancelled() => {
                cut_short = Some(Stopped::Interrupted);
                break;
            }
            piece = reply.next() => match piece {
                Some(piece) => piece,
                None => break,
            },
        };

        let piece = match piece {
            Ok(piece) => piece,
            Err(error) => {
                cut_short = Some(Stopped::Failed(error));
                break;
            }
        };

        match piece {
            Piece::Say(text) => {
                // Splits the wait in two at the only place it can be split. Everything
                // before this is the model thinking; everything between here and
                // `first_audio_ms` is synthesis. They are fixed by different work, and
                // one number covering both says which is worth doing.
                if said.is_empty() && !text.is_empty() {
                    tracing::info!(
                        first_word_ms = started.elapsed().as_millis(),
                        "the model wrote its first words"
                    );
                }

                said.push_str(&text);
                for clause in clauses.push(&text) {
                    send(&dispatch, &synthesizer, &voicing, clause).await;
                }
            }
            // Gathered, never synthesized. A tool call is the model addressing the
            // agent, not the caller, and reading one out loud is the failure this
            // separation exists to make impossible.
            Piece::Call(call) => calls.push(call),
        }
    }

    // Always flushed, never conditionally: the reply's last sentence has no whitespace
    // after its full stop, so it is still held here on every turn — and on a broken one,
    // so is whatever was written before the break. An empty buffer yields nothing.
    if let Some(clause) = clauses.flush() {
        send(&dispatch, &synthesizer, &voicing, clause).await;
    }

    // Closes the drain once it has taken everything already dispatched.
    drop(dispatch);

    // The one place the "at least one of words or calls" invariant on [`Made`] is
    // settled. A pass that only called a tool is not empty — `skip_turn` is exactly
    // that, and it is the model working, not failing.
    let produced = !said.is_empty() || !calls.is_empty();
    let spoken = match (produced, cut_short) {
        (true, cut_short) => Spoken::Did(Made {
            text: (!said.is_empty()).then_some(said),
            calls,
            cut_short,
        }),
        // The stream's contract: a pass that produced nothing ends with a reason.
        (false, Some(reason)) => Spoken::Nothing(reason),
        (false, None) => Spoken::Nothing(Stopped::Failed(LlmError::Empty)),
    };

    (spoken, Speaking { drain })
}

/// Queues every clause onto the track, each one drained to its end before the next is
/// taken.
///
/// The whole ordering guarantee, in one place. The clauses behind the one being drained
/// keep decoding into their own queues meanwhile, so waiting here costs nothing but the
/// order it enforces.
async fn queue_in_order(
    voice: &Voice,
    pending: &mut mpsc::Receiver<mpsc::Receiver<Result<Vec<i16>, TtsError>>>,
    since: Instant,
) {
    let mut silent_so_far = true;

    while let Some(mut clause) = pending.recv().await {
        while let Some(chunk) = clause.recv().await {
            match chunk {
                Ok(samples) => {
                    // The end of the caller's wait, and the only latency they experience
                    // directly: every later clause is queued behind audio already
                    // playing. Measured here rather than when synthesis returned, because
                    // a clause that has been decoded but not queued is still silence.
                    if std::mem::take(&mut silent_so_far) {
                        tracing::info!(
                            first_audio_ms = since.elapsed().as_millis(),
                            "the agent started speaking"
                        );
                    }
                    voice.enqueue(&samples)
                }
                // A clause that cannot be synthesized leaves a hole in a sentence the
                // caller is already hearing. Saying so and carrying on beats dropping
                // the rest of the reply, but it is never silent about it — the published
                // transcript will claim words that were not spoken.
                Err(error) => {
                    tracing::error!(%error, "a clause of the reply went unspoken");
                }
            }
        }
    }
}

/// Starts synthesizing one clause and queues it behind the ones before it.
///
/// The synthesis runs immediately, in its own task, decoding into a queue of its own —
/// which is what lets a clause be most of the way ready by the time its turn comes.
/// Sending is what applies backpressure: with [`IN_FLIGHT`] clauses already waiting,
/// this blocks, which stops the reply being read faster than it can be spoken.
async fn send(
    dispatch: &mpsc::Sender<mpsc::Receiver<Result<Vec<i16>, TtsError>>>,
    synthesizer: &Arc<dyn Synthesizer>,
    voicing: &Voicing,
    clause: String,
) {
    let synthesizer = synthesizer.clone();
    let voicing = voicing.clone();
    let (decoded, audio) = mpsc::channel(READ_AHEAD);

    tokio::spawn(async move {
        let mut speech = synthesizer.speak(voicing, clause);
        while let Some(chunk) = speech.next().await {
            // A closed receiver is the conversation ending mid-clause. Stop decoding
            // rather than finishing a sentence nobody is listening to.
            if decoded.send(chunk).await.is_err() {
                return;
            }
        }
    });

    // A closed drain means the conversation ended mid-reply. The clause is left to be
    // dropped; there is nobody left to hear it.
    let _ = dispatch.send(audio).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A synthesizer that takes a stated time per clause and encodes each clause's
    /// position in the samples it returns, so the order they land in is readable.
    ///
    /// Hands its audio back in two chunks, like the real one, so the drain is exercised
    /// on a clause that arrives in pieces rather than all at once.
    struct Fake {
        /// Clause text to (delay, sample value).
        script: Vec<(&'static str, u64, i16)>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Synthesizer for Fake {
        fn speak(&self, _voicing: Voicing, text: String) -> Speech {
            self.asked.lock().expect("not poisoned").push(text.clone());

            let found = self
                .script
                .iter()
                .find(|(clause, _, _)| text.contains(clause))
                .map(|(_, delay, value)| (*delay, *value));

            Box::pin(
                stream::once(async move {
                    let Some((delay, value)) = found else {
                        let unknown = TtsError::Undecodable(format!("unscripted clause: {text}"));
                        return stream::iter(vec![Err(unknown)]);
                    };
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    stream::iter(vec![Ok(vec![value; 2]), Ok(vec![value; 2])])
                })
                .flatten(),
            )
        }
    }

    /// A voice with no room behind it, so the tests read the queue rather than a track.
    fn test_voice() -> Voice {
        Voice::for_test()
    }

    fn reply_of(pieces: &[&'static str]) -> Reply<'static> {
        Box::pin(stream::iter(
            pieces
                .iter()
                .map(|piece| Ok(Piece::Say((*piece).to_owned())))
                .collect::<Vec<_>>(),
        ))
    }

    fn failing_reply(pieces: &[&'static str], error: LlmError) -> Reply<'static> {
        let mut items: Vec<Result<Piece, LlmError>> =
            pieces.iter().map(|p| Ok(Piece::Say((*p).to_owned()))).collect();
        items.push(Err(error));
        Box::pin(stream::iter(items))
    }

    /// A reply that says something and then asks for a tool, the way a turn that acts
    /// on what the caller said actually arrives.
    fn reply_calling(text: &str, name: &str) -> Reply<'static> {
        let call = ToolCall {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            input: serde_json::Map::new(),
        };
        let mut items = vec![Ok(Piece::Call(call))];
        if !text.is_empty() {
            items.insert(0, Ok(Piece::Say(text.to_owned())));
        }
        Box::pin(stream::iter(items))
    }

    /// Waits until the reply has actually reached the track.
    ///
    /// Polled rather than slept on: a sleep long enough to be safe is a bet on a machine
    /// nobody has run this on yet, and this returns the instant the condition holds.
    async fn wait_until_speaking(voice: &Voice) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while voice.queued().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reply never reached the track");
    }

    /// The single most noticeable failure in a voice product: the agent keeps talking
    /// after the caller has started.
    #[tokio::test]
    async fn an_interrupted_reply_stops_being_heard() {
        let synthesizer = Arc::new(Fake {
            // The second clause is slow, so it is still in flight when the caller cuts in.
            script: vec![("first sentence", 1, 1), ("second sentence", 300, 2)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let voice = test_voice();
        let interrupted = CancellationToken::new();
        let reply = reply_of(&[
            "This is the first sentence of the reply. ",
            "And this is the second sentence of it.",
        ]);

        let (_, speaking) =
            speak(&voice, synthesizer, Voicing::default(), interrupted.clone(), reply).await;
        wait_until_speaking(&voice).await;

        interrupted.cancel();
        speaking.finish().await;

        assert!(
            voice.queued().is_empty(),
            "the caller was still being talked at after interrupting: {:?}",
            voice.queued()
        );
    }

    /// A turn cut off before the model wrote anything is not a turn that failed, and the
    /// two must not arrive as the same value: barge-in happens constantly in a working
    /// conversation, and reported as a fault it buries the real ones.
    #[tokio::test]
    async fn being_cut_off_before_the_first_word_is_not_a_failure() {
        let synthesizer = Arc::new(Fake { script: vec![], asked: Arc::new(Mutex::new(Vec::new())) });
        let interrupted = CancellationToken::new();
        interrupted.cancel();

        let reply = reply_of(&["This reply is never read."]);
        let (spoken, speaking) =
            speak(&test_voice(), synthesizer, Voicing::default(), interrupted, reply).await;
        speaking.finish().await;

        assert!(matches!(spoken, Spoken::Nothing(Stopped::Interrupted)), "{spoken:?}");
    }

    /// The failure this module is built around: the second clause synthesizes far faster
    /// than the first, and must still be heard second.
    #[tokio::test]
    async fn clauses_are_spoken_in_the_order_they_were_written() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let synthesizer = Arc::new(Fake {
            script: vec![("first sentence", 120, 1), ("second sentence", 5, 2)],
            asked: asked.clone(),
        });

        let voice = test_voice();
        let reply = reply_of(&[
            "This is the first sentence of the reply. ",
            "And this is the second sentence of it.",
        ]);

        let (spoken, speaking) = speak(&voice, synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        assert!(matches!(spoken, Spoken::Did(Made { cut_short: None, .. })));
        assert_eq!(asked.lock().unwrap().len(), 2, "expected one request per sentence");

        // The slow clause first, despite finishing last.
        let queued = voice.queued();
        assert_eq!(queued, vec![1, 1, 1, 1, 2, 2, 2, 2], "clauses came out shuffled");
    }

    /// A tool call must never reach synthesis. Reading `sendMessageToSession` aloud is
    /// the failure the piece type exists to make impossible, and the way to prove it is
    /// to check that nothing was asked of the synthesizer at all.
    #[tokio::test]
    async fn a_tool_call_is_carried_out_of_the_turn_rather_than_synthesized() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let synthesizer = Arc::new(Fake { script: vec![], asked: asked.clone() });

        let (spoken, speaking) = speak(
            &test_voice(),
            synthesizer,
            Voicing::default(),
            CancellationToken::new(),
            reply_calling("", "skip_turn"),
        )
        .await;
        speaking.finish().await;

        match spoken {
            Spoken::Did(Made { text, calls, .. }) => {
                assert_eq!(text, None, "a turn that only called a tool said nothing");
                assert_eq!(calls.len(), 1, "{calls:?}");
                assert_eq!(calls[0].name, "skip_turn");
            }
            other => panic!("a turn that called a tool produced something, got {other:?}"),
        }

        assert!(asked.lock().unwrap().is_empty(), "a tool call was sent to synthesis");
    }

    /// The ordinary shape of an acted-on turn: a sentence for the caller to hear and a
    /// call for the agent to run, from the same pass.
    #[tokio::test]
    async fn words_and_a_call_from_one_pass_both_come_back() {
        let synthesizer = Arc::new(Fake {
            script: vec![("Sending that now.", 1, 1)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let (spoken, speaking) = speak(
            &test_voice(),
            synthesizer,
            Voicing::default(),
            CancellationToken::new(),
            reply_calling("Sending that now.", "sendMessageToSession"),
        )
        .await;
        speaking.finish().await;

        match spoken {
            Spoken::Did(Made { text, calls, .. }) => {
                assert_eq!(text.as_deref(), Some("Sending that now."));
                assert_eq!(calls.len(), 1, "{calls:?}");
            }
            other => panic!("expected words and a call, got {other:?}"),
        }
    }

    /// Overlapping is the point of the ordering machinery — sequential synthesis would
    /// be slower than not streaming at all.
    #[tokio::test]
    async fn clauses_are_synthesized_at_the_same_time() {
        let synthesizer = Arc::new(Fake {
            script: vec![("first sentence", 200, 1), ("second sentence", 200, 2)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let voice = test_voice();
        let reply = reply_of(&[
            "This is the first sentence of the reply. ",
            "And this is the second sentence of it.",
        ]);

        let started = std::time::Instant::now();
        let (_, speaking) = speak(&voice, synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        assert!(
            started.elapsed() < Duration::from_millis(350),
            "two 200ms clauses took {:?} — they were synthesized one after the other",
            started.elapsed()
        );
    }

    /// The transcript must be everything the model wrote, whatever the cuts were.
    #[tokio::test]
    async fn the_text_is_the_whole_reply_regardless_of_how_it_was_cut() {
        let synthesizer = Arc::new(Fake {
            script: vec![("first sentence", 1, 1), ("second sentence", 1, 2)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let reply = reply_of(&[
            "This is the first sentence of the reply. ",
            "And this is the second sentence of it.",
        ]);
        let (spoken, speaking) = speak(&test_voice(), synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        match spoken {
            Spoken::Did(Made { text, .. }) => assert_eq!(
                text.as_deref(),
                Some("This is the first sentence of the reply. And this is the second sentence of it.")
            ),
            other => panic!("expected words, got {other:?}"),
        }
    }

    /// A model that said nothing is a failure with a reason, never an empty success.
    #[tokio::test]
    async fn a_turn_that_produced_no_words_carries_its_reason() {
        let synthesizer = Arc::new(Fake { script: vec![], asked: Arc::new(Mutex::new(Vec::new())) });
        let reply: Reply<'static> = Box::pin(stream::iter(vec![Err(LlmError::Declined)]));

        let (spoken, speaking) = speak(&test_voice(), synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        assert!(matches!(spoken, Spoken::Nothing(Stopped::Failed(LlmError::Declined))), "{spoken:?}");
    }

    /// A turn that broke halfway still says what it had, and still reports the break.
    #[tokio::test]
    async fn a_turn_cut_short_keeps_what_was_written() {
        let synthesizer = Arc::new(Fake {
            script: vec![("first sentence", 1, 1)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let voice = test_voice();
        let reply = failing_reply(
            &["This is the first sentence of the reply."],
            LlmError::Transport("connection reset".to_owned()),
        );

        let (spoken, speaking) = speak(&voice, synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        match spoken {
            Spoken::Did(Made { text, cut_short, .. }) => {
                assert_eq!(text.as_deref(), Some("This is the first sentence of the reply."));
                assert!(matches!(cut_short, Some(Stopped::Failed(LlmError::Transport(_)))), "{cut_short:?}");
            }
            other => panic!("expected the partial reply, got {other:?}"),
        }
        assert!(!voice.queued().is_empty(), "the part that was written went unspoken");
    }

    /// One clause failing must not take the rest of the reply with it.
    #[tokio::test]
    async fn a_clause_that_fails_does_not_silence_the_others() {
        let synthesizer = Arc::new(Fake {
            // The first sentence is unscripted, so its synthesis fails.
            script: vec![("second sentence", 1, 2)],
            asked: Arc::new(Mutex::new(Vec::new())),
        });

        let voice = test_voice();
        let reply = reply_of(&[
            "This is the first sentence of the reply. ",
            "And this is the second sentence of it.",
        ]);

        let (_, speaking) = speak(&voice, synthesizer, Voicing::default(), CancellationToken::new(), reply).await;
        speaking.finish().await;

        assert_eq!(voice.queued(), vec![2, 2, 2, 2], "the surviving clause was dropped too");
    }
}
