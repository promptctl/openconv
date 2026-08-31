//! Turning text into the samples the agent's track carries.
//!
//! Speech is not synthesized here. The server behind `OPENCONV_TTS_URL` serves the
//! ElevenLabs `/v1/text-to-speech` surface, and this is the client for it — the HTTP
//! call, and the two conversions needed to get what comes back onto a LiveKit track.
//!
//! This crate has no opinion on which server that is, which is the point: elvenspeak
//! (`~/code/elvenspeak`) is what answers today, and it replaced elvenreader-server
//! without a line changing here.
//!
//! # Voices are not mapped here
//!
//! Happy's settings screen stores an ElevenLabs voice ID, and the server owns the table
//! that resolves one: elvenspeak's `aliases.toml` maps IDs it does not serve onto ones
//! it does, and falls back for everything else — an unrecognised ID comes back as a
//! successful response in a substitute voice, not an error. So the ID is passed straight
//! through. A second copy of that table here would be a second answer to "which voice is
//! this", and the two would drift the first time either is edited.
//!
//! That contract is load-bearing rather than incidental, and it is pinned from the other
//! side: elvenspeak's `test_unknown_voice_substitutes_and_says_so` names this client in
//! its docstring. A server that 404s foreign IDs would leave every caller silent while
//! looking, from here, exactly like a server that is down.
//!
//! Substitution is also observable, which is what keeps the passthrough honest: every
//! response carries `x-elvenspeak-voice` naming what actually spoke, and
//! `x-elvenspeak-voice-requested` when that differs from what was asked for. Nothing
//! here reads them — they are for whoever is asking why a caller sounds wrong.
//!
//! # What comes back, and what the track wants
//!
//! MPEG audio, 44.1 kHz mono in practice — elvenspeak's default `output_format` is
//! `mp3_44100_128`, and it emits no ID3 tag, so the body starts on a frame sync.
//! LiveKit wants signed 16-bit PCM at [`audio::SAMPLE_RATE`]. Neither the decode nor the
//! rate change is optional, and neither assumes the rate: every frame states its own, so
//! a voice that arrives at some other rate is handled by the same path rather than
//! quietly playing at the wrong pitch.
//!
//! # Why the audio is decoded on the way in
//!
//! Because the alternative asserts something untrue about audio — that a clause's
//! samples all exist at one instant — and against a server that genuinely streams it is
//! also what gets the caller talking sooner.
//!
//! # What synthesis costs, and why the number matters
//!
//! Measured against elvenspeak's piper engine (`tests/live_speech.rs` prints these; an
//! idle M-series Mac, not the deployed node): first byte in a few milliseconds, then
//! roughly **0.1 s fixed per request plus 0.06 s per second of speech**.
//!
//! The shape matters more than the figures, because it decides how replies should be
//! cut. Under a fixed per-request cost the right move is fewer, larger requests; under a
//! proportional one it is to start the first clause as early as possible, since every
//! clause pays only for itself. This is firmly the second — the proportional term
//! dominates by the second or third second of speech — and [`crate::speak`] cuts on that
//! basis. An earlier version of this document recorded the opposite ("about five seconds
//! a request, whether the clause is three seconds of speech or eight"). That was true of
//! elvenreader-server and is wrong now in both magnitude and shape, which is the failure
//! worth guarding against: a number here that has quietly stopped describing the server.
//!
//! Two cautions before anyone tunes against the figures. They move with load — the same
//! test run beside a `cargo clippy` gave 0.15 s per second of speech, two and a half
//! times the idle number — so a busy node is the case to design for, not this one. And
//! throughput is not the tight constraint anyway: across clean runs the first clause's
//! audio landed within about 0.15 s either side of the model finishing its reply,
//! sometimes ahead and sometimes behind. Comfortable on total synthesis, break-even on
//! the only latency a caller experiences.
//!
//! [`audio::SAMPLE_RATE`]: crate::audio::SAMPLE_RATE

use crate::audio;
use crate::llm::with_cause;
use crate::resample::Resampler;
use crate::speak::{Speech, Synthesizer};
use futures_util::stream::{self, StreamExt};
use minimp3::{Decoder, Error as Mp3Error, Frame};
use std::fmt;
use tokio::io::AsyncRead;

/// Speech, as the server behind `OPENCONV_TTS_URL` serves it.
///
/// Cloning is cheap and shares one connection pool — [`reqwest::Client`] is a handle,
/// not a resource — which is what lets each clause be synthesized in its own task.
#[derive(Clone)]
pub struct Tts {
    http: reqwest::Client,
    /// Origin of the text-to-speech server, e.g. `http://127.0.0.1:11000`, without a
    /// trailing slash.
    base_url: String,
    /// Used when the client asks for no particular voice. ElevenLabs' own default, which
    /// is what Happy's settings screen starts from, so the untouched path agrees with
    /// what the app would have sent.
    default_voice: String,
}

/// Generous against a local engine on purpose. Synthesis of one clause is well under a
/// second today, so this is not sized to the expected cost — it is sized so that a slow
/// engine, a cold model load, or a loaded node does not turn a working request into a
/// dropped clause, while a wedged one still cannot hold the reply hostage indefinitely.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Tts {
    pub fn new(base_url: String, default_voice: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("HTTP client with only a timeout configured cannot fail to build"),
            base_url: base_url.trim_end_matches('/').to_owned(),
            default_voice,
        }
    }

    /// Says one piece of text, as samples arriving for [`crate::audio::Voice::enqueue`].
    ///
    /// The request is made here so that a service that is down, or that refuses the
    /// voice, fails before any of it is queued for the caller to hear.
    pub async fn synthesize(&self, voice: Option<&str>, text: &str) -> Result<Speech, TtsError> {
        let voice = voice.unwrap_or(&self.default_voice);
        let url = format!("{}/v1/text-to-speech/{voice}/stream", self.base_url);

        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({"text": text}))
            .send()
            .await
            .map_err(|error| TtsError::Unreachable(with_cause(&error)))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(TtsError::Refused { status: status.as_u16(), body });
        }

        // `StreamReader` needs one error type, and the only thing downstream does with
        // this one is report it, so the HTTP failure is carried through as its message.
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| std::io::Error::other(with_cause(&error))));

        Ok(decode(tokio_util::io::StreamReader::new(body)))
    }

}

/// Everything but the network, for the tests: decodes bytes already in hand.
#[cfg(test)]
async fn decode_bytes(mpeg: &[u8]) -> Result<Vec<i16>, TtsError> {
    crate::speak::collect(decode(std::io::Cursor::new(mpeg.to_vec()))).await
}

impl Synthesizer for Tts {
    fn speak(&self, voice: Option<String>, text: String) -> Speech {
        // Owns its inputs so the clause can be synthesized in a task of its own, which
        // is what lets several be in flight at once.
        let tts = self.clone();

        // The request has not been made yet, so its failure has to become the stream's
        // first item — there is no other channel to report it on.
        Box::pin(
            stream::once(async move {
                match tts.synthesize(voice.as_deref(), &text).await {
                    Ok(speech) => speech,
                    Err(error) => Box::pin(stream::once(async move { Err(error) })) as Speech,
                }
            })
            .flatten(),
        )
    }
}

/// Roughly how much audio to hand over at a time.
///
/// One MPEG frame is about 26 ms, which is a lot of very small handovers for a reply
/// several seconds long. A fifth of a second is coarse enough to be cheap and far finer
/// than the second-odd this streaming is here to save.
const CHUNK_SAMPLES: usize = (audio::SAMPLE_RATE / 5) as usize;

/// Decodes MPEG audio into the track's format — mono, [`audio::SAMPLE_RATE`], 16-bit —
/// handing back each stretch as it is ready rather than at the end.
///
/// Takes a reader rather than a response so the awkward half — rates that are not ours,
/// channel counts that are not one, seams between frames — is testable against bytes
/// rather than against a running server.
fn decode<R: AsyncRead + Unpin + Send + 'static>(mpeg: R) -> Speech {
    struct Decoding<R> {
        decoder: Decoder<R>,
        /// Built from the first frame rather than assumed, and kept for every frame
        /// after it — a resampler restarted per frame clicks at each boundary.
        resampler: Option<Resampler>,
        ready: Vec<i16>,
        /// Whether anything was decoded, which is only answerable at the end.
        decoded_anything: bool,
    }

    let decoding = Decoding {
        decoder: Decoder::new(mpeg),
        resampler: None,
        ready: Vec::with_capacity(CHUNK_SAMPLES),
        decoded_anything: false,
    };

    Box::pin(stream::unfold(Some(decoding), |state| async move {
        let mut decoding = state?;

        loop {
            let frame = match decoding.decoder.next_frame_future().await {
                Ok(frame) => frame,
                Err(Mp3Error::Eof) => {
                    // Bytes that decode to nothing are a failure wearing the shape of a
                    // short reply: the caller hears silence where a sentence belongs and
                    // nothing says why.
                    let ending = match (decoding.decoded_anything, decoding.ready.is_empty()) {
                        (false, _) => Some(Err(TtsError::Undecodable(
                            "the response held no audio".to_owned(),
                        ))),
                        (true, false) => Some(Ok(std::mem::take(&mut decoding.ready))),
                        (true, true) => None,
                    };
                    return ending.map(|item| (item, None));
                }
                Err(error) => {
                    return Some((Err(TtsError::Undecodable(error.to_string())), None));
                }
            };

            let Frame { data, sample_rate, channels, .. } = frame;
            let resampler = decoding.resampler.get_or_insert_with(|| {
                #[allow(clippy::cast_sign_loss)]
                Resampler::new(sample_rate as u32, audio::SAMPLE_RATE)
            });

            decoding.decoded_anything = true;
            decoding.ready.extend(resampler.push(&to_mono(&data, channels)));

            if decoding.ready.len() >= CHUNK_SAMPLES {
                let chunk = std::mem::replace(&mut decoding.ready, Vec::with_capacity(CHUNK_SAMPLES));
                return Some((Ok(chunk), Some(decoding)));
            }
        }
    }))
}

/// Flattens interleaved channels into the one the track carries.
///
/// Mono is not a special case — it is the average of a single channel, so the same path
/// runs whatever arrives.
fn to_mono(interleaved: &[i16], channels: usize) -> Vec<i16> {
    interleaved
        .chunks(channels.max(1))
        .map(|frame| {
            let total: i32 = frame.iter().map(|&sample| i32::from(sample)).sum();
            (total / frame.len() as i32) as i16
        })
        .collect()
}

#[derive(Debug)]
pub enum TtsError {
    /// The text-to-speech server could not be reached, or gave up partway.
    Unreachable(String),
    /// A non-2xx response. The body is carried because the server explains itself there.
    Refused { status: u16, body: String },
    /// A response that was not audio this can play.
    Undecodable(String),
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(error) => write!(f, "could not reach text-to-speech: {error}"),
            Self::Refused { status, body } => {
                write!(f, "text-to-speech returned HTTP {status}: {body}")
            }
            Self::Undecodable(error) => write!(f, "could not decode the speech audio: {error}"),
        }
    }
}

impl std::error::Error for TtsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_averaged_down_to_one_channel() {
        // Interleaved left/right pairs.
        assert_eq!(to_mono(&[100, 300, -50, 50], 2), vec![200, 0]);
    }

    #[test]
    fn mono_passes_through_untouched() {
        assert_eq!(to_mono(&[1, 2, 3], 1), vec![1, 2, 3]);
    }

    /// A channel count of zero is not something a decoder should produce, but dividing
    /// by it is a panic in the middle of a conversation.
    #[test]
    fn a_nonsense_channel_count_does_not_divide_by_zero() {
        assert_eq!(to_mono(&[7, 8], 0), vec![7, 8]);
    }

    /// Bytes that are not audio must fail loudly rather than come back as a short
    /// silence that reads as a working turn.
    #[tokio::test]
    async fn undecodable_bytes_are_an_error_rather_than_silence() {
        assert!(matches!(decode_bytes(b"this is not an mp3").await, Err(TtsError::Undecodable(_))));
        assert!(matches!(decode_bytes(&[]).await, Err(TtsError::Undecodable(_))));
    }

    /// The one thing a caller can check without a server: real MPEG audio comes back at
    /// the rate and channel count the track publishes.
    ///
    /// The fixture is a short clause synthesized by a real server, kept because a
    /// decoder that silently changes behaviour is otherwise only findable by listening.
    #[tokio::test]
    async fn a_real_response_decodes_to_the_tracks_format() {
        let mpeg = include_bytes!("../tests/fixtures/clause.mp3");
        let samples = decode_bytes(mpeg).await.expect("fixture decodes");

        let seconds = samples.len() as f32 / audio::SAMPLE_RATE as f32;
        assert!(
            (0.5..10.0).contains(&seconds),
            "a short clause became {seconds:.2}s of audio"
        );
        assert!(
            samples.iter().any(|&sample| sample.abs() > i16::MAX / 50),
            "decoded to something inaudible"
        );
    }
}
