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

import { Room, RoomEvent, AudioStream } from "@livekit/rtc-node";

const openconv = (process.argv[2] ?? "http://127.0.0.1:8080").replace(/\/$/, "");
const livekitUrl = process.argv[3] ?? "wss://livekit.sanctuary.gdn";
const xiApiKey = process.env.OPENCONV_API_KEY;
if (!xiApiKey) throw new Error("missing OPENCONV_API_KEY");

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push({ name, ok, detail });
  console.log(`${ok ? "  ok  " : " FAIL "} ${name}${detail ? ` — ${detail}` : ""}`);
};

const deadline = (ms, what) =>
  new Promise((_, reject) => setTimeout(() => reject(new Error(`timed out waiting for ${what}`)), ms));

// ---- mint a token, which is also what dispatches the agent ----
const response = await fetch(
  `${openconv}/v1/convai/conversation/token?agent_id=agent_happy&participant_name=u_agentcheck`,
  { headers: { "xi-api-key": xiApiKey } },
);
if (!response.ok) throw new Error(`mint failed: HTTP ${response.status} ${await response.text()}`);
const { token } = await response.json();
const conversationId = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString()).video.room;
console.log(`joining ${conversationId} at ${livekitUrl}\n`);

// ---- join as the human would ----
const room = new Room();

const controlEvents = [];
const participants = [];
let audioTrack = null;

room.on(RoomEvent.DataReceived, (payload) => {
  try {
    controlEvents.push(JSON.parse(new TextDecoder().decode(payload)));
  } catch {
    controlEvents.push({ type: "<not json>" });
  }
});
room.on(RoomEvent.ParticipantConnected, (p) => participants.push(p.identity));
// Start reading the moment the track is subscribed, the way a client that is playing
// audio does. Reading later instead — after the control-event assertions below — misses
// whatever the agent said on subscribe, and reports a working track as silent.
const heard = { frames: 0, peak: 0 };
room.on(RoomEvent.TrackSubscribed, (track) => {
  if (audioTrack) return;
  audioTrack = track;
  (async () => {
    for await (const frame of new AudioStream(track)) {
      heard.frames += 1;
      for (const sample of frame.data) heard.peak = Math.max(heard.peak, Math.abs(sample));
    }
  })().catch(() => {});
});

await Promise.race([
  room.connect(livekitUrl, token, { autoSubscribe: true, dynacast: false }),
  deadline(20_000, "the room connection"),
]);
check("the client joined the conversation room", true, conversationId);

// The agent may already have been in the room when we connected, in which case there is
// no ParticipantConnected event to catch — so consult the roster too.
const waitFor = async (predicate, ms, what) => {
  const started = Date.now();
  while (Date.now() - started < ms) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  console.error(`  (gave up waiting for ${what})`);
  return false;
};

const agentPresent = () =>
  participants.some((identity) => identity.startsWith("agent_")) ||
  Array.from(room.remoteParticipants.values()).some((p) => p.identity.startsWith("agent_"));

check(
  "the agent is a connected participant",
  await waitFor(agentPresent, 25_000, "the agent to join"),
  Array.from(room.remoteParticipants.values()).map((p) => p.identity).join(", "),
);

// ---- the control channel ----
await waitFor(() => controlEvents.length >= 2, 15_000, "control events");

check("control events arrived", controlEvents.length > 0, `${controlEvents.length} received`);

// The rule that decides whether the real SDK ever resolves its connect promise. Its
// listener is {once:true}: if this is not first, startSession() hangs forever.
check(
  "the FIRST control event is conversation_initiation_metadata",
  controlEvents[0]?.type === "conversation_initiation_metadata",
  controlEvents[0]?.type ?? "<none>",
);

const metadata = controlEvents.find((e) => e.type === "conversation_initiation_metadata");
check(
  "the announcement echoes this conversation's id",
  metadata?.conversation_initiation_metadata_event?.conversation_id === conversationId,
  metadata?.conversation_initiation_metadata_event?.conversation_id,
);
check(
  "the announcement declares both audio formats",
  Boolean(metadata?.conversation_initiation_metadata_event?.agent_output_audio_format) &&
    Boolean(metadata?.conversation_initiation_metadata_event?.user_input_audio_format),
  `${metadata?.conversation_initiation_metadata_event?.agent_output_audio_format} / ${metadata?.conversation_initiation_metadata_event?.user_input_audio_format}`,
);

// This is what fires the app's onVadScore callback.
const vad = controlEvents.find((e) => e.type === "vad_score");
check("a vad_score event arrived", Boolean(vad), JSON.stringify(vad?.vad_score_event ?? null));

// ---- the audio track ----
check(
  "the agent published an audio track",
  await waitFor(() => audioTrack !== null, 15_000, "the agent's audio track"),
  audioTrack?.kind !== undefined ? `kind=${audioTrack.kind}` : "",
);

// A subscribed-but-silent track is exactly what a broken pump looks like, and it is
// indistinguishable from a working one until you measure the samples.
await waitFor(() => heard.frames > 50, 10_000, "audio frames");
check("audio frames are flowing", heard.frames > 0, `${heard.frames} frames`);
check(
  "the track carried audible sound, not silence",
  heard.peak > 1000,
  `peak amplitude ${heard.peak}`,
);

await room.disconnect();

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
process.exit(0);
