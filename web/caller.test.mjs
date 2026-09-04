// What `caller.js` promises about the message that tells an agent which voice to use,
// checked without a browser and without an SFU.
//
//   node --test web/caller.test.mjs
//
// No `NODE_PATH`, unlike `scripts/lib/caller.test.mjs`: this module's only import is the
// vendored `livekit-client`, which loads in node, and nothing here reaches a room.
//
// The half of this branch that needs a real room — that a voice list landing mid-join is
// not dropped, which is a fact about when `call` is assigned — is checked against a live
// one and is not here. What is here is the claim that holds without a page at all, and it
// is the one this branch turns on: `useChosenVoice` reads the control at the moment it
// sends, so there is no copy left to go stale.

import { test } from "node:test";
import assert from "node:assert/strict";

import { Call } from "./caller.js";

/** A room that records what was published to it rather than reaching an SFU. */
const roomOf = (identities, publishData) => {
  const published = [];

  return {
    published,
    remoteParticipants: new Map(identities.map((identity) => [identity, {}])),
    localParticipant: {
      publishData:
        publishData ??
        (async (payload) => published.push(JSON.parse(new TextDecoder().decode(payload)))),
    },
  };
};

const callInto = (room, chosenVoice) => new Call(room, null, "conv_test", true, chosenVoice);

test("only the agents in the room are told, and told the message the SDK opens on", async () => {
  const room = roomOf(["agent_one", "u_browser", "agent_two"]);

  await callInto(room, () => "bm_george").useChosenVoice();

  assert.equal(room.published.length, 2, "one message per agent, and none for the caller");
  for (const message of room.published) {
    assert.equal(message.type, "conversation_initiation_client_data");
    assert.equal(message.conversation_config_override.tts.voice_id, "bm_george");
  }
});

test("the voice is read when it is sent, not copied when the call was made", async () => {
  // The whole point of `chosenVoice` being a reader. A copy taken at construction would
  // make both sends name the first voice, which is the bug this shape exists to prevent.
  let showing = "af_heart";
  const room = roomOf(["agent_one"]);
  const call = callInto(room, () => showing);

  await call.useChosenVoice();
  showing = "bm_george";
  await call.useChosenVoice();

  assert.deepEqual(
    room.published.map((message) => message.conversation_config_override.tts.voice_id),
    ["af_heart", "bm_george"],
  );
});

test("picking no voice asks for none, rather than for a voice named the empty string", async () => {
  // `null` and `""` are different questions to the far side: serde reads null as "no
  // particular voice", where "" asks it to resolve an empty voice id.
  const room = roomOf(["agent_one"]);

  await callInto(room, () => "").useChosenVoice();

  assert.equal(room.published[0].conversation_config_override.tts.voice_id, null);
});

test("a room with no agent in it is not a case of its own", async () => {
  const room = roomOf(["u_browser"]);

  await callInto(room, () => "bm_george").useChosenVoice();

  assert.deepEqual(room.published, []);
});

test("a send that fails names the agent it could not be delivered to", async () => {
  const room = roomOf(["agent_one"], async () => {
    throw new Error("data channel closed");
  });

  await assert.rejects(callInto(room, () => "bm_george").useChosenVoice(), (failure) => {
    assert.match(failure.message, /agent_one could not be told which voice to speak in/);
    assert.match(failure.message, /data channel closed/);
    return true;
  });
});
