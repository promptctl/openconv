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

import { Caller, Checks, readEnvironment, recordSpeech } from "./lib/caller.mjs";

/// Everyday nouns rather than a random string, because the word has to survive being
/// spoken by one synthesizer and heard by a speech model: "xk7q" is a test of nothing
/// but whisper's spelling. Drawn per run so a pass cannot be a coincidence twice.
///
/// Every entry was checked through the real path — `say` into `transcribe_wav` — rather
/// than chosen for sounding distinctive. Two obvious candidates did not survive it and
/// are deliberately absent: base.en hears "penguin" as "pen win" and "walrus" as
/// "waras", which fails this script for a reason that has nothing to do with the agent.
/// Re-check any word before adding it here.
const WORDS = [
  "banana",
  "umbrella",
  "trumpet",
  "cactus",
  "harmonica",
  "lantern",
  "rooster",
  "pumpkin",
];

const PROMPT =
  "You are a voice assistant under test. Do exactly what the caller asks, and reply " +
  "with nothing else. No greeting, no explanation, no markdown.";

/// Matching is on words, so "Banana." counts and spacing or punctuation never decides
/// whether the pipeline worked.
const said = (text, word) => (text ?? "").toLowerCase().includes(word);

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const word = WORDS[Math.floor(Math.random() * WORDS.length)];
const line = `Please reply with only the word ${word}.`;
const recording = recordSpeech(line, join(mkdtempSync(join(tmpdir(), "openconv-")), "caller.wav"));

console.log(`the caller will say: "${line}"`);

const caller = await Caller.join({ openconv, livekitUrl, xiApiKey, participantName: "u_livecall" });
console.log(`joined ${caller.conversationId} at ${livekitUrl}\n`);

checks.record(
  "the agent is in the conversation",
  await caller.waitFor(() => caller.agentPresent(), 25_000, "the agent to join"),
  caller.roster().join(", "),
);

// The SDK configures the conversation before anyone speaks. Sent here for the same
// reason: the prompt override is what makes the agent's reply worth asserting on.
await caller.send({
  type: "conversation_initiation_client_data",
  conversation_config_override: { agent: { prompt: { prompt: PROMPT } } },
});

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

const transcript = () =>
  caller.controlEvents
    .filter((event) => event.type === "user_transcript")
    .map((event) => event.user_transcription_event?.user_transcript ?? "");

checks.record(
  "the caller's words reached speech-to-text",
  await caller.waitFor(
    () => transcript().some((text) => said(text, word)),
    60_000,
    "a final transcript of the caller",
  ),
  JSON.stringify(transcript()),
);

const replies = () =>
  caller.controlEvents
    .filter((event) => event.type === "agent_response")
    .map((event) => event.agent_response_event?.agent_response ?? "");

checks.record(
  "the agent answered what the caller actually said",
  await caller.waitFor(() => replies().some((text) => said(text, word)), 60_000, "the reply"),
  JSON.stringify(replies()),
);

// The reply is published as text before its audio has been synthesized, so the sound
// is a separate fact from the answer and gets a separate wait. This is the leg that
// only exists once TTS is wired into the published track.
//
// Two hundred milliseconds of sound is the bar because it is what separates a spoken
// word from a click, and the reply here is deliberately one word: a bar set to the
// length of some particular answer fails on a short one that was perfectly audible.
const AUDIBLE_MS = 200;
checks.record(
  "the answer came back as sound in the room",
  await caller.waitFor(
    () => caller.heard.audibleFrames - before.audibleFrames >= AUDIBLE_MS / 10,
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
