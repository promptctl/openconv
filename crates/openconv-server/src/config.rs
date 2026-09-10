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
    /// The credential callers present as `xi-api-key`, when this deployment asks for one.
    ///
    /// Absent by default, which is a deployment saying its network is its boundary. Set it
    /// and every route asks for it; nothing else changes, and nothing else has to.
    pub api_key: Option<XiApiKey>,
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

        // Reporting every missing name in one pass beats failing on the first: an
        // operator bringing the service up for the first time gets the whole list.
        let (
            Some(livekit_api_key),
            Some(livekit_api_secret),
            Some(anthropic_api_key),
        ) = (livekit_api_key, livekit_api_secret, anthropic_api_key)
        else {
            return Err(ConfigError::Missing(missing));
        };

        // Not in that pass, because it is not missing: asking for no credential is a whole
        // answer, and the only unusable reading of this variable is the one `api_key_from`
        // refuses.
        let api_key = api_key_from(std::env::var("OPENCONV_API_KEY").ok())?;

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
            api_key,
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

/// The credential this deployment asks callers for, read from what the environment said.
///
/// Three readings, because the third is the one that bites. Unset asks for none: the
/// deployment's network is its boundary, which is true of an openconv reachable only over
/// a tailnet by the service that calls it. A value is the credential every caller
/// presents. Empty is neither — a Nomad template whose Vault lookup found nothing renders
/// `OPENCONV_API_KEY=` with nothing after it, and that is a deployment that meant to ask
/// for a credential and was handed none. Reading it as "asks for none" is how a service
/// comes up open, minting LiveKit tokens and spending an Anthropic budget for anyone who
/// can reach it, with nothing reporting a problem — so it stops instead.
/// [LAW:no-silent-failure]
///
/// A pure function of the value rather than a reader of the variable, so the rule can be
/// pinned by tests: setting environment variables is process-wide, and a test that does
/// it races every other test in the binary. [LAW:effects-at-boundaries]
///
/// All three readings are taken from the trimmed value, so what is judged empty and what
/// is later compared are the same string. A template that renders the key with a trailing
/// newline is the same accident as one that renders it empty, and reading the two
/// differently is how a deployment refuses the very key its operator configured, in a
/// `401` that says the caller got it wrong. [LAW:parse-dont-validate]
fn api_key_from(value: Option<String>) -> Result<Option<XiApiKey>, ConfigError> {
    match value.as_deref().map(str::trim) {
        None => Ok(None),
        Some("") => Err(ConfigError::EmptyApiKey),
        Some(key) => Ok(Some(XiApiKey::new(key))),
    }
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
    /// `OPENCONV_API_KEY` was set to nothing.
    EmptyApiKey,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(names) => write!(
                f,
                "missing required environment {}: {}. The LiveKit pair lives in Vault at \
                 secret/livekit",
                if names.len() == 1 { "variable" } else { "variables" },
                names.join(", ")
            ),
            Self::NotASocketAddr(value) => {
                write!(f, "OPENCONV_BIND={value:?} is not a socket address")
            }
            Self::EmptyApiKey => f.write_str(
                "OPENCONV_API_KEY is set to nothing, which asks callers for a credential \
                 nobody can present — the usual cause is a Vault lookup that found nothing. \
                 Give it the key callers must send, or unset it to ask for no credential at \
                 all",
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
    fn a_value_is_the_credential_callers_must_present() {
        let configured = api_key_from(Some("sk-abc".to_owned())).expect("a stated key");
        assert_eq!(configured, Some(XiApiKey::new("sk-abc")));
    }

    #[test]
    fn an_unset_variable_asks_callers_for_no_credential() {
        assert_eq!(api_key_from(None).expect("no key is an answer"), None);
    }

    /// The failure this shape exists to prevent: a deployment that meant to ask for a
    /// credential, whose secret rendered empty, must stop rather than come up open.
    #[test]
    fn an_empty_value_stops_the_process_rather_than_opening_it() {
        for empty in ["", " ", "\n"] {
            assert!(
                matches!(api_key_from(Some(empty.to_owned())), Err(ConfigError::EmptyApiKey)),
                "{empty:?}",
            );
        }
    }

    /// The near miss of the one above: a template that renders the key with a newline
    /// after it configured a key, not nothing, and the caller sending that key must be
    /// admitted — HTTP strips the same whitespace from the header before the extractor
    /// ever sees it, so an untrimmed value here is one no caller could ever present.
    #[test]
    fn a_value_is_read_without_the_whitespace_a_template_wrapped_it_in() {
        let configured = api_key_from(Some("sk-abc\n".to_owned())).expect("a stated key");
        assert_eq!(configured, Some(XiApiKey::new("sk-abc")));
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let rendered = format!("{:?}", XiApiKey::new("super-secret-value"));
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }
}
