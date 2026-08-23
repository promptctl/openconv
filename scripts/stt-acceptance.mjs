// Speaks a sentence into a live conversation and checks the agent heard it.
//
//   OPENCONV_API_KEY=... node scripts/stt-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs `npm install @livekit/rtc-node` and macOS `say`, which supplies the voice.
// Using real synthesized speech rather than a recorded fixture keeps the check honest:
// it exercises the resampling, the endpointer, and the model on audio that arrived over
// WebRTC, which is the only path that matters.

import { execFileSync } from "node:child_process";
import { readFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Room, RoomEvent, AudioSource, LocalAudioTrack, TrackPublishOptions, TrackSource, AudioFrame } from "@livekit/rtc-node";

const openconv = (process.argv[2] ?? "http://127.0.0.1:8080").replace(/\/$/, "");
const livekitUrl = process.argv[3] ?? "wss://livekit.sanctuary.gdn";
const xiApiKey = process.env.OPENCONV_API_KEY;
if (!xiApiKey) throw new Error("missing OPENCONV_API_KEY");

const SPOKEN = "Hello, can you hear me? This is a test of the voice agent.";
const SAMPLE_RATE = 16000;

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push({ name, ok, detail });
  console.log(`${ok ? "  ok  " : " FAIL "} ${name}${detail ? ` — ${detail}` : ""}`);
};

const waitFor = async (predicate, ms, what) => {
  const started = Date.now();
  while (Date.now() - started < ms) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, 100));
  }
  console.error(`  (gave up waiting for ${what})`);
  return false;
};

/// Renders the sentence with the system voice and returns 16 kHz mono PCM.
function speak(sentence) {
  const wav = join(mkdtempSync(join(tmpdir(), "openconv-stt-")), "speech.wav");
  execFileSync("say", ["-o", wav, "--data-format=LEI16@16000", sentence]);

  const bytes = readFileSync(wav);
  // Walk the chunk list; `say` writes a LIST chunk before the audio, so a fixed offset
  // would read metadata as samples.
  let offset = 12;
  while (offset + 8 <= bytes.length) {
    const id = bytes.toString("ascii", offset, offset + 4);
    const size = bytes.readUInt32LE(offset + 4);
    if (id === "data") {
      const pcm = new Int16Array(size / 2);
      for (let i = 0; i < pcm.length; i += 1) pcm[i] = bytes.readInt16LE(offset + 8 + i * 2);
      return pcm;
    }
    offset += 8 + size + (size % 2);
  }
  throw new Error("no data chunk in generated wav");
}

// ---- open a conversation ----
const response = await fetch(
  `${openconv}/v1/convai/conversation/token?agent_id=agent_happy&participant_name=u_stt`,
  { headers: { "xi-api-key": xiApiKey } },
);
if (!response.ok) throw new Error(`mint failed: HTTP ${response.status} ${await response.text()}`);
const { token } = await response.json();
const conversationId = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString()).video.room;
console.log(`speaking into ${conversationId}\n`);

const room = new Room();
const transcripts = [];
room.on(RoomEvent.DataReceived, (payload) => {
  try {
    const event = JSON.parse(new TextDecoder().decode(payload));
    if (event.type === "user_transcript" || event.type === "tentative_user_transcript") {
      transcripts.push(event);
    }
  } catch {
    /* not ours */
  }
});

await room.connect(livekitUrl, token, { autoSubscribe: true, dynacast: false });
check("joined the conversation", true, conversationId);

check(
  "the agent is present to listen",
  await waitFor(
    () => Array.from(room.remoteParticipants.values()).some((p) => p.identity.startsWith("agent_")),
    30_000,
    "the agent",
  ),
);

// ---- publish a microphone carrying the sentence ----
const source = new AudioSource(SAMPLE_RATE, 1);
const track = LocalAudioTrack.createAudioTrack("caller", source);
await room.localParticipant.publishTrack(
  track,
  new TrackPublishOptions({ source: TrackSource.SOURCE_MICROPHONE }),
);
check("published the caller's microphone", true);

const FRAME = SAMPLE_RATE / 100;
const silence = new Int16Array(FRAME);
const pushSilence = async (frames) => {
  for (let i = 0; i < frames; i += 1) {
    await source.captureFrame(new AudioFrame(silence, SAMPLE_RATE, 1, FRAME));
  }
};

// A real microphone streams silence before anyone speaks, and this must too. Speaking
// the instant the track is published pushes the opening word while the agent's
// subscription is still being established, and the first word is simply gone — which
// looks exactly like a transcription error and is not one.
await new Promise((r) => setTimeout(r, 2000));
await pushSilence(150);

const pcm = speak(SPOKEN);
console.log(`  speaking ${(pcm.length / SAMPLE_RATE).toFixed(2)}s of audio...`);

// Pushed in real time. Sending it as fast as possible would not resemble a person
// talking, and the endpointer would see one burst rather than an utterance.
for (let offset = 0; offset < pcm.length; offset += FRAME) {
  const slice = pcm.subarray(offset, Math.min(offset + FRAME, pcm.length));
  const frame = new AudioFrame(Int16Array.from(slice), SAMPLE_RATE, 1, slice.length);
  await source.captureFrame(frame);
}

// Then silence, so the endpointer sees the caller stop talking.
await pushSilence(120);
check("finished speaking", true);

// ---- did it hear? ----
const gotFinal = await waitFor(
  () => transcripts.some((t) => t.type === "user_transcript"),
  45_000,
  "a final transcript",
);

check("a user_transcript event arrived", gotFinal, `${transcripts.length} transcript event(s)`);

const final = transcripts.filter((t) => t.type === "user_transcript").pop();
const heard = final?.user_transcription_event?.user_transcript ?? "";
console.log(`  agent heard: ${JSON.stringify(heard)}`);

// Word overlap rather than string equality: the point is that it heard the sentence,
// not that a speech model reproduced punctuation exactly.
const words = (text) =>
  new Set(text.toLowerCase().replace(/[^a-z0-9 ]/g, "").split(/\s+/).filter(Boolean));
const spokenWords = words(SPOKEN);
const heardWords = words(heard);
const matched = [...spokenWords].filter((w) => heardWords.has(w));
const accuracy = spokenWords.size === 0 ? 0 : matched.length / spokenWords.size;

check(
  "the transcript matches what was said",
  accuracy >= 0.8,
  `${Math.round(accuracy * 100)}% of words (${matched.length}/${spokenWords.size})`,
);
check("the transcript carries an event id", Number.isInteger(final?.user_transcription_event?.event_id));

await room.disconnect();

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
process.exit(0);
