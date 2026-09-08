//! The one place the process reads its environment.
//!
//! Everything downstream of [`Config::from_env`] runs on values known to exist, so no
//! handler ever asks whether a credential was configured. A missing variable stops the
//! process at startup with every missing name listed at once, rather than surfacing as
//! a 500 on the first voice call of the day.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Everything the service needs to run, with no absent values left in it.
#[derive(Clone, Debug)]
pub struct Config {
    /// Origin of the LiveKit deployment, e.g. `https://livekit.sanctuary.gdn`. Room
    /// service calls are Twirp POSTs against this, and the agent dials the same host
    /// over `wss://` for its own signaling.
    ///
    /// This is the address *this process* uses, which a deployment is free to make an
    /// in-cluster one the outside world cannot resolve.
    pub livekit_url: String,
    /// The same deployment, at the address a browser can reach.
    ///
    /// [`Self::livekit_url`] answers "how does this process reach the SFU"; a browser
    /// asks a different question, and the two have different answers whenever the
    /// service and the SFU sit behind the same private network. The homelab is exactly
    /// that case: the agent takes a LAN address straight from Consul to keep its own
    /// signaling off the tailnet, and a browser cannot route to it — and, served from
    /// an `https://` page, would refuse the `ws://` it implies as mixed content before
    /// ever trying.
    ///
    /// Defaults to [`Self::livekit_url`], so a deployment whose SFU is reachable the
    /// same way from both sides configures nothing and cannot drift. Setting it is a
    /// deliberate statement that the two routes differ — it must still name the *same*
    /// deployment, because a token minted here and offered to another deployment's SFU
    /// does not error: the caller joins a room the agent is not in and hears silence.
    pub public_livekit_url: String,
    /// Credentials from Vault at `secret/livekit`, used both to sign participant
    /// tokens and to authenticate our own room service calls.
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    /// How this deployment decides a caller may use it.
    pub caller_auth: CallerAuth,
    pub bind: SocketAddr,
    /// Append-only record of conversations, read back by `GET /v1/convai/conversations`.
    pub conversation_log: PathBuf,
    /// The whisper.cpp model the agent hears with. Loaded once at startup and shared by
    /// every conversation.
    pub whisper_model: PathBuf,
    /// Credentials and model for the LLM that decides what the agent says.
    pub anthropic_api_key: String,
    pub llm_model: String,
    /// Origin of the text-to-speech server, which turns the agent's words into speech. Not a
    /// credential — it is reached over the private network and takes no API key.
    pub tts_url: String,
    /// The voice used when the client asks for none, as an ElevenLabs voice ID.
    ///
    /// A default rather than a mapping: the text-to-speech server resolves IDs it does not
    /// serve, so this only decides which ID it is asked to resolve.
    pub tts_voice: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut missing = Vec::new();
        let mut required = |name: &'static str| {
            std::env::var(name).map_err(|_| missing.push(name)).ok()
        };

        let livekit_api_key = required("LIVEKIT_API_KEY");
        let livekit_api_secret = required("LIVEKIT_API_SECRET");
        let anthropic_api_key = required("ANTHROPIC_API_KEY");

        // Two variables answering one question, and a deployment answers it once. Both
        // set is a contradiction rather than a precedence puzzle: an operator who wrote
        // a secret *and* asked for no authentication does not have a preference for this
        // code to discover, they have a mistake to be told about. Silence is not an
        // answer either — see [`CallerAuth`] for why unset cannot mean open.
        // [LAW:no-silent-failure]
        let caller_auth = match caller_auth_from(
            env_value("OPENCONV_API_KEY"),
            env_value("OPENCONV_ALLOW_UNAUTHENTICATED").is_some_and(|value| is_affirmative(&value)),
        ) {
            Ok(auth) => Some(auth),
            Err(AuthProblem::Contradictory) => return Err(ConfigError::ContradictoryAuth),
            Err(AuthProblem::Unstated) => {
                // Named as one entry so the operator is told about the choice rather
                // than about a variable, and still inside the one pass below.
                missing.push("OPENCONV_API_KEY or OPENCONV_ALLOW_UNAUTHENTICATED");
                None
            }
        };

        // Reporting every missing name in one pass beats failing on the first: an
        // operator bringing the service up for the first time gets the whole list.
        let (
            Some(livekit_api_key),
            Some(livekit_api_secret),
            Some(caller_auth),
            Some(anthropic_api_key),
        ) = (livekit_api_key, livekit_api_secret, caller_auth, anthropic_api_key)
        else {
            return Err(ConfigError::Missing(missing));
        };

        let livekit_url = optional("LIVEKIT_URL", "https://livekit.sanctuary.gdn")
            .trim_end_matches('/')
            .to_owned();

        // [LAW:one-source-of-truth] Defaulting to `livekit_url` rather than to a second
        // literal keeps one value until a deployment says otherwise, so the pair cannot
        // silently disagree about which SFU is meant.
        let public_livekit_url = optional("OPENCONV_PUBLIC_LIVEKIT_URL", &livekit_url)
            .trim_end_matches('/')
            .to_owned();

        let bind_spec = optional("OPENCONV_BIND", "0.0.0.0:8080");
        let bind = bind_spec
            .parse()
            .map_err(|_| ConfigError::NotASocketAddr(bind_spec))?;

        Ok(Self {
            livekit_url,
            public_livekit_url,
            livekit_api_key,
            livekit_api_secret,
            caller_auth,
            bind,
            conversation_log: PathBuf::from(optional(
                "OPENCONV_CONVERSATION_LOG",
                "conversations.jsonl",
            )),
            whisper_model: PathBuf::from(optional(
                "OPENCONV_WHISPER_MODEL",
                &default_whisper_model(),
            )),
            anthropic_api_key,
            llm_model: optional("OPENCONV_LLM_MODEL", "claude-opus-5"),
            tts_url: optional("OPENCONV_TTS_URL", "http://127.0.0.1:11000")
                .trim_end_matches('/')
                .to_owned(),
            // ElevenLabs' own default, and what Happy's settings screen starts from, so
            // an untouched app and an untouched deployment agree on the voice.
            tts_voice: optional("OPENCONV_TTS_VOICE", "21m00Tcm4TlvDq8ikWAM"),
        })
    }
}

fn optional(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

/// A variable's value, with an empty one read as absent.
///
/// A Nomad template whose Vault lookup found nothing renders the name with nothing after
/// it, which arrives here as `Some("")`. Treating that as a configured empty secret would
/// mean every caller authenticates by sending an empty header. [LAW:no-silent-failure]
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Whether the operator asked, in so many words, for no authentication.
///
/// Only an unambiguous yes counts. Anything else — a typo, `false`, an empty render —
/// reads as "not asked for", so the process stops and names the choice instead of
/// starting up open. The three spellings are liberality at the outermost edge, where it
/// belongs: the cost of rejecting `1` is an operator staring at a variable they did set.
fn is_affirmative(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
}

/// Which of the two states the environment describes, or why it describes neither.
///
/// A pure function of the two values rather than a reader of them, so the rule can be
/// pinned by tests: setting environment variables is process-wide, and a test that does
/// it races every other test in the binary. [LAW:effects-at-boundaries]
fn caller_auth_from(secret: Option<String>, unauthenticated: bool) -> Result<CallerAuth, AuthProblem> {
    match (secret, unauthenticated) {
        (Some(_), true) => Err(AuthProblem::Contradictory),
        (Some(secret), false) => Ok(CallerAuth::SharedSecret(XiApiKey(secret))),
        (None, true) => Ok(CallerAuth::Open),
        (None, false) => Err(AuthProblem::Unstated),
    }
}

/// Why the environment named no authentication decision this process can act on.
#[derive(Debug, PartialEq, Eq)]
enum AuthProblem {
    /// A secret and a request for no authentication, which ask for opposite things.
    Contradictory,
    /// Neither, which asks for nothing at all.
    Unstated,
}

/// Where `scripts/fetch-whisper-model.sh` puts the model.
///
/// Outside the repository because it is a hundred-odd megabytes of weights that no
/// commit should carry, and under the user's cache rather than a temporary directory so
/// it survives a reboot and is fetched once.
fn default_whisper_model() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    format!("{home}/.cache/openconv/models/ggml-base.en.bin")
}

/// How this deployment decides a caller may use it.
///
/// [LAW:types-are-the-program] Two arms rather than an `Option<XiApiKey>`, because the
/// absence of a credential is a decision an operator makes and not a value they forgot,
/// and an optional reads identically for both. The reading that costs something is the
/// wrong one: a deployment that meant to authenticate, whose secret rendered empty, would
/// come up open — minting LiveKit tokens and spending an Anthropic budget for anyone who
/// can reach it, with nothing anywhere reporting a problem. Naming the two states makes
/// that silence unrepresentable; `from_env` refuses every reading but one.
// Equality runs through `XiApiKey`'s constant-time compare, so deriving it here adds no
// timing channel that the extractor did not already have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallerAuth {
    /// Every caller presents this value in `xi-api-key`.
    SharedSecret(XiApiKey),
    /// No credential is asked for: whatever can reach this service may use it.
    ///
    /// What a deployment states when it is its own only caller — a Happy pointed at an
    /// openconv on the same private network, where a shared secret is a value to
    /// provision, sync and rotate in exchange for nothing the network is not already
    /// enforcing. It is a statement about the network, so it is only ever true of a
    /// deployment whose network is the boundary.
    Open,
}

/// The shared secret callers present as `xi-api-key`.
///
/// A newtype rather than a `String` so it cannot be swapped with the LiveKit
/// credentials it sits beside, and so its comparison is the only one available: the
/// [`PartialEq`] impl is constant-time, which a bare `String` comparison is not.
#[derive(Clone)]
pub struct XiApiKey(String);

impl XiApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl PartialEq for XiApiKey {
    /// Compares without an early return, so the time taken does not reveal how long a
    /// prefix of the secret a caller has guessed. The length check is not secret —
    /// lengths leak through the request anyway.
    fn eq(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.0.as_bytes(), other.0.as_bytes());
        ours.len() == theirs.len()
            && ours
                .iter()
                .zip(theirs)
                .fold(0u8, |differences, (a, b)| differences | (a ^ b))
                == 0
    }
}

impl Eq for XiApiKey {}

/// Keeps the secret out of logs and panic messages, which is the whole reason the
/// derive is not used here.
impl fmt::Debug for XiApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XiApiKey(<redacted>)")
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(Vec<&'static str>),
    NotASocketAddr(String),
    /// Both `OPENCONV_API_KEY` and `OPENCONV_ALLOW_UNAUTHENTICATED` were set.
    ContradictoryAuth,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(names) => write!(
                f,
                "missing required environment {}: {}. The LiveKit pair lives in Vault at \
                 secret/livekit. Authentication is answered by either OPENCONV_API_KEY or \
                 OPENCONV_ALLOW_UNAUTHENTICATED=true",
                if names.len() == 1 { "variable" } else { "variables" },
                names.join(", ")
            ),
            Self::NotASocketAddr(value) => {
                write!(f, "OPENCONV_BIND={value:?} is not a socket address")
            }
            Self::ContradictoryAuth => f.write_str(
                "OPENCONV_API_KEY and OPENCONV_ALLOW_UNAUTHENTICATED are both set, and they \
                 ask for opposite things. Set the key to require it in `xi-api-key`, or set \
                 OPENCONV_ALLOW_UNAUTHENTICATED=true to ask callers for no credential at all \
                 — never both",
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_compare_by_value() {
        assert_eq!(XiApiKey::new("sk-abc"), XiApiKey::new("sk-abc"));
        assert_ne!(XiApiKey::new("sk-abc"), XiApiKey::new("sk-abd"));
        assert_ne!(XiApiKey::new("sk-abc"), XiApiKey::new("sk-abcd"));
        assert_ne!(XiApiKey::new(""), XiApiKey::new("sk-abc"));
    }

    #[test]
    fn a_secret_is_the_credential_callers_must_present() {
        let auth = caller_auth_from(Some("sk-abc".to_owned()), false).expect("a stated secret");
        let CallerAuth::SharedSecret(expected) = auth else {
            panic!("a secret asks for authentication");
        };
        assert_eq!(expected, XiApiKey::new("sk-abc"));
    }

    #[test]
    fn asking_for_no_authentication_is_an_answer_rather_than_an_absence() {
        assert!(matches!(caller_auth_from(None, true), Ok(CallerAuth::Open)));
    }

    /// The failure this whole shape exists to prevent: a deployment that meant to
    /// authenticate, whose secret rendered empty, must stop rather than come up open.
    #[test]
    fn an_unstated_decision_stops_the_process_rather_than_opening_it() {
        assert_eq!(caller_auth_from(None, false), Err(AuthProblem::Unstated));
    }

    #[test]
    fn a_secret_and_a_request_for_none_is_refused_rather_than_ranked() {
        assert_eq!(
            caller_auth_from(Some("sk-abc".to_owned()), true),
            Err(AuthProblem::Contradictory),
        );
    }

    #[test]
    fn only_an_unambiguous_yes_opens_a_deployment() {
        for yes in ["true", "TRUE", " true ", "1", "yes"] {
            assert!(is_affirmative(yes), "{yes:?}");
        }
        // `false` and a typo have to land on the same side, and it has to be the side
        // that keeps the credential: reading either as a yes is the silent open.
        for no in ["false", "0", "no", "ture", ""] {
            assert!(!is_affirmative(no), "{no:?}");
        }
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let rendered = format!("{:?}", XiApiKey::new("super-secret-value"));
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }
}
