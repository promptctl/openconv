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

use crate::tools::{ToolCall, ToolResult};
use futures_util::stream::{self, Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;

/// One side of one exchange.
///
/// An enum rather than a `{role: String, text: String}` pair, because the role is a
/// closed set and a typo in a role string is a request the API rejects at the far end
/// rather than a mistake the compiler catches here.
#[derive(Clone, Debug, PartialEq)]
pub enum Turn {
    Caller(String),
    Agent(String),
    /// Something the client handed the agent to know, which nobody said out loud.
    ///
    /// Distinct from [`Turn::Caller`] because the difference decides whether the agent
    /// speaks: the app pushes session focus changes, new coding-agent messages and
    /// status updates through this channel continuously, and answering them would turn a
    /// background feed into a monologue. Folding both into one variant would put that
    /// distinction back into whoever remembers to check it.
    Context(String),
    /// The tools the model asked for, together with what they returned.
    ///
    /// One variant carrying both halves rather than two variants that happen to be
    /// pushed in order. The API rejects any request where a `tool_use` block has no
    /// `tool_result` answering it — the whole conversation, not just the turn — so
    /// "the agent asked and nobody answered" is a state history must not be able to
    /// hold. Here it cannot be written down.
    Used { calls: Vec<ToolCall>, results: Vec<ToolResult> },
}

impl Turn {
    /// This turn as the messages the API reads.
    ///
    /// One turn is usually one message and sometimes two: a tool exchange is an
    /// assistant message asking and a user message answering, which is how the Messages
    /// API spells a round trip. Consecutive same-role messages are folded together at
    /// the far end, so an agent that spoke *and* called a tool needs no special casing
    /// here — the text and the call are two messages that arrive as one turn.
    fn messages(&self) -> Vec<Value> {
        match self {
            Self::Caller(text) => vec![json!({"role": "user", "content": text})],
            Self::Agent(text) => vec![json!({"role": "assistant", "content": text})],
            // The user role, tagged. The Messages API has two roles and neither of them
            // is "things the app told me", so the only place that distinction can live on
            // the wire is inside the text — and this is the one place it is written, so
            // the tag cannot drift from what the model was taught to expect.
            //
            // The tag is not what keeps the agent quiet; not starting a turn is. What it
            // buys is the model knowing whose words these were, so a burst of session
            // updates is never read back as something the caller claimed.
            Self::Context(text) => {
                vec![json!({"role": "user", "content": format!("<session_update>\n{text}\n</session_update>")})]
            }
            Self::Used { calls, results } => vec![
                json!({"role": "assistant", "content": calls.iter().map(|call| json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.input,
                })).collect::<Vec<_>>()}),
                json!({"role": "user", "content": results.iter().map(|result| json!({
                    "type": "tool_result",
                    "tool_use_id": result.id,
                    "content": result.content,
                    "is_error": result.is_error,
                })).collect::<Vec<_>>()}),
            ],
        }
    }
}

/// One thing the model produced.
///
/// Words and tool calls arrive interleaved on the same connection, so the stream that
/// carries one carries the other. Naming that in the item type is what keeps the
/// difference from being re-derived by everyone downstream: the speech path matches on
/// [`Piece::Say`] and cannot accidentally read a tool call aloud, and the turn loop
/// matches on [`Piece::Call`] and cannot miss one.
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    /// Words to speak.
    Say(String),
    /// A tool the model wants run before it goes on.
    Call(ToolCall),
}

/// A reply arriving in pieces.
///
/// Failure is an item rather than a separate outcome around the stream, because the two
/// ways a turn can break — never starting, and stopping halfway — are handled the same
/// way by everyone who cares: say so in the logs and drop the turn.
pub type Reply<'a> = Pin<Box<dyn Stream<Item = Result<Piece, LlmError>> + Send + 'a>>;

/// What the agent says next, given the conversation so far.
///
/// `tools` is passed in rather than held by the implementation because the model is
/// the thing that varies here, not the tool set: a local model behind this trait is
/// offered exactly the tools the agent has, without being told about them twice.
pub trait Llm: Send + Sync {
    fn respond<'a>(&'a self, system: &'a str, turns: &'a [Turn], tools: &'a [Value]) -> Reply<'a>;
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
    async fn open(
        &self,
        system: &str,
        turns: &[Turn],
        tools: &[Value],
    ) -> Result<reqwest::Response, LlmError> {
        let messages: Vec<_> = turns.iter().flat_map(Turn::messages).collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": messages,
            // Sent every turn, in the order the registry lists them. The tool list is
            // the front of the prompt prefix the API caches, so a set that serializes
            // differently between two turns costs a cache miss the caller hears as
            // latency.
            "tools": tools,
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
    fn respond<'a>(&'a self, system: &'a str, turns: &'a [Turn], tools: &'a [Value]) -> Reply<'a> {
        let opening = async move {
            match self.open(system, turns, tools).await {
                Ok(response) => pieces(response.bytes_stream()),
                // The one failure that has no stream behind it, given the shape of one.
                Err(error) => Box::pin(stream::once(async move { Err(error) })) as Reply<'static>,
            }
        };

        Box::pin(stream::once(opening).flatten())
    }
}

/// Turns the response body into what the model produced.
///
/// Reads server-sent events off the wire and keeps the two things that mean something:
/// the words to speak, and the tools to run. Thinking streams through the same
/// connection and is neither — the same filtering the non-streaming path did over
/// `content`, done as the blocks arrive rather than after.
fn pieces<S, E>(body: S) -> Reply<'static>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: fmt::Display,
{
    struct Reading<S> {
        body: S,
        /// Events do not respect chunk boundaries, so a line can arrive in two pieces.
        partial: String,
        /// One chunk can complete several events, and the stream yields one item a poll.
        ready: VecDeque<Result<Piece, LlmError>>,
        /// The tool call currently arriving, if one is.
        ///
        /// A tool call is spread over three events — a start naming it, deltas carrying
        /// its arguments a few characters at a time, and a stop — so unlike text it
        /// cannot be read off any single one. Blocks stream one after another rather
        /// than interleaved, so there is at most one part-built call at a time and
        /// holding it as an `Option` rather than a map by index is the honest shape.
        building: Option<Building>,
        /// Whether this stream has yielded anything at all, of either kind. A response
        /// with nothing in it is only knowable at the end, and a turn that already
        /// explained itself — a decline, a broken connection — must not then be
        /// reported a second time as merely empty.
        reported: bool,
    }

    let reading = Reading {
        body: Box::pin(body),
        partial: String::new(),
        ready: VecDeque::new(),
        building: None,
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
                    take_lines(&mut reading.partial, &mut reading.ready, &mut reading.building);
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

/// A tool call part-way through arriving.
///
/// The arguments are accumulated as text rather than parsed as they go, because JSON
/// arrives a few characters at a time and half of an object is not an object.
struct Building {
    id: String,
    name: String,
    arguments: String,
}

/// Drains every complete line out of the buffer, queueing whatever they say.
///
/// The trailing partial line is left in place for the next chunk to finish.
fn take_lines(
    partial: &mut String,
    ready: &mut VecDeque<Result<Piece, LlmError>>,
    building: &mut Option<Building>,
) {
    while let Some(newline) = partial.find('\n') {
        let line = partial[..newline].trim_end().to_owned();
        partial.drain(..=newline);

        // `event:` lines restate the `type` inside the payload, and blank lines
        // separate events. The data is the only part that carries anything.
        let Some(payload) = line.strip_prefix("data:") else { continue };
        ready.extend(read_event(payload.trim(), building));
    }
}

/// Reads one event's payload into what it means for the reply.
///
/// Unreadable and unrecognised events yield nothing rather than ending the turn: the
/// event set grows over time, and an agent that hangs up on an unfamiliar one would
/// break on an API addition it did not need to understand.
fn read_event(payload: &str, building: &mut Option<Building>) -> Option<Result<Piece, LlmError>> {
    let event: serde_json::Value = serde_json::from_str(payload).ok()?;

    match event["type"].as_str()? {
        // A tool call opens here, naming itself before any of its arguments exist.
        "content_block_start" => {
            let block = &event["content_block"];
            if block["type"] == "tool_use" {
                *building = Some(Building {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: String::new(),
                });
            }
            None
        }

        "content_block_delta" => {
            let delta = &event["delta"];
            match delta["type"].as_str()? {
                "text_delta" => delta["text"].as_str().map(|text| Ok(Piece::Say(text.to_owned()))),
                // The arguments of the call opened above, arriving in fragments.
                "input_json_delta" => {
                    let fragment = delta["partial_json"].as_str()?;
                    building.as_mut()?.arguments.push_str(fragment);
                    None
                }
                // Thinking arrives here too, as `thinking_delta`, and is neither words
                // to speak nor a tool to run.
                _ => None,
            }
        }

        // The call is whole now, so it can be parsed and handed on.
        "content_block_stop" => Some(finish(building.take()?)),

        // A safety decline arrives as a successful response that simply stops, so it has
        // to be read off the stop reason — otherwise it looks like the model having
        // nothing to say.
        "message_delta" => (event["delta"]["stop_reason"] == "refusal").then_some(Err(LlmError::Declined)),
        "error" => Some(Err(LlmError::Malformed(event["error"].to_string()))),
        _ => None,
    }
}

/// Turns a finished tool call into the call the agent will run.
///
/// A tool taking no arguments streams no `input_json_delta` at all, so an empty buffer
/// is the ordinary way `skip_turn` arrives rather than a fault — it reads as the empty
/// object it means. Anything else that will not parse is reported: a call the agent
/// cannot read is one it cannot run, and guessing at the arguments of a tool that sends
/// messages into someone's coding session is not a thing to do quietly.
fn finish(call: Building) -> Result<Piece, LlmError> {
    let arguments = match call.arguments.trim().is_empty() {
        true => Ok(serde_json::Map::new()),
        false => serde_json::from_str(&call.arguments),
    };

    match arguments {
        Ok(input) => Ok(Piece::Call(ToolCall { id: call.id, name: call.name, input })),
        Err(error) => Err(LlmError::Malformed(format!(
            "could not read the arguments of {}: {error}: {}",
            call.name, call.arguments
        ))),
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
    async fn read(chunks: &[&str]) -> Vec<Result<Piece, LlmError>> {
        let owned: Vec<bytes::Bytes> = chunks.iter().map(|c| bytes::Bytes::from(c.to_string())).collect();
        let body = stream::iter(owned.into_iter().map(Ok::<_, Infallible>));
        pieces(body).collect().await
    }

    /// The text that survived, ignoring how it was split up.
    async fn spoken(chunks: &[&str]) -> String {
        read(chunks)
            .await
            .into_iter()
            .flatten()
            .filter_map(|piece| match piece {
                Piece::Say(text) => Some(text),
                Piece::Call(_) => None,
            })
            .collect()
    }

    /// The tools the model asked for, in the order it asked.
    async fn called(chunks: &[&str]) -> Vec<ToolCall> {
        read(chunks)
            .await
            .into_iter()
            .flatten()
            .filter_map(|piece| match piece {
                Piece::Call(call) => Some(call),
                Piece::Say(_) => None,
            })
            .collect()
    }

    fn delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\
             \"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
    }

    /// One tool call, in the three events it actually arrives as, with the arguments
    /// split across deltas the way the API sends them.
    fn tool_call(id: &str, name: &str, arguments: &[&str]) -> String {
        let start = format!(
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":\
             {{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n"
        );
        let fragments: String = arguments
            .iter()
            .map(|fragment| {
                let escaped = fragment.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
                     {{\"type\":\"input_json_delta\",\"partial_json\":\"{escaped}\"}}}}\n\n"
                )
            })
            .collect();
        format!("{start}{fragments}data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n")
    }

    #[test]
    fn a_spoken_turn_is_one_message_in_the_role_that_said_it() {
        let caller = Turn::Caller("hi".into()).messages();
        assert_eq!(caller.len(), 1);
        assert_eq!(caller[0]["role"], "user");
        assert_eq!(caller[0]["content"], "hi");

        let agent = Turn::Agent("hello".into()).messages();
        assert_eq!(agent[0]["role"], "assistant");
    }

    /// The Messages API has no third role, so context has to reach the model as a user
    /// message — but one the model can tell apart from something the caller actually
    /// said. Without that, a burst of session updates reads back as the caller's own
    /// claims about their code.
    #[test]
    fn context_reaches_the_model_marked_as_something_nobody_said() {
        let context = Turn::Context("Claude Code finished the migration".into()).messages();
        assert_eq!(context.len(), 1);
        assert_eq!(context[0]["role"], "user");

        let content = context[0]["content"].as_str().expect("context is plain text");
        assert!(content.contains("Claude Code finished the migration"), "{content}");
        assert_ne!(
            content,
            Turn::Caller("Claude Code finished the migration".into()).messages()[0]["content"]
                .as_str()
                .unwrap(),
            "context and speech must not arrive identically"
        );
    }

    /// The pairing the API insists on: an assistant message asking, and a user message
    /// answering every call it made. A `tool_use` left unanswered rejects the whole
    /// request, so this is the shape the history type exists to guarantee.
    #[test]
    fn a_tool_exchange_is_the_ask_and_the_answer_together() {
        let used = Turn::Used {
            calls: vec![ToolCall {
                id: "toolu_1".into(),
                name: "sendMessageToSession".into(),
                input: serde_json::Map::new(),
            }],
            results: vec![ToolResult { id: "toolu_1".into(), content: "sent".into(), is_error: false }],
        }
        .messages();

        assert_eq!(used.len(), 2, "{used:?}");
        assert_eq!(used[0]["role"], "assistant");
        assert_eq!(used[0]["content"][0]["type"], "tool_use");
        assert_eq!(used[0]["content"][0]["id"], "toolu_1");
        assert_eq!(used[1]["role"], "user");
        assert_eq!(used[1]["content"][0]["type"], "tool_result");
        // The id the model minted is the id the result is matched by; a second
        // identifier would be a second chance to mismatch them.
        assert_eq!(used[1]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(used[1]["content"][0]["is_error"], false);
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

    /// Thinking shares the connection and is not words to say.
    #[tokio::test]
    async fn thinking_is_not_spoken() {
        let body = format!(
            "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"thinking_delta\",\
             \"thinking\":\"the caller wants the tests run\"}}}}\n\n\
             data: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"text\"}}}}\n\n\
             {}",
            delta("Running the tests.")
        );
        assert_eq!(spoken(&[&body]).await, "Running the tests.");
    }

    /// The whole point of the piece type: a tool call is not read out loud.
    #[tokio::test]
    async fn a_tool_call_is_handed_over_rather_than_spoken() {
        let body = format!(
            "{}{}",
            delta("Sending that now."),
            tool_call(
                "toolu_01",
                "sendMessageToSession",
                &[r#"{"sessionId": "sess_"#, r#"42", "message": "run the tests"}"#],
            )
        );

        assert_eq!(spoken(&[&body]).await, "Sending that now.");

        let calls = called(&[&body]).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].id, "toolu_01");
        assert_eq!(calls[0].name, "sendMessageToSession");
        assert_eq!(calls[0].input["sessionId"], "sess_42");
        assert_eq!(calls[0].input["message"], "run the tests");
    }

    /// Arguments arrive a few characters at a time and are only valid JSON once the
    /// block closes, so a call split anywhere must still parse as the one object it is.
    #[tokio::test]
    async fn arguments_split_across_deltas_are_reassembled() {
        let whole = tool_call("toolu_02", "processPermissionRequest", &[
            r#"{"requestId""#,
            r#": "req_7", "de"#,
            r#"cision": "allow"}"#,
        ]);

        let calls = called(&[&whole]).await;
        assert_eq!(calls[0].input["requestId"], "req_7");
        assert_eq!(calls[0].input["decision"], "allow");
    }

    /// A tool taking no arguments streams no argument deltas at all. That is how
    /// `skip_turn` always arrives, so an empty buffer has to read as the empty object
    /// it means rather than as a parse failure.
    #[tokio::test]
    async fn a_tool_with_no_arguments_arrives_as_an_empty_object() {
        let calls = called(&[&tool_call("toolu_03", "skip_turn", &[])]).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "skip_turn");
        assert!(calls[0].input.is_empty(), "{:?}", calls[0].input);
    }

    /// Two calls in one turn is ordinary — the model may ask for several at once.
    #[tokio::test]
    async fn several_calls_in_one_turn_all_come_through() {
        let body = format!(
            "{}{}",
            tool_call("toolu_a", "sendMessageToSession", &[r#"{"sessionId":"s","message":"m"}"#]),
            tool_call("toolu_b", "skip_turn", &[]),
        );

        let calls = called(&[&body]).await;
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].id, "toolu_a");
        assert_eq!(calls[1].id, "toolu_b");
    }

    /// Guessing at the arguments of a tool that sends messages into someone's coding
    /// session is not a thing to do quietly.
    #[tokio::test]
    async fn arguments_that_will_not_parse_are_reported_rather_than_guessed_at() {
        let broken = tool_call("toolu_04", "sendMessageToSession", &[r#"{"sessionId": "#]);
        let items = read(&[&broken]).await;
        assert!(
            matches!(items.first(), Some(Err(LlmError::Malformed(_)))),
            "{items:?}"
        );
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
