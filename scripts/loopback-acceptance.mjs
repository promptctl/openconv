// Proves the transport itself: audio published into a LiveKit room reaches every
// subscriber in that room, whole and as loud as it left.
//
//   LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
//     NODE_PATH=/path/to/node_modules node scripts/loopback-acceptance.mjs [wss://url]
//
// Nothing here involves openconv. No server, no agent, no speech model, no LLM — one
// room this script creates, three clients it joins, and the same recording the speech
// acceptance runs speak. That is the whole point: every measurement of
// openconv-openconv-bwy.26 so far has been taken with an instrument nobody had checked,
// and a subscriber reporting silence is indistinguishable from a publisher that sent it.
//
// It speaks through `Caller` rather than through a bespoke client for the same reason a
// probe is worth building at all: a client with its own idea of how to publish measures
// its own idea of how to publish. This is the code the acceptance scripts run.
//
// Two listeners rather than one, because one cannot answer the question it raises. A lone
// subscriber that hears silence has three candidate causes — the publisher never sent it,
// the SFU lost it, or this subscriber's own decode path dropped it — and reports the same
// number for all three. Two subscribers on one track split that: they disagree only if
// the loss is per-subscriber. `reportedSpeaking` splits it again, from above, since the
// SFU forms its own opinion of how loud the publisher was from the RTP audio-level header
// rather than from anything either listener decoded.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

import { Caller, Checks, SPOKEN, millis, recordSpeech, sounding } from "./lib/caller.mjs";
import { Rooms, joinToken } from "./lib/livekit.mjs";

// `SPOKEN` is imported rather than repeated: a tone would survive encodings that speech
// does not — Opus, voice activity detection and noise suppression all treat a steady sine
// differently from a voice — so the probe has to carry the signal that is actually going
// missing, which means the sentence stt-acceptance sends and not one that resembles it.

/// The identity that publishes. Every listener's account is of this participant.
const SPEAKER = "probe-speaker";

/// Two ears on one track. Named rather than counted so a failure says which one lost it.
const LISTENERS = ["probe-listener-a", "probe-listener-b"];

/// How long the tail of an utterance is given to cross the network after the publisher's
/// own queue has drained. The SFU, the jitter buffer and each subscriber's sink all sit
/// between the last captured frame and the last delivered one.
const SETTLE_MS = 5_000;

/// How much quieter a subscriber may hear the utterance before that counts as loss. Opus
/// is lossy but not by 6 dB on a peak; a factor of two is loose enough that codec choice
/// cannot fail this run, and still eighty decibels away from the digital silence this
/// ticket keeps finding.
const QUIETEST_ACCEPTABLE = 0.5;

/// How much of the utterance's audible duration must survive. Not all of it: the first
/// and last window straddle the boundary between silence and speech, and which side of
/// the threshold they land on depends on where the encoder happened to cut a packet.
///
/// [LAW:one-source-of-truth] Read by the wait as well as the check. Waiting for a
/// stricter number than the check accepts is waiting for something this comment says will
/// not reliably happen: every healthy run then burns the whole settle window and prints
/// "gave up" on its way to passing, which teaches a reader to ignore the one line that
/// says audio went missing.
const SHORTEST_ACCEPTABLE = 0.9;

/** The one boundary: everything below runs on values known to exist. */
function readConfig(env, argv) {
  const missing = ["LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"].filter((name) => !env[name]);
  if (missing.length > 0) {
    throw new Error(`missing ${missing.join(" and ")} — read them from Vault at secret/livekit`);
  }
  return {
    apiKey: env.LIVEKIT_API_KEY,
    apiSecret: env.LIVEKIT_API_SECRET,
    livekitUrl: (argv[2] ?? "wss://livekit.sanctuary.gdn").replace(/\/$/, ""),
  };
}

const { apiKey, apiSecret, livekitUrl } = readConfig(process.env, process.argv);
const checks = new Checks();

// Created rather than joined: the deployment runs with room.auto_create off, and a
// conversation room is capped at two participants — this one is ours, so it can be
// whatever shape the measurement needs.
const rooms = new Rooms({ url: livekitUrl, apiKey, apiSecret });
const name = `probe_${randomUUID()}`;
await rooms.call("CreateRoom", { name, empty_timeout: 120, max_participants: 8 });
console.log(`created ${name} at ${livekitUrl}\n`);

const at = (identity) =>
  Caller.at(livekitUrl, joinToken({ apiKey, apiSecret, room: name, identity }));

const clients = [];
const recording = recordSpeech(SPOKEN, join(mkdtempSync(join(tmpdir(), "openconv-loopback-")), "speech.wav"));
const published = sounding(recording.samples, recording.sampleRate);

/// The audible duration a listener has to reach. One number for the wait and the check,
/// so the wait ends the moment the run is healthy rather than at the settle window.
const ENOUGH = Math.ceil(published.audibleFrames * SHORTEST_ACCEPTABLE);

// From here the room exists and is this script's to clean up. Everything that can throw —
// three connections with their own timeouts, `say`, every wait — sits inside, because a
// failure that skipped the cleanup would leave connected participants in a room on the
// real deployment, and a room with clients in it is not empty, so `empty_timeout` does
// not start counting until the process dies.
let listeners = [];
try {
  // The listeners first. Publishing waits for a remote subscription before it captures a
  // frame, and a publisher alone in a room would wait for a subscriber who never arrives.
  listeners = await Promise.all(LISTENERS.map(at));
  clients.push(...listeners);
  const speaker = await at(SPEAKER);
  clients.push(speaker);

  checks.record(
    "every listener sees the speaker in the room",
    (
      await Promise.all(
        listeners.map((listener) =>
          listener.waitFor(() => listener.roster().includes(SPEAKER), 10_000, "the speaker to join"),
        ),
      )
    ).every(Boolean),
    listeners.map((listener) => listener.roster().length).join(" and ") + " participants seen",
  );

  console.log(
    `\nspeaking ${millis(published.frames)} ms, of which ${millis(published.audibleFrames)} ms audible, peak ${published.peak}\n`,
  );

  // Published, then gated, then spoken — three steps rather than `speak()`'s one, because
  // `microphone()` resolves on the *first* remote subscription and this room has two. A
  // listener still subscribing while the lead-in plays loses the front of the utterance,
  // which is this probe's own symptom wearing the costume of the bug it hunts.
  const mic = await speaker.microphone(recording.sampleRate);
  checks.record(
    "every listener is subscribed before a word is spoken",
    (
      await Promise.all(
        listeners.map((listener) =>
          listener.waitFor(() => listener.subscribed(), 10_000, "a track to listen to"),
        ),
      )
    ).every(Boolean),
    `${listeners.filter((listener) => listener.subscribed()).length}/${listeners.length} subscribed`,
  );

  await mic.say(recording);
  await Promise.all(
    listeners.map((listener) =>
      listener.waitFor(
        () => listener.heard.audibleFrames >= ENOUGH,
        SETTLE_MS,
        "the whole utterance to arrive",
      ),
    ),
  );
} finally {
  // Reported rather than swallowed: a client that could not leave is a finding about the
  // room, and letting it pass silently here would hide it behind whatever sent us into
  // this block. `allSettled` so one bad disconnect cannot skip the delete.
  const left = await Promise.allSettled(clients.map((client) => client.leave()));
  for (const outcome of left.filter((outcome) => outcome.status === "rejected")) {
    console.error(`  (a client failed to leave ${name}: ${outcome.reason})`);
  }
  await rooms.call("DeleteRoom", { room: name });
}

console.log();
for (const [index, listener] of listeners.entries()) {
  const heard = listener.heard;
  const who = LISTENERS[index];
  console.log(
    `${who}: heard ${millis(heard.frames)} ms, of which ${millis(heard.audibleFrames)} ms audible, ` +
      `peak ${heard.peak}, SFU called the speaker ${listener.reportedSpeaking.has(SPEAKER) ? "loud" : "silent"}`,
  );
}
console.log();

for (const [index, listener] of listeners.entries()) {
  const heard = listener.heard;
  const who = LISTENERS[index];

  // Reported before anything else is judged: a reader that died on its first frame leaves
  // the same zero counts as a track that was never spoken into, and only this tells them
  // apart.
  checks.record(
    `${who}: the audio stream survived the call`,
    heard.error === null,
    heard.error ? String(heard.error) : `${heard.frames} frames read`,
  );

  checks.record(`${who}: received frames at all`, heard.frames > 0, `${heard.frames} frames`);

  // The failure this ticket keeps finding, stated directly. `loudest = 3.05e-5` on the
  // agent's side is one least significant bit of an i16 — a complete, on-time, empty track.
  checks.record(
    `${who}: heard sound rather than digital silence`,
    heard.peak > 0 && heard.peak / published.peak >= QUIETEST_ACCEPTABLE,
    `peak ${heard.peak} against ${published.peak} published (${(heard.peak / published.peak).toFixed(3)}x)`,
  );

  // And the other half of the symptom: audio that arrives loud but stops partway, which a
  // peak taken across the whole call cannot see.
  checks.record(
    `${who}: heard the whole utterance, not a fragment`,
    heard.audibleFrames >= ENOUGH,
    `${millis(heard.audibleFrames)} ms audible against ${millis(published.audibleFrames)} ms published`,
  );

  // Where the loss happened, from a reading neither this client nor the network produced.
  // A speaker the SFU never called loud was already silent when its own libwebrtc stamped
  // the packet, which puts the loss in the publisher rather than anywhere downstream.
  checks.record(
    `${who}: the SFU also considered the speaker audible`,
    listener.reportedSpeaking.has(SPEAKER),
    listener.reportedSpeaking.size > 0 ? [...listener.reportedSpeaking].join(", ") : "nobody, ever",
  );
}

// Outside the cleanup block on purpose: `finish` exits the process, so anything placed
// after it inside a `finally` would never run.
checks.finish();
