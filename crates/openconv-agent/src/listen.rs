//! Turning the caller's audio track into everything the conversation needs to know
//! about it.
//!
//! The wiring across the speech path: it pulls frames off the subscribed track, scores
//! each one with [`crate::vad`], runs them through the pure [`crate::endpoint`], and
//! hands whatever that yields to the [`crate::transcribe`] model. Everything worth
//! knowing goes out as one kind of value, [`Noticed`], to whoever asked to listen —
//! publishing is not this module's business, because the control channel does not exist
//! yet when the track is subscribed.
//!
//! LiveKit is asked for 16 kHz mono directly, so libwebrtc does the resampling. Doing
//! it here would mean owning a resampler and every rounding decision in it, to arrive
//! at the same samples.

use crate::endpoint::{to_f32, Endpointer, Heard, SAMPLE_RATE};
use crate::transcribe::{Transcript, Transcriber};
use crate::vad::{Reporter, Score, SpeechDetector};
use futures_util::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_stream::native::NativeAudioStream;
use livekit::track::RemoteAudioTrack;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mono: one caller, one microphone. Whisper takes a single channel anyway.
const CHANNELS: i32 = 1;

/// What the caller said, as it firms up.
///
/// The two variants mirror the protocol's own distinction between a tentative and a
/// settled transcript, so the caller of this module never has to carry a `bool`
/// alongside the text to remember which kind it is holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Speech {
    /// The caller is still talking. This is the best reading so far and it will change.
    Tentative(String),
    /// The caller stopped. This reading is settled.
    Final(String),
}

impl Speech {
    pub fn text(&self) -> &str {
        match self {
            Self::Tentative(text) | Self::Final(text) => text,
        }
    }
}

/// Something worth telling the conversation about the caller's track.
///
/// One value rather than three channels, because the conversation loop cares about the
/// *order* these happened in: a turn that starts before the barge-in that should have
/// cancelled it is the whole bug this type exists to make unrepresentable.
#[derive(Clone, Debug, PartialEq)]
pub enum Noticed {
    /// How likely the caller is talking, sampled down to a rate the data channel and
    /// the app's microphone indicator can both live with.
    Speaking(Score),
    /// The caller has started talking. What stops the agent talking over them.
    Started,
    /// What the caller said, as it firms up.
    Said(Speech),
}

/// The agent's ear on one caller's track: the frames, and the moment they started
/// arriving.
///
/// One value rather than a stream and an instant travelling separately, because the
/// instant means nothing except "when this stream went live" — and it has to be taken
/// where the sink attaches rather than where the listener is first polled, since the
/// distance between those two is one of the things it exists to measure.
pub struct Ear {
    frames: NativeAudioStream,
    attached: Instant,
}

/// Starts capturing the caller's audio.
///
/// Separate from [`listen`], and called before it is spawned, because this call is the
/// moment the agent starts hearing. libwebrtc keeps nothing from before the sink is
/// attached — audio published into that gap is discarded where no counter, no log line and
/// no test can see it. Attaching on the loop that learned the track exists makes
/// "subscribed" and "listening" one moment; attaching inside the spawned task made them
/// two, joined by nothing but whenever the scheduler first polled it.
///
/// Asking for 16 kHz mono here is what makes libwebrtc resample on the way out, so the
/// frames arrive in exactly the shape both models want.
///
// [LAW:no-ambient-temporal-coupling] the instant the agent goes live has an owner.
pub fn attach(track: &RemoteAudioTrack) -> Ear {
    Ear {
        frames: NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, CHANNELS),
        attached: Instant::now(),
    }
}

/// Listens to one caller's track until the conversation it belongs to ends.
///
/// `ended` is not a courtesy. A [`NativeAudioStream`] yields `None` only once it has been
/// closed, and the only thing that closes it is its own `Drop` — which lives inside this
/// future, waiting on the very stream it would have to close. A caller who leaves takes
/// their audio with them but not the stream, so without a signal from outside this task
/// parks forever, holding a native sink, a handle on a dead track and a speech detector
/// for the life of the process. Nothing notices, because a parked task costs nothing
/// anyone is measuring.
pub async fn listen(
    ear: Ear,
    mut detector: SpeechDetector,
    transcriber: Arc<Transcriber>,
    noticed: mpsc::Sender<Noticed>,
    ended: CancellationToken,
) {
    let Ear { mut frames, attached } = ear;

    let mut endpointer = Endpointer::new();
    let mut reporter = Reporter::new();
    let mut arriving = Arriving::attached_at(attached);

    let ending = loop {
        let frame = match next_frame(&mut frames, &ended).await {
            Next::Frame(frame) => frame,
            Next::Ended(ending) => break ending,
        };
        let arrived = Instant::now();

        let samples = to_f32(&frame.data);
        let score = detector.observe(&samples);
        arriving.observe(arrived, &samples, score);

        // A report closes its window on a fixed cadence, so this is a windowed value
        // arriving rather than a step being skipped — see [`Reporter`].
        if let Some(report) = reporter.observe(score) {
            // The score stream, server-side. The client is sent the same numbers, but a
            // client is exactly the wrong place to read them from when the question is why
            // an utterance ended: a data channel that has stopped delivering and a
            // detector that has stopped hearing produce the same empty timeline.
            //
            // At trace, so it costs nothing until someone asks — and asking is one
            // `RUST_LOG` away rather than a rebuild.
            tracing::trace!(score = report.as_f64(), "scored the caller's audio");

            if noticed.send(Noticed::Speaking(report)).await.is_err() {
                break Ending::NobodyListening;
            }
        }

        // One unconditional path per frame; only the value coming back varies.
        let (audio, kind): (Vec<f32>, fn(String) -> Speech) =
            match endpointer.push(&samples, score.is_speech()) {
                Heard::Nothing => continue,
                Heard::SpeechStarted => {
                    // Sent before any transcript, because it is what cancels an answer
                    // the caller has decided not to wait for. A transcript of what they
                    // are saying arrives seconds later; the agent has to stop now.
                    if noticed.send(Noticed::Started).await.is_err() {
                        break Ending::NobodyListening;
                    }
                    continue;
                }
                Heard::Partial(audio) => (audio, Speech::Tentative),
                Heard::Utterance(audio) => (audio, Speech::Final),
            };

        if let Some(speech) = transcribe(&transcriber, audio, kind).await {
            // A closed receiver means the conversation ended while we were transcribing.
            if noticed.send(Noticed::Said(speech)).await.is_err() {
                break Ending::NobodyListening;
            }
        }
    };

    arriving.report();

    // A caller who hangs up mid-sentence still said it — but only to someone still there
    // to hear it, which is the distinction [`Ending`] exists to carry. Transcription is by
    // far the most expensive thing this module does, and running it for a value nobody
    // reads is an inference taken from the calls that are still live.
    let owed = match ending {
        Ending::TrackStopped => endpointer.flush(),
        Ending::NobodyListening => None,
    };

    if let Some(audio) = owed {
        if let Some(speech) = transcribe(&transcriber, audio, Speech::Final).await {
            let _ = noticed.send(Noticed::Said(speech)).await;
        }
    }
}

/// The next frame, or nothing once there will be no more.
///
/// The two ways listening stops — the track closing and the conversation ending — come
/// back as the same value, so the loop above has one exit and the flush and the report
/// after it happen on both. `biased` because which of the two won is not something to
/// leave to a coin flip: a conversation that has ended is over even if frames are still
/// queued behind it.
/// What came next on the caller's track.
#[derive(Debug)]
enum Next {
    Frame(AudioFrame<'static>),
    Ended(Ending),
}

/// Why the listener stopped, and therefore what is still owed.
///
/// Two variants rather than one "the loop finished", because they are owed different
/// things. A track that stopped may have cut the caller off mid-word, and the conversation
/// is still there to be told what it was. A conversation that has ended has nobody left to
/// tell, and transcribing for it costs a whole speech-to-text inference — the most
/// expensive thing this module does — taken from the calls that are still live.
///
/// The distinction is not academic. Locals drop in reverse order, so a conversation ending
/// cancels the token and then closes the channel, in that order, with nothing awaited in
/// between: by the time the woken listener has transcribed anything there is reliably no
/// receiver left. Collapsing these two into one exit made that waste the *ordinary* path,
/// not the edge case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The caller's track stopped producing frames.
    TrackStopped,
    /// The conversation is over — cancelled, or its receiver already gone.
    NobodyListening,
}

/// Generic over the stream so the one case that matters can be tested: a stream that never
/// yields and never ends, which is what a [`NativeAudioStream`] becomes the moment its
/// caller hangs up. There is no way to build one of those in a test, and it is the exact
/// shape that used to park this task forever.
///
/// `biased` because which of the two won is not something to leave to a coin flip: a
/// conversation that has ended is over even if frames are still queued behind it.
async fn next_frame<S>(frames: &mut S, ended: &CancellationToken) -> Next
where
    S: futures_util::Stream<Item = AudioFrame<'static>> + Unpin,
{
    tokio::select! {
        biased;
        () = ended.cancelled() => Next::Ended(Ending::NobodyListening),
        frame = frames.next() => match frame {
            Some(frame) => Next::Frame(frame),
            None => Next::Ended(Ending::TrackStopped),
        },
    }
}

/// The account of what the agent heard on one call, and what it made of it.
///
/// A transcript that comes back short has four possible causes and they are
/// indistinguishable from the transcript alone: no audio arrived, audio arrived with a gap
/// in it, audio arrived carrying silence, or audio arrived carrying speech that the voice
/// activity model scored as silence. Every one of those has been mistaken for one of the
/// others in this project's history — most recently for the speech model mishearing words.
///
/// So each is a number here, and all four go out on one line whether the call went well or
/// badly. A number that only appears when things are bad cannot be compared against
/// anything.
///
/// - `samples` against the clock is whether audio arrived, and whether any went missing.
///   Both ways it goes missing are silent: libwebrtc discards whatever was published
///   before [`attach`], and its sink queue holds about a tenth of a second and then drops
///   the *oldest* frames when this task falls behind.
/// - `loudest` is whether that audio carried anything. A caller whose microphone published
///   pure zeroes delivers a perfect, complete, empty track.
/// - `most_speechlike` is what [`crate::vad`] made of it. Loud audio with a low peak score
///   is the model failing to recognise a voice, which is a different repair entirely from
///   a caller who never spoke.
///
// [LAW:no-silent-failure] each way of hearing nothing is reported as itself, rather than
// left to be guessed at from a transcript that came back short.
struct Arriving {
    attached: Instant,
    samples: usize,
    /// How long the first frame took to arrive, once one has. `None` is a track that has
    /// delivered nothing at all, which is a different fact from having delivered silence.
    first: Option<Duration>,
    /// The largest sample seen, on the same 0.0–1.0 scale as [`to_f32`]'s output.
    loudest: f32,
    most_speechlike: Score,
}

impl Arriving {
    fn attached_at(attached: Instant) -> Self {
        Self {
            attached,
            samples: 0,
            first: None,
            loudest: 0.0,
            most_speechlike: Score::SILENT,
        }
    }

    /// Folds in one frame, the moment it arrived, and what the detector made of it.
    ///
    /// `arrived` is passed in rather than read here because scoring a frame is the most
    /// expensive thing on this path — on the first frame of the process it also builds the
    /// voice activity model's session — and a wait measured after that work would report
    /// the model loading as the network being slow.
    fn observe(&mut self, arrived: Instant, frame: &[f32], score: Score) {
        self.samples += frame.len();
        self.loudest = frame.iter().fold(self.loudest, |loudest, sample| loudest.max(sample.abs()));
        self.most_speechlike = self.most_speechlike.max(score);

        // Only the first frame has a wait worth naming; every one after it arrives on the
        // steady 10 ms cadence, and the gap that matters is the one before this point.
        if self.first.is_none() {
            let waited = arrived.duration_since(self.attached);
            self.first = Some(waited);
            tracing::info!(
                waited_ms = waited.as_millis(),
                "the caller's first audio frame reached the agent"
            );
        }
    }

    fn report(&self) {
        tracing::info!(
            heard_ms = millis(self.samples),
            missing_ms = missing_millis(self.attached.elapsed(), self.samples),
            first_frame_ms = self.first.map(|first| first.as_millis()),
            loudest = self.loudest,
            most_speechlike = self.most_speechlike.as_f64(),
            "stopped listening to the caller"
        );
    }
}

/// What a run of samples is worth in milliseconds, at the one rate this module works at.
fn millis(samples: usize) -> u128 {
    (samples as u128) * 1_000 / (SAMPLE_RATE as u128)
}

/// Audio that never arrived, in milliseconds.
///
/// A track carries real time, so an attachment that has lasted `elapsed` should have
/// delivered exactly that much audio. Saturating rather than signed because a small
/// negative is the sink queue being read a frame ahead of the clock, which is not a
/// finding — only the shortfall is.
fn missing_millis(elapsed: Duration, samples: usize) -> u128 {
    elapsed.as_millis().saturating_sub(millis(samples))
}

/// Transcribes one stretch, or explains why it could not.
///
/// Returns `None` for silence and for failure, but never quietly: a failed
/// transcription is logged as an error, because an agent that has gone deaf must not
/// look like a caller who has gone quiet.
async fn transcribe(
    transcriber: &Transcriber,
    audio: Vec<f32>,
    kind: fn(String) -> Speech,
) -> Option<Speech> {
    let seconds = audio.len() as f32 / SAMPLE_RATE as f32;
    let started = std::time::Instant::now();

    match transcriber.transcribe(audio).await {
        Ok(Transcript::Speech(text)) => {
            tracing::debug!(
                seconds,
                took_ms = started.elapsed().as_millis(),
                "transcribed"
            );
            Some(kind(text))
        }
        Ok(Transcript::Nothing) => None,
        Err(error) => {
            tracing::error!(%error, seconds, "could not transcribe the caller");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_kinds_of_speech_expose_their_text() {
        assert_eq!(Speech::Tentative("hello".to_owned()).text(), "hello");
        assert_eq!(Speech::Final("hello there".to_owned()).text(), "hello there");
    }

    /// Tentative and final are different values, not the same string with a flag beside
    /// it — the app renders them differently.
    #[test]
    fn a_tentative_reading_is_not_equal_to_a_settled_one() {
        assert_ne!(
            Speech::Tentative("hello".to_owned()),
            Speech::Final("hello".to_owned())
        );
    }

    /// A second of attachment that delivered a second of audio lost nothing. Stated
    /// because it is the baseline every shortfall below is measured against.
    #[test]
    fn a_track_that_kept_up_with_the_clock_is_missing_nothing() {
        let second = SAMPLE_RATE as usize;
        assert_eq!(missing_millis(Duration::from_secs(1), second), 0);
        assert_eq!(missing_millis(Duration::from_secs(30), second * 30), 0);
    }

    /// The shape of the bug this counter exists for: audio published before the sink
    /// attached, or dropped from the sink queue, shows up as time with no samples under
    /// it.
    #[test]
    fn audio_that_never_arrived_is_counted_as_missing() {
        let half_a_second = SAMPLE_RATE as usize / 2;
        assert_eq!(missing_millis(Duration::from_secs(2), half_a_second), 1_500);
        assert_eq!(missing_millis(Duration::from_secs(4), 0), 4_000);
    }

    /// Reading a frame ahead of the clock is the sink queue doing its job, not a
    /// finding — the counter reports shortfall and never a surplus dressed as one.
    #[test]
    fn arriving_slightly_ahead_of_the_clock_is_not_a_shortfall() {
        let second = SAMPLE_RATE as usize;
        assert_eq!(missing_millis(Duration::from_millis(950), second), 0);
    }

    /// The leak this module used to have, as a contract: a caller who hangs up leaves an
    /// audio stream that will never yield and never end, and the listener has to stop
    /// anyway. `pending()` is that stream exactly — if the conversation ending did not
    /// reach the listener, this test would hang rather than fail, which is the same thing
    /// the deployed agent did.
    #[tokio::test]
    async fn a_stream_that_never_ends_still_stops_when_the_conversation_does() {
        let ended = CancellationToken::new();
        let mut forever = futures_util::stream::pending::<AudioFrame<'static>>();

        ended.cancel();

        let next = next_frame(&mut forever, &ended).await;
        assert!(
            matches!(next, Next::Ended(Ending::NobodyListening)),
            "a cancelled conversation left the listener waiting: {next:?}"
        );
    }

    /// The two endings are told apart, because only one of them is owed a last transcript.
    /// A track that runs out while the conversation is still live is a caller cut off
    /// mid-word; the conversation is still there to hear what it was.
    #[tokio::test]
    async fn a_track_running_out_is_a_different_ending_from_the_conversation_closing() {
        let ended = CancellationToken::new();
        let mut finished = futures_util::stream::empty::<AudioFrame<'static>>();

        let next = next_frame(&mut finished, &ended).await;
        assert!(
            matches!(next, Next::Ended(Ending::TrackStopped)),
            "a finished track read as the conversation ending: {next:?}"
        );
    }

    /// And while the conversation is live, a stream with nothing on it yet is not an
    /// ending — otherwise the listener would stop the first time the caller drew breath.
    #[tokio::test]
    async fn a_quiet_stream_is_not_mistaken_for_a_finished_one() {
        let ended = CancellationToken::new();
        let mut forever = futures_util::stream::pending::<AudioFrame<'static>>();

        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            next_frame(&mut forever, &ended),
        )
        .await;

        assert!(waited.is_err(), "the listener gave up on a live conversation");
    }
}
