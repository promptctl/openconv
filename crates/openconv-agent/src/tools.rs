//! The tools the agent may ask for, described in exactly one place.
//!
//! # Why one table and not two
//!
//! A tool has two sides that must agree: what the model is told it can call, and what
//! the agent does when it calls it. Written apart, they drift — a renamed tool leaves
//! the model asking for something nothing answers, and the failure is silent, because
//! an unrecognised name is indistinguishable from a model that simply chose not to
//! call anything. So the name, the schema, and the way it runs are one value, and both
//! sides are read off it.
//!
//! # Where a tool runs
//!
//! Two of these execute inside the Happy app: it registers handlers, the agent
//! publishes a `client_tool_call`, and the answer comes back over the data channel.
//! `skip_turn` is different and the difference is not a detail — Happy's system prompt
//! names it, but `realtimeClientTools.ts` has no handler for it, and the SDK answers a
//! call it does not recognise with an error result. Sending it to the client would
//! fail every time. It is a decision about whether this turn belongs to the agent at
//! all, so it is answered here.

use openconv_protocol::JsonObject;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::oneshot;

/// One tool, from both sides at once.
pub struct Tool {
    pub name: &'static str,
    description: &'static str,
    /// The JSON Schema the model fills in. Owned rather than borrowed because it is
    /// built, not written — a schema as a string literal is a parse waiting to fail at
    /// the worst moment, on a live call.
    parameters: Value,
    pub run: Run,
    pub then: Then,
}

/// Who answers when the model asks for this tool.
pub enum Run {
    /// The Happy app. The agent publishes the call and waits for the app's answer.
    OnTheClient,
    /// Nobody — there is nothing to execute, only a fixed answer. See the module docs
    /// for why `skip_turn` is not a client tool despite Happy's prompt naming it.
    Here(&'static str),
}

/// What the agent does once the tool has run.
///
/// The domain's own two-way distinction rather than a flag: a tool either produces
/// something the model should go on to speak about, or it *is* the model declining to
/// speak. Handled exhaustively in one place, so a third kind of tool cannot be added
/// without deciding which it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    /// Hand the result back and let the model carry on writing.
    Answer,
    /// The turn is over and the agent says nothing.
    Stop,
}

/// What the model asked for.
///
/// `id` is the model's own `toolu_…`, carried through as the conversation's
/// `tool_call_id` rather than paired with one minted here. Two identifiers for one call
/// is two chances to mismatch them, and the app never sees the model's.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: JsonObject,
}

/// What running it produced.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    /// Carried rather than folded into the text: the model treats a failure differently
    /// from a result that happens to read like one, and the API has a field for it.
    pub is_error: bool,
}

/// Every tool the agent declares, in a fixed order.
///
/// Fixed because the tool list is part of the prompt prefix the API caches: a set that
/// serializes differently between two requests is a cache miss on every turn, paid in
/// latency the caller hears. A slice rather than a value threaded through the agent —
/// it is built once, never written again, and read from both the request builder and
/// the turn loop.
pub fn all() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            Tool {
                name: "sendMessageToSession",
                // Lifted almost verbatim from Happy's own system prompt, because the
                // prompt and the tool description are read together by the model and
                // disagreeing about when to call it is worse than saying it twice.
                description: "Send a message to a coding agent session. This may take a \
                    long time to return, so do not call it until the user has fully \
                    formulated their request.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "The session to send to, usually the last focused one.",
                        },
                        "message": {
                            "type": "string",
                            "description": "What to say to the coding agent.",
                        },
                    },
                    "required": ["sessionId", "message"],
                }),
                run: Run::OnTheClient,
                then: Then::Answer,
            },
            Tool {
                name: "processPermissionRequest",
                // The "never on your own accord" half is the whole point of the tool:
                // an agent that approves a permission request the user did not approve
                // has let a coding agent do something nobody asked for.
                description: "Approve or deny a permission request from a coding agent \
                    session. Never decide on your own accord — wait for the user to \
                    explicitly approve or deny each request, unless they have said to \
                    accept future ones.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "requestId": {
                            "type": "string",
                            "description": "The permission request being answered.",
                        },
                        "decision": {
                            "type": "string",
                            "enum": ["allow", "deny"],
                            "description": "The user's decision, never your own.",
                        },
                    },
                    "required": ["requestId", "decision"],
                }),
                run: Run::OnTheClient,
                then: Then::Answer,
            },
            Tool {
                name: "skip_turn",
                description: "Say nothing at all this turn. Call this when the speaker \
                    is talking to another person in the room rather than to you, or when \
                    nothing needs to be said.",
                // No parameters, and `properties` is present-but-empty rather than
                // absent: the API wants an object schema, and an object schema with no
                // properties is what "takes no arguments" looks like.
                parameters: json!({"type": "object", "properties": {}}),
                run: Run::Here("skipped"),
                then: Then::Stop,
            },
        ]
    })
}

/// Finds the tool the model asked for.
///
/// `None` is a model asking for something this agent does not have — a prompt naming a
/// tool that was never declared, or a name it invented. The turn loop answers it as a
/// failed call rather than dropping it, so the model is told and can carry on.
pub fn named(name: &str) -> Option<&'static Tool> {
    all().iter().find(|tool| tool.name == name)
}

impl Tool {
    /// This tool as the Messages API wants it declared.
    ///
    /// The same value the model is shown and the same `name` the results are matched
    /// against, which is the point of the table.
    pub fn declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.parameters,
        })
    }
}

/// Every tool, declared, as the request body carries them.
///
/// Built once and handed out by reference rather than rebuilt per turn: these bytes are
/// the front of the prompt prefix the API caches, and a list rebuilt each time is a list
/// that can serialize differently each time.
pub fn declarations() -> &'static [Value] {
    static DECLARED: OnceLock<Vec<Value>> = OnceLock::new();
    DECLARED.get_or_init(|| all().iter().map(Tool::declaration).collect())
}

/// Client tool calls waiting on the app's answer.
///
/// The two halves of a client tool call happen in different places: the agent's turn
/// publishes the call from its own task, and the answer arrives on the room's data
/// channel, which only the conversation loop reads. This is the seam between them —
/// the one owner of "which calls are outstanding", with the two operations that are
/// allowed on it and nothing else.
///
/// An answer for a call nobody is waiting on is dropped and said out loud. It means a
/// duplicate result, or one for a turn that has already ended — worth seeing in a log,
/// never worth failing a conversation over.
#[derive(Default)]
pub struct Pending {
    waiting: Mutex<HashMap<String, oneshot::Sender<ToolResult>>>,
}

impl Pending {
    /// Registers interest in one call's answer, before the call is published.
    ///
    /// Before, not after: the app can answer faster than the publishing task is
    /// rescheduled, and an answer that arrives before anyone is waiting for it is an
    /// answer that is lost.
    pub fn expect(&self, id: &str) -> oneshot::Receiver<ToolResult> {
        let (answer, awaited) = oneshot::channel();
        self.waiting
            .lock()
            .expect("the pending-call map is only ever locked to insert or remove one entry")
            .insert(id.to_owned(), answer);
        awaited
    }

    /// Hands an answer to whoever is waiting for it.
    pub fn deliver(&self, result: ToolResult) {
        let waiting = self
            .waiting
            .lock()
            .expect("the pending-call map is only ever locked to insert or remove one entry")
            .remove(&result.id);

        match waiting {
            Some(answer) => {
                // The receiver is gone when the turn ended without it — a conversation
                // that hung up mid-call. Nothing left to tell.
                let _ = answer.send(result);
            }
            None => tracing::warn!(
                tool_call_id = %result.id,
                "the client answered a tool call nobody was waiting on"
            ),
        }
    }

    /// Gives up on one call's answer.
    ///
    /// Called when the wait ended without one, so a timed-out call does not sit in the
    /// map for the rest of the conversation holding a sender nobody will ever use.
    pub fn give_up(&self, id: &str) {
        self.waiting
            .lock()
            .expect("the pending-call map is only ever locked to insert or remove one entry")
            .remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this module exists to prevent: the model told about a tool that is
    /// then matched by a different name, so every call it makes goes unanswered.
    #[test]
    fn every_declared_tool_can_be_found_by_the_name_it_was_declared_under() {
        for declared in declarations() {
            let name = declared["name"].as_str().expect("a tool declares a name");
            assert!(named(name).is_some(), "{name} is declared but cannot be found");
        }
    }

    #[test]
    fn the_two_tools_happy_registers_run_on_the_client() {
        for name in ["sendMessageToSession", "processPermissionRequest"] {
            let tool = named(name).expect(name);
            assert!(matches!(tool.run, Run::OnTheClient), "{name}");
            assert_eq!(tool.then, Then::Answer, "{name}");
        }
    }

    /// Happy's prompt names `skip_turn` but registers no handler for it, so a call sent
    /// to the client comes back as an error every time. It has to be answered here.
    #[test]
    fn skip_turn_is_answered_here_and_ends_the_turn() {
        let tool = named("skip_turn").expect("skip_turn");
        assert!(matches!(tool.run, Run::Here(_)));
        assert_eq!(tool.then, Then::Stop);
    }

    #[test]
    fn a_tool_the_agent_does_not_have_is_not_found() {
        assert!(named("rm_minus_rf").is_none());
    }

    /// Every declaration has to be the shape the Messages API accepts, or the whole
    /// request is rejected and the caller hears silence.
    #[test]
    fn declarations_have_the_shape_the_api_requires() {
        for declared in declarations() {
            assert!(declared["name"].is_string(), "{declared}");
            assert!(declared["description"].is_string(), "{declared}");
            assert_eq!(declared["input_schema"]["type"], "object", "{declared}");
            assert!(declared["input_schema"]["properties"].is_object(), "{declared}");
        }
    }

    #[tokio::test]
    async fn an_answer_reaches_whoever_was_waiting_for_it() {
        let pending = Pending::default();
        let awaited = pending.expect("toolu_1");

        pending.deliver(ToolResult {
            id: "toolu_1".into(),
            content: "sent".into(),
            is_error: false,
        });

        let result = awaited.await.expect("the answer was delivered");
        assert_eq!(result.content, "sent");
    }

    /// Two calls in flight at once is the ordinary case — the model may ask for several
    /// in one turn, and the app answers them in whatever order they finish.
    #[tokio::test]
    async fn answers_go_to_their_own_callers_whatever_order_they_arrive_in() {
        let pending = Pending::default();
        let first = pending.expect("toolu_1");
        let second = pending.expect("toolu_2");

        pending.deliver(ToolResult { id: "toolu_2".into(), content: "b".into(), is_error: false });
        pending.deliver(ToolResult { id: "toolu_1".into(), content: "a".into(), is_error: false });

        assert_eq!(first.await.expect("first").content, "a");
        assert_eq!(second.await.expect("second").content, "b");
    }

    /// A duplicate answer, or one for a turn that already ended. Dropping it is right;
    /// panicking on it would let the client end a conversation.
    #[test]
    fn an_answer_nobody_awaits_is_dropped_rather_than_fatal() {
        let pending = Pending::default();
        pending.deliver(ToolResult { id: "toolu_9".into(), content: "?".into(), is_error: false });
    }

    #[tokio::test]
    async fn giving_up_leaves_nothing_behind_for_a_late_answer_to_find() {
        let pending = Pending::default();
        let awaited = pending.expect("toolu_1");
        pending.give_up("toolu_1");

        // The sender was dropped with the entry, so the wait ends rather than hanging.
        assert!(awaited.await.is_err());
        pending.deliver(ToolResult { id: "toolu_1".into(), content: "late".into(), is_error: false });
    }
}
