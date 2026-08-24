//! The agent's published audio track, and the pump that keeps it fed.
//!
//! # One track, always running
//!
//! The pump does not start when the agent has something to say and stop when it does
//! not. It emits a frame every tick for as long as the agent is connected, and what
//! varies is the *contents* of that frame — queued speech if there is any, silence if
//! there is not. A track that stops being fed produces gaps that surface as clicks and
//! stalled playback on the far side, and a pump that only runs sometimes has to be
//! started and stopped from wherever speech happens to originate.
//!
//! That leaves [`Voice::enqueue`] as the whole interface for making the agent talk: put
//! samples in, they go out in order. Streaming text-to-speech becomes a producer
//! feeding this queue rather than a second thing that also drives the track.

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::prelude::{AudioFrame, AudioSourceOptions, RtcAudioSource};
use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, TrackSource};
use livekit::Room;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 48 kHz mono, matching what the agent announces as `agent_output_audio_format` and
/// what WebRTC wants natively — anything else means resampling twice for no gain.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 1;

/// WebRTC's native frame size. Ten milliseconds is not a tuning knob here: libwebrtc
/// consumes audio in 10 ms units, and any other size is repacked internally.
const FRAME_MILLIS: u64 = 10;
const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as u64 * FRAME_MILLIS / 1000) as usize;

/// The agent's voice: a published audio track plus the queue feeding it.
#[derive(Clone)]
pub struct Voice {
    source: NativeAudioSource,
    /// Samples waiting to go out. Drained a frame at a time by [`Voice::run`].
    pending: Arc<Mutex<VecDeque<i16>>>,
}

impl Voice {
    /// Publishes the agent's audio track into the room.
    pub async fn publish(room: &Room) -> Result<Self, VoiceError> {
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                // All three are meant for a microphone in a room with a human in it.
                // This track carries synthesized speech, and letting WebRTC gate,
                // cancel, or level it would chew holes in it.
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            SAMPLE_RATE,
            CHANNELS,
            // Buffering is the pump's job, below, so the source holds none.
            0,
        );

        let track = LocalAudioTrack::create_audio_track(
            "agent",
            RtcAudioSource::Native(source.clone()),
        );

        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    // Discontinuous transmission stops sending packets during silence.
                    // The far side then has nothing arriving between utterances, which
                    // reads as a stalled connection rather than as a quiet agent.
                    dtx: false,
                    // Redundant encoding: speech survives isolated packet loss instead
                    // of dropping a syllable.
                    red: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| VoiceError::Publish(error.to_string()))?;

        Ok(Self { source, pending: Arc::new(Mutex::new(VecDeque::new())) })
    }

    /// Queues samples for the agent to speak. Signed 16-bit, 48 kHz, mono.
    pub fn enqueue(&self, samples: &[i16]) {
        self.pending.lock().expect("voice queue poisoned").extend(samples);
    }

    /// Drops anything queued but not yet spoken.
    ///
    /// What interruption is made of: when the user talks over the agent, the words
    /// already in flight have to go, and the pump keeps running so the track never
    /// goes dead.
    pub fn silence(&self) {
        self.pending.lock().expect("voice queue poisoned").clear();
    }

    /// Takes exactly one frame, padding with silence when the queue runs short.
    ///
    /// Silence is the identity value here rather than a special case, which is what
    /// lets the pump run one unconditional path whether or not the agent is speaking.
    fn next_frame(&self) -> Vec<i16> {
        let mut pending = self.pending.lock().expect("voice queue poisoned");
        let take = SAMPLES_PER_FRAME.min(pending.len());
        let mut frame: Vec<i16> = pending.drain(..take).collect();
        frame.resize(SAMPLES_PER_FRAME, 0);
        frame
    }

    /// Feeds the track for as long as the agent is connected.
    ///
    /// The single owner of this track's timing: one tick, one frame, no other clock in
    /// the crate decides when audio moves.
    pub async fn run(self) {
        let mut ticks = tokio::time::interval(Duration::from_millis(FRAME_MILLIS));
        // A pump that fell behind must not then sprint to catch up, which would emit a
        // burst of frames faster than real time and desynchronize playback.
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticks.tick().await;

            let frame = self.next_frame();
            let captured = self
                .source
                .capture_frame(&AudioFrame {
                    data: frame.into(),
                    sample_rate: SAMPLE_RATE,
                    num_channels: CHANNELS,
                    samples_per_channel: SAMPLES_PER_FRAME as u32,
                })
                .await;

            // The room closing ends the track. Anything else is worth knowing about
            // rather than silently pumping into a dead source forever.
            if let Err(error) = captured {
                tracing::debug!(%error, "audio pump stopping");
                return;
            }
        }
    }
}

#[derive(Debug)]
pub enum VoiceError {
    Publish(String),
}

impl std::fmt::Display for VoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Publish(error) => write!(f, "could not publish the agent's audio track: {error}"),
        }
    }
}

impl std::error::Error for VoiceError {}

/// Building a voice with no room behind it, so the speech path can be tested against
/// what came out rather than against a running SFU.
///
/// Test-only, and deliberately so: a queue nobody can read is the right shape for
/// production, where the pump is the only reader and reading is not a thing to do to a
/// track. It exists here because the guarantee worth testing — that clauses reach the
/// caller in the order they were written — is invisible from outside otherwise.
#[cfg(test)]
impl Voice {
    pub fn for_test() -> Self {
        Self {
            source: NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE, CHANNELS, 0),
            pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Everything queued and not yet sent, in the order it will go out.
    pub fn queued(&self) -> Vec<i16> {
        self.pending.lock().expect("voice queue poisoned").iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue's behaviour is the part worth pinning, and it needs no room: a frame
    /// is always a full frame, and silence is what a short queue is padded with.
    fn queue(samples: &[i16]) -> Arc<Mutex<VecDeque<i16>>> {
        Arc::new(Mutex::new(samples.iter().copied().collect()))
    }

    /// Built without publishing a track, because the queue is what these assert on.
    fn voice_with(pending: Arc<Mutex<VecDeque<i16>>>) -> Voice {
        Voice {
            source: NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE, CHANNELS, 0),
            pending,
        }
    }

    #[test]
    fn an_empty_queue_yields_a_full_frame_of_silence() {
        let frame = voice_with(queue(&[])).next_frame();
        assert_eq!(frame.len(), SAMPLES_PER_FRAME);
        assert!(frame.iter().all(|&sample| sample == 0));
    }

    #[test]
    fn a_short_queue_is_padded_rather_than_truncated() {
        let frame = voice_with(queue(&[1, 2, 3])).next_frame();
        assert_eq!(frame.len(), SAMPLES_PER_FRAME);
        assert_eq!(&frame[..3], &[1, 2, 3]);
        assert!(frame[3..].iter().all(|&sample| sample == 0));
    }

    #[test]
    fn speech_comes_out_in_order_across_frames() {
        let samples: Vec<i16> = (0..SAMPLES_PER_FRAME as i16 * 2).collect();
        let voice = voice_with(queue(&samples));

        let first = voice.next_frame();
        let second = voice.next_frame();

        assert_eq!(first, samples[..SAMPLES_PER_FRAME]);
        assert_eq!(second, samples[SAMPLES_PER_FRAME..]);
        assert!(voice.next_frame().iter().all(|&sample| sample == 0));
    }

    #[test]
    fn silencing_drops_what_had_not_been_spoken() {
        let voice = voice_with(queue(&[1; 5_000]));
        voice.silence();
        assert!(voice.next_frame().iter().all(|&sample| sample == 0));
    }

    /// What the speech pipeline's tests read, so it is worth pinning that it reads the
    /// queue in the order the pump will drain it.
    #[test]
    fn the_queue_can_be_read_back_in_send_order() {
        let voice = voice_with(queue(&[]));
        voice.enqueue(&[1, 2]);
        voice.enqueue(&[3]);
        assert_eq!(voice.queued(), vec![1, 2, 3]);
    }
}
