//! Deciding where one utterance ends and the next begins.
//!
//! Everything here is a pure function of the frames pushed into it. It reads no clock
//! and touches no device: "six hundred milliseconds of silence" is stored as *sixty
//! frames*, because frames arrive at a fixed rate and counting them is exact where
//! consulting a clock is a race. That is what lets the hardest judgement in the speech
//! path — when has someone finished talking — be tested against fixed input with no
//! microphone, no model, and no waiting.
//!
//! The speech/silence decision itself is deliberately the smallest replaceable piece.
//! Ticket .6 brings a real voice-activity model; it substitutes [`SpeechDetector`] and
//! leaves the segmentation, the pre-roll, and the partial cadence alone.

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
    /// Speech just began. Ticket .6 turns this into barge-in.
    SpeechStarted,
    /// The utterance so far, worth a provisional transcript. Not final: more is coming.
    Partial(Vec<f32>),
    /// A complete utterance. The speaker stopped, or hit the ceiling.
    Utterance(Vec<f32>),
}

/// Splits a stream of frames into utterances.
pub struct Endpointer {
    detector: SpeechDetector,
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
            detector: SpeechDetector::new(),
            speaking: false,
            run: 0,
            since_partial: 0,
            preroll: VecDeque::with_capacity(PREROLL * SAMPLES_PER_FRAME),
            utterance: Vec::new(),
        }
    }

    /// Feeds one frame. See [`Heard`] for what comes back.
    pub fn push(&mut self, frame: &[f32]) -> Heard {
        let is_speech = self.detector.observe(frame);

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

/// Decides whether a frame contains speech.
///
/// Energy against an adaptive noise floor rather than a fixed threshold, because the
/// only thing known about a caller's microphone is that its level is not ours to
/// predict — a fixed threshold either deafens a quiet headset or hears a noisy room
/// talking constantly.
///
/// The whole of this type is what ticket .6 replaces.
pub struct SpeechDetector {
    noise_floor: f32,
}

/// Below this, treat it as digital silence regardless of the floor. Stops the adaptive
/// floor from collapsing toward zero during a muted call and then hearing speech in the
/// dither.
const ABSOLUTE_FLOOR: f32 = 0.003;

/// How far above the noise floor a frame must sit to count as speech.
const SPEECH_MARGIN: f32 = 3.0;

/// How fast the floor follows the room while nothing is being said.
const FLOOR_ADAPT_QUIET: f32 = 0.02;

/// How fast it follows while the frame reads as speech.
///
/// Not zero, and that is the whole point. Adapting only on quiet frames looks right and
/// deadlocks: a room already louder than the opening threshold reads as speech, speech
/// does not move the floor, so the floor never rises and the detector hears the room
/// talking forever. Creeping upward even during speech breaks the deadlock, and it is
/// slow enough — roughly a minute to cross a room — that no single spoken sentence can
/// drag the floor up to meet itself and cut its own utterance short.
const FLOOR_ADAPT_LOUD: f32 = 0.000_3;

impl Default for SpeechDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechDetector {
    pub fn new() -> Self {
        Self { noise_floor: ABSOLUTE_FLOOR }
    }

    /// Classifies a frame and folds it into the noise estimate.
    pub fn observe(&mut self, frame: &[f32]) -> bool {
        let energy = rms(frame);
        let is_speech = energy > (self.noise_floor * SPEECH_MARGIN).max(ABSOLUTE_FLOOR);

        // The floor always follows the room; only the speed depends on what was heard.
        // Unconditional by design — see FLOOR_ADAPT_LOUD for the deadlock that a
        // quiet-frames-only update produces.
        let rate = if is_speech { FLOOR_ADAPT_LOUD } else { FLOOR_ADAPT_QUIET };
        self.noise_floor += (energy - self.noise_floor) * rate;

        is_speech
    }
}

fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    (frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32).sqrt()
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

    fn quiet() -> Vec<f32> {
        vec![0.0; SAMPLES_PER_FRAME]
    }

    /// A frame of loud-ish noise. Alternating sign so its RMS is its amplitude.
    fn loud() -> Vec<f32> {
        (0..SAMPLES_PER_FRAME)
            .map(|i| if i % 2 == 0 { 0.4 } else { -0.4 })
            .collect()
    }

    fn push_many(endpointer: &mut Endpointer, frame: &[f32], count: usize) -> Vec<Heard> {
        (0..count).map(|_| endpointer.push(frame)).collect()
    }

    #[test]
    fn silence_alone_never_produces_an_utterance() {
        let mut endpointer = Endpointer::new();
        let heard = push_many(&mut endpointer, &quiet(), 500);
        assert!(heard.iter().all(|h| *h == Heard::Nothing));
    }

    #[test]
    fn a_brief_noise_does_not_open_an_utterance() {
        let mut endpointer = Endpointer::new();
        // Shorter than the onset requirement — a click, not a word.
        let heard = push_many(&mut endpointer, &loud(), SPEECH_ONSET - 1);
        assert!(heard.iter().all(|h| *h == Heard::Nothing), "{heard:?}");
    }

    #[test]
    fn speech_then_silence_yields_one_utterance() {
        let mut endpointer = Endpointer::new();

        let onset = push_many(&mut endpointer, &loud(), SPEECH_ONSET);
        assert_eq!(onset.last(), Some(&Heard::SpeechStarted));

        // Still talking: no utterance yet, however long.
        let during = push_many(&mut endpointer, &loud(), 100);
        assert!(during.iter().all(|h| !matches!(h, Heard::Utterance(_))));

        let after = push_many(&mut endpointer, &quiet(), SILENCE_ENDPOINT + 1);
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
        push_many(&mut endpointer, &quiet(), PREROLL * 2);
        push_many(&mut endpointer, &loud(), SPEECH_ONSET);

        let utterance = first_utterance(push_many(&mut endpointer, &quiet(), SILENCE_ENDPOINT + 1))
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
        push_many(&mut endpointer, &loud(), SPEECH_ONSET);

        let during = push_many(&mut endpointer, &loud(), PARTIAL_CADENCE * 2 + 2);
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
        let heard = push_many(&mut endpointer, &loud(), MAX_UTTERANCE + SPEECH_ONSET + 10);

        let finals = heard.iter().filter(|h| matches!(h, Heard::Utterance(_))).count();
        assert!(finals >= 1, "a talker who never pauses produced no utterance");
    }

    #[test]
    fn flushing_recovers_a_sentence_cut_off_by_hanging_up() {
        let mut endpointer = Endpointer::new();
        push_many(&mut endpointer, &loud(), SPEECH_ONSET + 20);

        let remaining = endpointer.flush().expect("audio in progress");
        assert!(!remaining.is_empty());
        assert_eq!(endpointer.flush(), None, "flushing twice yields nothing the second time");
    }

    #[test]
    fn a_loud_room_does_not_read_as_constant_speech() {
        let mut detector = SpeechDetector::new();
        // Steady noise well above the absolute floor. The adaptive floor should climb
        // to meet it and stop calling it speech.
        let room: Vec<f32> = (0..SAMPLES_PER_FRAME)
            .map(|i| if i % 2 == 0 { 0.02 } else { -0.02 })
            .collect();

        // Forty seconds of it. The floor should have climbed to meet the room.
        for _ in 0..4_000 {
            detector.observe(&room);
        }
        assert!(!detector.observe(&room), "steady room noise still reads as speech");
        // ...but real speech over that room still does.
        assert!(detector.observe(&loud()));
    }

    #[test]
    fn conversion_from_livekit_samples_spans_minus_one_to_one() {
        let converted = to_f32(&[0, i16::MAX, i16::MIN]);
        assert_eq!(converted[0], 0.0);
        assert!((converted[1] - 1.0).abs() < 1e-4);
        assert!((converted[2] + 1.0).abs() < 1e-6);
    }
}
