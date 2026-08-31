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
//
// And each client's own peer connection is asked what it saw, which is the reading that
// says *why*. Decoded audio cannot tell a path that dropped the packets from one that
// never carried them, and those are different bugs in different systems: the first is the
// network, the second is whatever was supposed to send. `path:` names the route ICE
// actually selected and `rtp:` counts what came over it, so a run states its own transport
// conditions instead of leaving them to be inferred from a symptom.

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

import { Caller, Checks, SPOKEN, millis, recordSpeech, sounding } from "./lib/caller.mjs";
import { Rooms, joinToken, livekitCredentials } from "./lib/livekit.mjs";

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

/// How long a client is given to finish joining and subscribing before the run is void.
const SETUP_MS = 10_000;

/**
 * Waits for something every listener has to have done, and voids the run if one has not.
 *
 * Not a `checks.record`, though both of these once were. `Checks` is for claims about the
 * thing under test, and everything this script claims is about audio — what the transport
 * carried. Whether the clients finished joining and subscribing is a precondition: if it
 * does not hold there is no answer to give, and speaking anyway produces a short reading
 * that the checks below would attribute to the transport. That is an answer-shaped void —
 * a number with the exact shape of a measurement and a different meaning — and it is the
 * specific false negative the publish-wait-speak split exists to prevent, so recording it
 * and continuing is worse than not checking: it prints FAIL on one line and manufactures
 * evidence for the wrong conclusion on the next.
 *
 * [LAW:one-type-per-behavior] One helper for both waits: they differ in a predicate and a
 * name, which are values, not in what happens when they fail.
 */
async function required(listeners, predicate, what) {
  const met = await Promise.all(
    // The identity goes into `what` as well as into the message below, because `waitFor`
    // prints its own "gave up" line before this throw is ever reached — and a run that
    // says which ear gave up, then which ear never got there, needs no second run.
    listeners.map((listener) =>
      listener.waitFor(() => predicate(listener), SETUP_MS, `${listener.identity}: ${what}`),
    ),
  );

  const missed = listeners.filter((_, index) => !met[index]);
  if (missed.length === 0) return;

  throw new Error(
    `waited ${SETUP_MS / 1000}s for ${what} and ${missed.map((l) => l.identity).join(", ")} ` +
      `never got there — stopping, because every number this script would print from here ` +
      `on would be about that and not about the transport`,
  );
}

/**
 * The measuring stick, checked before anything is measured against it.
 *
 * Every check below is a ratio against this recording, so a recording with nothing in it
 * does not fail the run — it certifies it. `ENOUGH` becomes `ceil(0 * 0.9)` = 0, and
 * `heard.audibleFrames >= 0` holds for a listener that received not one frame; `heard.peak
 * / published.peak` becomes `Infinity`, which clears `QUIETEST_ACCEPTABLE` the moment Opus
 * dithers a single non-zero sample out of silence. Quiet TTS output or a wrong voice on a
 * runner is enough to get there.
 *
 * That is this script's own documented failure — an answer-shaped void, a green run whose
 * numbers mean something other than what they appear to — arriving through the instrument
 * rather than through the transport. So it is refused here, loudly, at the one place the
 * recording becomes a thing worth comparing against. [LAW:parse-dont-validate]
 */
function audible(published) {
  if (published.peak > 0 && published.audibleFrames > 0) return published;

  throw new Error(
    `the recording to measure against is silent (peak ${published.peak}, ` +
      `${millis(published.audibleFrames)} ms audible) — stopping, because every check below ` +
      `is a ratio against it and would pass for a listener that heard nothing at all`,
  );
}

/**
 * The one boundary: everything below runs on values known to exist.
 *
 * The credential check is `livekitCredentials`, which owns both the check and the census of
 * who shares it — one fact, one home, and no list here to fall out of date. The URL is this
 * script's own: it dials as a client, so it defaults to `wss://` and strips a trailing
 * slash, neither of which a room-service-only caller wants.
 */
function readConfig(env, argv) {
  return {
    ...livekitCredentials(env),
    livekitUrl: (argv[2] ?? "wss://livekit.sanctuary.gdn").replace(/\/$/, ""),
  };
}

const { apiKey, apiSecret, livekitUrl } = readConfig(process.env, process.argv);
const checks = new Checks();

// Recorded before the room exists, not because the order reads better but because
// `recordSpeech` shells out to `say`, which throws on a non-zero exit or a missing binary
// — on any non-macOS runner, every time. Anything that can throw between `CreateRoom` and
// the `try` below leaks a room on the real deployment, so the region between them is kept
// empty rather than kept safe. [LAW:no-ambient-temporal-coupling]
const recording = recordSpeech(SPOKEN, join(mkdtempSync(join(tmpdir(), "openconv-loopback-")), "speech.wav"));
const published = audible(sounding(recording.samples, recording.sampleRate));

/// The audible duration a listener has to reach. One number for the wait and the check,
/// so the wait ends the moment the run is healthy rather than at the settle window.
const ENOUGH = Math.ceil(published.audibleFrames * SHORTEST_ACCEPTABLE);

// Created rather than joined: the deployment runs with room.auto_create off, and a
// conversation room is capped at two participants — this one is ours, so it can be
// whatever shape the measurement needs.
const rooms = new Rooms({ url: livekitUrl, apiKey, apiSecret });
const name = `probe_${randomUUID()}`;

/// The identity travels with the client rather than being recovered later by zipping this
/// array against `LISTENERS` by index — two parallel arrays that stay aligned only as long
/// as nobody reorders one, in a script whose whole point is saying which ear lost the
/// audio. [LAW:one-source-of-truth]
const at = async (identity) =>
  Object.assign(
    await Caller.at(livekitUrl, joinToken({ apiKey, apiSecret, room: name, identity })),
    { identity },
  );

const clients = [];
let listeners = [];
/// Hoisted beside `listeners` and for the same reason: the report below runs after the
/// cleanup block, outside the scope the clients are created in.
let publisher = null;

// The last statement before the `try`, deliberately: from here the room exists and is this
// script's to clean up, and everything that can throw — three connections with their own
// timeouts, every wait — sits inside, because a failure that skipped the cleanup would
// leave connected participants in a room on the real deployment, and a room with clients in
// it is not empty, so `empty_timeout` does not start counting until the process dies.
await rooms.call("CreateRoom", { name, empty_timeout: 120, max_participants: 8 });
try {
  console.log(`created ${name} at ${livekitUrl}\n`);

  // The listeners first. Publishing waits for a remote subscription before it captures a
  // frame, and a publisher alone in a room would wait for a subscriber who never arrives.
  listeners = await Promise.all(LISTENERS.map(at));
  clients.push(...listeners);
  publisher = await at(SPEAKER);
  clients.push(publisher);

  await required(listeners, (listener) => listener.roster().includes(SPEAKER), `${SPEAKER} to join`);

  console.log(
    `\nspeaking ${millis(published.frames)} ms, of which ${millis(published.audibleFrames)} ms audible, peak ${published.peak}\n`,
  );

  // Published, then gated, then spoken — three steps rather than `speak()`'s one, because
  // `microphone()` resolves on the *first* remote subscription and this room has two. A
  // listener still subscribing while the lead-in plays loses the front of the utterance,
  // which is this probe's own symptom wearing the costume of the bug it hunts.
  const mic = await publisher.microphone(recording.sampleRate);
  await required(listeners, (listener) => listener.subscribed(), "a track to listen to");
  console.log(`  ${listeners.length}/${listeners.length} listeners subscribed before a word was spoken\n`);

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

  // Taken here rather than in the report below, because `leave()` tears the peer
  // connection down and takes its stats with it. Stored per client and rendered later for
  // the same reason `heard.error` is kept: a run whose stats could not be read must still
  // print what it heard, since on a failing run that is the reason the run happened.
  await Promise.all(
    clients.map(async (client) => {
      client.delivery = await client.delivered().then(
        (reading) => ({ reading, error: null }),
        (error) => ({ reading: null, error }),
      );
    }),
  );
} finally {
  // Reported rather than swallowed: a client that could not leave is a finding about the
  // room, and letting it pass silently here would hide it behind whatever sent us into
  // this block. `allSettled` so one bad disconnect cannot skip the delete.
  const left = await Promise.allSettled(clients.map((client) => client.leave()));
  for (const outcome of left.filter((outcome) => outcome.status === "rejected")) {
    console.error(`  (a client failed to leave ${name}: ${outcome.reason})`);
  }

  // Loud on stderr but not fatal. A throw from inside `finally` replaces the run's whole
  // report — every check already recorded — with a stack trace about cleanup, and on a
  // failing run that report is the reason the run happened. A leaked room is worth
  // shouting about; it is not worth the findings. An error from the try block still
  // propagates past this, so a real cause is never masked by cleanup noise.
  try {
    await rooms.call("DeleteRoom", { room: name });
  } catch (error) {
    console.error(`  (${name} was left behind on the SFU: ${error.message})`);
  }
}

/**
 * The path one end of the call chose, from the transport's own selected pair.
 *
 * The line openconv-openconv-bwy.28 was filed to get. Its hypothesis is a fallback to
 * ICE/TCP on 7881 under a jittery link, and nothing in the audio can confirm or refute
 * that — a run that took the TCP path and a run that did not produce the same silence.
 * Printed on every run rather than only on failures, because the claim being tested is a
 * correlation and a reading taken only when things break has nothing to correlate against.
 *
 * `dtls` is on every line for that same reason rather than appearing only when it is
 * unhealthy. A transport can hold a selected pair with ICE connected while the DTLS
 * handshake is still unfinished — a state that carries no media and otherwise renders as an
 * ordinary success line, leaving `rtp: 0 packets` as the only hint and pointing the reader
 * at the network instead of at the handshake. Shown always, a reader can also tell a
 * completed handshake from a build that does not report one.
 */
const pathLine = (transport) =>
  transport.selected === null
    ? `  path: ICE never settled on a pair (${transport.iceState})`
    : `  path: ${transport.selected.protocol} ` +
      `${transport.selected.local.address}:${transport.selected.local.port} -> ` +
      `${transport.selected.remote.address}:${transport.selected.remote.port} ` +
      `(${transport.selected.local.type}/${transport.selected.remote.type}, ` +
      `rtt ${transport.selected.rttMs.toFixed(1)} ms, ` +
      `${transport.pairChanges} pair change${transport.pairChanges === 1 ? "" : "s"}, ` +
      `dtls ${transport.dtlsState})`;

/**
 * What arrived over that path, as the decoder counted it.
 *
 * Three causes of a silent track, separated: packets the network never delivered
 * (`lost`), samples the jitter buffer invented to cover for them (`concealed`), and a
 * stream that arrived whole and still decoded to nothing — which is both counters at zero
 * beside a `level` of one least significant bit, and puts the silence upstream of this
 * client altogether.
 *
 * Read `packets` first, and read the others only against a local `livekit-server --dev`
 * run for scale. Measured over six clean local runs, a listener that received the whole
 * utterance perfectly still reported ~214 packets with ~200 of them `discarded` and
 * `concealed` anywhere from 0 to 2506 samples — so neither of those numbers is a fault on
 * its own, and a reader who arrives at a failing run and seizes on "200 discarded" is
 * reading the healthy baseline. The count that separates the two populations is `packets`
 * itself: ~214 local and ~240 on a healthy deployed listener, against 0-23 on a failing
 * one, with `lost` at zero on both sides of the split.
 */
const rtpLine = (audio) =>
  `  rtp:  ${audio.packetsReceived} packets, ${audio.packetsLost} lost, ` +
  `${audio.packetsDiscarded} discarded, jitter ${audio.jitterMs.toFixed(1)} ms, ` +
  `concealed ${audio.concealedSamples} samples in ${audio.concealmentEvents} events, ` +
  `level ${audio.audioLevel.toExponential(2)}`;

/// A connection carrying no inbound audio stream at all, said out loud. Mapping an empty
/// list prints nothing, and nothing is what a healthy publisher prints too — so silence
/// here would read as "no inbound audio to report" on the one client whose inbound audio is
/// the entire question. Observed on a real failing run before it was written down.
const rtpLines = (reading) =>
  reading.inboundAudio.length > 0
    ? reading.inboundAudio.map(rtpLine)
    : ["  rtp:  this connection reported no inbound audio stream"];

/// Both arms print. A client whose stats could not be read says so on the same line the
/// reading would have used, rather than leaving a gap a reader has to notice.
const account = ({ reading, error }) =>
  error === null
    ? reading.transports.map(pathLine)
    : [`  path: no account of this call — ${error.message}`];

/// Split from `account` rather than selected inside it by a flag: the publisher subscribes
/// to nothing, so its report is the path alone, and a listener's is the path and what came
/// over it. Two callers composing two renderers, not one renderer asking who called it.
const received = ({ reading, error }) => (error === null ? rtpLines(reading) : []);

console.log();
for (const listener of listeners) {
  const heard = listener.heard;
  const who = listener.identity;
  console.log(
    `${who}: heard ${millis(heard.frames)} ms, of which ${millis(heard.audibleFrames)} ms audible, ` +
      `peak ${heard.peak}, SFU called the speaker ${listener.reportedSpeaking.has(SPEAKER) ? "loud" : "silent"}`,
  );
  for (const line of [...account(listener.delivery), ...received(listener.delivery)]) {
    console.log(line);
  }
}

// The publisher's own path too: an utterance can be lost on the way *into* the SFU as
// easily as on the way out, and the two are different bugs.
console.log(`\n${SPEAKER}:`);
for (const line of account(publisher.delivery)) console.log(line);
console.log();

for (const listener of listeners) {
  const heard = listener.heard;
  const who = listener.identity;

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
