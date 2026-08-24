//! Deciding, moment to moment, whether the caller is speaking.
//!
//! Two jobs come out of one signal, and they want it at different rates:
//!
//! - The **pipeline** needs a speech/silence verdict on every frame, because
//!   [`crate::endpoint`] counts frames to decide where an utterance ends.
//! - The **client** needs an occasional number to drive its microphone indicator, and
//!   would be drowned by a hundred data messages a second.
//!
//! So [`SpeechDetector`] answers per frame and [`Reporter`] samples that answer down to
//! something publishable. Neither reads a clock: both count frames, which arrive at a
//! fixed rate, so the same input always produces the same output.
//!
//! # Why a model and not an energy threshold
//!
//! What this replaced was root-mean-square energy against an adaptive noise floor. It
//! worked on a quiet caller in a quiet room and failed the moment either was untrue: a
//! keyboard, a fan spinning up, a television are all "loud", and the floor could only
//! chase them by going deaf to the quiet talker beside them. Silero is trained on the
//! difference between a voice and a noise, which is the distinction actually being asked
//! for, and it hands back a probability rather than a verdict — which is exactly the
//! shape the client wants for a level meter.

use voice_activity_detector::VoiceActivityDetector;

use crate::endpoint::SAMPLE_RATE;

/// How many samples the model scores at once.
///
/// Not a tuning knob: Silero is trained on 512-sample windows at 16 kHz and pads or
/// truncates anything else, so any other value quietly degrades the model rather than
/// configuring it.
const WINDOW: usize = 512;

/// Where a score stops being silence and starts being speech.
///
/// Silero's own recommended operating point, and the same 0.5 the client applies to the
/// scores it is sent — so the microphone indicator lights up for the same audio the
/// pipeline treats as an utterance. There is deliberately no hysteresis here: the
/// run-length counting in [`crate::endpoint`] already refuses to open on a click or
/// close on a breath, and a second layer of smoothing would only make those two numbers
/// harder to reason about.
const SPEECH_THRESHOLD: f32 = 0.5;

/// How many frames one score is reported over.
///
/// The app thresholds at 0.5 with a 300 ms debounce before it moves its microphone
/// indicator, so a report every 100 ms is three chances to change its mind before it
/// acts — responsive, and a tenth of what publishing every frame would put on the
/// reliable data channel.
const REPORT_EVERY: usize = 10;

/// How likely a stretch of audio is speech, from 0.0 to 1.0.
///
/// A type rather than a bare `f32` because two different consumers read it — the
/// endpointer as a verdict, the client as a level — and both would otherwise be holding
/// an unlabelled float with its own idea of what counts as speech.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Score(f32);

impl Score {
    /// Nothing was heard. What a detector reports before its first window has closed.
    pub const SILENT: Self = Self(0.0);

    /// The one place a raw model output becomes a score.
    ///
    /// The model's head is a sigmoid, so the value is in range by construction. When it
    /// is not, the model is not the one this was written against, and that is said out
    /// loud rather than published: a NaN reaches the client as JSON `null`, where it
    /// silently stops the microphone indicator rather than breaking it visibly.
    fn from_model(probability: f32) -> Self {
        // [LAW:no-silent-failure] the impossible value is reported, not smoothed away.
        if !(0.0..=1.0).contains(&probability) {
            tracing::error!(probability, "the voice activity model returned an impossible score");
            return Self::SILENT;
        }
        Self(probability)
    }

    /// Whether this counts as the caller talking.
    pub fn is_speech(self) -> bool {
        self.0 >= SPEECH_THRESHOLD
    }

    /// The score as the wire carries it.
    pub fn as_f64(self) -> f64 {
        f64::from(self.0)
    }

    /// The louder of two scores.
    fn max(self, other: Self) -> Self {
        match self.0 >= other.0 {
            true => self,
            false => other,
        }
    }
}

/// Scores the caller's audio, a window at a time.
///
/// Fed frames and asked for a score on each one, because that is the rate the audio
/// arrives at; internally it scores whole windows, because that is the rate the model
/// works at. The two do not divide evenly, and [`SpeechDetector::latest`] is what covers
/// the gap.
pub struct SpeechDetector {
    model: VoiceActivityDetector,
    /// Samples heard since the last window closed.
    heard: Vec<f32>,
    /// The last window's score, reported on every frame until the next window closes.
    ///
    /// Holding the previous answer rather than reporting nothing is what lets
    /// [`SpeechDetector::observe`] return a score on every frame. The alternative is an
    /// optional score that every caller has to decide what to do with, twice per window,
    /// forever.
    latest: Score,
}

impl SpeechDetector {
    /// Loads the model.
    ///
    /// Once per conversation: the weights are compiled in and could be shared, but the
    /// recurrent state behind them is one caller's audio and cannot be.
    pub fn new() -> Result<Self, VadUnavailable> {
        let model = VoiceActivityDetector::builder()
            .sample_rate(i64::from(SAMPLE_RATE))
            .chunk_size(WINDOW)
            .build()
            .map_err(|error| VadUnavailable(error.to_string()))?;

        Ok(Self { model, heard: Vec::with_capacity(WINDOW), latest: Score::SILENT })
    }

    /// Folds in one frame and reports how likely the caller is speaking.
    pub fn observe(&mut self, frame: &[f32]) -> Score {
        self.heard.extend_from_slice(frame);

        // A loop rather than a single check: frames larger than one window must not
        // leave a backlog of audio the model never sees.
        while self.heard.len() >= WINDOW {
            let window: Vec<f32> = self.heard.drain(..WINDOW).collect();
            self.latest = Score::from_model(self.model.predict(window));
        }

        self.latest
    }
}

/// Turns a score on every frame into a score worth sending.
///
/// Reports the peak over its window rather than the value at the end of it, because a
/// level meter fed instantaneous samples flickers through the gaps between syllables —
/// the caller is still talking, and the indicator should still say so.
pub struct Reporter {
    frames: usize,
    peak: Score,
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    pub fn new() -> Self {
        Self { frames: 0, peak: Score::SILENT }
    }

    /// Folds one frame's score in, and yields a score to publish when the window closes.
    ///
    /// `None` means the window is still open, not that nothing was heard.
    pub fn observe(&mut self, score: Score) -> Option<Score> {
        self.peak = self.peak.max(score);
        self.frames += 1;

        (self.frames >= REPORT_EVERY).then(|| {
            self.frames = 0;
            std::mem::replace(&mut self.peak, Score::SILENT)
        })
    }
}

/// The voice activity model could not be loaded.
///
/// An agent that cannot tell speech from silence never decides an utterance ended, so it
/// never answers anything. That is worth failing the conversation for rather than
/// joining one that will sit there silently.
#[derive(Debug)]
pub struct VadUnavailable(String);

impl std::fmt::Display for VadUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not load the voice activity model: {}", self.0)
    }
}

impl std::error::Error for VadUnavailable {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::SAMPLES_PER_FRAME;

    const FRAMES_PER_SECOND: usize = SAMPLE_RATE as usize / SAMPLES_PER_FRAME;

    fn quiet() -> Vec<f32> {
        vec![0.0; SAMPLES_PER_FRAME]
    }

    /// A second of a loud 300 Hz tone. Loud enough that energy alone calls it speech,
    /// and a held tone is exactly what a voice model should refuse.
    fn tone() -> Vec<f32> {
        (0..SAMPLE_RATE as usize)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                0.4 * (std::f32::consts::TAU * 300.0 * t).sin()
            })
            .collect()
    }

    fn frame_at(samples: &[f32], index: usize) -> &[f32] {
        let start = index * SAMPLES_PER_FRAME;
        &samples[start..start + SAMPLES_PER_FRAME]
    }

    /// Every frame's score, in order.
    fn score_all(detector: &mut SpeechDetector, samples: &[f32]) -> Vec<Score> {
        (0..samples.len() / SAMPLES_PER_FRAME)
            .map(|i| detector.observe(frame_at(samples, i)))
            .collect()
    }

    #[test]
    fn silence_does_not_read_as_speech() {
        let mut detector = SpeechDetector::new().expect("the model is compiled in");
        for _ in 0..FRAMES_PER_SECOND {
            assert!(!detector.observe(&quiet()).is_speech());
        }
    }

    /// The failure the energy detector could not avoid: something loud that is not a
    /// voice. A fan, a keyboard, a held note all opened an utterance before.
    #[test]
    fn a_loud_tone_is_not_a_voice() {
        let mut detector = SpeechDetector::new().expect("the model is compiled in");
        let peak = score_all(&mut detector, &tone()).into_iter().fold(Score::SILENT, Score::max);
        assert!(!peak.is_speech(), "a 300 Hz tone scored {peak:?}");
    }

    /// A score on every frame, not only on the frames where a window happens to close —
    /// otherwise the endpointer would see silence between every scored frame and never
    /// decide anyone was talking.
    #[test]
    fn every_frame_gets_a_score() {
        let mut detector = SpeechDetector::new().expect("the model is compiled in");
        let scores = score_all(&mut detector, &tone());
        assert_eq!(scores.len(), FRAMES_PER_SECOND);
    }

    /// What the detector exists to do, asserted against an actual voice. macOS-only
    /// because that is where a voice can be synthesized on demand; the assembled path is
    /// covered on any platform by `scripts/live-call-acceptance.mjs`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_voice_reads_as_speech() {
        let speech = super::spoken::sample("Hello there. This is a test of speech detection.");
        let mut detector = SpeechDetector::new().expect("the model is compiled in");

        let scores = score_all(&mut detector, &speech);
        let speaking = scores.iter().filter(|score| score.is_speech()).count();

        assert!(
            speaking > scores.len() / 2,
            "only {speaking} of {} frames of speech read as speech",
            scores.len()
        );
    }

    #[test]
    fn a_reporter_yields_one_score_per_window_and_nothing_between() {
        let mut reporter = Reporter::new();
        let yielded: Vec<Option<Score>> =
            (0..REPORT_EVERY * 2).map(|_| reporter.observe(Score(0.9))).collect();

        let reports: Vec<&Score> = yielded.iter().flatten().collect();
        assert_eq!(reports.len(), 2, "expected one report per window, got {yielded:?}");
        assert_eq!(*reports[0], Score(0.9));
    }

    /// The point of reporting a peak: one quiet frame mid-word must not drop the
    /// caller's microphone indicator.
    #[test]
    fn a_report_carries_the_loudest_moment_of_its_window() {
        let mut reporter = Reporter::new();

        let reported = (0..REPORT_EVERY)
            .filter_map(|i| {
                let score = match i == 3 {
                    true => Score(0.95),
                    false => Score(0.05),
                };
                reporter.observe(score)
            })
            .next();

        assert_eq!(reported, Some(Score(0.95)));
    }

    #[test]
    fn windows_do_not_carry_their_peak_into_the_next_one() {
        let mut reporter = Reporter::new();
        for _ in 0..REPORT_EVERY {
            reporter.observe(Score(0.99));
        }

        let second = (0..REPORT_EVERY).filter_map(|_| reporter.observe(Score(0.01))).next();
        assert_eq!(second, Some(Score(0.01)), "the previous window's peak leaked forward");
    }

    /// A model that hands back nonsense must not put nonsense on the wire.
    #[test]
    fn an_impossible_model_output_reads_as_silence() {
        assert_eq!(Score::from_model(f32::NAN), Score::SILENT);
        assert_eq!(Score::from_model(-1.0), Score::SILENT);
        assert_eq!(Score::from_model(2.0), Score::SILENT);
    }

    #[test]
    fn the_threshold_is_the_one_the_client_applies() {
        assert!(Score(0.5).is_speech());
        assert!(!Score(0.49).is_speech());
    }
}

/// A few seconds of real speech, synthesized rather than committed.
///
/// A voice model can only be tested against a voice, and a recording of one is a binary
/// this repository should not carry. macOS can make one on demand, so the test that
/// needs it builds it the first time and reuses it after.
#[cfg(all(test, target_os = "macos"))]
mod spoken {
    use crate::endpoint::SAMPLE_RATE;
    use std::path::{Path, PathBuf};

    /// The line, spoken, at [`SAMPLE_RATE`] and mono.
    pub fn sample(line: &str) -> Vec<f32> {
        read_wav(&synthesize(line))
    }

    fn synthesize(line: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("openconv-vad-tests");
        std::fs::create_dir_all(&dir).expect("a scratch directory");

        let wav = dir.join(format!("{}.wav", line.len()));
        if wav.exists() {
            return wav;
        }

        let aiff = wav.with_extension("aiff");
        run("say", &["-o", path(&aiff), line]);
        run(
            "afconvert",
            &["-f", "WAVE", "-d", &format!("LEI16@{SAMPLE_RATE}"), "-c", "1", path(&aiff), path(&wav)],
        );
        wav
    }

    fn path(of: &Path) -> &str {
        of.to_str().expect("a scratch path is valid UTF-8")
    }

    fn run(program: &str, args: &[&str]) {
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("could not run {program}: {error}"));

        assert!(
            output.status.success(),
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Reads 16-bit mono PCM out of a RIFF/WAVE file.
    ///
    /// Written here rather than pulled in as a dependency because it reads exactly one
    /// shape of file: the one `afconvert` was asked for on the line above.
    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("the synthesized wav");
        let mut at = 12; // past "RIFF", the file size, and "WAVE"

        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four bytes"));
            let size = size as usize;
            let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];

            if id == b"data" {
                return body
                    .chunks_exact(2)
                    .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32_768.0)
                    .collect();
            }
            at += 8 + size + (size % 2);
        }
        panic!("no data chunk in {}", path.display());
    }
}
