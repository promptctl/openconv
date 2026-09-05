// What `Caller` promises about openconv's control events, checked without a room.
//
//   NODE_PATH=/path/to/node_modules node --test scripts/lib/caller.test.mjs
//
// `NODE_PATH` for the same reason every script here needs it: this imports caller.mjs,
// which imports @livekit/rtc-node. Nothing else is required — no runner, no package.json,
// no network, no LiveKit deployment. The accessors are pure functions of an array, which
// is exactly why the part of this module that decides whether a run crashes or reports
// can be pinned here rather than argued about over a live call.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Caller, delivery, millis, readRecording, sounding, transportOf } from "./caller.mjs";
import { NotTold } from "../../web/conversation.js";

/// A caller that has "received" these events, with no room behind it. The accessors read
/// `controlEvents` and nothing else, so this is the whole of their input.
const heard = (controlEvents) => Object.assign(Object.create(Caller.prototype), { controlEvents });

/// The helpers each acceptance script used to carry, before they were folded onto the
/// shared client. Kept here as the thing the accessors must still agree with: the refactor
/// claimed to change where this logic lives, not what it does.
const inlineTranscripts = (events) =>
  events
    .filter((event) => event.type === "user_transcript")
    .map((event) => event.user_transcription_event?.user_transcript ?? "");

const inlineReplies = (events) =>
  events
    .filter((event) => event.type === "agent_response")
    .map((event) => event.agent_response_event?.agent_response ?? "");

/// Arrival order interleaved, with the tentative/settled distinction, a repeat of a
/// once-only event, and the `<not json>` frame the data-channel handler keeps rather than
/// drops — because that is the shape a real call leaves in the array.
const CALL = [
  {
    type: "conversation_initiation_metadata",
    conversation_initiation_metadata_event: { conversation_id: "conv_1" },
  },
  { type: "tentative_user_transcript", user_transcription_event: { user_transcript: "please rep" } },
  { type: "vad_score", vad_score_event: { vad_score: 0.9 } },
  {
    type: "user_transcript",
    user_transcription_event: { user_transcript: "Please reply with the word cactus.", event_id: 4 },
  },
  { type: "agent_response", agent_response_event: { agent_response: "Cactus" } },
  { type: "<not json>", raw: "garbage" },
  { type: "user_transcript", user_transcription_event: { user_transcript: "thanks", event_id: 7 } },
  { type: "agent_response", agent_response_event: { agent_response: "You're welcome." } },
  {
    type: "conversation_initiation_metadata",
    conversation_initiation_metadata_event: { conversation_id: "conv_LATER" },
  },
];

test("the accessors agree with the helpers they replaced", () => {
  const caller = heard(CALL);
  assert.deepEqual(caller.transcripts(), inlineTranscripts(CALL));
  assert.deepEqual(caller.replies(), inlineReplies(CALL));
});

test("events selects by type in arrival order, control takes the first", () => {
  const caller = heard(CALL);
  assert.deepEqual(caller.events("user_transcript").length, 2);
  assert.deepEqual(caller.events("nothing_like_this"), []);
  assert.equal(
    caller.control("conversation_initiation_metadata").conversation_initiation_metadata_event
      .conversation_id,
    "conv_1",
  );
});

test("transcript payloads arrive unparsed, so a missing event_id stays reportable", () => {
  // stt-acceptance exists to report an id that never came. If this accessor refused one,
  // it would crash the script that came to ask the question instead of answering it.
  const caller = heard([{ type: "user_transcript", user_transcription_event: { event_id: 4 } }]);
  assert.equal(caller.transcriptEvents().at(-1).event_id, 4);
  assert.equal(heard([{ type: "user_transcript", user_transcription_event: {} }]).transcriptEvents().at(-1).event_id, undefined);
});

test("a transcript of silence is an answer, not a fault", () => {
  // The case that decides the whole design: a caller who said nothing settles as "", and
  // collapsing that into the malformed arm would destroy the distinction from the other
  // side just as surely as laundering a malformed event into "" destroys it from this one.
  assert.deepEqual(
    heard([{ type: "user_transcript", user_transcription_event: { user_transcript: "" } }]).transcripts(),
    [""],
  );
  assert.deepEqual(
    heard([{ type: "agent_response", agent_response_event: { agent_response: "" } }]).replies(),
    [""],
  );
});

test("a malformed event is named, never laundered into an empty string", () => {
  const malformed = [
    ["the wrapper is absent", { type: "user_transcript" }, "user_transcript"],
    [
      "the leaf is absent",
      { type: "user_transcript", user_transcription_event: { event_id: 4 } },
      "user_transcript",
    ],
    [
      "the leaf is null",
      { type: "user_transcript", user_transcription_event: { user_transcript: null } },
      "user_transcript",
    ],
    ["the reply wrapper is absent", { type: "agent_response" }, "agent_response"],
    [
      "the reply leaf is absent",
      { type: "agent_response", agent_response_event: {} },
      "agent_response",
    ],
  ];

  for (const [what, event, field] of malformed) {
    const caller = heard([event]);
    const read = () => (field === "user_transcript" ? caller.transcripts() : caller.replies());

    // The old helpers turned every one of these into a transcript of silence, which is
    // the bug: a protocol failure arrived looking exactly like a quiet caller.
    assert.deepEqual(
      field === "user_transcript" ? inlineTranscripts([event]) : inlineReplies([event]),
      [""],
      `the helper being replaced swallowed ${what}`,
    );

    assert.throws(
      read,
      (error) => error instanceof TypeError && error.message.includes(field),
      `${what} must be named, not swallowed`,
    );
  }
});

/// A run of `samples` at a given amplitude, in whole 10 ms windows.
const at = (amplitude, windows, sampleRate = 48_000) =>
  Int16Array.from({ length: (sampleRate / 100) * windows }, () => amplitude);

const concat = (...runs) => {
  const out = new Int16Array(runs.reduce((total, run) => total + run.length, 0));
  runs.reduce((offset, run) => (out.set(run, offset), offset + run.length), 0);
  return out;
};

test("silence and sound are told apart by amplitude, not by length", () => {
  assert.deepEqual(sounding(at(0, 100), 48_000), { frames: 100, audibleFrames: 0, peak: 0 });
  assert.deepEqual(sounding(at(19838, 100), 48_000), {
    frames: 100,
    audibleFrames: 100,
    peak: 19838,
  });
});

test("audibleFrames is a duration, so a track that stopped partway says so", () => {
  // The symptom openconv-openconv-bwy.26 is filed on: a full-length track carrying the
  // front of an utterance and then nothing. A peak taken across the whole run is 19838
  // either way, so only the windowed count can see it.
  const cutOff = concat(at(19838, 30), at(0, 70));
  const whole = at(19838, 100);

  assert.equal(sounding(cutOff, 48_000).peak, sounding(whole, 48_000).peak);
  assert.equal(sounding(cutOff, 48_000).frames, sounding(whole, 48_000).frames);
  assert.equal(sounding(cutOff, 48_000).audibleFrames, 30);
});

test("one least significant bit is silence, not sound", () => {
  // What the agent logs as loudest=3.05e-5 and this probe as peak 1. A reading that
  // called it audible would report the exact failure being chased as a healthy call.
  assert.equal(sounding(at(1, 100), 48_000).audibleFrames, 0);
  assert.equal(sounding(at(1, 100), 48_000).peak, 1);
});

test("the AUDIBLE threshold is exclusive: 1000 is silence, 1001 is sound", () => {
  // Every number this investigation turns on is denominated in this threshold, and the
  // comparison is a strict `peak > AUDIBLE`. The cases above — 0, 1, 19838 — are all far
  // enough from 1000 that `>` and `>=` are indistinguishable to them, so nothing pinned
  // which one it was. Flipping the comparison would move the boundary by one bit and stay
  // green everywhere else in this file.
  assert.equal(sounding(at(1000, 100), 48_000).audibleFrames, 0, "exactly at the threshold is silence");
  assert.equal(sounding(at(1001, 100), 48_000).audibleFrames, 100, "one above it is sound");

  // The peak is reported either way — the threshold decides what counts as an audible
  // *window*, never what the loudest sample was.
  assert.equal(sounding(at(1000, 100), 48_000).peak, 1000);
});

test("a sample rate that does not divide into whole windows still measures the sound", () => {
  // 22 050 / 100 is 220.5. A fractional stride walks off the end of the array into
  // `undefined`, and Math.abs(undefined) is NaN — a peak that compares false against
  // every threshold, reporting a loud recording as silent.
  const reading = sounding(at(19838, 50, 22_050), 22_050);

  assert.ok(Number.isFinite(reading.peak), `peak was ${reading.peak}`);
  assert.equal(reading.peak, 19838);
  assert.equal(reading.audibleFrames, reading.frames);
});

test("a rate too low to fill a window is refused, not looped over forever", () => {
  // The worst failure mode this module could have: `perFrame` of 0 makes the window loop
  // step by zero and spin, and a hang reaches no log at all — quieter than any wrong
  // number. `readRecording` refuses such a rate at the parse, but a frame off the SFU has
  // no parser of ours in front of it, so the refusal is here too.
  for (const rate of [0, -48_000, 4]) {
    assert.throws(
      () => sounding(at(19838, 1), rate),
      (error) => error instanceof RangeError && error.message.includes(String(rate)),
      `a ${rate} Hz rate must be named, not spun on`,
    );
  }
});

test("millis is the one place a frame becomes a duration", () => {
  assert.equal(millis(100), 1000);
  assert.equal(millis(0), 0);
  // What the scripts actually ask it: 200 ms of sound is 20 frames, and the comparison
  // reads as the duration it is rather than as a count divided by a literal.
  assert.ok(millis(20) >= 200);
  assert.ok(millis(19) < 200);
});

/// A minimal RIFF/WAVE file, built rather than checked in so a test can say which field it
/// is corrupting. Defaults are what `say -o --data-format LEI16@48000` produces.
const wav = ({ sampleRate = 48_000, channels = 1, bitsPerSample = 16, encoding = 1 } = {}) => {
  const samples = Int16Array.from([0, 19838, -19838, 0]);
  const file = Buffer.alloc(44 + samples.byteLength);
  file.write("RIFF", 0);
  file.writeUInt32LE(36 + samples.byteLength, 4);
  file.write("WAVE", 8);
  file.write("fmt ", 12);
  file.writeUInt32LE(16, 16);
  file.writeUInt16LE(encoding, 20);
  file.writeUInt16LE(channels, 22);
  file.writeUInt32LE(sampleRate, 24);
  file.writeUInt32LE((sampleRate * channels * bitsPerSample) / 8, 28);
  file.writeUInt16LE((channels * bitsPerSample) / 8, 32);
  file.writeUInt16LE(bitsPerSample, 34);
  file.write("data", 36);
  file.writeUInt32LE(samples.byteLength, 40);
  Buffer.from(samples.buffer).copy(file, 44);

  const path = join(mkdtempSync(join(tmpdir(), "openconv-wav-")), "built.wav");
  writeFileSync(path, file);
  return path;
};

test("a recording that declares no sample rate is refused at the parse", () => {
  // The boundary half of the zero-rate fix, and the half that matters more: `sounding`
  // refuses such a rate too, but this is the parser whose output nothing downstream
  // re-checks, so a zero surviving here is a zero that reaches a window loop stepping by
  // zero — a hang rather than a wrong answer.
  assert.throws(
    () => readRecording(wav({ sampleRate: 0 })),
    (error) => error.message.includes("0 Hz"),
    "a zero sample rate must be named, not passed through",
  );

  // And the same builder parses when only that field is sound, so the test above cannot
  // be passing by refusing every WAV it is handed.
  const good = readRecording(wav());
  assert.equal(good.sampleRate, 48_000);
  assert.equal(good.samples.length, 4);
  assert.equal(sounding(good.samples, good.sampleRate).peak, 19838);
});

/// Real `getRtcStats().toJson()` output, captured from a listener on a call to
/// livekit.sanctuary.gdn and trimmed to the entries `delivery` reads. Transcribed rather
/// than invented because the shape is the whole difficulty: 64-bit counters arrive as
/// strings, proto2 optionals vanish when unset, and every entry is a one-key variant. A
/// hand-written approximation of that would test the approximation.
const stats = ({
  candidate = {},
  inbound = {},
  received = {},
  pair = {},
  transport = {},
  extra = [],
  arrival = "publisherStats",
} = {}) => {
  const entries = [
    {
      transport: {
        rtc: { id: "T01", timestamp: "1788132842843992" },
        transport: {
          iceState: "ICE_TRANSPORT_CONNECTED",
          dtlsState: "DTLS_TRANSPORT_CONNECTED",
          selectedCandidatePairId: "CP+xEdxmt3_zNHUDiTQ",
          selectedCandidatePairChanges: 1,
          ...transport,
        },
      },
    },
    {
      candidatePair: {
        rtc: { id: "CP+xEdxmt3_zNHUDiTQ", timestamp: "1788132842843992" },
        candidatePair: {
          transportId: "T01",
          localCandidateId: "I+xEdxmt3",
          remoteCandidateId: "IzNHUDiTQ",
          state: "PAIR_SUCCEEDED",
          nominated: true,
          bytesReceived: "2938",
          currentRoundTripTime: 0.027,
          ...pair,
        },
      },
    },
    {
      localCandidate: {
        rtc: { id: "I+xEdxmt3", timestamp: "1788132842843992" },
        candidate: {
          address: "192.168.7.189",
          port: 54_647,
          protocol: "udp",
          candidateType: "HOST",
          ...candidate,
        },
      },
    },
    {
      remoteCandidate: {
        rtc: { id: "IzNHUDiTQ", timestamp: "1788132842843992" },
        candidate: { address: "192.168.7.208", port: 7882, protocol: "udp", candidateType: "HOST" },
      },
    },
    {
      inboundRtp: {
        rtc: { id: "IT01A152043745", timestamp: "1788132842843992" },
        stream: { ssrc: 152_043_745, kind: "audio", transportId: "T01" },
        received: { packetsReceived: "16", packetsLost: "0", jitter: 0.011, ...received },
        inbound: {
          packetsDiscarded: "7",
          concealedSamples: "0",
          silentConcealedSamples: "0",
          concealmentEvents: "0",
          totalSamplesReceived: "95520",
          audioLevel: 6.103701895199438e-5,
          ...inbound,
        },
      },
    },
    ...extra,
  ];
  return { publisherStats: [], subscriberStats: [], [arrival]: entries };
};

test("delivery names the path the media actually took", () => {
  const [transport] = delivery(stats()).transports;

  // The reading openconv-openconv-bwy.28 was filed to get: UDP on 7882, not a fallback to
  // ICE/TCP on 7881. The whole point is that it is per-run, so it is read from the pair
  // the transport itself selected rather than from whichever pair happens to look best.
  assert.equal(transport.selected.protocol, "udp");
  assert.equal(transport.selected.remote.port, 7882);
  assert.equal(transport.selected.remote.address, "192.168.7.208");
  // Both ends' candidate types, because the report prints them as a pair — `HOST/HOST` at
  // loopback-acceptance.mjs:278 — and asserting only the local one lets a typo render the
  // remote half as `undefined` in the line this probe exists to produce.
  assert.equal(transport.selected.local.type, "HOST");
  assert.equal(transport.selected.remote.type, "HOST");
  assert.equal(transport.pairChanges, 1);

  // Both of the transport's own reported states, not the ICE one alone: a path can be ICE
  // connected with DTLS still handshaking, and a reading that showed only the first would
  // call that settled.
  assert.equal(transport.iceState, "ICE_TRANSPORT_CONNECTED");
  assert.equal(transport.dtlsState, "DTLS_TRANSPORT_CONNECTED");
});

test("seconds on the wire are milliseconds in the reading", () => {
  // Two fields cross this boundary — the selected pair's round trip and the jitter buffer's
  // delay — and they are one contract rather than two coincidences, so they are asserted
  // together: a reader comparing either against the ICMP figures on the ticket must not be
  // converting in their head. Both are driven nonzero deliberately. Zero is the one input
  // that survives every way of getting the conversion wrong — a dropped `* 1000`, an
  // inverted one, a read of the wrong field — so a suite that only ever asserts the zero
  // case is not testing the conversion at all.
  const reading = delivery(stats());

  assert.equal(Math.round(reading.transports[0].selected.rttMs), 27);
  assert.equal(Math.round(reading.inboundAudio[0].jitterMs), 11);
});

test("a TCP fallback is visible as one, which is the question being asked", () => {
  // The hypothesis .28 records: TCP head-of-line blocking under a jittery link. A run that
  // took that path has to be distinguishable from one that did not, or the correlation the
  // probe exists to draw cannot be drawn.
  const fell = delivery(
    stats({ candidate: { protocol: "tcp", port: 7881, tcpType: "TCP_CANDIDATE_TYPE_PASSIVE" } }),
  );
  assert.equal(fell.transports[0].selected.protocol, "tcp");
  assert.equal(fell.transports[0].selected.local.tcpType, "TCP_CANDIDATE_TYPE_PASSIVE");

  // And absent on the UDP run rather than reported as some falsy stand-in, so presence
  // alone answers the question.
  assert.equal(delivery(stats()).transports[0].selected.local.tcpType, undefined);
});

test("counters are numbers, not the strings the wire carries", () => {
  // 64-bit fields arrive from protobuf as strings, where "0" is truthy and "10" < "9".
  // A comparison against a threshold would silently do the wrong thing on every one.
  const reading = delivery(
    stats({
      inbound: {
        concealedSamples: "48000",
        silentConcealedSamples: "31000",
        concealmentEvents: "12",
      },
    }),
  );
  const [audio] = reading.inboundAudio;

  assert.strictEqual(audio.concealedSamples, 48_000);
  // Distinct from the concealed total above, so a read of the wrong neighbouring field
  // fails here rather than passing on a number that happens to match.
  assert.strictEqual(audio.silentConcealedSamples, 31_000);
  assert.strictEqual(audio.concealmentEvents, 12);
  assert.strictEqual(audio.packetsReceived, 16);
  assert.strictEqual(audio.packetsLost, 0);
  assert.strictEqual(audio.packetsDiscarded, 7);
  assert.strictEqual(audio.samplesReceived, 95_520);

  // Same wire representation on the other side of the reading: the selected pair's byte
  // count arrives as a 64-bit string too, and nothing would notice it staying one until a
  // threshold comparison quietly went lexicographic.
  assert.strictEqual(reading.transports[0].selected.bytesReceived, 2938);

  // The signature this whole line of investigation keeps finding: a track that arrived
  // complete and decoded to one or two least significant bits of an i16.
  assert.ok(audio.audioLevel < 1e-4);
});

test("stats with no transport are refused rather than summarized", () => {
  // The failure mode this parser exists to prevent. Summarizing an unmeasured call yields
  // zero loss and zero concealment — indistinguishable, in the report, from a flawless one.
  assert.throws(
    () => delivery({ publisherStats: [], subscriberStats: [] }),
    (error) => error.message.includes("no transport"),
    "a reading with no connection behind it must not be reported as a clean one",
  );
});

test("a transport that has not settled on a pair says so", () => {
  // A real state of a connecting transport, and not one to fill in with a pair-shaped
  // default that would name a path the media never took.
  const unsettled = delivery(stats({ transport: { selectedCandidatePairId: "" } }));
  assert.equal(unsettled.transports[0].selected, null);
});

test("stats are read wherever rtc-node files them", () => {
  // Observed on @livekit/rtc-node 0.13: a client that only subscribes reports its inbound
  // audio under `publisherStats` and leaves `subscriberStats` empty. Reading one array by
  // the client's role would go blind on exactly the listener being measured, so both are
  // read and neither is chosen between.
  assert.deepEqual(delivery(stats({ arrival: "subscriberStats" })), delivery(stats()));
});

test("two peer connections that reuse an id do not overwrite each other", () => {
  // Ids in these stats are assigned per RTCPeerConnection, so a client that both publishes
  // and subscribes has two connections each naming their first transport `T01` and their
  // first pair the same string. Indexed into one map, the second silently overwrites the
  // first and a transport resolves against the wrong connection's candidates — a corrupted
  // path reported with total confidence, which is worse than no path at all.
  const publisher = stats({ candidate: { address: "10.0.0.1", port: 1111 } });
  const subscriber = stats({ candidate: { address: "10.0.0.2", port: 2222 } });
  const both = delivery({
    publisherStats: publisher.publisherStats,
    subscriberStats: subscriber.publisherStats,
  });

  assert.equal(both.transports.length, 2, "both connections must be reported");
  assert.deepEqual(
    both.transports.map((transport) => transport.selected.local.address),
    ["10.0.0.1", "10.0.0.2"],
    "each transport keeps its own connection's candidates",
  );
  assert.deepEqual(
    both.transports.map((transport) => transport.selected.local.port),
    [1111, 2222],
  );
  assert.equal(both.inboundAudio.length, 2);
});

test("a non-audio stream is excluded from the audio reading", () => {
  // The filter is only meaningful against something that must be excluded. With every
  // fixture supplying audio alone, a typo in the field name or the literal would pass the
  // whole suite while letting video RTP into an audio-only reading.
  const withVideo = delivery(
    stats({
      extra: [
        {
          inboundRtp: {
            rtc: { id: "IT01V900", timestamp: "1788132842843992" },
            stream: { ssrc: 900, kind: "video", transportId: "T01" },
            received: { packetsReceived: "5000", packetsLost: "3", jitter: 0.02 },
            inbound: {
              packetsDiscarded: "0",
              concealedSamples: "0",
              silentConcealedSamples: "0",
              concealmentEvents: "0",
              totalSamplesReceived: "0",
              audioLevel: 0,
            },
          },
        },
      ],
    }),
  );

  assert.equal(withVideo.inboundAudio.length, 1);
  assert.equal(withVideo.inboundAudio[0].ssrc, 152_043_745);
});

test("counters that are legitimately zero render as zero, never NaN or undefined", () => {
  // These stats are proto2 and every field read here is declared `required`, so a zero
  // arrives as an explicit zero rather than vanishing the way a proto3 scalar would at its
  // default. That is a claim about the schema, and this is where it stops being a claim: a
  // guard against absence would be defending a state the wire cannot produce, while a
  // silent NaN would corrupt the diagnostic this parser exists to print.
  const quiet = delivery(
    stats({
      received: { jitter: 0 },
      pair: { currentRoundTripTime: 0 },
      transport: { selectedCandidatePairChanges: 0 },
      inbound: { audioLevel: 0 },
    }),
  );

  assert.strictEqual(quiet.transports[0].selected.rttMs, 0);
  assert.strictEqual(quiet.transports[0].pairChanges, 0);
  assert.strictEqual(quiet.inboundAudio[0].jitterMs, 0);
  assert.strictEqual(quiet.inboundAudio[0].audioLevel, 0);

  // And the rendering those feed, which is where a NaN would actually be seen: every one
  // of these formats to a number a reader can act on.
  assert.equal(quiet.transports[0].selected.rttMs.toFixed(1), "0.0");
  assert.equal(quiet.inboundAudio[0].jitterMs.toFixed(1), "0.0");
  assert.equal(quiet.inboundAudio[0].audioLevel.toExponential(2), "0.00e+0");
});

/** A room that records what it was asked to do rather than reaching an SFU. */
const roomOf = (identities) => {
  const published = [];

  return {
    published,
    remoteParticipants: new Map(identities.map((identity) => [identity, { identity }])),
    localParticipant: {
      publishData: async (payload, options) =>
        published.push({ message: JSON.parse(new TextDecoder().decode(payload)), options }),
    },
  };
};

test("the roster is read as identities, which is what the handshake matches on", () => {
  // `remoteParticipants` is keyed by identity and valued by participant objects. Handing
  // over the values would make every identity `undefined`, no participant would look like
  // an agent, and the conversation would be configured for nobody — in silence. The script
  // would then assert against an agent left on the deployment default and blame the agent.
  const room = roomOf(["agent_one", "u_acceptance"]);

  assert.deepEqual(transportOf(room, "ws://sfu").participants(), ["agent_one", "u_acceptance"]);
});

test("a control message goes out reliably, because a dropped one is never noticed", async () => {
  // The data channel's unreliable mode is lossy by design. A configuration lost that way
  // leaves the agent on the deployment default with every script believing it was told —
  // and a script that believes it configured a voice will report the wrong thing failing.
  const room = roomOf([]);

  await transportOf(room, "ws://sfu").publishBytes(new TextEncoder().encode('{"type":"x"}'));

  assert.equal(room.published.length, 1);
  assert.deepEqual(room.published[0].message, { type: "x" });
  assert.equal(room.published[0].options.reliable, true);
});

/// A caller whose agent appears only after `agentAfterMs`, holding `configuring` as the
/// record of the publishes it is waiting on.
///
/// `agentConfigured` reads exactly two things — the roster, through the transport, and the
/// publish attempts recorded on `configuring` — so a roster that answers on a timer and a
/// list of attempts are the whole of its input. A configure that never settles is the
/// default because it is the case with no other way to reach it; passing settled ones is
/// what the ordinary path looks like. No room is constructed and nothing reaches an SFU.
const callerAwaiting = (agentAfterMs, configuring = [new Promise(() => {})]) => {
  const start = Date.now();

  return Object.assign(Object.create(Caller.prototype), {
    participants: [],
    configuring,
    transport: {
      participants: () => (Date.now() - start >= agentAfterMs ? ["agent_late"] : []),
    },
  });
};

test("an agent present and told inside the budget is what being configured means", async () => {
  // The other `agentConfigured` test seeds a configure that never settles, so it can only
  // ever reach the rejection branch. The path all seven acceptance scripts wait on — agent
  // in the room, every publish settled, inside the budget — had nothing asserting it at
  // all, and a regression that made it hang, reject, or answer `false` would have left
  // this suite green. [LAW:behavior-not-structure]
  const caller = callerAwaiting(0, [Promise.resolve()]);

  assert.equal(await caller.agentConfigured(500), true);
});

test("a configure that never reached the agent ends the run rather than keeping the call", async () => {
  // `web/caller.js` catches this exact type to keep a working call. A script must not: the
  // conversation it would keep is one the agent was never told about, so every assertion
  // after it measures an agent still on the deployment default — and reports green. The
  // open-time sweep is not recorded in `configuring` (only arrivals are), so `agentConfigured`
  // would not catch it either. This is the only thing holding this side to its half.
  const caller = Object.assign(Object.create(Caller.prototype), {
    conversation: {
      open: async () => {
        throw new NotTold("conv_kept", new Error("agent_one could not be told what this conversation is"));
      },
    },
  });

  await assert.rejects(
    () => caller.open({}),
    (failure) => {
      assert.ok(failure instanceof NotTold, `kept the call and threw ${failure.name} instead`);
      assert.equal(failure.conversationId, "conv_kept");
      return true;
    },
  );
  assert.equal(caller.conversationId, undefined, "a call that was never told recorded no conversation");
});

test("the budget bounds the whole wait, not each wait inside it", async () => {
  // Two waits run in sequence in `agentConfigured`, and each used to be given the caller's
  // full argument. An agent arriving late in the window then handed the configure a fresh
  // budget, so a script that asked for 25 seconds could sit for 50 before saying anything.
  // The failure still surfaced; the point is when. A CI run left holding a doubled timeout
  // on every stalled data channel is the cost, and nothing in the call site says it.
  const ms = 600;
  const caller = callerAwaiting(500);

  const start = Date.now();
  await assert.rejects(
    () => caller.agentConfigured(ms),
    /timed out waiting for the agent to be told what this conversation is/,
  );
  const elapsed = Date.now() - start;

  assert.ok(
    elapsed < ms * 1.5,
    `gave up after ${elapsed}ms, past the ${ms}ms the caller asked for`,
  );
});
