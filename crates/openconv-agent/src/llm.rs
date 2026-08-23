//! Deciding what the agent says next.
//!
//! One trait, so the choice of model is a value rather than a shape. Claude is the
//! default; a local model behind the same trait changes which [`Llm`] the process is
//! built with and nothing else.
//!
//! Rust has no official Anthropic SDK, so the Claude implementation speaks the Messages
//! API over HTTP directly.

use std::fmt;

/// One side of one exchange.
///
/// A two-variant enum rather than a `{role: String, text: String}` pair, because the
/// role is a closed set and a typo in a role string is a request the API rejects at the
/// far end rather than a mistake the compiler catches here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    Caller(String),
    Agent(String),
}

impl Turn {
    fn role(&self) -> &'static str {
        match self {
            Self::Caller(_) => "user",
            Self::Agent(_) => "assistant",
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Caller(text) | Self::Agent(text) => text,
        }
    }
}

/// What the agent says next, given the conversation so far.
pub trait Llm: Send + Sync {
    fn respond<'a>(
        &'a self,
        system: &'a str,
        turns: &'a [Turn],
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + 'a>>;
}

/// Claude, over the Messages API.
pub struct Claude {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

/// Two speech-shaped sentences fit well inside this; the ceiling exists to stop a
/// runaway answer from being synthesized into a minute of unwanted speech.
const MAX_TOKENS: u32 = 1024;

/// The Messages API version this code is written against.
const API_VERSION: &str = "2023-06-01";

impl Claude {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                // A caller is waiting in silence for this. Better to fail the turn and
                // say so than to leave them listening to nothing indefinitely.
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client with only a timeout configured cannot fail to build"),
            api_key,
            model,
        }
    }
}

impl Llm for Claude {
    fn respond<'a>(
        &'a self,
        system: &'a str,
        turns: &'a [Turn],
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let messages: Vec<_> = turns
                .iter()
                .map(|turn| serde_json::json!({"role": turn.role(), "content": turn.text()}))
                .collect();

            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": MAX_TOKENS,
                "system": system,
                "messages": messages,
                // Low effort, thinking left on. This is a conversation: the caller is
                // waiting in real time, and depth past what a spoken reply needs is
                // latency they hear as silence.
                //
                // Thinking stays on deliberately. Disabling it is the larger latency
                // saving and it breaks tool use — the model then occasionally writes a
                // tool call into its visible text instead of emitting a tool-use block,
                // so the call silently never runs and the words are spoken aloud
                // instead. The tools this agent needs arrive in ticket .9, and that
                // failure would be invisible when they do.
                "output_config": {"effort": "low"},
            });

            let response = self
                .http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send()
                .await
                .map_err(|error| LlmError::Transport(error.to_string()))?;

            let status = response.status();
            let payload = response
                .text()
                .await
                .map_err(|error| LlmError::Transport(error.to_string()))?;

            if !status.is_success() {
                return Err(LlmError::Refused { status: status.as_u16(), body: payload });
            }

            parse_reply(&payload)
        })
    }
}

/// Pulls the spoken reply out of a Messages API response.
///
/// Concatenates the text blocks and ignores the rest: a response can carry thinking
/// blocks and, once ticket .9 lands, tool-use blocks, none of which are words to say
/// out loud.
fn parse_reply(payload: &str) -> Result<String, LlmError> {
    let json: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| LlmError::Malformed(error.to_string()))?;

    // A safety decline arrives as a successful response with an empty body rather than
    // an error, so it has to be checked before reading content — otherwise it reads as
    // the model having nothing to say.
    if json["stop_reason"] == "refusal" {
        return Err(LlmError::Declined);
    }

    let blocks = json["content"]
        .as_array()
        .ok_or_else(|| LlmError::Malformed("response has no content array".to_owned()))?;

    let spoken: String = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("");

    match spoken.trim() {
        "" => Err(LlmError::Empty),
        text => Ok(text.to_owned()),
    }
}

#[derive(Debug)]
pub enum LlmError {
    Transport(String),
    /// A non-2xx response. The body is carried because the API explains itself there.
    Refused { status: u16, body: String },
    Malformed(String),
    /// The model declined on safety grounds — a successful HTTP response with
    /// `stop_reason: "refusal"` and nothing to say.
    Declined,
    /// A successful response with no text in it, which is not something to speak.
    Empty,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "could not reach the model: {error}"),
            Self::Refused { status, body } => write!(f, "model returned HTTP {status}: {body}"),
            Self::Malformed(error) => write!(f, "could not read the model's response: {error}"),
            Self::Declined => f.write_str("the model declined to answer"),
            Self::Empty => f.write_str("the model returned no text to speak"),
        }
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_map_to_the_api_roles() {
        assert_eq!(Turn::Caller("hi".into()).role(), "user");
        assert_eq!(Turn::Agent("hello".into()).role(), "assistant");
    }

    #[test]
    fn the_spoken_reply_is_the_text_blocks() {
        let reply = parse_reply(
            r#"{"content":[{"type":"text","text":"Sure, "},{"type":"text","text":"sending that now."}]}"#,
        )
        .unwrap();
        assert_eq!(reply, "Sure, sending that now.");
    }

    /// Thinking and tool-use blocks are not words to say out loud.
    #[test]
    fn non_text_blocks_are_not_spoken() {
        let reply = parse_reply(
            r#"{"content":[
                {"type":"thinking","thinking":"the caller wants the tests run"},
                {"type":"text","text":"Running the tests."},
                {"type":"tool_use","id":"t1","name":"sendMessageToSession","input":{}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(reply, "Running the tests.");
    }

    /// A decline is an HTTP 200 with an empty body — checked before content, or it
    /// reads as the model simply having nothing to say.
    #[test]
    fn a_safety_decline_is_distinguished_from_silence() {
        assert!(matches!(
            parse_reply(r#"{"stop_reason":"refusal","content":[]}"#),
            Err(LlmError::Declined)
        ));
        assert!(matches!(
            parse_reply(r#"{"stop_reason":"end_turn","content":[]}"#),
            Err(LlmError::Empty)
        ));
    }

    #[test]
    fn a_reply_of_only_whitespace_is_nothing_to_say() {
        assert!(matches!(
            parse_reply(r#"{"content":[{"type":"text","text":"   \n "}]}"#),
            Err(LlmError::Empty)
        ));
    }

    #[test]
    fn a_malformed_response_is_an_error_rather_than_silence() {
        assert!(matches!(parse_reply("not json"), Err(LlmError::Malformed(_))));
        assert!(matches!(parse_reply(r#"{"ok":true}"#), Err(LlmError::Malformed(_))));
    }
}
