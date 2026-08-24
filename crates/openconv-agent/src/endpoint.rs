//! Deciding where one utterance ends and the next begins.
//!
//! Everything here is a pure function of the frames pushed into it. It reads no clock
//! and touches no device: "six hundred milliseconds of silence" is stored as *sixty
//! frames*, because frames arrive at a fixed rate and counting them is exact where
//! consulting a clock is a race. That is what lets the hardest judgement in the speech
//! path — when has someone finished talking — be tested against fixed input with no
//! microphone, no model, and no waiting.
//!
//! Whether a frame *is* speech is not decided here. It arrives as an argument, from
//! [`crate::vad`], which is what keeps segmentation testable against a plain sequence of
//! verdicts rather than against audio a model has to be loaded to score.

use std::collections::VecDeque;

/// What the speech path runs at. Whisper wants 16 kHz mono, and LiveKit will deliver
/// exactly that if asked, so nothing here resamples anything.
pub const SAMPLE_RATE: u32 = 16_000;

/// Ten milliseconds, matching the rate frames arrive at.
pub const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize) / 100;

const fn frames(millis: usize) -> usize {
    millis / 10
}

/// How much speech must arrive before an utterance is considered started. Short enough
/// not to clip a word, long enough that a keyboard click or a door does not open one.
const SPEECH_ONSET: usize = frames(60);

/// How much silence ends an utterance.
///
/// The single most felt number in the whole conversation. Too short and the agent
/// interrupts someone drawing breath mid-sentence; too long and every reply feels
/// laggy. 600 ms is the usual conversational compromise.
const SILENCE_ENDPOINT: usize = frames(600);

/// Audio kept from *before* speech was detected, and prepended to the utterance.
///
/// Detection necessarily lags onset — it takes [`SPEECH_ONSET`] of evidence to decide
/// speech began. Without pre-roll the utterance starts after that evidence, and the
/// leading consonant is already gone: "hello" reaches the model as "ello".
const PREROLL: usize = frames(300);

/// How often a provisional transcript is worth producing while someone is still
/// talking. Frequent enough that the app's text visibly keeps up, rare enough that the
/// model is not re-transcribing the same audio continuously.
const PARTIAL_CADENCE: usize = frames(900);

/// A hard ceiling on one utterance. Whisper's context window is 30 seconds, so audio
/// past that is not merely expensive, it is discarded by the model. Someone still
/// talking gets cut into another utterance rather than silently truncated.
const MAX_UTTERANCE: usize = frames(25_000);

/// What pushing a frame produced.
///
/// Every frame goes through the same path; only which of these comes back varies. The
/// caller never asks whether to run the endpointer.
#[derive(Clone, Debug, PartialEq)]
pub enum Heard {
    /// Nothing worth acting on.
    Nothing,
    /// Speech just began. What barge-in is made of: the agent stops talking here.
    SpeechStarted,
    /// The utterance so far, worth a provisional transcript. Not final: more is coming.
    Partial(Vec<f32>),
    /// A complete utterance. The speaker stopped, or hit the ceiling.
    Utterance(Vec<f32>),
}

/// Splits a stream of frames into utterances.
pub struct Endpointer {
    speaking: bool,
    /// Consecutive frames agreeing with the state we are *not* in — the evidence
    /// needed to switch.
    run: usize,
    since_partial: usize,
    preroll: VecDeque<f32>,
    utterance: Vec<f32>,
}

impl Default for Endpointer {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpointer {
    pub fn new() -> Self {
        Self {
            speaking: false,
            run: 0,
            since_partial: 0,
            preroll: VecDeque::with_capacity(PREROLL * SAMPLES_PER_FRAME),
            utterance: Vec::new(),
        }
    }

    /// Feeds one frame and what [`crate::vad`] made of it. See [`Heard`] for what comes
    /// back.
    pub fn push(&mut self, frame: &[f32], is_speech: bool) -> Heard {
        match self.speaking {
            false => self.while_idle(frame, is_speech),
            true => self.while_speaking(frame, is_speech),
        }
    }

    /// Ends any utterance in progress, so a caller hanging up does not take their last
    /// sentence with them.
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        self.speaking = false;
        self.run = 0;
        (!self.utterance.is_empty()).then(|| std::mem::take(&mut self.utterance))
    }

    fn while_idle(&mut self, frame: &[f32], is_speech: bool) -> Heard {
        // Always remembered, so the start of a word is already in hand by the time we
        // decide a word started.
        self.preroll.extend(frame);
        let excess = self.preroll.len().saturating_sub(PREROLL * SAMPLES_PER_FRAME);
        self.preroll.drain(..excess);

        self.run = if is_speech { self.run + 1 } else { 0 };

        if self.run < SPEECH_ONSET {
            return Heard::Nothing;
        }

        self.speaking = true;
        self.run = 0;
        self.since_partial = 0;
        self.utterance = self.preroll.drain(..).collect();
        Heard::SpeechStarted
    }

    fn while_speaking(&mut self, frame: &[f32], is_speech: bool) -> Heard {
        self.utterance.extend_from_slice(frame);
        self.since_partial += 1;

        // Silence inside an utterance is kept, not skipped: the pauses between words
        // are what makes the audio sound like speech to the model.
        self.run = if is_speech { 0 } else { self.run + 1 };

        let ended = self.run >= SILENCE_ENDPOINT;
        let too_long = self.utterance.len() >= MAX_UTTERANCE * SAMPLES_PER_FRAME;

        if ended || too_long {
            self.speaking = false;
            self.run = 0;
            return Heard::Utterance(std::mem::take(&mut self.utterance));
        }

        if self.since_partial >= PARTIAL_CADENCE {
            self.since_partial = 0;
            return Heard::Partial(self.utterance.clone());
        }

        Heard::Nothing
    }
}

/// Converts LiveKit's interleaved 16-bit samples to the floats the model wants.
///
/// The one place the two representations meet, so the scale factor is stated once.
pub fn to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&sample| sample as f32 / -(i16::MIN as f32)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame of audio. What is *in* it stopped mattering when the speech verdict
    /// became an argument — these tests drive the verdict directly.
    fn frame() -> Vec<f32> {
        vec![0.25; SAMPLES_PER_FRAME]
    }

    fn push_many(endpointer: &mut Endpointer, is_speech: bool, count: usize) -> Vec<Heard> {
        let frame = frame();
        (0..count).map(|_| endpointer.push(&frame, is_speech)).collect()
    }

    const SPEECH: bool = true;
    const SILENCE: bool = false;

    #[test]
    fn silence_alone_never_produces_an_utterance() {
        let mut endpointer = Endpointer::new();
        let heard = push_many(&mut endpointer, SILENCE, 500);
        assert!(heard.iter().all(|h| *h == Heard::Nothing));
    }

    #[test]
    fn a_brief_noise_does_not_open_an_utterance() {
        let mut endpointer = Endpointer::new();
        // Shorter than the onset requirement — a click, not a word.
        let heard = push_many(&mut endpointer, SPEECH, SPEECH_ONSET - 1);
        assert!(heard.iter().all(|h| *h == Heard::Nothing), "{heard:?}");
    }

    #[test]
    fn speech_then_silence_yields_one_utterance() {
        let mut endpointer = Endpointer::new();

        let onset = push_many(&mut endpointer, SPEECH, SPEECH_ONSET);
        assert_eq!(onset.last(), Some(&Heard::SpeechStarted));

        // Still talking: no utterance yet, however long.
        let during = push_many(&mut endpointer, SPEECH, 100);
        assert!(during.iter().all(|h| !matches!(h, Heard::Utterance(_))));

        let after = push_many(&mut endpointer, SILENCE, SILENCE_ENDPOINT + 1);
        let finals: Vec<_> = after.iter().filter(|h| matches!(h, Heard::Utterance(_))).collect();
        assert_eq!(finals.len(), 1, "expected exactly one utterance");
    }

    fn first_utterance(heard: Vec<Heard>) -> Option<Vec<f32>> {
        heard.into_iter().find_map(|h| match h {
            Heard::Utterance(samples) => Some(samples),
            _ => None,
        })
    }

    /// The whole reason pre-roll exists: an utterance must contain audio from *before*
    /// the detector had made up its mind, or every word loses its first consonant.
    #[test]
    fn an_utterance_includes_audio_from_before_onset_was_detected() {
        let mut endpointer = Endpointer::new();
        push_many(&mut endpointer, SILENCE, PREROLL * 2);
        push_many(&mut endpointer, SPEECH, SPEECH_ONSET);

        let utterance = first_utterance(push_many(&mut endpointer, SILENCE, SILENCE_ENDPOINT + 1))
            .expect("an utterance");

        // Speech that triggered detection, plus the silence that ended it, plus the
        // pre-roll ahead of both — strictly more than the trigger and tail alone.
        let trigger_and_tail = (SPEECH_ONSET + SILENCE_ENDPOINT + 1) * SAMPLES_PER_FRAME;
        assert!(
            utterance.len() > trigger_and_tail,
            "no pre-roll: utterance is {} samples, trigger and tail alone are {trigger_and_tail}",
            utterance.len()
        );
    }

    #[test]
    fn a_long_utterance_produces_partials_before_it_ends() {
        let mut endpointer = Endpointer::new();
        push_many(&mut endpointer, SPEECH, SPEECH_ONSET);

        let during = push_many(&mut endpointer, SPEECH, PARTIAL_CADENCE * 2 + 2);
        let partials: Vec<_> = during
            .iter()
            .filter_map(|h| match h {
                Heard::Partial(samples) => Some(samples.len()),
                _ => None,
            })
            .collect();

        assert!(partials.len() >= 2, "expected repeated partials, got {partials:?}");
        // Each partial covers everything heard so far, so they grow.
        assert!(partials[1] > partials[0]);
    }

    /// Whisper discards audio past 30 seconds, so a monologue must be cut rather than
    /// silently truncated by the model.
    #[test]
    fn an_endless_talker_is_cut_into_utterances() {
        let mut endpointer = Endpointer::new();
        let heard = push_many(&mut endpointer, SPEECH, MAX_UTTERANCE + SPEECH_ONSET + 10);

        let finals = heard.iter().filter(|h| matches!(h, Heard::Utterance(_))).count();
        assert!(finals >= 1, "a talker who never pauses produced no utterance");
    }

    #[test]
    fn flushing_recovers_a_sentence_cut_off_by_hanging_up() {
        let mut endpointer = Endpointer::new();
        push_many(&mut endpointer, SPEECH, SPEECH_ONSET + 20);

        let remaining = endpointer.flush().expect("audio in progress");
        assert!(!remaining.is_empty());
        assert_eq!(endpointer.flush(), None, "flushing twice yields nothing the second time");
    }

    /// A pause for breath is not the end of a turn. The hangover is the number that
    /// decides it, so it is worth an assertion of its own rather than only being implied
    /// by the utterance tests.
    #[test]
    fn a_pause_shorter_than_the_hangover_does_not_end_an_utterance() {
        let mut endpointer = Endpointer::new();
        push_many(&mut endpointer, SPEECH, SPEECH_ONSET);

        let pause = push_many(&mut endpointer, SILENCE, SILENCE_ENDPOINT - 1);
        assert!(
            pause.iter().all(|h| !matches!(h, Heard::Utterance(_))),
            "a pause for breath ended the turn"
        );

        // ...and picking the sentence back up does not open a second utterance.
        let resumed = push_many(&mut endpointer, SPEECH, SPEECH_ONSET);
        assert!(!resumed.contains(&Heard::SpeechStarted), "carrying on read as a new turn");
    }

    #[test]
    fn conversion_from_livekit_samples_spans_minus_one_to_one() {
        let converted = to_f32(&[0, i16::MAX, i16::MIN]);
        assert_eq!(converted[0], 0.0);
        assert!((converted[1] - 1.0).abs() < 1e-4);
        assert!((converted[2] + 1.0).abs() < 1e-6);
    }
}
