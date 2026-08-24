// Verifies that the agent can drive the app, by being the app on the other end.
//
//   OPENCONV_API_KEY=... node scripts/tools-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node and macOS `say`:
//
//   NODE_PATH=/path/to/node_modules node scripts/tools-acceptance.mjs
//
// Two claims, neither reachable from a unit test, because both are about a round trip
// that leaves this process and comes back:
//
// 1. **A client tool round-trips, and the answer reaches the model.** The agent
//    publishes a `client_tool_call`, this script answers it the way Happy's SDK does,
//    and the agent then says something it could only say having read the answer. That
//    last step is the whole feature: an agent that publishes the call and ignores what
//    comes back still looks correct on the wire and is useless — it can ask a coding
//    session to do things and never learn whether they happened.
//
// 2. **`skip_turn` is never dispatched to the client.** Happy's system prompt names
//    `skip_turn`, but `realtimeClientTools.ts` registers no handler for it, and the SDK
//    answers a call it does not recognise with `is_error: true`. So an agent that treats
//    it as a client tool fails every time it tries to stay quiet — which is exactly when
//    nobody is watching. This asserts the call never goes out.
//
// The first claim is checked against a *later* agent response rather than any response,
// because the agent usually says something before calling the tool ("Sending that now")
// and counting that would pass an agent that never read the result at all.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Caller, Checks, readEnvironment, recordSpeech } from "./lib/caller.mjs";

/// A session id the model has no way to produce unless the configuration reached it.
///
/// The same trick `llm-acceptance` uses on the prompt, applied one layer further in: it
/// has to survive into the *arguments of a tool call*, which is the path this ticket
/// added. A generic assistant cannot guess it, so a passing run proves the dynamic
/// variable travelled from the client, through the prompt, into what the model asked for.
const SESSION_ID = "kestrel-7";

/// Instructs the one behaviour under test and nothing else. Deliberately explicit about
/// calling the tool: this is a check of the plumbing, not of how readily a model reaches
/// for a tool it was told about in passing.
const DRIVING_PROMPT =
  "You are a voice interface for a coding assistant. The session you are driving is " +
  `${SESSION_ID}. When the caller asks for anything to be done, you MUST call the ` +
  "sendMessageToSession tool with that session id and their request as the message. " +
  "Do not describe the tool or ask permission — call it. After the tool returns, say " +
  "exactly what it returned and nothing else.";

/// The caller is talking to someone else in the room, which is the case `skip_turn`
/// exists for.
const SKIPPING_PROMPT =
  "You are a voice interface for a coding assistant. You only answer when addressed " +
  "directly as 'Happy'. If the speaker is talking to another person in the room, you " +
  "MUST call the skip_turn tool and say absolutely nothing.";

const ASK = "Please tell the session to run the tests.";
const ASIDE = "Marcus, could you pass me that coffee before the meeting starts.";

/// How long the agent is given to hear an utterance, decide, and publish a tool call.
/// Covers transcription, the endpointer's 600 ms of quiet, and a model turn.
const CALL_WITHIN_MS = 45_000;

/// How long the agent is given to speak again once the tool has been answered. A second
/// model turn, so the same order of magnitude as the first.
const ACKNOWLEDGE_WITHIN_MS = 45_000;

/// How long to watch for a `skip_turn` dispatch that must never come, and for speech
/// that must never arrive. Long enough that "it had not happened yet" is not the reason
/// the check passed.
const SILENCE_WINDOW_MS = 25_000;

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const scratch = mkdtempSync(join(tmpdir(), "openconv-tools-"));
const ask = recordSpeech(ASK, join(scratch, "ask.wav"));
const aside = recordSpeech(ASIDE, join(scratch, "aside.wav"));

/** Every tool call the agent has published, in arrival order. */
const toolCalls = (caller) =>
  caller.controlEvents
    .filter((event) => event.type === "client_tool_call")
    .map((event) => event.client_tool_call);

/** Everything the agent has said, in arrival order. */
const responses = (caller) =>
  caller.controlEvents
    .filter((event) => event.type === "agent_response")
    .map((event) => event.agent_response_event.agent_response);

/** Opens a conversation configured with one prompt, and waits for the announcement. */
async function converse(prompt, name) {
  const caller = await Caller.join({ openconv, livekitUrl, xiApiKey });
  await caller.microphone();
  await caller.send({
    type: "conversation_initiation_client_data",
    conversation_config_override: { agent: { prompt: { prompt } } },
  });

  const announced = await caller.waitFor(
    () => caller.control("conversation_initiation_metadata"),
    20_000,
    "the announcement",
  );
  checks.record(`${name}: the conversation was announced`, announced);
  return caller;
}

// ---------------------------------------------------------------------------
// 1. A client tool round-trips, and the answer reaches the model.
// ---------------------------------------------------------------------------

const driving = await converse(DRIVING_PROMPT, "driving");
await driving.speak(ask);

const called = await driving.waitFor(
  () => toolCalls(driving).some((call) => call.tool_name === "sendMessageToSession"),
  CALL_WITHIN_MS,
  "the agent to call sendMessageToSession",
);
checks.record(
  "the agent asked the app to send a message to the session",
  called,
  called ? "" : `saw ${JSON.stringify(toolCalls(driving))}`,
);

const call = toolCalls(driving).find((each) => each.tool_name === "sendMessageToSession");

// The wire shape the SDK reads. A call missing its id cannot be answered at all, and
// the SDK matches its handler by `tool_name`, so both are load-bearing rather than
// decorative.
checks.record(
  "the call carries the id the app answers with",
  Boolean(call?.tool_call_id),
  call ? `tool_call_id=${call.tool_call_id}` : "no call to inspect",
);

// The strong one: the id existed only in the injected configuration, so an agent that
// dropped the client's config on the floor cannot produce it here.
checks.record(
  "the injected session id reached the tool's arguments",
  call?.parameters?.sessionId === SESSION_ID,
  `sessionId=${JSON.stringify(call?.parameters?.sessionId)}`,
);

checks.record(
  "the caller's request reached the tool's arguments",
  typeof call?.parameters?.message === "string" && /test/i.test(call.parameters.message),
  `message=${JSON.stringify(call?.parameters?.message)}`,
);

// Everything said up to now, so the acknowledgement can be told from the agent's own
// narration before the call went out.
const saidBeforeAnswering = responses(driving).length;

if (call) {
  // Exactly what Happy's `sendMessageToSession` returns today, brackets and all, so the
  // agent is answering the real string rather than a tidied stand-in.
  await driving.send({
    type: "client_tool_result",
    tool_call_id: call.tool_call_id,
    result: "sent [DO NOT say anything else, simply say 'sent']",
    is_error: false,
  });
}

const acknowledged = await driving.waitFor(
  () => responses(driving).length > saidBeforeAnswering,
  ACKNOWLEDGE_WITHIN_MS,
  "the agent to speak again after the tool was answered",
);
checks.record("the agent spoke again once the tool had been answered", acknowledged);

const afterwards = responses(driving).slice(saidBeforeAnswering).join(" ");
checks.record(
  "what it said came from the tool's answer",
  /sent/i.test(afterwards),
  `said ${JSON.stringify(afterwards)}`,
);

await driving.leave();

// ---------------------------------------------------------------------------
// 2. skip_turn is never dispatched to the client.
// ---------------------------------------------------------------------------

const skipping = await converse(SKIPPING_PROMPT, "skipping");
await skipping.speak(aside);

// Waits for the failure rather than for the success: there is nothing to observe when
// the agent correctly stays quiet, so the check is that the window closes with neither
// a dispatch nor a word.
await skipping.waitFor(
  () => toolCalls(skipping).some((each) => each.tool_name === "skip_turn"),
  SILENCE_WINDOW_MS,
  "a skip_turn dispatch that must never arrive",
);

const dispatched = toolCalls(skipping).filter((each) => each.tool_name === "skip_turn");
checks.record(
  "skip_turn was never sent to the app",
  dispatched.length === 0,
  dispatched.length ? "the app would have answered it with is_error" : "",
);

// The behavioural half. Model-dependent in a way the check above is not — it asks
// whether the model took the instruction, not whether the agent routed it correctly —
// so a failure here is worth re-running before believing.
const spoke = responses(skipping);
checks.record(
  "the agent stayed silent when the caller was addressing someone else",
  spoke.length === 0,
  spoke.length ? `said ${JSON.stringify(spoke.join(" "))}` : "",
);

await skipping.leave();
checks.finish();
