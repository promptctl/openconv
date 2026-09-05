// Verifies what voice activity detection is for, by being the human on the other end.
//
//   OPENCONV_API_KEY=... node scripts/vad-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node and macOS `say`:
//
//   NODE_PATH=/path/to/node_modules node scripts/vad-acceptance.mjs
//
// Two claims, neither of which any unit test can reach, because both are about what a
// caller experiences in a live room:
//
// 1. **The scores track real speech.** The app drives its microphone indicator from
//    `vad_score` events, thresholding at 0.5 with a 300 ms debounce. So the scores have
//    to keep arriving whether or not anyone is talking, and they have to be high while
//    the caller talks and low while they do not. A stream of zeroes satisfies "events
//    arrive" and drives nothing.
//
// 2. **Talking over the agent stops it.** The caller starts speaking while the agent is
//    mid-answer, and the agent has to go quiet — promptly, and having said so on the
//    control channel, because the client drops its own buffered audio when it sees the
//    `interruption` event and keeps playing the abandoned reply if it never does.
//
// The second is measured against sound on the track rather than against control events
// alone: an agent that publishes `interruption` and keeps talking has failed the thing
// the event exists to promise.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Caller, Checks, readEnvironment, recordSpeech } from "./lib/caller.mjs";

/// Long enough that the agent is still speaking when it is interrupted. A one-word reply
/// is over before the caller can talk over it, and the barge-in check would then be
/// measuring nothing.
const PROMPT =
  "You are a voice assistant under test. Whatever the caller says, answer by counting " +
  "slowly from one to forty, one number per sentence, and nothing else.";

/// What the caller says to set the agent counting, and what they cut in with.
const OPENER = "Please start counting now.";
const INTERJECTION = "Actually, stop, I have changed my mind about that.";

/// Where a score stops being silence and starts being speech — the same 0.5 the app
/// applies and the agent's own threshold, so this asserts the number the product uses.
const SPEECH = 0.5;

/// How long after the caller starts talking the agent must have stopped making sound.
///
/// The budget covers the agent needing 60 ms of speech before it will call it speech,
/// the round trip to the SFU and back, and the caller's own jitter buffer draining. The
/// number that matters to a listener is well under this; a bar set at the typical value
/// would fail on a slow network for a reason that is not a regression.
const STOP_WITHIN_MS = 1_500;

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const scratch = mkdtempSync(join(tmpdir(), "openconv-"));
const opener = recordSpeech(OPENER, join(scratch, "opener.wav"));
const interjection = recordSpeech(INTERJECTION, join(scratch, "interjection.wav"));

const caller = await Caller.join({
  openconv,
  livekitUrl,
  xiApiKey,
  participantName: "u_vad",
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

/** Every score published so far, in arrival order. */
const scores = () =>
  caller.controlEvents
    .filter((event) => event.type === "vad_score")
    .map((event) => event.vad_score_event?.vad_score);

/** The scores published since a mark, which is how a stretch of the call is measured. */
const scoresSince = (mark) => scores().slice(mark);

// ── The scores, over silence and over speech ────────────────────────────────────────

// The microphone is opened before anything is said, so the agent is scoring a real
// silent track rather than no track at all — which is what the app shows before the
// caller speaks, and where a detector that reads its own noise floor as speech shows up.
await caller.microphone(opener.sampleRate);

const quietFrom = scores().length;
await caller.waitFor(() => scoresSince(quietFrom).length >= 20, 20_000, "scores over silence");
const overSilence = scoresSince(quietFrom);

checks.record(
  "scores arrive continuously, not only at turn boundaries",
  overSilence.length >= 20,
  `${overSilence.length} scores in ~2s of silence`,
);

checks.record(
  "every score is a number the app can threshold",
  overSilence.length > 0 && overSilence.every((score) => Number.isFinite(score) && score >= 0 && score <= 1),
  JSON.stringify(overSilence.slice(0, 5)),
);

checks.record(
  "silence does not read as speech",
  overSilence.every((score) => score < SPEECH),
  `highest score over silence was ${Math.max(...overSilence).toFixed(3)}`,
);

const speakingFrom = scores().length;
await caller.speak(opener, { leadInMs: 200, tailOffMs: 1500 });
const overSpeech = scoresSince(speakingFrom);

checks.record(
  "speech reads as speech",
  overSpeech.some((score) => score >= SPEECH),
  `highest score while the caller spoke was ${Math.max(0, ...overSpeech).toFixed(3)}`,
);

// ── Barge-in ────────────────────────────────────────────────────────────────────────

console.log("\nwaiting for the agent to start answering, then talking over it\n");

const beforeAnswer = caller.mark();
const answering = await caller.waitFor(
  () => caller.heard.audibleFrames - beforeAnswer.audibleFrames > 50,
  120_000,
  "the agent to start speaking",
);

checks.record("the agent answered out loud", answering, `peak ${caller.heard.peak}`);

const interruptionsBefore = caller.controlEvents.filter((e) => e.type === "interruption").length;
const mic = await caller.microphone(interjection.sampleRate);

// No lead-in: the caller cuts in, and the clock for "how fast did it stop" starts on the
// first sample of the interjection rather than on a second of silence before it.
const cutInAt = Date.now();
await mic.say(interjection, { leadInMs: 0, tailOffMs: 1500 });

checks.record(
  "the agent said it had been interrupted",
  await caller.waitFor(
    () => caller.controlEvents.filter((e) => e.type === "interruption").length > interruptionsBefore,
    10_000,
    "an interruption event",
  ),
);

// Measured from the last frame that actually carried sound. The track keeps delivering
// frames after the agent stops talking — they are just silent — so counting frames
// cannot tell a stopped agent from a running one.
const stoppedAfterMs = caller.heard.lastAudibleAt - cutInAt;

checks.record(
  "the agent stopped talking when it was talked over",
  stoppedAfterMs < STOP_WITHIN_MS,
  `last audible ${stoppedAfterMs} ms after the caller cut in (budget ${STOP_WITHIN_MS} ms)`,
);

// The counting prompt runs to forty, so an agent that merely paused would be audible
// again by now. This is what separates "stopped" from "took a breath".
const afterStopping = caller.mark();
await new Promise((resolve) => setTimeout(resolve, 1_000));

checks.record(
  "it stayed stopped rather than finishing the abandoned answer",
  caller.heard.audibleFrames - afterStopping.audibleFrames < 25,
  `${caller.heard.audibleFrames - afterStopping.audibleFrames} audible frames in the second after`,
);

checks.record(
  "the audio reader ran to the end of the call",
  caller.heard.error === null,
  caller.heard.error ? String(caller.heard.error) : `${caller.heard.frames} frames read`,
);

await caller.leave();
checks.finish();
