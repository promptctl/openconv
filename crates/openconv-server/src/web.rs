//! The browser client, served by the same process that mints its tokens.
//!
//! Every other way to exercise a live call is a Node script: they can assert that a
//! conversation happened but never tell you what it sounded like. This is the page that
//! lets someone open a URL, talk, and hear the answer.
//!
//! Served from here rather than from a static file server, for two reasons that are
//! both about the page being *this* deployment's client rather than a client in
//! general. Same-origin means the token mint is an ordinary `fetch` — no CORS layer
//! widened across a credentialed API for a page's sake. And the SFU to dial comes from
//! the deployment's own configuration rather than from a text box, because a token
//! minted here and offered to a different deployment's SFU does not error: the client
//! joins a room the agent is not in and the caller hears silence.
//!
//! The page is given [`LiveKit::public_signaling_url`], not the address the agent uses.
//! They are usually the same string and were once the same value, which is the bug this
//! separation fixes: the homelab points the agent at a LAN address from Consul, and
//! serving that to a browser produced a `ws://` URL on an `https://` page that browsers
//! refuse as mixed content, reported only as a transport error naming nothing.

use crate::state::AppState;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use openconv_agent::tts::{TtsError, VoiceListing};
use openconv_protocol::Language;
use serde::Serialize;

/// Where the page lives. The trailing slash is load-bearing: the page imports its own
/// files by relative path, which is what lets `web/` also be opened from a plain file
/// server, and `./app.js` under `/call` resolves to `/app.js`.
const MOUNT: &str = "/call/";

/// One file of the page, embedded at build time.
struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
}

/// The page, in the order a browser asks for it.
///
/// Compiled in rather than read from disk, so the page cannot be missing from a build
/// that exists — a static directory left out of a container image is a 404 discovered
/// in a browser, in production, by someone who was trying to debug something else.
const ASSETS: &[Asset] = &[
    Asset {
        path: "/call/",
        content_type: "text/html; charset=utf-8",
        body: include_str!("../../../web/index.html"),
    },
    Asset {
        path: "/call/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../../../web/app.js"),
    },
    Asset {
        path: "/call/transcript.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../../../web/transcript.js"),
    },
    Asset {
        path: "/call/caller.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../../../web/caller.js"),
    },
    // The handshake itself, which the acceptance runs under `scripts/` import off the
    // filesystem while the page fetches it from here. One file with two consumers is the
    // point of it: they used to hold the sequence separately and drifted apart.
    Asset {
        path: "/call/conversation.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../../../web/conversation.js"),
    },
    // Vendored, not fetched from a CDN at load time: an ES module import carries no
    // Subresource Integrity, and a page holding an API key and an open microphone is
    // worth pinning to bytes rather than to a version string. `web/vendor/PROVENANCE.md`
    // records where it came from and its digest.
    Asset {
        path: "/call/vendor/livekit-client.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("../../../web/vendor/livekit-client.js"),
    },
];

/// The names under [`MOUNT`] that answer with something computed rather than with a
/// file.
///
/// Listed because the test that catches a page asking for a name nothing serves reads
/// [`ASSETS`], which these are not in — and a bare exception for whichever one existed
/// first is how the second one gets no check at all. Kept here rather than down in the
/// tests, beside the [`router`] whose `route` calls it mirrors, because two lines apart
/// is the only distance at which the mirror is checked by anyone reading either.
#[cfg(test)]
const ENDPOINTS: &[&str] = &["config", "voices"];

pub fn router() -> Router<AppState> {
    let assets = ASSETS.iter().fold(Router::new(), |router, asset| {
        router.route(
            asset.path,
            get(|| async { ([(CONTENT_TYPE, asset.content_type)], asset.body) }),
        )
    });

    assets
        // Without this, `/call` serves the page and then resolves `./app.js` against
        // `/`, so the browser fetches `/app.js` and the page loads as unstyled markup
        // that does nothing.
        .route("/call", get(|| async { Redirect::permanent(MOUNT) }))
        .route("/call/config", get(config))
        .route("/call/voices", get(voices))
}

/// What the page cannot know without being told.
#[derive(Debug, Serialize)]
struct CallConfig {
    livekit_url: String,
    /// The languages a conversation may be switched to.
    ///
    /// Here rather than in the page's own markup because the union is closed: a code this
    /// crate does not accept fails the whole client message to deserialize, taking the
    /// prompt, the voice and the first message down with it and leaving the conversation
    /// on the deployment default in silence. Answered from [`Language::ALL`], so the page
    /// can only offer what openconv can actually be told. [LAW:one-source-of-truth]
    ///
    /// Beside the SFU rather than on a route of its own, which is the split [`voices`]
    /// argues for: both of these are values this process already holds, and neither can
    /// fail in a way the other should have to survive.
    languages: Vec<String>,
}

/// Unauthenticated, like `/health`: the SFU hostname is what every client dials and is
/// not a credential. The token is the credential, and that mint is authenticated.
async fn config(State(state): State<AppState>) -> impl IntoResponse {
    Json(CallConfig {
        livekit_url: state.livekit.public_signaling_url(),
        languages: Language::ALL.iter().map(|language| language.code()).collect(),
    })
}

/// The voices the page can offer, read from the text-to-speech server this deployment
/// speaks through.
#[derive(Debug, Serialize)]
struct CallVoices {
    voices: Vec<VoiceListing>,
}

/// What this deployment can be asked to sound like.
///
/// A route of its own rather than another field on [`CallConfig`], because the two fail
/// differently and only one of them is survivable. This one crosses the network to
/// another service; that one reads a string this process already holds. Answered
/// together, a text-to-speech server that is down would stop the page learning which SFU
/// to dial, and nobody could join at all — the whole call lost over the part of it that
/// is a dropdown. [LAW:decomposition]
///
/// Asked of the same [`Tts`] every conversation is spoken through, so the voices offered
/// are the voices a call in this deployment can actually reach. A client built here
/// against its own address would be free to list a server nothing speaks through.
/// [LAW:one-source-of-truth]
///
/// [`Tts`]: openconv_agent::tts::Tts
async fn voices(State(state): State<AppState>) -> Result<Json<CallVoices>, NoVoices> {
    Ok(Json(CallVoices { voices: state.tts.voices().await? }))
}

/// Why the page has no voices to offer.
///
/// [LAW:no-silent-failure] An empty list would be the answer-shaped void here: it has
/// exactly the shape of a real answer — "this deployment serves no voices" — while
/// meaning "nobody could ask". The page draws those two differently because a caller
/// left on a voice they did not choose deserves to know which of them happened.
#[derive(Debug)]
struct NoVoices(TtsError);

impl From<TtsError> for NoVoices {
    fn from(error: TtsError) -> Self {
        Self(error)
    }
}

impl IntoResponse for NoVoices {
    fn into_response(self) -> Response {
        // 502 rather than 500, which is the difference between "restart openconv" and
        // "go look at the text-to-speech server". What that server said goes to the
        // operator who can act on it rather than into an unauthenticated body, which is
        // the same split [`ApiError`]'s own upstream arms make. [LAW:one-source-of-truth]
        tracing::error!(error = %self.0, "could not read the voice listing");
        let said = "the text-to-speech server did not answer with a voice listing";
        (StatusCode::BAD_GATEWAY, said).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openconv_protocol::*;

    /// Every relative path one file of the page names, as a browser would resolve them
    /// against [`MOUNT`].
    fn referenced_by(body: &str) -> Vec<&str> {
        body.match_indices("\"./")
            .map(|(at, _)| {
                let rest = &body[at + 3..];
                &rest[..rest.find('"').expect("an unterminated reference")]
            })
            .collect()
    }

    /// A name with no route behind it is a blank page whose only symptom is a 404 in
    /// devtools. Read out of the files themselves rather than listed here, so adding a
    /// module to `web/` and forgetting to serve it fails at `cargo test` instead of in
    /// somebody's browser — and read out of *every* file, because the page loads
    /// `app.js`, which loads the rest.
    #[test]
    fn every_file_the_page_asks_for_is_served() {
        let mut found = 0;

        for asset in ASSETS {
            for name in referenced_by(asset.body) {
                found += 1;

                // A route rather than a file, answered by a handler instead of by
                // bytes. That each of these answers at all is asserted by its handler
                // existing, which the router wires.
                if ENDPOINTS.contains(&name) {
                    continue;
                }

                assert!(
                    ASSETS.iter().any(|served| served.path == format!("{MOUNT}{name}")),
                    "{} asks for {name}, which nothing serves",
                    asset.path
                );
            }
        }

        // Otherwise a page that stopped referencing anything at all — its module tag
        // deleted, say — would pass this test by having nothing to check.
        assert!(found > 0, "the page references none of its own files");
    }

    /// A JavaScript module served as `text/html` is refused outright by every browser,
    /// and the page fails with a MIME error that says nothing about voice.
    #[test]
    fn scripts_are_served_as_javascript() {
        for asset in ASSETS.iter().filter(|asset| asset.path.ends_with(".js")) {
            assert_eq!(asset.content_type, "text/javascript; charset=utf-8");
        }
    }

    /// The page reads each message by field name, and a renamed field does not fail —
    /// it renders `undefined`, or drops to the raw view, in a browser nobody is running
    /// during the change that caused it.
    ///
    /// The names come from serializing the real types rather than from being typed out
    /// here, so this compares the page against the protocol itself. The list is what
    /// `openconv-agent` publishes today (`grep 'ServerEvent::' crates/openconv-agent`);
    /// a variant added there and not here is not caught, but a variant *renamed* is.
    #[test]
    fn the_page_reads_the_field_names_the_protocol_actually_uses() {
        let published = [
            ServerEvent::ConversationMetadata {
                conversation_initiation_metadata_event: ConversationInitiationMetadataEvent {
                    conversation_id: "conv_x".to_owned(),
                    agent_output_audio_format: AudioFormat::Pcm48000,
                    user_input_audio_format: AudioFormat::Pcm48000,
                },
            },
            ServerEvent::UserTranscript {
                user_transcription_event: UserTranscriptionEvent {
                    user_transcript: "run the tests".to_owned(),
                    event_id: EventId(1),
                },
            },
            ServerEvent::TentativeUserTranscript {
                tentative_user_transcription_event: TentativeUserTranscriptionEvent {
                    user_transcript: "run the".to_owned(),
                    event_id: EventId(2),
                },
            },
            ServerEvent::AgentResponse {
                agent_response_event: AgentResponseEvent {
                    agent_response: "Running them now.".to_owned(),
                    event_id: EventId(3),
                },
            },
            ServerEvent::Interruption {
                interruption_event: InterruptionEvent { event_id: EventId(4) },
            },
            ServerEvent::VadScore { vad_score_event: VadScoreEvent { vad_score: 0.9 } },
            ServerEvent::ClientToolCall {
                client_tool_call: ClientToolCall {
                    tool_name: "sendMessageToSession".to_owned(),
                    tool_call_id: "call_1".to_owned(),
                    parameters: Default::default(),
                    event_id: EventId(5),
                },
            },
        ];

        let views = ASSETS
            .iter()
            .find(|asset| asset.path.ends_with("transcript.js"))
            .expect("the page has no view module")
            .body;

        for event in published {
            let serde_json::Value::Object(message) =
                serde_json::to_value(&event).expect("serializes")
            else {
                panic!("a control message is not a JSON object");
            };

            let kind = message["type"].as_str().expect("a message with no type");
            assert!(
                views.contains(kind),
                "the agent publishes {kind:?} messages, which the page has no view for"
            );

            for (name, payload) in &message {
                // A payload carrying nothing but its own `event_id` has nothing for a
                // view to read — an interruption is entirely said by having happened —
                // so the page is right not to name it. Decided from the value rather
                // than from a list of exceptions, which would go stale silently.
                let carries_content = payload
                    .as_object()
                    .is_some_and(|fields| fields.keys().any(|field| field != "event_id"));

                assert!(
                    !carries_content || views.contains(name.as_str()),
                    "the agent publishes {name:?}, which the page never reads"
                );
            }
        }
    }

    /// The 502 body is read by a caller who presented no credential, and `TtsError`
    /// carries the far side's own words: a URL that would not answer, the body it refused
    /// with, a path off this filesystem. That text reached an unauthenticated caller once
    /// already and was taken out by hand; this is what keeps it out.
    ///
    /// Every variant, because the leak was never about one of them — it was about
    /// `Display` being handed to the response at all.
    #[tokio::test]
    async fn a_refused_voice_listing_tells_the_caller_nothing_about_why() {
        let carried = [
            (TtsError::Unreachable("http://10.4.0.7:20977/v1/voices".to_owned()), "10.4.0.7"),
            (TtsError::Refused { status: 401, body: "bad token sk-abcdef".to_owned() }, "sk-abcdef"),
            (TtsError::Undecodable("/srv/openconv/voices/heart.onnx".to_owned()), "heart.onnx"),
            (TtsError::Unreadable("missing `voices` at line 3".to_owned()), "line 3"),
        ];

        for (error, secret) in carried {
            let spoken = error.to_string();
            assert!(spoken.contains(secret), "the fixture stopped carrying {secret:?}");

            let response = NoVoices(error).into_response();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("a body the caller could read");
            let body = String::from_utf8(body.to_vec()).expect("a body that is text");

            // Non-empty first: a handler that answered with nothing would keep every
            // secret and pass every `does not contain` below without saying a word.
            assert!(!body.is_empty(), "the caller is told nothing at all");
            assert!(!body.contains(secret), "the 502 body leaked {secret:?}: {body}");
            assert_ne!(body, spoken, "the 502 body is the error's own Display");
        }
    }
}
