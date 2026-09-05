//! Turning a stretch of utterance audio into text.
//!
//! This is the effect side of the speech path: it owns a model, burns CPU, and can
//! fail. Deciding *what* audio to hand it belongs to [`crate::endpoint`], which stays
//! pure precisely so that this module can be the only part that needs a model file to
//! exercise.
//!
//! Inference is synchronous and long — hundreds of milliseconds — so every call runs on
//! a blocking thread. Running it on the async runtime would stall every other
//! conversation in the process for the duration, including the audio pumps that must
//! emit a frame every ten milliseconds.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// A loaded speech-to-text model, shared by every conversation in the process.
///
/// One model, not one per conversation: the weights are hundreds of megabytes and
/// identical for everybody, so loading them per call would cost more than the
/// transcription.
pub struct Transcriber {
    context: Arc<WhisperContext>,
    threads: i32,
}

impl Transcriber {
    /// Loads the model, or fails loudly.
    ///
    /// Called at startup rather than on the first utterance, so a missing or corrupt
    /// model file stops the process instead of turning into an agent that joins calls
    /// and cannot hear.
    pub fn load(model: &Path) -> Result<Self, TranscribeError> {
        let parameters = WhisperContextParameters::default();
        require_acceleration(parameters.use_gpu)?;

        let context = WhisperContext::new_with_params(model, parameters)
            .map_err(|error| TranscribeError::Load { model: model.to_owned(), error })?;

        // Leave a core for everything else in the process — the audio pumps have a
        // deadline every ten milliseconds and must not queue behind inference.
        let threads = std::thread::available_parallelism()
            .map(|cores| (cores.get().saturating_sub(1)).max(1))
            .unwrap_or(1) as i32;

        let transcriber = Self { context: Arc::new(context), threads };

        // Run the model twice on silence now, and throw both transcripts away.
        //
        // The first call is not a measurement, it is the one-off cost of a backend
        // waking up — Metal compiles and loads a shader library, upwards of fifteen
        // seconds on a genuinely cold machine. Paid here it is startup; paid lazily it
        // lands on the first thing the first caller ever says. The second call is what
        // every utterance after it actually costs, and it is the only one a budget can
        // be set against.
        let silence = vec![0.0; crate::endpoint::SAMPLE_RATE as usize];
        let warm_up = || {
            transcribe_blocking(&transcriber.context, threads, &silence)
                .map_err(|error| TranscribeError::Warmup(Box::new(error)))
        };

        warm_up()?;
        let measuring = Instant::now();
        warm_up()?;
        let warm_inference = measuring.elapsed();

        require_fast_inference(warm_inference)?;

        // `backends` records which backend was compiled in and, for CUDA, the device
        // architectures it was built for. That second part is the reason it earns a
        // field: it is how `ARCHS = 750` was confirmed to be the single architecture
        // this image targets rather than the dozen nvcc defaults to, a question the
        // build log cannot answer because cargo swallows build-script output.
        //
        // It is deliberately NOT a claim about which backend served a given inference.
        // `whisper_print_system_info` reports compile-time flags, so it reads identically
        // however the run went. `warm_inference_ms` is the field that answers that, and
        // the check above is what stops anyone having to read it.
        tracing::info!(
            model = %model.display(),
            threads,
            backends = whisper_rs::print_system_info(),
            warm_inference_ms = warm_inference.as_millis(),
            "speech-to-text model loaded"
        );

        Ok(transcriber)
    }

    /// Transcribes one stretch of 16 kHz mono audio.
    pub async fn transcribe(&self, samples: Vec<f32>) -> Result<Transcript, TranscribeError> {
        let context = self.context.clone();
        let threads = self.threads;

        tokio::task::spawn_blocking(move || transcribe_blocking(&context, threads, &samples))
            .await
            .map_err(|_| TranscribeError::Cancelled)?
    }
}

/// Refuses a build that compiled no GPU backend at all.
///
/// The compile-time half of [`require_fast_inference`], which is where the reason this
/// service refuses to run unaccelerated is written down. Two checks rather than one
/// because they answer different questions: a binary that could never be fast is not the
/// same fact as a process that is not fast, and the first is knowable without a machine,
/// without a model file, and without the load-and-two-inferences it takes to measure the
/// second. [FRAMING:representation] compile-time beats runtime — check each
/// fact where it lives.
///
/// Callers pass whisper-rs's own `cfg!(feature = "_gpu")`, which answers the one
/// question that was answered wrongly for the whole life of the deployment: did this
/// build compile in any GPU backend at all? See the per-target features in Cargo.toml —
/// a target absent from that list builds fine and lands here.
///
/// A function rather than an `if` in `load`, because the flag it reads is fixed at
/// compile time: inline, the one check this file exists to add would be the only line in
/// it that no test could ever drive, and an edit inverting it would compile clean and
/// surface the way the original incident did — in production, silently.
fn require_acceleration(use_gpu: bool) -> Result<(), TranscribeError> {
    match use_gpu {
        true => Ok(()),
        false => Err(TranscribeError::NoAcceleration),
    }
}

/// The model [`WARM_INFERENCE_BUDGET`] was calibrated against.
///
/// Named here rather than read from the configured path, because it is a fact about how
/// the budget was arrived at and not about what this process happens to be loading. A
/// deployment that changes the model has not invalidated the string; it has invalidated
/// the calibration, and this is what says so in the refusal.
const CALIBRATION_MODEL: &str = "ggml-base.en";

/// The longest a warm inference over one second of silence may take.
///
/// Calibrated on both supported targets rather than reasoned about. Every figure below is
/// a *warm* measurement — the second inference, which is the one the check reads — over
/// one second of silence with [`CALIBRATION_MODEL`]:
///
/// | target         | accelerated | fallen back to the CPU |
/// |----------------|-------------|------------------------|
/// | M2 Max, Metal  | 50-56ms     | 565ms-3.0s             |
/// | RTX 2070, CUDA | 41ms        | 1675-1679ms            |
///
/// The binding pair is the narrowest across targets: the slowest *healthy* measurement
/// against the fastest *unhealthy* one. Warm CUDA is the fastest figure in the table, not
/// the slowest, so the healthy end is Metal's 56ms and the unhealthy end is macOS's
/// 565ms — a 10x gap, and the only one the check has to split. Their geometric middle is
/// 178ms, where the multiplicative margin on each side is equal; 250ms sits deliberately
/// above it, 4.5x clear of the slowest healthy measurement and 2.3x clear of the fastest
/// unhealthy one. That asymmetry is chosen, not sloppy: a false refusal takes the service
/// down, where a missed one is only today's bug. Widening it further to catch a slower
/// future CPU eats the headroom a contended GPU needs.
///
/// What it therefore encodes is how long [`CALIBRATION_MODEL`] takes on hardware that is
/// working, so a heavier model does not shrink the margin — it invalidates the number.
/// Deliberately not scaled to the configured model: a budget the deployment computes for
/// itself is one that can never fail ([LAW:no-silent-failure]), and a per-model table
/// would be a hand-maintained second map of how fast this hardware runs each model,
/// drifting from the first card or model file that changes ([LAW:one-source-of-truth]).
/// The startup measurement is the map that redraws itself; this is the requirement it is
/// read against, and the refusal names the model when it fires.
const WARM_INFERENCE_BUDGET: Duration = Duration::from_millis(250);

/// Refuses a process whose inference is too slow to serve calls.
///
/// [LAW:no-silent-failure] The subject is inference speed, not the GPU. Two unrelated
/// deployments land here — a GPU backend that compiled in and then failed to initialise,
/// and a healthy card running a model heavier than [`CALIBRATION_MODEL`] — and the check
/// is deliberately blind to which, because the service does not need a GPU, it needs
/// inference that keeps up with a conversation. The refusal names both, since only the
/// first leaves a whisper.cpp line on stderr for the reader to go and find.
///
/// That first cause is why the check exists, being the last of the three ways to lose the
/// GPU still left quiet. A backend can compile in, find its driver, and still fail to
/// initialise — the card out of memory or held by another process, a compute capability
/// the image was not built for, a device index that does not exist. whisper.cpp answers
/// that by logging one line and appending a CPU backend anyway (`whisper_backend_init`
/// always does), so `create_state` and `full` both return Ok, the warmup succeeds, and
/// the process reports itself healthy while every utterance afterwards runs on the CPU.
/// That is the failure this service cannot survive: inference falls behind realtime, the
/// listener stalls inside it, and the audio sink it has stopped draining discards its
/// oldest frames — the caller's words are destroyed rather than delayed.
///
/// Latency rather than configuration because whisper.cpp offers nothing else: `whisper.h`
/// exposes `use_gpu` and `gpu_device` as *inputs* and no way at all to ask which backend
/// a context ended up with. Timing is not a proxy for that question, though — it is the
/// question, and the one both causes answer the same way. [`WARM_INFERENCE_BUDGET`] is
/// where the requirement it is read against is written down.
fn require_fast_inference(warm_inference: Duration) -> Result<(), TranscribeError> {
    match warm_inference <= WARM_INFERENCE_BUDGET {
        true => Ok(()),
        false => Err(TranscribeError::SlowInference { warm_inference }),
    }
}

fn transcribe_blocking(
    context: &WhisperContext,
    threads: i32,
    samples: &[f32],
) -> Result<Transcript, TranscribeError> {
    let mut state = context.create_state().map_err(TranscribeError::Inference)?;

    // Greedy rather than beam search. The ticket's constraint is latency, not word
    // error rate: a transcript that lands after the caller has given up is worse than a
    // slightly worse one that lands in time.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_translate(false);
    params.set_language(Some("en"));
    // Whisper prints to stdout by default, which would interleave with our logs.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state.full(params, samples).map_err(TranscribeError::Inference)?;

    let mut text = String::new();
    for index in 0..state.full_n_segments() {
        let Some(segment) = state.get_segment(index) else { continue };
        text.push_str(&segment.to_str_lossy().map_err(TranscribeError::Inference)?);
    }

    Ok(Transcript::from_model_output(&text))
}

/// What the model made of a stretch of audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transcript {
    /// Words were spoken.
    Speech(String),
    /// The model found no speech in the audio.
    ///
    /// A real answer and not a failure, which is why it is a variant rather than an
    /// empty string. Collapsing the two would make "the caller said nothing" and "the
    /// model broke" arrive as the same value, and nothing downstream could ever tell
    /// them apart again.
    Nothing,
}

impl Transcript {
    /// Interprets raw model output.
    ///
    /// Whisper narrates silence rather than returning nothing: `[BLANK_AUDIO]`,
    /// `(silence)`, `[ Silence ]` and friends are annotations about the audio, not
    /// words the caller said, and passing them to an LLM as user speech produces an
    /// agent that answers noises.
    fn from_model_output(raw: &str) -> Self {
        let text = raw.trim();

        // Every bracketed or parenthesised run is an annotation; what remains is speech.
        let spoken: String = strip_annotations(text).trim().to_owned();

        match spoken.is_empty() {
            true => Self::Nothing,
            false => Self::Speech(spoken),
        }
    }

    pub fn spoken(&self) -> Option<&str> {
        match self {
            Self::Speech(text) => Some(text),
            Self::Nothing => None,
        }
    }
}

fn strip_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;

    for character in text.chars() {
        match character {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(character),
            _ => {}
        }
    }
    out
}

#[derive(Debug)]
pub enum TranscribeError {
    Load { model: PathBuf, error: whisper_rs::WhisperError },
    /// Built with no GPU backend at all, which this service cannot run on.
    ///
    /// A property of the binary rather than of the machine it landed on: by the time
    /// this is returned, no amount of hardware will change the answer.
    NoAcceleration,
    Inference(whisper_rs::WhisperError),
    /// The model loaded but could not run. A model file that parses and cannot infer
    /// is still a broken deployment, and better found at startup than mid-call.
    Warmup(Box<TranscribeError>),
    /// The model ran, and ran at CPU speed.
    ///
    /// A property of this deployment rather than of the binary, which is the contrast
    /// with [`Self::NoAcceleration`] — but not one that always clears on its own. A card
    /// held by another process gives it back, and the same image on the same host then
    /// starts healthy; a configured model heavier than the budget was calibrated against
    /// never will. The refusal message separates the two, and this doc must not flatten
    /// them back together: read as purely transient, it tells a supervisor to retry, and
    /// retrying a too-heavy model is a crash loop that never ends.
    SlowInference { warm_inference: Duration },
    /// The blocking thread went away — the process is shutting down.
    Cancelled,
}

impl fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { model, error } => write!(
                f,
                "could not load the speech-to-text model at {}: {error}. Fetch one with \
                 scripts/fetch-whisper-model.sh, or point OPENCONV_WHISPER_MODEL at it",
                model.display()
            ),
            Self::NoAcceleration => write!(
                f,
                "this build of openconv compiled no GPU backend into whisper, and \
                 speech-to-text on the CPU alone runs far behind realtime — it does not \
                 make calls slow, it makes them lose the caller's words. Nothing names a \
                 backend for {}: the per-target whisper-rs features in \
                 crates/openconv-agent/Cargo.toml are the list this check enforces, and \
                 this target is not on it",
                std::env::consts::OS,
            ),
            Self::SlowInference { warm_inference } => write!(
                f,
                "a warm speech-to-text inference took {}ms against a budget of {}ms. At \
                 that speed inference runs behind realtime, the audio sink drops the \
                 front of every utterance, and the service loses the caller's words \
                 rather than answering slowly — refusing to start is the only way that \
                 reaches anyone. Two things produce it. Either a GPU backend compiled in \
                 and then failed to initialise, which whisper.cpp answers by falling back \
                 to the CPU without erroring and reports on stderr just above this line \
                 as `no GPU found` or `failed to initialize <backend> backend` — look for \
                 a card out of memory or held by another process, or a container started \
                 without the nvidia runtime's devices. Or OPENCONV_WHISPER_MODEL \
                 points at a model heavier than {CALIBRATION_MODEL}, which is the one \
                 this budget was calibrated against",
                warm_inference.as_millis(),
                WARM_INFERENCE_BUDGET.as_millis(),
            ),
            Self::Inference(error) => write!(f, "speech-to-text failed: {error}"),
            Self::Warmup(error) => write!(f, "speech-to-text model failed its warm-up: {error}"),
            Self::Cancelled => f.write_str("speech-to-text was cancelled"),
        }
    }
}

impl std::error::Error for TranscribeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the rest of the agent depends on: silence is an answer, and it
    /// is not the same answer as words.
    #[test]
    fn silence_annotations_are_not_speech() {
        for raw in [
            "[BLANK_AUDIO]",
            " [ Silence ] ",
            "(silence)",
            "[_BEG_]",
            "",
            "   ",
            "[BLANK_AUDIO] (silence)",
        ] {
            assert_eq!(
                Transcript::from_model_output(raw),
                Transcript::Nothing,
                "{raw:?} was read as speech"
            );
        }
    }

    #[test]
    fn spoken_words_survive_intact() {
        assert_eq!(
            Transcript::from_model_output("  Hello, can you hear me?  "),
            Transcript::Speech("Hello, can you hear me?".to_owned())
        );
    }

    /// Whisper often annotates *and* transcribes in one string. The words are the part
    /// that matters, and dropping the whole line because it carries an annotation would
    /// lose real speech.
    #[test]
    fn words_are_kept_when_an_annotation_sits_beside_them() {
        assert_eq!(
            Transcript::from_model_output("[BLANK_AUDIO] Hello there"),
            Transcript::Speech("Hello there".to_owned())
        );
        assert_eq!(
            Transcript::from_model_output("(clears throat) yes please"),
            Transcript::Speech("yes please".to_owned())
        );
    }

    /// The guard that exists so the original incident cannot recur. Inverting its
    /// condition compiles; this is what notices.
    #[test]
    fn a_build_with_no_gpu_backend_refuses_to_start() {
        assert!(require_acceleration(true).is_ok());

        let Err(error) = require_acceleration(false) else {
            panic!("a build with no GPU backend was allowed to start");
        };
        assert!(matches!(error, TranscribeError::NoAcceleration));

        // The message has to send a reader somewhere. Naming the target it was built
        // for is the part that turns it into an actionable report rather than a verdict.
        assert!(error.to_string().contains(std::env::consts::OS));
    }

    /// The calibration, kept where an edit to the budget has to face it.
    ///
    /// These are measurements, not invented boundaries: every figure was observed on a
    /// supported target with `ggml-base.en` on one second of silence. A budget moved far
    /// enough in either direction to stop separating them stops doing its job, and this
    /// is what says so.
    #[test]
    fn accelerated_inference_passes_and_cpu_inference_does_not() {
        for accelerated in [Duration::from_millis(41), Duration::from_millis(56)] {
            assert!(
                require_fast_inference(accelerated).is_ok(),
                "{accelerated:?} was measured on a healthy GPU and was refused"
            );
        }

        for fallen_back in [Duration::from_millis(565), Duration::from_millis(1675)] {
            let Err(error) = require_fast_inference(fallen_back) else {
                panic!("{fallen_back:?} is CPU speed and was allowed to serve calls");
            };
            assert!(matches!(error, TranscribeError::SlowInference { .. }));
        }
    }

    /// A refusal has to send a reader somewhere, and it cannot send them somewhere wrong.
    ///
    /// Two causes produce this error and only one of them leaves a whisper.cpp line on
    /// stderr, so a message that names just the backend sends an operator who changed the
    /// model hunting for a log line that does not exist. Both numbers are formatted from
    /// the constant, so the message cannot drift from the budget it reports either.
    #[test]
    fn the_refusal_reports_the_numbers_and_both_causes() {
        let error = require_fast_inference(Duration::from_millis(1675)).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("1675ms"), "{message}");
        assert!(message.contains(&format!("{}ms", WARM_INFERENCE_BUDGET.as_millis())), "{message}");
        assert!(message.contains("failed to initialize"), "{message}");
        assert!(message.contains(CALIBRATION_MODEL), "{message}");
    }

    #[test]
    fn spoken_exposes_only_real_speech() {
        assert_eq!(Transcript::Nothing.spoken(), None);
        assert_eq!(Transcript::Speech("hi".to_owned()).spoken(), Some("hi"));
    }
}
