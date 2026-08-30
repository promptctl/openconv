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

import { Caller } from "./caller.mjs";

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
