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
    /// service calls are Twirp POSTs against this; clients reach the same host over
    /// `wss://` for signaling.
    pub livekit_url: String,
    /// Credentials from Vault at `secret/livekit`, used both to sign participant
    /// tokens and to authenticate our own room service calls.
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    /// The value callers must present in `xi-api-key`. Happy's server holds the
    /// matching value in its `ELEVENLABS_API_KEY`, which is what makes openconv a
    /// drop-in for `api.elevenlabs.io` from its point of view.
    pub xi_api_key: XiApiKey,
    pub bind: SocketAddr,
    /// Append-only record of conversations, read back by `GET /v1/convai/conversations`.
    pub conversation_log: PathBuf,
    /// The whisper.cpp model the agent hears with. Loaded once at startup and shared by
    /// every conversation.
    pub whisper_model: PathBuf,
    /// Credentials and model for the LLM that decides what the agent says.
    pub anthropic_api_key: String,
    pub llm_model: String,
    /// Origin of elvenreader-server, which turns the agent's words into speech. Not a
    /// credential — it is reached over the private network and takes no API key.
    pub tts_url: String,
    /// The voice used when the client asks for none, as an ElevenLabs voice ID.
    ///
    /// A default rather than a mapping: elvenreader-server resolves IDs it does not
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
        let xi_api_key = required("OPENCONV_API_KEY");
        let anthropic_api_key = required("ANTHROPIC_API_KEY");

        // Reporting every missing name in one pass beats failing on the first: an
        // operator bringing the service up for the first time gets the whole list.
        let (
            Some(livekit_api_key),
            Some(livekit_api_secret),
            Some(xi_api_key),
            Some(anthropic_api_key),
        ) = (livekit_api_key, livekit_api_secret, xi_api_key, anthropic_api_key)
        else {
            return Err(ConfigError::Missing(missing));
        };

        let livekit_url = optional("LIVEKIT_URL", "https://livekit.sanctuary.gdn")
            .trim_end_matches('/')
            .to_owned();

        let bind_spec = optional("OPENCONV_BIND", "0.0.0.0:8080");
        let bind = bind_spec
            .parse()
            .map_err(|_| ConfigError::NotASocketAddr(bind_spec))?;

        Ok(Self {
            livekit_url,
            livekit_api_key,
            livekit_api_secret,
            xi_api_key: XiApiKey(xi_api_key),
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
    fn debug_does_not_leak_the_secret() {
        let rendered = format!("{:?}", XiApiKey::new("super-secret-value"));
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }
}
