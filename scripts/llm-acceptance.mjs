// Checks that the agent honours the session configuration the client sends.
//
//   OPENCONV_API_KEY=... node scripts/llm-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node and macOS `say`:
//
//   NODE_PATH=/path/to/node_modules node scripts/llm-acceptance.mjs
//
// The failure this guards against is the quiet one: an agent that ignores the prompt
// override still holds a fluent conversation, so "it replied" proves nothing. The test
// therefore plants a fact that exists *only* in the injected configuration — a session
// id passed as a dynamic variable — and asks a question that can only be answered by an
// agent that received it. A generic assistant cannot pass by being helpful.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Caller, Checks, readEnvironment, recordSpeech } from "./lib/caller.mjs";

/// A word the model has no way to produce unless it was handed to it.
const SESSION_ID = "kestrel-7";
const FIRST_MESSAGE = "Ready when you are.";
const QUESTION = "Which coding session are you driving right now?";

const PROMPT =
  "You are the voice interface for a coding assistant. You are driving coding " +
  "session {{sessionId}}. When asked which session you are driving, reply with " +
  "the session id exactly as written and nothing else.";

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const recording = recordSpeech(QUESTION, join(mkdtempSync(join(tmpdir(), "openconv-llm-")), "q.wav"));

const caller = await Caller.join({ openconv, livekitUrl, xiApiKey, participantName: "u_llm" });
console.log(`configuring ${caller.conversationId}\n`);

checks.record("joined the conversation", true, caller.conversationId);

checks.record(
  "the agent is present",
  await caller.waitFor(() => caller.agentPresent(), 30_000, "the agent"),
  caller.roster().join(", "),
);

// ---- send the session configuration, exactly as the SDK does ----
await caller.send({
  type: "conversation_initiation_client_data",
  conversation_config_override: {
    agent: { prompt: { prompt: PROMPT }, first_message: FIRST_MESSAGE },
  },
  dynamic_variables: { sessionId: SESSION_ID },
});
checks.record("sent the conversation configuration", true);

// ---- the first message opens the conversation, before anyone speaks ----
// The wait is for something to say, the check is for it being the *configured* thing: an
// agent that opens with a greeting of its own reaches this line just as fast.
await caller.waitFor(() => caller.replies().length > 0, 20_000, "the first message");
checks.record(
  "the agent opened with the configured first message",
  caller.replies()[0] === FIRST_MESSAGE,
  caller.replies()[0] ?? "<the agent said nothing>",
);

// ---- now ask the question only a configured agent can answer ----
const before = caller.replies().length;
console.log(`  asking: ${JSON.stringify(QUESTION)}`);
await caller.speak(recording);

const answered = await caller.waitFor(
  () => caller.replies().length > before,
  60_000,
  "an answer",
);
checks.record("the agent answered", answered, `${caller.replies().length - before} response(s)`);

const answer = caller.replies().at(-1) ?? "";
console.log(`  agent said: ${JSON.stringify(answer)}`);

// The whole point: this word reached the model only through dynamic_variables.
checks.record(
  "the answer reflects the injected session context",
  answer.toLowerCase().includes(SESSION_ID.toLowerCase()),
  `looking for ${JSON.stringify(SESSION_ID)}`,
);
checks.record(
  "the reply is short enough to speak aloud",
  answer.length > 0 && answer.length < 300,
  `${answer.length} chars`,
);

await caller.leave();
checks.finish();
