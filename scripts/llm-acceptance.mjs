// Checks that the agent honours the session configuration the client sends.
//
//   OPENCONV_API_KEY=... node scripts/llm-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// The failure this guards against is the quiet one: an agent that ignores the prompt
// override still holds a fluent conversation, so "it replied" proves nothing. The test
// therefore plants a fact that exists *only* in the injected configuration — a session
// id passed as a dynamic variable — and asks a question that can only be answered by an
// agent that received it. A generic assistant cannot pass by being helpful.

import { execFileSync } from "node:child_process";
import { readFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Room, RoomEvent, AudioSource, LocalAudioTrack, TrackPublishOptions, TrackSource, AudioFrame } from "@livekit/rtc-node";

const openconv = (process.argv[2] ?? "http://127.0.0.1:8080").replace(/\/$/, "");
const livekitUrl = process.argv[3] ?? "wss://livekit.sanctuary.gdn";
const xiApiKey = process.env.OPENCONV_API_KEY;
if (!xiApiKey) throw new Error("missing OPENCONV_API_KEY");

// A word the model has no way to produce unless it was handed to it.
const SESSION_ID = "kestrel-7";
const FIRST_MESSAGE = "Ready when you are.";
const QUESTION = "Which coding session are you driving right now?";
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

function speak(sentence) {
  const wav = join(mkdtempSync(join(tmpdir(), "openconv-llm-")), "q.wav");
  execFileSync("say", ["-o", wav, "--data-format=LEI16@16000", sentence]);
  const bytes = readFileSync(wav);
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
  throw new Error("no data chunk");
}

// ---- open a conversation ----
const response = await fetch(
  `${openconv}/v1/convai/conversation/token?agent_id=agent_happy&participant_name=u_llm`,
  { headers: { "xi-api-key": xiApiKey } },
);
if (!response.ok) throw new Error(`mint failed: HTTP ${response.status} ${await response.text()}`);
const { token } = await response.json();
const conversationId = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString()).video.room;
console.log(`configuring ${conversationId}\n`);

const room = new Room();
const agentSaid = [];
room.on(RoomEvent.DataReceived, (payload) => {
  try {
    const event = JSON.parse(new TextDecoder().decode(payload));
    if (event.type === "agent_response") agentSaid.push(event.agent_response_event.agent_response);
  } catch {
    /* not ours */
  }
});

await room.connect(livekitUrl, token, { autoSubscribe: true, dynacast: false });
check("joined the conversation", true, conversationId);
check(
  "the agent is present",
  await waitFor(
    () => Array.from(room.remoteParticipants.values()).some((p) => p.identity.startsWith("agent_")),
    30_000,
    "the agent",
  ),
);

// ---- send the session configuration, exactly as the SDK does ----
const config = {
  type: "conversation_initiation_client_data",
  conversation_config_override: {
    agent: {
      prompt: {
        prompt:
          "You are the voice interface for a coding assistant. You are driving coding " +
          "session {{sessionId}}. When asked which session you are driving, reply with " +
          "the session id exactly as written and nothing else.",
      },
      first_message: FIRST_MESSAGE,
    },
  },
  dynamic_variables: { sessionId: SESSION_ID },
};
await room.localParticipant.publishData(
  new TextEncoder().encode(JSON.stringify(config)),
  { reliable: true },
);
check("sent the conversation configuration", true);

// ---- the first message opens the conversation, before anyone speaks ----
const gotFirst = await waitFor(() => agentSaid.length > 0, 20_000, "the first message");
check("the agent opened with the configured first message", gotFirst && agentSaid[0] === FIRST_MESSAGE, agentSaid[0]);

// ---- now ask the question only a configured agent can answer ----
const source = new AudioSource(SAMPLE_RATE, 1);
await room.localParticipant.publishTrack(
  LocalAudioTrack.createAudioTrack("caller", source),
  new TrackPublishOptions({ source: TrackSource.SOURCE_MICROPHONE }),
);

const FRAME = SAMPLE_RATE / 100;
const silence = new Int16Array(FRAME);
const pushSilence = async (n) => {
  for (let i = 0; i < n; i += 1) await source.captureFrame(new AudioFrame(silence, SAMPLE_RATE, 1, FRAME));
};

await new Promise((r) => setTimeout(r, 2000));
await pushSilence(150);

const before = agentSaid.length;
const pcm = speak(QUESTION);
console.log(`  asking: ${JSON.stringify(QUESTION)}`);
for (let offset = 0; offset < pcm.length; offset += FRAME) {
  const slice = pcm.subarray(offset, Math.min(offset + FRAME, pcm.length));
  await source.captureFrame(new AudioFrame(Int16Array.from(slice), SAMPLE_RATE, 1, slice.length));
}
await pushSilence(120);

const answered = await waitFor(() => agentSaid.length > before, 60_000, "an answer");
check("the agent answered", answered, `${agentSaid.length - before} response(s)`);

const answer = agentSaid[agentSaid.length - 1] ?? "";
console.log(`  agent said: ${JSON.stringify(answer)}`);

// The whole point: this word reached the model only through dynamic_variables.
check(
  "the answer reflects the injected session context",
  answer.toLowerCase().includes(SESSION_ID.toLowerCase()),
  `looking for ${JSON.stringify(SESSION_ID)}`,
);
check(
  "the reply is short enough to speak aloud",
  answer.length > 0 && answer.length < 300,
  `${answer.length} chars`,
);

await room.disconnect();

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
process.exit(0);
