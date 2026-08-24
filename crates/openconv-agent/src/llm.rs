//! Deciding what the agent says next.
//!
//! One trait, so the choice of model is a value rather than a shape. Claude is the
//! default; a local model behind the same trait changes which [`Llm`] the process is
//! built with and nothing else.
//!
//! # Why a stream and not a string
//!
//! A reply is not produced all at once, and saying that it is costs the caller a second
//! of silence per turn: the agent cannot begin speaking the first sentence until the
//! last one has been written. [`Llm::respond`] therefore hands back the reply in the
//! pieces it actually arrives in, and speaking early falls out of that rather than being
//! arranged on top of it.
//!
//! Rust has no official Anthropic SDK, so the Claude implementation speaks the Messages
//! API over HTTP directly — here, the streaming half of it, as server-sent events.

use futures_util::stream::{self, Stream, StreamExt};
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;

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

/// A reply arriving in pieces.
///
/// Failure is an item rather than a separate outcome around the stream, because the two
/// ways a turn can break — never starting, and stopping halfway — are handled the same
/// way by everyone who cares: say so in the logs and drop the turn.
pub type Reply<'a> = Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send + 'a>>;

/// What the agent says next, given the conversation so far.
pub trait Llm: Send + Sync {
    fn respond<'a>(&'a self, system: &'a str, turns: &'a [Turn]) -> Reply<'a>;
}

/// Claude, over the Messages API.
pub struct Claude {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

/// Covers thinking as well as spoken words — the two share this budget, and the model
/// thinks by default. Two speech-shaped sentences fit inside what is left over with room
/// to spare; the ceiling exists to stop a runaway answer from being synthesized into a
/// minute of unwanted speech, not to trim a normal one.
const MAX_TOKENS: u32 = 4096;

/// The Messages API version this code is written against.
const API_VERSION: &str = "2023-06-01";

impl Claude {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                // Bounds the whole turn, not the wait for the first word: a streaming
                // response only completes when the model stops writing. Better to fail
                // the turn and say so than to leave a caller listening to nothing.
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("HTTP client with only a timeout configured cannot fail to build"),
            api_key,
            model,
        }
    }

    /// Opens the stream, or explains why it could not be opened.
    async fn open(&self, system: &str, turns: &[Turn]) -> Result<reqwest::Response, LlmError> {
        let messages: Vec<_> = turns
            .iter()
            .map(|turn| serde_json::json!({"role": turn.role(), "content": turn.text()}))
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": messages,
            "stream": true,
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
            .map_err(|error| LlmError::Transport(with_cause(&error)))?;

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        // The error body arrives as one piece even on a streaming request, because the
        // API rejects before it starts streaming. It is worth reading: the API explains
        // itself there, and a bare status code sends whoever reads the log guessing.
        let explanation = response.text().await.unwrap_or_default();
        Err(LlmError::Refused { status: status.as_u16(), body: explanation })
    }
}

impl Llm for Claude {
    fn respond<'a>(&'a self, system: &'a str, turns: &'a [Turn]) -> Reply<'a> {
        let opening = async move {
            match self.open(system, turns).await {
                Ok(response) => spoken_text(response.bytes_stream()),
                // The one failure that has no stream behind it, given the shape of one.
                Err(error) => Box::pin(stream::once(async move { Err(error) })) as Reply<'static>,
            }
        };

        Box::pin(stream::once(opening).flatten())
    }
}

/// Turns the response body into the words to say out loud.
///
/// Reads server-sent events off the wire and keeps only the text deltas. Thinking and,
/// once ticket .9 lands, tool-use blocks stream through the same connection and are not
/// words to speak — the same filtering the non-streaming path did over `content`, done
/// as the blocks arrive rather than after.
fn spoken_text<S, E>(body: S) -> Reply<'static>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: fmt::Display,
{
    struct Reading<S> {
        body: S,
        /// Events do not respect chunk boundaries, so a line can arrive in two pieces.
        partial: String,
        /// One chunk can complete several events, and the stream yields one item a poll.
        ready: VecDeque<Result<String, LlmError>>,
        /// Whether this stream has yielded anything at all, of either kind. A response
        /// with no text in it is only knowable at the end, and a turn that already
        /// explained itself — a decline, a broken connection — must not then be
        /// reported a second time as merely empty.
        reported: bool,
    }

    let reading = Reading {
        body: Box::pin(body),
        partial: String::new(),
        ready: VecDeque::new(),
        reported: false,
    };

    Box::pin(stream::unfold(Some(reading), |state| async move {
        let mut reading = state?;

        loop {
            if let Some(item) = reading.ready.pop_front() {
                reading.reported = true;
                return Some((item, Some(reading)));
            }

            let Some(chunk) = reading.body.next().await else {
                // Ending having said nothing at all is a failure, not a silent turn: an
                // empty body must be visible in the logs rather than looking like an
                // agent with nothing to add.
                let ending = (!reading.reported).then_some(Err(LlmError::Empty));
                return ending.map(|item| (item, None));
            };

            match chunk {
                Ok(bytes) => {
                    reading.partial.push_str(&String::from_utf8_lossy(&bytes));
                    take_lines(&mut reading.partial, &mut reading.ready);
                }
                Err(error) => {
                    // A broken connection mid-reply is the end of this turn. Reporting
                    // it and stopping beats reconnecting into the middle of a sentence.
                    return Some((Err(LlmError::Transport(error.to_string())), None));
                }
            }
        }
    }))
}

/// Drains every complete line out of the buffer, queueing whatever they say.
///
/// The trailing partial line is left in place for the next chunk to finish.
fn take_lines(partial: &mut String, ready: &mut VecDeque<Result<String, LlmError>>) {
    while let Some(newline) = partial.find('\n') {
        let line = partial[..newline].trim_end().to_owned();
        partial.drain(..=newline);

        // `event:` lines restate the `type` inside the payload, and blank lines
        // separate events. The data is the only part that carries anything.
        let Some(payload) = line.strip_prefix("data:") else { continue };
        ready.extend(read_event(payload.trim()));
    }
}

/// Reads one event's payload into what it means for the reply.
///
/// Unreadable and unrecognised events yield nothing rather than ending the turn: the
/// event set grows over time, and an agent that hangs up on an unfamiliar one would
/// break on an API addition it did not need to understand.
fn read_event(payload: &str) -> Option<Result<String, LlmError>> {
    let event: serde_json::Value = serde_json::from_str(payload).ok()?;

    match event["type"].as_str()? {
        "content_block_delta" => {
            let delta = &event["delta"];
            // Thinking arrives here too, as `thinking_delta`. Only text is spoken.
            (delta["type"] == "text_delta")
                .then(|| delta["text"].as_str())
                .flatten()
                .map(|text| Ok(text.to_owned()))
        }
        // A safety decline arrives as a successful response that simply stops, so it has
        // to be read off the stop reason — otherwise it looks like the model having
        // nothing to say.
        "message_delta" => (event["delta"]["stop_reason"] == "refusal").then_some(Err(LlmError::Declined)),
        "error" => Some(Err(LlmError::Malformed(event["error"].to_string()))),
        _ => None,
    }
}

/// Renders an error together with what caused it.
///
/// HTTP clients wrap the interesting part — a refused connection, a missing TLS trust
/// store, a name that does not resolve — inside a generic outer message. Printing only
/// that outer message tells whoever is reading the log at 3am that something failed and
/// nothing about what.
pub(crate) fn with_cause(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut cause = error.source();

    while let Some(inner) = cause {
        rendered.push_str(": ");
        rendered.push_str(&inner.to_string());
        cause = inner.source();
    }
    rendered
}

#[derive(Debug)]
pub enum LlmError {
    Transport(String),
    /// A non-2xx response. The body is carried because the API explains itself there.
    Refused { status: u16, body: String },
    Malformed(String),
    /// The model declined on safety grounds — a successful response carrying a
    /// `stop_reason` of `refusal` and nothing to say.
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
    use std::convert::Infallible;

    /// Feeds a canned response body in as it would arrive off the wire.
    async fn read(chunks: &[&str]) -> Vec<Result<String, LlmError>> {
        let owned: Vec<bytes::Bytes> = chunks.iter().map(|c| bytes::Bytes::from(c.to_string())).collect();
        let body = stream::iter(owned.into_iter().map(Ok::<_, Infallible>));
        spoken_text(body).collect().await
    }

    /// The text that survived, ignoring how it was split up.
    async fn spoken(chunks: &[&str]) -> String {
        read(chunks).await.into_iter().flatten().collect()
    }

    fn delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\
             \"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
    }

    #[test]
    fn turns_map_to_the_api_roles() {
        assert_eq!(Turn::Caller("hi".into()).role(), "user");
        assert_eq!(Turn::Agent("hello".into()).role(), "assistant");
    }

    #[tokio::test]
    async fn the_spoken_reply_is_the_text_deltas_in_order() {
        let body = format!("{}{}", delta("Sure, "), delta("sending that now."));
        assert_eq!(spoken(&[&body]).await, "Sure, sending that now.");
    }

    /// The reason the reply is a stream at all: pieces come out as they arrive, not
    /// gathered up at the end.
    #[tokio::test]
    async fn text_is_yielded_piece_by_piece() {
        let pieces = read(&[&delta("One. "), &delta("Two.")]).await;
        assert_eq!(pieces.len(), 2, "{pieces:?}");
    }

    /// Thinking and tool-use blocks share the connection and are not words to say.
    #[tokio::test]
    async fn non_text_deltas_are_not_spoken() {
        let body = format!(
            "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"thinking_delta\",\
             \"thinking\":\"the caller wants the tests run\"}}}}\n\n\
             data: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"text\"}}}}\n\n\
             {}\
             data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"input_json_delta\",\
             \"partial_json\":\"{{\\\"a\\\":1}}\"}}}}\n\n",
            delta("Running the tests.")
        );
        assert_eq!(spoken(&[&body]).await, "Running the tests.");
    }

    /// An event split across two chunks is one event, not two broken ones.
    #[tokio::test]
    async fn an_event_split_mid_line_is_reassembled() {
        let whole = delta("Hello there");
        let (head, tail) = whole.split_at(whole.len() / 2);
        assert_eq!(spoken(&[head, tail]).await, "Hello there");
    }

    /// A decline explains itself once. Following it with "and also it was empty" sends
    /// whoever reads the log looking for a second, different fault.
    #[tokio::test]
    async fn a_decline_is_distinguished_from_silence() {
        let declined = read(&["data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\n"])
            .await;
        assert!(matches!(declined.as_slice(), [Err(LlmError::Declined)]), "{declined:?}");

        let ended = read(&["data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"]).await;
        assert!(matches!(ended.as_slice(), [Err(LlmError::Empty)]), "{ended:?}");
    }

    /// A turn that produced nothing must be loud, or a mute agent reads as a thoughtful
    /// pause that never ends.
    #[tokio::test]
    async fn a_reply_with_no_text_in_it_is_an_error() {
        let nothing = read(&["data: {\"type\":\"message_stop\"}\n\n"]).await;
        assert!(matches!(nothing.as_slice(), [Err(LlmError::Empty)]), "{nothing:?}");
    }

    /// ...but a turn that did say something is not also reported as empty at the end.
    #[tokio::test]
    async fn a_reply_that_said_something_ends_cleanly() {
        let items = read(&[&delta("All done.")]).await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
    }

    #[tokio::test]
    async fn an_unreadable_event_does_not_end_the_turn() {
        let body = format!("data: not json\n\n{}", delta("Still here."));
        assert_eq!(spoken(&[&body]).await, "Still here.");
    }

    #[tokio::test]
    async fn an_error_event_surfaces_rather_than_reading_as_the_end() {
        let items = read(&["data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n"]).await;
        assert!(matches!(items.first(), Some(Err(LlmError::Malformed(_)))), "{items:?}");
    }
}
