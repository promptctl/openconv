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

use crate::livekit::LiveKitError;
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
const ENDPOINTS: &[&str] = &["config", "voices", "health"];

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
        .route("/call/health", get(health))
}

/// What the page cannot know without being told.
#[derive(Debug, Serialize)]
struct CallConfig {
    livekit_url: String,
    /// Whether this deployment asks callers for an `xi-api-key`.
    ///
    /// The page cannot know it and must not guess: a form that demands a key nothing
    /// checks asks for a secret that does not exist, and one that omits a key the mint
    /// requires sends a request that comes back 401 with the field to fix it hidden.
    /// [LAW:one-source-of-truth]
    requires_api_key: bool,
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
        requires_api_key: state.api_key.is_some(),
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

/// What the caller cannot reach, named — so that silence on a call stops being one
/// symptom with four causes.
///
/// A dead text-to-speech server, an SFU this deployment cannot talk to, an agent that
/// never arrived and a microphone that never opened all present to a caller as the same
/// nothing, and telling them apart has meant a shell on the cluster. This is the half of
/// that a server can answer about itself. [LAW:no-silent-failure]
///
/// A route of its own for the reason [`voices`] is one: it crosses the network, and a
/// readout answered beside [`config`] would take the SFU address down with it and leave
/// nobody able to join at all — losing the whole call over the part of it that reports on
/// calls. [LAW:decomposition]
///
/// Distinct from `/health`, which answers whether this process is up. A process can be
/// perfectly up and unable to speak, and that gap is exactly what this names.
#[derive(Debug, Serialize)]
struct SpeechPath {
    stages: Vec<Stage>,
}

/// One dependency a conversation runs through, and whether this deployment reached it.
///
/// The stages are instances of one type rather than a field each, so the page draws
/// whatever it is sent and a stage added here needs no page change to appear.
/// [LAW:one-type-per-behavior]
///
/// `name` is also the cell the page draws it into, and the page names cells of its own —
/// `agent`, `room`, `call`, `audio`, `hearing`, `voice`. A stage taking one of those would
/// give that cell two writers and show whichever wrote last, so a new stage gets a name
/// none of them uses. `agent` is the trap worth naming: it is the obvious third stage and
/// the page already draws it from the room's roster. [LAW:one-source-of-truth]
#[derive(Debug, Serialize)]
struct Stage {
    name: &'static str,
    #[serde(flatten)]
    reach: Reach,
}

/// Whether a stage answered.
///
/// A union rather than a bool beside an optional reason, which would admit both states
/// that mean nothing — reached with a complaint, unreached with no account of itself.
/// [LAW:types-are-the-program]
#[derive(Debug, Serialize)]
#[serde(tag = "reach", rename_all = "snake_case")]
enum Reach {
    Reachable,
    Unreachable {
        /// Why, in words chosen here rather than carried off the failure.
        ///
        /// `&'static str` and not `String`, and that is the whole guarantee: this body
        /// goes to an unauthenticated caller, upstream errors carry bearer tokens, LAN
        /// addresses and paths off this filesystem, and a literal cannot hold any of
        /// them. The leak is unrepresentable rather than guarded against — the same
        /// split [`NoVoices`] makes, made by the type this time.
        /// [LAW:types-are-the-program]
        because: &'static str,
    },
}

/// A failure that has a public account of itself, separate from the one an operator needs.
///
/// One trait rather than a rendering per stage, so [`reached`] is a single function over
/// every upstream this route probes. [LAW:one-type-per-behavior]
trait Unreached: std::error::Error {
    fn because(&self) -> &'static str;
}

/// Matched exhaustively, with no catch-all arm: a variant added to [`TtsError`] should
/// stop the build here and be given its own sentence, rather than be absorbed into the
/// nearest one and reported as a fault it is not.
impl Unreached for TtsError {
    fn because(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "the text-to-speech server did not answer",
            Self::Refused { .. } => "the text-to-speech server refused the request",
            Self::Undecodable(_) => "the text-to-speech server answered with something that is not audio",
            Self::Unreadable(_) => "the text-to-speech server's voice listing could not be read",
        }
    }
}

impl Unreached for LiveKitError {
    fn because(&self) -> &'static str {
        match self {
            // Not "did not answer": this variant also carries an SFU that answered
            // promptly with 401 on a rotated credential, and sending a reader to check
            // whether a healthy SFU is up is the wrong errand.
            Self::ListRooms(_) => "the SFU would not say which rooms are open",
            Self::CreateRoom(_) => "the SFU refused to open a room",
            Self::MintToken(_) => "this deployment could not sign a token for the SFU",
            Self::Metadata(_) => "this deployment could not describe a conversation to the SFU",
        }
    }
}

/// One stage's outcome, told twice: in full to whoever can act on it, and in this file's
/// own words to the page.
///
/// The single place that split is made, so a stage added here cannot be the one that
/// reports an upstream's `Display` to an unauthenticated caller by having been written
/// slightly differently. [LAW:single-enforcer]
fn reached<E: Unreached>(name: &'static str, outcome: Result<(), E>) -> Stage {
    let reach = match outcome {
        Ok(()) => Reach::Reachable,
        Err(error) => {
            // The detail goes where an operator will find it, which is the only place it
            // can go: the body below is read by a caller who presented no credential.
            tracing::error!(stage = name, error = %error, "a stage of the speech path did not answer");
            Reach::Unreachable { because: error.because() }
        }
    };

    Stage { name, reach }
}

/// Whether this deployment can currently reach what a conversation needs.
///
/// Probed with calls this deployment already makes — the voice listing every caller's
/// dropdown is built from, and the room listing [`crate::reconcile`] trusts — rather than
/// with a ping written for this route, so a green stage is a fact about the real client,
/// address and credential rather than about a code path only this handler runs.
/// [LAW:one-source-of-truth]
///
/// Both are asked, always, and neither can cut the other short: a dead text-to-speech
/// server that stopped this from reporting on the SFU would be this ticket's own bug
/// wearing a new shape. [LAW:dataflow-not-control-flow] Concurrently because they are
/// independent, and a caller looking at this page is waiting on it.
///
/// 200 whatever the stages say. The readout succeeded — it is the subject that is
/// unwell — and answering 5xx would fail the page's own fetch and draw nothing at all,
/// which is the silence this exists to end. [LAW:no-silent-failure]
///
/// Unauthenticated like the routes beside it, and it reaches two upstreams per hit rather
/// than reading state this process holds. Uncached on purpose: a readout answering from a
/// few seconds ago states a past fact as a present one, which is the failure it exists to
/// refuse. What bounds the load is the deployment rather than this code — nothing here is
/// exposed to the internet, so its callers are tailnet peers and the page's one per load.
///
/// What it does *not* prove: that speech comes out. `voices` is a listing, served from a
/// different path than the `/v1/text-to-speech/{voice}/stream` a conversation runs on, so
/// a router answering its listing while the engine behind it refuses every synthesis
/// reads green here. This narrows the causes of silence; it does not exhaust them, and a
/// green strip beside a silent call means the fault is past where this can see.
///
/// Nor that a browser can reach the SFU. This deployment dials [`LiveKit::signaling_url`]
/// and hands the page [`LiveKit::public_signaling_url`], which a homelab deliberately
/// makes different addresses — so a reachable SFU here and a browser that cannot join are
/// compatible readings, and that gap is `.15`'s to close.
///
/// [`LiveKit::signaling_url`]: crate::livekit::LiveKit::signaling_url
/// [`LiveKit::public_signaling_url`]: crate::livekit::LiveKit::public_signaling_url
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let (voices, rooms) = tokio::join!(state.tts.voices(), state.livekit.live_rooms());

    Json(SpeechPath {
        stages: vec![
            reached("sfu", rooms.map(drop)),
            reached("text-to-speech", voices.map(drop)),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit_api::access_token::AccessTokenError;
    use livekit_api::services::ServiceError;
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

    /// Every `TtsError`, as a stage of the readout, beside the secret it carries.
    ///
    /// The same fixtures the 502 leak test uses, because the exposure is the same one: an
    /// unauthenticated caller, and errors whose `Display` names a LAN address, a bearer
    /// token and a path off this filesystem.
    fn carried() -> [(TtsError, &'static str); 4] {
        [
            (TtsError::Unreachable("http://10.4.0.7:20977/v1/voices".to_owned()), "10.4.0.7"),
            (TtsError::Refused { status: 401, body: "bad token sk-abcdef".to_owned() }, "sk-abcdef"),
            (TtsError::Undecodable("/srv/openconv/voices/heart.onnx".to_owned()), "heart.onnx"),
            (TtsError::Unreadable("missing `voices` at line 3".to_owned()), "line 3"),
        ]
    }

    /// Every `LiveKitError`, the other half of what a stage can be.
    ///
    /// No secret beside each one, unlike [`carried`]: these never reach a body, and what
    /// is being held to a bar here is the sentence this file gives them.
    fn refused() -> [LiveKitError; 4] {
        [
            LiveKitError::ListRooms(ServiceError::Env(std::env::VarError::NotPresent)),
            LiveKitError::CreateRoom(ServiceError::Env(std::env::VarError::NotPresent)),
            LiveKitError::MintToken(AccessTokenError::InvalidKeys),
            LiveKitError::Metadata(serde_json::from_str::<i32>("{").expect_err("not a number")),
        ]
    }

    /// The readout is unauthenticated, so a stage that failed must name the stage and
    /// nothing else about the far side.
    ///
    /// Asserted against the serialized body rather than against `because` alone: what
    /// reaches the caller is the JSON, and a later field carrying the error would pass a
    /// check that only ever read the one field known to be safe.
    #[test]
    fn an_unreachable_stage_tells_the_caller_nothing_about_why() {
        for (error, secret) in carried() {
            // The fixture has to still carry the secret for this to be proving anything.
            assert!(error.to_string().contains(secret), "the fixture stopped carrying {secret:?}");

            let stage = reached("text-to-speech", Err(error));
            let body = serde_json::to_string(&stage).expect("serializes");

            assert!(!body.contains(secret), "the readout leaked {secret:?}: {body}");
            assert!(body.contains("text-to-speech"), "the readout does not name the stage: {body}");
            assert!(body.contains("unreachable"), "a stage that failed does not say so: {body}");
        }
    }

    /// The point of naming a stage is naming *which* fault it has, so a reader knows
    /// whether to restart something or go and look at it.
    ///
    /// Collapsing these onto one sentence would leave every assertion above passing while
    /// the page went back to reporting one symptom for many causes — which is the whole of
    /// what this ticket exists to end.
    ///
    /// Both impls, because a sentence is only unique against the ones it shares a readout
    /// with, and only `ListRooms` of the SFU's four ever reaches a screen — so the other
    /// three could be given each other's words with nothing on the page to contradict it.
    #[test]
    fn each_way_a_stage_can_fail_reads_differently() {
        let mut said: Vec<&str> = carried()
            .iter()
            .map(|(error, _)| error.because())
            .chain(refused().iter().map(|error| error.because()))
            .collect();
        said.sort_unstable();
        let total = said.len();
        said.dedup();

        assert_eq!(said.len(), total, "two ways of failing report the same sentence");
        assert!(said.iter().all(|because| !because.is_empty()), "a failure with no account");
    }

    /// A stage that answered carries no complaint at all — not an empty one, which the
    /// page would have to decide how to read.
    #[test]
    fn a_stage_that_answered_says_only_that() {
        let body = serde_json::to_value(reached("sfu", Ok::<(), TtsError>(())))
            .expect("serializes");

        assert_eq!(body, serde_json::json!({"name": "sfu", "reach": "reachable"}));
    }

    /// Whether the page reads `name` written as `syntax`, where `{}` stands for the name.
    ///
    /// Bare containment is not a guard, because most short names are already somewhere in
    /// a 500-line file: `ok` is inside `response.ok`, `stage` inside `for (const stage of`,
    /// and `name` and `status` are everywhere. Nor is it enough to demand the surrounding
    /// syntax, since one name can sit inside another wearing it — `.stage` inside
    /// `.stages`, `reachable:` inside `unreachable:`. So each syntax delimits one end of
    /// the name and this checks the other, which is the end a longer name would run past.
    fn read_as(page: &str, syntax: &str, name: &str) -> bool {
        let needle = syntax.replace("{}", name);
        let extends = |c: char| c.is_alphanumeric() || c == '_' || c == '$';

        page.match_indices(&needle).any(|(at, _)| {
            if syntax.ends_with("{}") {
                !page[at + needle.len()..].chars().next().is_some_and(extends)
            } else {
                !page[..at].chars().next_back().is_some_and(extends)
            }
        })
    }

    /// The page reads this body by field name, and a rename does not fail — it renders
    /// `undefined` into a status cell, in a browser nobody is running during the change
    /// that caused it.
    ///
    /// The names come from serializing the real type rather than from being typed out
    /// here, so this compares the page against the route itself. The same guard the
    /// control-message test above puts on the protocol.
    #[test]
    fn the_page_reads_the_field_names_this_route_actually_sends() {
        let readout = SpeechPath {
            stages: vec![
                reached("sfu", Ok::<(), TtsError>(())),
                reached("text-to-speech", Err(TtsError::Unreachable("nope".to_owned()))),
            ],
        };
        let serde_json::Value::Object(body) =
            serde_json::to_value(&readout).expect("serializes")
        else {
            panic!("the readout is not a JSON object");
        };

        let page = ASSETS
            .iter()
            .find(|asset| asset.path.ends_with("app.js"))
            .expect("the page has no module that reads this")
            .body;

        // Searched for in the syntax that actually reads them: a field as `.field`, and a
        // `reach` word as the `word:` that keys the page's table.
        let mut wanted: Vec<(&'static str, String)> =
            body.keys().map(|field| (".{}", field.clone())).collect();
        for stage in body["stages"].as_array().expect("stages is a list") {
            let stage = stage.as_object().expect("a stage is an object");
            wanted.extend(stage.keys().map(|field| (".{}", field.clone())));
            wanted.push((
                "{}:",
                stage["reach"].as_str().expect("a stage with no reach").to_owned(),
            ));
        }

        // Otherwise a serialization that produced nothing would pass by having nothing to
        // check, which is the shape of failure this whole route exists to refuse.
        assert!(wanted.len() > 4, "the readout named almost nothing: {wanted:?}");

        for (syntax, name) in wanted {
            assert!(
                read_as(page, syntax, &name),
                "this route sends {name:?}, which the page never reads as {syntax:?}"
            );
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
        for (error, secret) in carried() {
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
