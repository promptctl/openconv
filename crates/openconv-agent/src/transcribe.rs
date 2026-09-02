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

        // Run one inference now, on silence, and throw the result away.
        //
        // The first call through the GPU backend compiles and loads a shader library,
        // which takes upwards of fifteen seconds on a cold machine. Paid here it is
        // startup; paid lazily it lands on the first thing the first caller ever says,
        // and that caller waits fifteen seconds for a reply to "hello".
        let warmup = std::time::Instant::now();
        transcribe_blocking(&transcriber.context, threads, &vec![0.0; 16_000])
            .map_err(|error| TranscribeError::Warmup(Box::new(error)))?;

        // `backends` records which backend was compiled in and, for CUDA, the device
        // architectures it was built for. That second part is the reason it earns a
        // field: it is how `ARCHS = 750` was confirmed to be the single architecture
        // this image targets rather than the dozen nvcc defaults to, a question the
        // build log cannot answer because cargo swallows build-script output.
        //
        // It is deliberately NOT a claim about which backend served a given inference.
        // `whisper_print_system_info` reports compile-time flags, so it reads identically
        // however the run went — and there is no gap here for it to have covered anyway.
        // A Linux container holding the CUDA backend but no card cannot fall back to the
        // CPU quietly, because it cannot start: `libcuda.so.1` comes from the nvidia
        // container runtime, so without it the dynamic loader fails the exec outright,
        // before `main`. Measured against this image, not assumed.
        //
        // Two of the three ways to lose the GPU are therefore loud: no backend compiled
        // in, caught by the check above; an absent card, caught by the loader. The third
        // — a backend that initialises and fails — stays silent, and `ready_in_ms` on
        // this line is the only thing that would betray it, at 84ms against 41601ms for
        // the CPU. Reading that is currently a human's job. openconv-openconv-bwy.34 is
        // making it the process's.
        tracing::info!(
            model = %model.display(),
            threads,
            backends = whisper_rs::print_system_info(),
            ready_in_ms = warmup.elapsed().as_millis(),
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
/// [LAW:no-silent-failure] whisper.cpp does not refuse to run unaccelerated. It prints
/// one WARN, transcribes on the CPU, and lets the process report itself healthy — and
/// for this service that is not a slower conversation but a broken one. Inference falls
/// behind realtime, the listener stalls inside it, and the audio sink it has stopped
/// draining drops its oldest frames, so the caller's words are destroyed rather than
/// delayed. Nothing errors and nothing is missing; the service simply stops working.
/// Refusing to start is the only way that failure ever reaches anyone.
///
/// Callers pass whisper-rs's own `cfg!(feature = "_gpu")`, which answers the one
/// question that was answered wrongly for the whole life of the deployment: did this
/// build compile in any GPU backend at all? See the per-target features in Cargo.toml —
/// a target absent from that list builds fine and lands here.
///
/// Build-time only, and deliberately not claimed as more. A backend that compiles in and
/// then fails to *initialise* — the card out of memory, a compute capability the image
/// was not built for — is one whisper.cpp answers by falling back to the CPU and
/// returning Ok, which this cannot see. That gap is real and is openconv-openconv-bwy.34;
/// closing it needs a timing signal rather than a configuration one, since only latency
/// distinguishes the two at runtime.
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
            Self::Inference(error) => write!(f, "speech-to-text failed: {error}"),
            Self::Warmup(error) => write!(f, "speech-to-text model failed its first run: {error}"),
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

    #[test]
    fn spoken_exposes_only_real_speech() {
        assert_eq!(Transcript::Nothing.spoken(), None);
        assert_eq!(Transcript::Speech("hi".to_owned()).spoken(), Some("hi"));
    }
}
