//! Saying a reply out loud while it is still being written.
//!
//! Three things have to be true at once, and they pull against each other:
//!
//! 1. **Start early.** The first clause goes to synthesis as soon as the model has
//!    written it, not when the reply is finished. That is the second of silence this
//!    module exists to remove.
//! 2. **Overlap.** Synthesis costs a few seconds of mostly fixed overhead per request,
//!    so a reply cut into three clauses and synthesized one after another is *slower*
//!    than not cutting it at all. Clauses are therefore in flight together.
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

use crate::audio::Voice;
use crate::clause::Clauses;
use crate::llm::{LlmError, Reply};
use crate::tts::TtsError;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How many clauses may be synthesized at once.
///
/// Enough that a long reply overlaps its requests rather than paying the per-request
/// overhead end to end; small enough that one caller cannot open a dozen connections to
/// a text-to-speech server shared with every other conversation.
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
    fn speak(&self, voice: Option<String>, text: String) -> Speech;
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

/// What the model managed to say.
///
/// Two variants rather than a string that might be empty beside an error that might be
/// absent: a turn either produced words — possibly fewer than a whole answer — or it
/// produced a reason it did not.
#[derive(Debug)]
pub enum Spoken {
    /// Everything the model wrote, with the reason it stopped if it stopped early.
    Said { text: String, cut_short: Option<LlmError> },
    /// The turn yielded nothing to say, and why.
    Nothing(LlmError),
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

impl Speaking {
    /// Waits until every clause of this reply has been queued onto the track.
    pub async fn finish(self) {
        // The drain only ends by running out of clauses. A join error is the runtime
        // shutting down underneath it, which the conversation is ending for anyway.
        let _ = self.drain.await;
    }
}

/// Speaks a reply as it is written.
pub async fn speak(
    voice: &Voice,
    synthesizer: Arc<dyn Synthesizer>,
    voice_id: Option<String>,
    mut reply: Reply<'_>,
) -> (Spoken, Speaking) {
    let (dispatch, mut pending) =
        mpsc::channel::<mpsc::Receiver<Result<Vec<i16>, TtsError>>>(IN_FLIGHT);

    // The single owner of what the caller hears, in the order they hear it. Draining
    // each clause to its end before taking the next is the whole ordering guarantee —
    // and the clauses behind it keep decoding into their own queues meanwhile, so
    // waiting here costs nothing but the order it enforces.
    let drain = tokio::spawn({
        let voice = voice.clone();
        async move {
            while let Some(mut clause) = pending.recv().await {
                while let Some(chunk) = clause.recv().await {
                    match chunk {
                        Ok(samples) => voice.enqueue(&samples),
                        // A clause that cannot be synthesized leaves a hole in a
                        // sentence the caller is already hearing. Saying so and carrying
                        // on beats dropping the rest of the reply, but it is never
                        // silent about it — the published transcript will claim words
                        // that were not spoken.
                        Err(error) => {
                            tracing::error!(%error, "a clause of the reply went unspoken");
                        }
                    }
                }
            }
        }
    });

    let mut clauses = Clauses::new();
    let mut said = String::new();
    let mut cut_short = None;

    while let Some(piece) = reply.next().await {
        let piece = match piece {
            Ok(piece) => piece,
            Err(error) => {
                cut_short = Some(error);
                break;
            }
        };

        said.push_str(&piece);
        for clause in clauses.push(&piece) {
            send(&dispatch, &synthesizer, voice_id.as_deref(), clause).await;
        }
    }

    // Always flushed, never conditionally: the reply's last sentence has no whitespace
    // after its full stop, so it is still held here on every turn — and on a broken one,
    // so is whatever was written before the break. An empty buffer yields nothing.
    if let Some(clause) = clauses.flush() {
        send(&dispatch, &synthesizer, voice_id.as_deref(), clause).await;
    }

    // Closes the drain once it has taken everything already dispatched.
    drop(dispatch);

    let spoken = match (said.is_empty(), cut_short) {
        // The stream's contract: a turn that says nothing ends with a reason.
        (true, Some(error)) => Spoken::Nothing(error),
        (true, None) => Spoken::Nothing(LlmError::Empty),
        (false, cut_short) => Spoken::Said { text: said, cut_short },
    };

    (spoken, Speaking { drain })
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
    voice_id: Option<&str>,
    clause: String,
) {
    let synthesizer = synthesizer.clone();
    let voice_id = voice_id.map(str::to_owned);
    let (decoded, audio) = mpsc::channel(READ_AHEAD);

    tokio::spawn(async move {
        let mut speech = synthesizer.speak(voice_id, clause);
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
        fn speak(&self, _voice: Option<String>, text: String) -> Speech {
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
            pieces.iter().map(|piece| Ok((*piece).to_owned())).collect::<Vec<_>>(),
        ))
    }

    fn failing_reply(pieces: &[&'static str], error: LlmError) -> Reply<'static> {
        let mut items: Vec<Result<String, LlmError>> =
            pieces.iter().map(|p| Ok((*p).to_owned())).collect();
        items.push(Err(error));
        Box::pin(stream::iter(items))
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

        let (spoken, speaking) = speak(&voice, synthesizer, None, reply).await;
        speaking.finish().await;

        assert!(matches!(spoken, Spoken::Said { cut_short: None, .. }));
        assert_eq!(asked.lock().unwrap().len(), 2, "expected one request per sentence");

        // The slow clause first, despite finishing last.
        let queued = voice.queued();
        assert_eq!(queued, vec![1, 1, 1, 1, 2, 2, 2, 2], "clauses came out shuffled");
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
        let (_, speaking) = speak(&voice, synthesizer, None, reply).await;
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
        let (spoken, speaking) = speak(&test_voice(), synthesizer, None, reply).await;
        speaking.finish().await;

        match spoken {
            Spoken::Said { text, .. } => assert_eq!(
                text,
                "This is the first sentence of the reply. And this is the second sentence of it."
            ),
            other => panic!("expected words, got {other:?}"),
        }
    }

    /// A model that said nothing is a failure with a reason, never an empty success.
    #[tokio::test]
    async fn a_turn_that_produced_no_words_carries_its_reason() {
        let synthesizer = Arc::new(Fake { script: vec![], asked: Arc::new(Mutex::new(Vec::new())) });
        let reply: Reply<'static> = Box::pin(stream::iter(vec![Err(LlmError::Declined)]));

        let (spoken, speaking) = speak(&test_voice(), synthesizer, None, reply).await;
        speaking.finish().await;

        assert!(matches!(spoken, Spoken::Nothing(LlmError::Declined)), "{spoken:?}");
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

        let (spoken, speaking) = speak(&voice, synthesizer, None, reply).await;
        speaking.finish().await;

        match spoken {
            Spoken::Said { text, cut_short } => {
                assert_eq!(text, "This is the first sentence of the reply.");
                assert!(matches!(cut_short, Some(LlmError::Transport(_))), "{cut_short:?}");
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

        let (_, speaking) = speak(&voice, synthesizer, None, reply).await;
        speaking.finish().await;

        assert_eq!(voice.queued(), vec![2, 2, 2, 2], "the surviving clause was dropped too");
    }
}
