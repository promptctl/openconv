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
        let mut problems = Vec::new();

        // Absent is a whole answer for this one and the default one: a deployment that
        // configures no credential asks callers for none. Present-but-unusable is not an
        // answer, and `read` is what tells the two apart.
        let api_key = match read("OPENCONV_API_KEY") {
            Ok(value) => value.map(XiApiKey::new),
            Err(problem) => {
                problems.push(problem);
                None
            }
        };

        // Scoped so the borrow of `problems` ends with the reads that contribute to it.
        let (livekit_api_key, livekit_api_secret, anthropic_api_key) = {
            let mut require = |name: &'static str| match read(name) {
                Ok(Some(value)) => Some(value),
                Ok(None) => {
                    problems.push(Problem::Missing(name));
                    None
                }
                Err(problem) => {
                    problems.push(problem);
                    None
                }
            };

            (
                require("LIVEKIT_API_KEY"),
                require("LIVEKIT_API_SECRET"),
                require("ANTHROPIC_API_KEY"),
            )
        };

        // Everything wrong with the environment in one report, rather than one deploy per
        // problem: an operator standing a deployment up for the first time is usually
        // missing more than one thing, and learning them one restart at a time is how a
        // fifteen-minute setup becomes an afternoon.
        let (
            Some(livekit_api_key),
            Some(livekit_api_secret),
            Some(anthropic_api_key),
        ) = (livekit_api_key, livekit_api_secret, anthropic_api_key)
        else {
            return Err(ConfigError::Environment(problems));
        };

        // Reached when all three are present and the optional key was the unusable one.
        if !problems.is_empty() {
            return Err(ConfigError::Environment(problems));
        }

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

/// The one way this process reads a variable: absent, usable, or a named problem.
///
/// [LAW:single-enforcer] Every secret goes through here, and that uniformity is the whole
/// point rather than tidiness. These four values arrive by the same Nomad-and-Vault path,
/// so they fail the same ways, and a reading applied to one of them is a reading the other
/// three need. Before this, `OPENCONV_API_KEY` refused an empty value while an equally
/// empty `LIVEKIT_API_KEY` sailed through as present — the same accident caught in one
/// place and waved through in another, to be discovered later as a cryptic auth failure on
/// the first call of the day.
///
/// The two failures it names are the two a template produces. Empty is a Vault lookup that
/// found nothing and rendered `NAME=` with nothing after it. Non-unicode is a mangled
/// render — and it matters most for the optional key, where `std::env::var(..).ok()` would
/// collapse it into "nobody configured a credential" and bring the deployment up open,
/// which is the exact outcome the empty check exists to prevent, reached by another door.
/// [LAW:no-silent-failure]
///
/// The value is trimmed, so what is judged empty and what is later compared are the same
/// string. A key rendered with a trailing newline configured that key — HTTP strips the
/// same whitespace from the header before it is ever compared, so keeping it would refuse
/// the very credential the operator set, in a `401` blaming the caller.
/// [LAW:parse-dont-validate]
fn read(name: &'static str) -> Result<Option<String>, Problem> {
    interpret(name, std::env::var(name))
}

/// The reading itself, as a function of what the environment said.
///
/// Split from [`read`] so the rule can be held by tests: setting a variable is
/// process-wide, so a test that did it would race every other test in this binary.
/// [LAW:effects-at-boundaries]
fn interpret(
    name: &'static str,
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, Problem> {
    match value {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(Problem::NotUnicode(name)),
        Ok(value) if value.trim().is_empty() => Err(Problem::Empty(name)),
        Ok(value) => Ok(Some(value.trim().to_owned())),
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

/// What was wrong with one variable, named so the operator can act on it without reading
/// this file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Missing(&'static str),
    /// Set, with nothing in it. Almost always a Vault lookup that found nothing.
    Empty(&'static str),
    /// Set, and not text. A mangled render rather than a choice anybody made.
    NotUnicode(&'static str),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "{name} is not set"),
            Self::Empty(name) => write!(
                f,
                "{name} is set to nothing — the usual cause is a Vault lookup that found \
                 nothing"
            ),
            Self::NotUnicode(name) => write!(f, "{name} is set to something that is not text"),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    /// Everything wrong with the environment, in one report.
    Environment(Vec<Problem>),
    NotASocketAddr(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(problems) => {
                write!(f, "the environment is not usable: ")?;
                for (position, problem) in problems.iter().enumerate() {
                    write!(f, "{}{problem}", if position == 0 { "" } else { "; " })?;
                }

                // The two hints worth carrying, because the fix for either problem is a
                // place rather than a syntax. Said once at the end rather than per
                // variable, so a report of four problems is still one paragraph.
                write!(
                    f,
                    ". The LiveKit pair lives in Vault at secret/livekit. OPENCONV_API_KEY \
                     is the credential callers must send — unset it entirely to ask callers \
                     for no credential at all"
                )
            }
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

    const NAME: &str = "OPENCONV_API_KEY";

    fn said(value: &str) -> Result<String, std::env::VarError> {
        Ok(value.to_owned())
    }

    #[test]
    fn a_value_is_the_credential_callers_must_present() {
        assert_eq!(interpret(NAME, said("sk-abc")), Ok(Some("sk-abc".to_owned())));
    }

    /// Absent is an answer rather than a problem — for the optional credential it is the
    /// default posture, and for a required one the caller turns it into `Missing`.
    #[test]
    fn an_unset_variable_is_absent_rather_than_a_problem() {
        assert_eq!(interpret(NAME, Err(std::env::VarError::NotPresent)), Ok(None));
    }

    /// The failure this shape exists to prevent: a deployment that meant to ask for a
    /// credential, whose secret rendered empty, must stop rather than come up open.
    #[test]
    fn an_empty_value_is_a_problem_rather_than_an_absent_one() {
        for empty in ["", " ", "\n"] {
            assert_eq!(interpret(NAME, said(empty)), Err(Problem::Empty(NAME)), "{empty:?}");
        }
    }

    /// The same failure by the other door: a mangled render must not read as "nobody
    /// configured a credential", which is what `std::env::var(..).ok()` would have made of
    /// it — and what would have brought the deployment up open.
    #[test]
    fn a_value_that_is_not_text_is_a_problem_rather_than_an_absent_one() {
        let mangled = Err(std::env::VarError::NotUnicode(std::ffi::OsString::from("sk-")));
        assert_eq!(interpret(NAME, mangled), Err(Problem::NotUnicode(NAME)));
    }

    /// The near miss of the empty case: a template that renders the key with a newline
    /// after it configured a key, not nothing, and the caller sending that key must be
    /// admitted — HTTP strips the same whitespace from the header before the extractor
    /// ever sees it, so an untrimmed value here is one no caller could ever present.
    #[test]
    fn a_value_is_read_without_the_whitespace_a_template_wrapped_it_in() {
        assert_eq!(interpret(NAME, said("sk-abc\n")), Ok(Some("sk-abc".to_owned())));
    }

    /// Every secret is read the same way, which is the point of there being one reader:
    /// the accident that empties one variable empties the others identically, and a
    /// deployment whose LiveKit key rendered empty must stop at startup rather than fail
    /// on its first room-service call.
    #[test]
    fn every_secret_is_read_by_the_same_rule() {
        for name in ["LIVEKIT_API_KEY", "LIVEKIT_API_SECRET", "ANTHROPIC_API_KEY"] {
            assert_eq!(interpret(name, said("")), Err(Problem::Empty(name)));
            assert_eq!(interpret(name, said(" value \n")), Ok(Some("value".to_owned())));
        }
    }

    /// A report names every problem it found, so an operator standing a deployment up
    /// learns all of them at once instead of one restart at a time.
    #[test]
    fn one_report_names_every_problem() {
        let rendered = ConfigError::Environment(vec![
            Problem::Missing("ANTHROPIC_API_KEY"),
            Problem::Empty("OPENCONV_API_KEY"),
        ])
        .to_string();

        assert!(rendered.contains("ANTHROPIC_API_KEY is not set"), "{rendered}");
        assert!(rendered.contains("OPENCONV_API_KEY is set to nothing"), "{rendered}");
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let rendered = format!("{:?}", XiApiKey::new("super-secret-value"));
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }
}
