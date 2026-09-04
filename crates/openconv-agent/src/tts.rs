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
//! Two figures for elvenspeak's piper engine, because they disagree by a factor of four
//! and only one of them describes what a caller waits through. On an idle M-series Mac
//! (`tests/live_speech.rs` prints these): first byte in a few milliseconds, then roughly
//! **0.1 s fixed per request plus 0.06 s per second of speech**. Against the deployed
//! `elvenspeak-router`, timed 2026-09-03 with the measured ~20 ms network baseline
//! subtracted: **0.18 s fixed plus 0.27 s per second of speech**, on the
//! `en_US-lessac-high` voice that deployment actually answers with. Tune against the
//! deployed pair; the Mac pair is kept only to show how far a laptop figure can be from
//! the node.
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
//! Two cautions before anyone tunes against the figures. They move with load — the Mac
//! test run beside a `cargo clippy` gave 0.15 s per second of speech, two and a half
//! times its own idle number — so a busy node is the case to design for. And on piper
//! throughput is not the tight constraint anyway: across clean runs the first clause's
//! audio landed within about 0.15 s either side of the model finishing its reply,
//! sometimes ahead and sometimes behind. Comfortable on total synthesis, break-even on
//! the only latency a caller experiences.
//!
//! # Which voice, not which model, decides what the caller waits through
//!
//! Everything above is piper, and piper is barely half of what the deployed router
//! serves. The four kokoro voices — `af_heart`, `am_michael`, `bf_emma`, `bm_george` —
//! cost roughly **0.8 s fixed plus 1.8 s per second of speech**, timed 2026-09-03 the
//! same way, three runs each of a two-second and a nine-second utterance. The five piper
//! voices sit between 0.04 and 0.22 s per second, so the first clause of a reply is
//! synthesized in 0.25–0.6 s on one of them and 4.0–4.7 s on a kokoro voice. Same server,
//! same request, same text: about four seconds of the caller's wait decided by nothing
//! but the voice id the client asked for.
//!
//! Which is what the break-even above is worth reading against, because it is piper's
//! alone. A slope of 1.8 is above real time — a kokoro clause takes longer to synthesize
//! than it takes to play, so the first one reaches the caller about four seconds *after*
//! the model has finished writing the whole reply, and only dispatching later clauses
//! concurrently keeps the gap from widening with each one.
//!
//! Worth setting beside the model, the other candidate for that wait. Across the calls
//! [`crate::speak`]'s marks were taken from, the model reached its first word in 0.6–0.8 s
//! on most turns and its first clause boundary by 1.7 s, on `claude-opus-5`. Neither
//! number moves when the voice changes; `first_audio_ms` moves by four seconds. A reply
//! that feels slow is a question about the voice first, and about the model only once a
//! piper voice is slow too.
//!
//! [`audio::SAMPLE_RATE`]: crate::audio::SAMPLE_RATE

use crate::audio;
use crate::llm::with_cause;
use crate::resample::Resampler;
use crate::speak::{Speech, Synthesizer, Voicing};
use openconv_protocol::Language;
use serde::{Deserialize, Serialize};
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

    /// Where a clause asking for this voicing is sent.
    ///
    /// Falling back to the deployment's default happens here, at the one place holding
    /// both the request and the default, so a caller that asked for no voice gets one
    /// and a caller that asked for one is never quietly given another.
    fn url_for(&self, voicing: &Voicing) -> String {
        let voice = voicing.voice_id.as_deref().unwrap_or(&self.default_voice);
        format!("{}/v1/text-to-speech/{voice}/stream", self.base_url)
    }

    /// What is sent with it.
    ///
    /// Separated from the call for the reason [`decode`] is: what has to be got right is
    /// a shape, and asserting a shape against a value costs nothing while asserting it
    /// against a running server costs a server. A function rather than an expression
    /// inlined below, so a test drives the same one the request does instead of a copy
    /// that agrees with itself.
    fn body_for<'a>(voicing: &'a Voicing, text: &'a str) -> Request<'a> {
        Request {
            text,
            model_id: voicing.model_id.as_deref(),
            language_code: voicing.language,
        }
    }

    /// Says one piece of text, as samples arriving for [`crate::audio::Voice::enqueue`].
    ///
    /// The request is made here so that a service that is down, or that refuses the
    /// voice or the engine, fails before any of it is queued for the caller to hear.
    pub async fn synthesize(&self, voicing: &Voicing, text: &str) -> Result<Speech, TtsError> {
        let url = self.url_for(voicing);

        let response = self
            .http
            .post(&url)
            .json(&Self::body_for(voicing, text))
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

    /// Every voice this deployment can be asked for, as the server behind
    /// `OPENCONV_TTS_URL` lists them.
    ///
    /// The counterpart to the passthrough this module's header describes, and the reason
    /// that passthrough is safe to build a chooser on. Synthesis substitutes a voice it
    /// does not have; discovery does not — elvenspeak 404s an unknown id on
    /// `/v1/voices/{id}` rather than answering for it. So this is the only way to find
    /// out what a deployment will actually speak in, and without it the sole honest
    /// interface is a text box in which a wrong answer is indistinguishable from a right
    /// one until somebody listens.
    ///
    /// Not cached. The listing is derived per request on the far side — behind
    /// `elvenspeak-router` it is a union over whichever engines Consul currently carries
    /// — so a copy held here would be a second answer to "what can this deployment
    /// speak", stale from the first engine that came or went. [LAW:one-source-of-truth]
    pub async fn voices(&self) -> Result<Vec<VoiceListing>, TtsError> {
        let response = self
            .http
            .get(format!("{}/v1/voices", self.base_url))
            .send()
            .await
            .map_err(|error| TtsError::Unreachable(with_cause(&error)))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(TtsError::Refused { status: status.as_u16(), body });
        }

        // Parsed rather than passed through. Every field this drops is one the page
        // would otherwise be free to start reading, and every field it keeps is one a
        // server that stopped sending fails on here — loudly, naming the shape — instead
        // of in a dropdown rendering `undefined` for somebody who was debugging
        // something else. [LAW:parse-dont-validate]
        Ok(response
            .json::<VoiceListingResponse>()
            .await
            .map_err(|error| TtsError::Unreadable(with_cause(&error)))?
            .voices)
    }
}

/// The envelope elvenspeak wraps its voices in, unwrapped here so nothing downstream
/// carries a shape whose only purpose was to match ElevenLabs' own.
#[derive(Debug, Deserialize)]
struct VoiceListingResponse {
    voices: Vec<VoiceListing>,
}

/// One voice a caller can ask for, in the two facts choosing between them needs.
///
/// Both halves come from the server and neither is composed here. `voice_id` is what a
/// request names, and `description` is the sentence that server writes about it —
/// "Kokoro Heart (en-us, female)", "Piper lessac (en_US, high)" — which already carries
/// the engine, the locale and the tier. Building a label out of the other fields instead
/// would mean deciding here which of a voice's several `models` is "really" its engine,
/// and that is a question elvenspeak answers and this crate must not start answering
/// twice. [LAW:one-source-of-truth]
///
/// Serialized as well as deserialized because the browser client is handed exactly this:
/// a voice on offer is one fact, so it is one type, whichever direction it is travelling
/// in. [LAW:one-type-per-behavior]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceListing {
    pub voice_id: String,
    pub description: String,
}

/// The request body, as the server reads it.
///
/// A struct rather than a `json!` literal so the wire shape is declarative: an absent
/// `model_id` is skipped by serde rather than by a branch, and the field names are
/// stated once beside the type carrying them.
///
/// Skipped rather than sent as null, and the difference is visible to a caller:
/// elvenspeak names a `model_id` it could not act on in `x-elvenspeak-ignored`, so a
/// null on every request would report a field nobody asked for in every response. The
/// language reads the same header for the same reason, which makes that header the
/// cheapest integration test either side has: send one, and the response says whether
/// the far end could act on it.
///
/// `language_code` is the name elvenspeak declares (its `api.py`, `SpeechRequest`),
/// which is ElevenLabs' own name for the field — matched rather than invented, because
/// a field this server does not recognise is a field it reports ignored and drops.
///
/// Serialized as the enum rather than as a string this side spelled itself. `Language`
/// carries the renames the published list uses — `pt-br` is not `ptbr` — and a `&str`
/// here would need a mapping to produce them, which is a second spelling of the same
/// vocabulary and wrong the first time either copy is edited.
#[derive(Serialize)]
struct Request<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language_code: Option<Language>,
}

/// Everything but the network, for the tests: decodes bytes already in hand.
#[cfg(test)]
async fn decode_bytes(mpeg: &[u8]) -> Result<Vec<i16>, TtsError> {
    crate::speak::collect(decode(std::io::Cursor::new(mpeg.to_vec()))).await
}

impl Synthesizer for Tts {
    fn speak(&self, voicing: Voicing, text: String) -> Speech {
        // Owns its inputs so the clause can be synthesized in a task of its own, which
        // is what lets several be in flight at once.
        let tts = self.clone();

        // The request has not been made yet, so its failure has to become the stream's
        // first item — there is no other channel to report it on.
        Box::pin(
            stream::once(async move {
                match tts.synthesize(&voicing, &text).await {
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
    /// A response that was not the voice listing it has to be. Reached, answered, and
    /// unreadable — which is a different fault from the three above and points at a
    /// different repair, so it is a different variant rather than the nearest one.
    Unreadable(String),
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(error) => write!(f, "could not reach text-to-speech: {error}"),
            Self::Refused { status, body } => {
                write!(f, "text-to-speech returned HTTP {status}: {body}")
            }
            Self::Undecodable(error) => write!(f, "could not decode the speech audio: {error}"),
            Self::Unreadable(error) => write!(f, "could not read the voice listing: {error}"),
        }
    }
}

impl std::error::Error for TtsError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice listing, exactly as `elvenspeak-router` served one on 2026-09-03 — every
    /// field it sends, in its order, for one voice of each engine behind it.
    ///
    /// Kept whole rather than trimmed to the two fields read. The thing worth asserting
    /// is that a real response parses, and a fixture edited down to what the parser
    /// already wants can only ever agree with it.
    const A_REAL_LISTING: &str = r#"{
      "voices": [
        {
          "voice_id": "af_heart",
          "name": "Heart",
          "category": "premade",
          "labels": {"engine": "kokoro", "gender": "female"},
          "description": "Kokoro Heart (en-us, female)",
          "preview_url": null,
          "available_for_tiers": [],
          "high_quality_base_model_ids": [],
          "samples": null,
          "settings": null,
          "sharing": null,
          "fine_tuning": {
            "is_allowed_to_fine_tune": false,
            "state": {},
            "verification_failures": [],
            "verification_attempts_count": 0,
            "manual_verification_requested": false
          },
          "aliases": [],
          "capabilities": ["speed", "timestamps"],
          "models": ["eleven_multilingual_v2", "kokoro"],
          "language": "en"
        },
        {
          "voice_id": "es_MX-claude-high",
          "name": "claude",
          "category": "premade",
          "labels": {"engine": "piper", "quality": "high"},
          "description": "Piper claude (es_MX, high)",
          "preview_url": null,
          "available_for_tiers": [],
          "high_quality_base_model_ids": [],
          "samples": null,
          "settings": null,
          "sharing": null,
          "fine_tuning": {
            "is_allowed_to_fine_tune": false,
            "state": {},
            "verification_failures": [],
            "verification_attempts_count": 0,
            "manual_verification_requested": false
          },
          "aliases": [],
          "capabilities": ["speed", "timestamps"],
          "models": ["eleven_flash_v2_5", "eleven_turbo_v2_5", "piper"],
          "language": "es"
        }
      ]
    }"#;

    /// The half of the seam a running server cannot be asked about cheaply: that the
    /// response shape this reads is the response shape that server sends.
    #[test]
    fn a_real_voice_listing_reads_as_voices_to_offer() {
        let listing: VoiceListingResponse =
            serde_json::from_str(A_REAL_LISTING).expect("a real listing parses");

        assert_eq!(
            listing.voices,
            vec![
                VoiceListing {
                    voice_id: "af_heart".to_owned(),
                    description: "Kokoro Heart (en-us, female)".to_owned(),
                },
                VoiceListing {
                    voice_id: "es_MX-claude-high".to_owned(),
                    description: "Piper claude (es_MX, high)".to_owned(),
                },
            ],
        );
    }

    /// A deployment serving nothing is a listing, not a failure — and the page draws the
    /// difference between that and a server it could not reach, so the two must not
    /// arrive here as the same value.
    #[test]
    fn a_deployment_with_no_voices_lists_none() {
        let listing: VoiceListingResponse =
            serde_json::from_str(r#"{"voices": []}"#).expect("an empty listing parses");

        assert!(listing.voices.is_empty());
    }

    /// The failure this exists to make loud. A body missing the fields a chooser is
    /// built from must stop here, because the alternative is a dropdown of blank rows
    /// that reads as a deployment with nothing to say.
    #[test]
    fn a_listing_missing_what_a_chooser_needs_is_refused() {
        let missing_description = r#"{"voices": [{"voice_id": "af_heart"}]}"#;

        serde_json::from_str::<VoiceListingResponse>(missing_description)
            .expect_err("a voice with no description is not a voice this can offer");
    }

    #[test]
    fn an_engine_the_caller_asked_for_reaches_the_request() {
        let voicing =
            Voicing { voice_id: None, model_id: Some("kokoro".into()), language: None };
        let body =
            serde_json::to_value(Tts::body_for(&voicing, "hello")).expect("serializes");

        assert_eq!(body, serde_json::json!({"text": "hello", "model_id": "kokoro"}));
    }

    #[test]
    fn asking_for_nothing_in_particular_sends_the_text_and_nothing_else() {
        // Not `null`, and not a default. elvenspeak names a `model_id` or a
        // `language_code` it could not act on in `x-elvenspeak-ignored`, so sending
        // either on every request would report a field nobody asked for in every
        // response — and what a caller who overrode nothing sends stays byte-for-byte
        // what they sent before either existed.
        //
        // Whole-body equality rather than two absence checks, which is what makes this
        // the guard against the tempting default: the day someone reads "no language
        // configured" as "English" and writes `en` in here, this fails. Nothing else
        // would — an agent that had configured nothing would simply start being pinned
        // to English by a caller that used to let the server decide.
        let body = serde_json::to_value(Tts::body_for(&Voicing::default(), "hello"))
            .expect("serializes");

        assert_eq!(body, serde_json::json!({"text": "hello"}));
    }

    /// The wire this ticket exists to run, from the sending end.
    ///
    /// The language was parsed out of the agent's config and dropped one struct short
    /// of the request for the whole life of the feature: an operator who set
    /// `language: es` got Spanish text read with English phonemes, which plays
    /// perfectly and is nonsense — the failure is inaudible as a failure, so nothing
    /// downstream of here can be the thing that catches it.
    ///
    /// `language_code` rather than `language`, because that is the field elvenspeak
    /// declares and ElevenLabs named. A field the server does not recognise is a field
    /// it reports ignored and drops, so the name is the whole of the contract.
    #[test]
    fn a_language_the_caller_asked_for_reaches_the_request() {
        let voicing = Voicing {
            voice_id: None,
            model_id: None,
            language: Some(Language::Es),
        };
        let body = serde_json::to_value(Tts::body_for(&voicing, "hola")).expect("serializes");

        assert_eq!(body, serde_json::json!({"text": "hola", "language_code": "es"}));
    }

    /// The one spelling a mapping written on this side would get wrong.
    ///
    /// `PtBr` is `pt-br` on the wire, and the enum already says so. This is here to fail
    /// if the language is ever converted to a string by anything other than that enum's
    /// own serde renames — the second copy of a vocabulary, which is wrong the first
    /// time either copy is edited and silent when it is.
    #[test]
    fn a_language_code_that_is_not_its_variant_name_is_spelled_as_the_wire_spells_it() {
        let voicing = Voicing {
            voice_id: None,
            model_id: None,
            language: Some(Language::PtBr),
        };
        let body = serde_json::to_value(Tts::body_for(&voicing, "olá")).expect("serializes");

        assert_eq!(body["language_code"], serde_json::json!("pt-br"));
    }

    /// All three axes at once, which no test above covers.
    ///
    /// Each of the others names one field and leaves the rest unset, so a `body_for`
    /// that could only carry one at a time would pass every one of them. A real request
    /// from a configured agent carries all three.
    #[test]
    fn the_three_axes_travel_together() {
        let voicing = Voicing {
            voice_id: Some("es_MX-claude-high".into()),
            model_id: Some("piper".into()),
            language: Some(Language::Es),
        };
        let body = serde_json::to_value(Tts::body_for(&voicing, "hola")).expect("serializes");

        assert_eq!(
            body,
            serde_json::json!({"text": "hola", "model_id": "piper", "language_code": "es"})
        );
    }

    #[test]
    fn the_voice_asked_for_is_the_one_in_the_path_and_absent_means_the_default() {
        let tts = Tts::new("http://server:11000/".into(), "default-voice".into());

        let asked =
            Voicing { voice_id: Some("af_heart".into()), model_id: None, language: None };
        assert_eq!(
            tts.url_for(&asked),
            "http://server:11000/v1/text-to-speech/af_heart/stream"
        );

        // Substituting the default for a voice that *was* asked for is the failure this
        // guards: the caller hears a different speaker and nothing says so.
        assert_eq!(
            tts.url_for(&Voicing::default()),
            "http://server:11000/v1/text-to-speech/default-voice/stream"
        );
    }

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
