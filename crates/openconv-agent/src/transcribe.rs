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
        // [LAW:no-silent-failure] whisper.cpp does not refuse to run unaccelerated. It
        // prints one WARN, transcribes on the CPU, and lets the process report itself
        // healthy — and for this service that is not a slower conversation but a broken
        // one. Inference falls behind realtime, the listener stalls inside it, and the
        // audio sink it has stopped draining drops its oldest frames, so the caller's
        // words are destroyed rather than delayed. Nothing errors and nothing is missing;
        // the service simply stops working. Refusing to start is the only way that
        // failure ever reaches anyone.
        //
        // `use_gpu` is whisper-rs's own `cfg!(feature = "_gpu")`, so this asks the one
        // question that was answered wrongly for the whole life of the deployment: did
        // this build compile in any GPU backend at all? See the per-target features in
        // Cargo.toml — a target absent from that list builds fine and lands here.
        let parameters = WhisperContextParameters::default();
        if !parameters.use_gpu {
            return Err(TranscribeError::NoAcceleration);
        }

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

        // `backends` is whisper.cpp's own account of what it can use, and it is here
        // because the check above cannot cover the whole failure. That one proves a
        // backend was *compiled in*; a container that was then started without the
        // nvidia runtime has the backend and no card, and whisper.cpp answers that by
        // falling back to the CPU just as quietly. Naming the backends on the line that
        // already reports the load turns "which one is it actually running on" from an
        // afternoon into a grep, which is what this cost the first time.
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
            Self::NoAcceleration => f.write_str(
                "this build of openconv compiled no GPU backend into whisper, and \
                 speech-to-text on the CPU alone runs far behind realtime — it does not \
                 make calls slow, it makes them lose the caller's words. Build for a \
                 target that names its backend in crates/openconv-agent/Cargo.toml \
                 (macOS: metal, Linux: cuda). A Linux image also needs the CUDA runtime \
                 libraries and a container started with the nvidia runtime, or the \
                 binary will not have loaded this far",
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

    #[test]
    fn spoken_exposes_only_real_speech() {
        assert_eq!(Transcript::Nothing.spoken(), None);
        assert_eq!(Transcript::Speech("hi".to_owned()).spoken(), Some("hi"));
    }
}
