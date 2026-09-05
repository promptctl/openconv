// Verifies one whole conversational turn, by being the human on the other end.
//
//   OPENCONV_API_KEY=... node scripts/live-call-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node and macOS `say`:
//
//   NODE_PATH=/path/to/node_modules node scripts/live-call-acceptance.mjs
//
// `agent-acceptance.mjs` checks what the SDK needs in order to *connect*. This checks
// the thing the service exists to do, which no unit test can reach: the caller speaks,
// the agent hears it, answers it, and the answer comes back as sound in the room. Every
// component of that path is covered by tests against the real dependency; the assembled
// path is only ever exercised here.
//
// The check is causal, not merely liveness. The caller asks for a word drawn at random
// each run, and that word has to come back — first in the transcript, proving the audio
// published here reached speech-to-text, then in the agent's reply, proving the model
// answered *this* utterance. An agent that greets everyone warmly and ignores them
// entirely passes a liveness check and fails this one.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { asksFor, AUDIBLE_MS, Caller, Checks, millis, readEnvironment, recordSpeech } from "./lib/caller.mjs";

const PROMPT =
  "You are a voice assistant under test. Do exactly what the caller asks, and reply " +
  "with nothing else. No greeting, no explanation, no markdown.";

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const { line, said } = asksFor();
const recording = recordSpeech(line, join(mkdtempSync(join(tmpdir(), "openconv-")), "caller.wav"));

console.log(`the caller will say: "${line}"`);

// The prompt override is what makes the agent's reply worth asserting on, and it travels
// with the handshake so that this run configures a conversation the same way the browser
// page does — one implementation, in `web/conversation.js`.
const caller = await Caller.join({
  openconv,
  livekitUrl,
  xiApiKey,
  participantName: "u_livecall",
  settings: { prompt: PROMPT },
});
console.log(`joined ${caller.conversationId} at ${livekitUrl}\n`);

checks.record(
  "the agent is in the conversation and holds its configuration",
  await caller.agentConfigured(25_000),
  caller.roster().join(", "),
);

checks.record(
  "the conversation was announced",
  await caller.waitFor(
    () => caller.control("conversation_initiation_metadata"),
    20_000,
    "the announcement",
  ),
);

// Everything the agent says from here has to be an answer, so the audio already on the
// track is not evidence of one.
const before = caller.mark();

const spoken = await caller.speak(recording);
console.log(`\nspoke ${spoken.toFixed(1)}s into the room, waiting to be answered\n`);

checks.record(
  "the caller's words reached speech-to-text",
  await caller.waitFor(
    () => caller.transcripts().some((text) => said(text)),
    60_000,
    "a final transcript of the caller",
  ),
  JSON.stringify(caller.transcripts()),
);

checks.record(
  "the agent answered what the caller actually said",
  await caller.waitFor(
    () => caller.replies().some((text) => said(text)),
    60_000,
    "the reply",
  ),
  JSON.stringify(caller.replies()),
);

// The reply is published as text before its audio has been synthesized, so the sound
// is a separate fact from the answer and gets a separate wait. This is the leg that
// only exists once TTS is wired into the published track.
checks.record(
  "the answer came back as sound in the room",
  await caller.waitFor(
    () => millis(caller.heard.audibleFrames - before.audibleFrames) >= AUDIBLE_MS,
    120_000,
    "the agent's speech",
  ),
  `${caller.heard.audibleFrames - before.audibleFrames} audible frames, peak ${caller.heard.peak}`,
);

// Zero frames from a reader that crashed and zero frames from a track nobody spoke into
// are the same number, and only this separates them.
checks.record(
  "the audio reader ran to the end of the call",
  caller.heard.error === null,
  caller.heard.error ? String(caller.heard.error) : `${caller.heard.frames} frames read`,
);

await caller.leave();
checks.finish();
