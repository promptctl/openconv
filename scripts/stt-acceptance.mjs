// Speaks a sentence into a live conversation and checks the agent heard it.
//
//   OPENCONV_API_KEY=... node scripts/stt-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node and macOS `say`, which supplies the voice:
//
//   NODE_PATH=/path/to/node_modules node scripts/stt-acceptance.mjs
//
// Using real synthesized speech rather than a recorded fixture keeps the check honest:
// it exercises the resampling, the endpointer, and the model on audio that arrived over
// WebRTC at the rate a browser publishes, which is the only path that matters.
//
// This is the narrow claim that words survive the trip into speech-to-text. Whether the
// agent then answers them is `live-call-acceptance.mjs`.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Caller, Checks, readEnvironment, recordSpeech } from "./lib/caller.mjs";

const SPOKEN = "Hello, can you hear me? This is a test of the voice agent.";

/// Word overlap rather than string equality: the point is that it heard the sentence,
/// not that a speech model reproduced punctuation exactly.
const words = (text) =>
  new Set(
    text
      .toLowerCase()
      .replace(/[^a-z0-9 ]/g, "")
      .split(/\s+/)
      .filter(Boolean),
  );

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const recording = recordSpeech(SPOKEN, join(mkdtempSync(join(tmpdir(), "openconv-stt-")), "speech.wav"));

const caller = await Caller.join({ openconv, livekitUrl, xiApiKey, participantName: "u_stt" });
console.log(`speaking into ${caller.conversationId}\n`);

checks.record("joined the conversation", true, caller.conversationId);

checks.record(
  "the agent is present to listen",
  await caller.waitFor(() => caller.agentPresent(), 30_000, "the agent"),
  caller.roster().join(", "),
);

// ---- speak ----
//
// Opened explicitly rather than through `speak()` so that publishing the microphone is
// its own claim: a track that never reaches the agent and a sentence the model could not
// make out both end with no transcript, and only this separates them.
const mic = await caller.microphone(recording.sampleRate);
checks.record("published the caller's microphone", true, `${recording.sampleRate} Hz`);

const spoken = await mic.say(recording);
checks.record("finished speaking", true, `${spoken.toFixed(2)}s of audio`);

// ---- did it hear? ----
checks.record(
  "a user_transcript event arrived",
  await caller.waitFor(() => caller.transcriptEvents().length > 0, 45_000, "a final transcript"),
  `${caller.events("tentative_user_transcript").length} tentative, ` +
    `${caller.transcriptEvents().length} final`,
);

// Every settled transcript, not the last one: the endpointer decides where an utterance
// stops, and one sentence spoken with a breath in it settles as two. Both halves were
// heard, so a claim about what reached speech-to-text has to count both — and with no
// transcripts at all this is "", which the accuracy below reports as zero.
const heard = caller.transcripts().join(" ");
console.log(`  agent heard: ${JSON.stringify(caller.transcripts())}`);

const spokenWords = words(SPOKEN);
const heardWords = words(heard);
const matched = [...spokenWords].filter((word) => heardWords.has(word));
const accuracy = matched.length / spokenWords.size;

checks.record(
  "the transcript matches what was said",
  accuracy >= 0.8,
  `${Math.round(accuracy * 100)}% of words (${matched.length}/${spokenWords.size})`,
);

// The event id is what lets a client correlate a transcript with the turn it belongs to,
// so its absence is a protocol failure even when the words came through perfectly — which
// is why the payload arrives here unparsed and this reports rather than throws.
const final = caller.transcriptEvents().at(-1);
checks.record(
  "the transcript carries an event id",
  Number.isInteger(final?.event_id),
  String(final?.event_id),
);

await caller.leave();
checks.finish();
