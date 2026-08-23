//! Turning the caller's audio track into transcripts.
//!
//! The wiring between the two halves of the speech path: it pulls frames off the
//! subscribed track, runs them through the pure [`crate::endpoint`], and hands whatever
//! that yields to the [`crate::transcribe`] model. What comes out is sent to whoever
//! asked to listen — publishing is not this module's business, because the control
//! channel does not exist yet when the track is subscribed.
//!
//! LiveKit is asked for 16 kHz mono directly, so libwebrtc does the resampling. Doing
//! it here would mean owning a resampler and every rounding decision in it, to arrive
//! at the same samples.

use crate::endpoint::{to_f32, Endpointer, Heard, SAMPLE_RATE};
use crate::transcribe::{Transcript, Transcriber};
use futures_util::StreamExt;
use libwebrtc::audio_stream::native::NativeAudioStream;
use livekit::track::RemoteAudioTrack;
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Listens to one caller's track for as long as it is published.
///
/// Runs until the track ends, then returns — a subscribed track that stops producing
/// frames is a caller who left, which the room event loop also notices.
pub async fn listen(
    track: RemoteAudioTrack,
    transcriber: Arc<Transcriber>,
    heard: mpsc::Sender<Speech>,
) {
    // Asking for 16 kHz mono here is what makes libwebrtc resample on the way out, so
    // the frames arrive in exactly the shape the model wants.
    let mut frames = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, CHANNELS);

    let mut endpointer = Endpointer::new();

    while let Some(frame) = frames.next().await {
        let samples = to_f32(&frame.data);

        // One unconditional path per frame; only the value coming back varies.
        let (audio, kind): (Vec<f32>, fn(String) -> Speech) = match endpointer.push(&samples) {
            Heard::Nothing => continue,
            Heard::SpeechStarted => continue,
            Heard::Partial(audio) => (audio, Speech::Tentative),
            Heard::Utterance(audio) => (audio, Speech::Final),
        };

        if let Some(speech) = transcribe(&transcriber, audio, kind).await {
            // A closed receiver means the conversation ended while we were transcribing.
            if heard.send(speech).await.is_err() {
                return;
            }
        }
    }

    // A caller who hangs up mid-sentence still said it.
    if let Some(audio) = endpointer.flush() {
        if let Some(speech) = transcribe(&transcriber, audio, Speech::Final).await {
            let _ = heard.send(speech).await;
        }
    }
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
}
