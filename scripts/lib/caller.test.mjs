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

import { Caller, millis, readRecording, sounding } from "./caller.mjs";

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
