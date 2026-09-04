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

// `SPOKEN` is shared with loopback-acceptance, which claims the transport carries the
// same sentence this script claims the agent transcribes. That pairing is only a bound on
// this one while both are speaking the same words.
import { Caller, Checks, SPOKEN, readEnvironment, recordSpeech } from "./lib/caller.mjs";

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

// This run overrides nothing, so the handshake sends a message full of nulls rather than
// skipping a step — and `SessionConfig::settle` reads that as "use the defaults", landing
// exactly where a conversation starts before any client speaks. Pinned by
// `a_message_that_overrides_nothing_settles_where_a_conversation_starts`, which is what
// lets every caller take one path instead of this one taking a shorter one.
checks.record(
  "the agent is present to listen",
  await caller.agentConfigured(30_000),
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
//
// The wait is for the words to stop arriving, not to start. The endpointer decides where
// an utterance stops, and one sentence spoken with a breath in it settles as two finals a
// moment apart — this deployment has returned ["Hello", "Bye."] for a single spoken line.
// Reading on the first one would score half a sentence and blame speech-to-text for it.
//
// Quiescence rather than an arrival event because there is no arrival event: nothing in
// the protocol announces that a segment was the last one, so holding still is the only
// signal there is, and it is named here rather than approximated with a sleep.
const QUIET_MS = 1500;
let counted = 0;
let lastArrivedAt = Date.now();
const stoppedArriving = () => {
  const finals = caller.transcriptEvents().length;
  if (finals !== counted) {
    counted = finals;
    lastArrivedAt = Date.now();
  }
  return finals > 0 && Date.now() - lastArrivedAt >= QUIET_MS;
};

checks.record(
  "a user_transcript event arrived",
  await caller.waitFor(stoppedArriving, 45_000, "the caller's words to stop arriving"),
  `${caller.events("tentative_user_transcript").length} tentative, ` +
    `${caller.transcriptEvents().length} final`,
);

// Every settled transcript joined, not the last one: both halves of a split utterance
// were heard, so a claim about what reached speech-to-text has to count both. With no
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
