// Verifies the agent by being the client: joins a real conversation room and asserts
// what the ElevenLabs SDK would need in order to work.
//
//   OPENCONV_API_KEY=... node scripts/agent-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node, which is not a dependency of anything else here:
//
//   npm install @livekit/rtc-node
//   NODE_PATH=/path/to/node_modules node scripts/agent-acceptance.mjs
//
// This stands in for "start a Happy session and watch it work". It checks the same
// three things that ticket says to check — the agent is a connected participant, its
// audio track carries sound, and a vad_score arrives — plus the ordering rule that
// decides whether the real SDK ever finishes connecting at all.
//
// Everything here is about *connecting*. Whether a caller can be heard and answered is
// `live-call-acceptance.mjs`, which holds a whole turn.

import { Caller, Checks, readEnvironment } from "./lib/caller.mjs";

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

const caller = await Caller.join({ openconv, livekitUrl, xiApiKey, participantName: "u_agentcheck" });
console.log(`joined ${caller.conversationId} at ${livekitUrl}\n`);

checks.record("the client joined the conversation room", true, caller.conversationId);

checks.record(
  "the agent is a connected participant and holds this conversation's configuration",
  await caller.agentConfigured(25_000),
  caller.roster().join(", "),
);

// ---- the control channel ----
await caller.waitFor(() => caller.controlEvents.length >= 1, 15_000, "control events");

checks.record(
  "control events arrived",
  caller.controlEvents.length > 0,
  `${caller.controlEvents.length} received`,
);

// The rule that decides whether the real SDK ever resolves its connect promise. Its
// listener is {once:true}: if this is not first, startSession() hangs forever.
checks.record(
  "the FIRST control event is conversation_initiation_metadata",
  caller.controlEvents[0]?.type === "conversation_initiation_metadata",
  caller.controlEvents[0]?.type ?? "<none>",
);

const announcement = caller.control("conversation_initiation_metadata")
  ?.conversation_initiation_metadata_event;

checks.record(
  "the announcement echoes this conversation's id",
  announcement?.conversation_id === caller.conversationId,
  announcement?.conversation_id,
);
checks.record(
  "the announcement declares both audio formats",
  Boolean(announcement?.agent_output_audio_format) &&
    Boolean(announcement?.user_input_audio_format),
  `${announcement?.agent_output_audio_format} / ${announcement?.user_input_audio_format}`,
);

// This is what fires the app's onVadScore callback.
//
// Scores are computed from the caller's own audio, so they follow the microphone rather
// than the announcement — publishing one is what the real SDK does on startSession, and
// a client with no microphone has nothing to score and no indicator to drive. What the
// scores are *worth* is `vad-acceptance.mjs`; this only asserts they start.
await caller.microphone();
await caller.waitFor(() => caller.control("vad_score"), 15_000, "a vad_score");

const vad = caller.control("vad_score");
checks.record("a vad_score event arrived", Boolean(vad), JSON.stringify(vad?.vad_score_event ?? null));

// ---- the audio track ----
checks.record(
  "the agent published an audio track",
  await caller.waitFor(() => caller.subscribed(), 15_000, "the agent's audio track"),
  caller.remoteTrack ? `kind=${caller.remoteTrack.kind}` : "",
);

// Frames, not volume. Nobody has spoken and no first message is configured, so this
// track *should* be carrying silence — and it once carried a tone on join, which is why
// this script used to assert audible samples here. Asserting it now would only be
// asking the agent to talk to itself. Whether speech reaches the track is a claim about
// an answer, so it is made where there is something to answer: `live-call-acceptance`.
await caller.waitFor(() => caller.heard.frames > 50, 10_000, "audio frames");
checks.record("audio frames are flowing", caller.heard.frames > 0, `${caller.heard.frames} frames`);

// Zero frames from a reader that crashed and zero frames from a track nobody spoke into
// are the same number, and only this separates them.
checks.record(
  "the audio reader ran to the end of the call",
  caller.heard.error === null,
  caller.heard.error ? String(caller.heard.error) : `${caller.heard.frames} frames read`,
);

await caller.leave();
checks.finish();
