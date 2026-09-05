// What `caller.js` promises about the browser half of the transport seam, and about what a
// configure that failed costs, checked without a browser and without an SFU.
//
//   node --test web/caller.test.mjs
//
// No `NODE_PATH`, unlike `scripts/lib/caller.test.mjs`: this module's only import is the
// vendored `livekit-client`, which loads in node, and nothing here reaches a room.
//
// The handshake these three operations carry is not tested here — it has no browser in it
// at all and is checked in `conversation.test.mjs` against a fake transport. What is here
// is the part that can only be wrong on this side: three room calls, in an SDK whose
// mistakes are all silent. An unreliable `publishData` drops a configuration under load
// and nothing reports it; a roster read as objects makes every identity `undefined`, so
// `isAgent` is false for everyone and the agent is simply never told. Both look exactly
// like the bug this whole seam was built to end.
//
// The half that needs a real room — that a voice list landing mid-join is not dropped — is
// a fact about when `call` is assigned and is checked against a live one, not here.

import { test } from "node:test";
import assert from "node:assert/strict";

import { keepTheCall, transportOf } from "./caller.js";
import { NotTold } from "./conversation.js";

/** A room that records what it was asked to do rather than reaching an SFU. */
const roomOf = (identities, publishData) => {
  const published = [];
  const dialled = [];

  return {
    published,
    dialled,
    remoteParticipants: new Map(identities.map((identity) => [identity, { identity }])),
    connect: async (url, token) => dialled.push({ url, token }),
    localParticipant: {
      publishData:
        publishData ??
        (async (payload, options) =>
          published.push({ message: JSON.parse(new TextDecoder().decode(payload)), options })),
    },
  };
};

test("the roster is read as identities, which is what the handshake matches on", () => {
  // `remoteParticipants` is keyed by identity and valued by participant objects. Handing
  // over the values would make every identity `undefined`, no participant would look like
  // an agent, and the conversation would be configured for nobody — in silence.
  const room = roomOf(["agent_one", "u_browser"]);

  assert.deepEqual(transportOf(room, "ws://sfu").participants(), ["agent_one", "u_browser"]);
});

test("a control message goes out reliably, because a dropped one is never noticed", async () => {
  // The data channel's unreliable mode is lossy by design. A configuration lost that way
  // leaves the agent on the deployment default with every client believing it was told.
  const room = roomOf([]);

  await transportOf(room, "ws://sfu").publishBytes(new TextEncoder().encode('{"type":"x"}'));

  assert.equal(room.published.length, 1);
  assert.deepEqual(room.published[0].message, { type: "x" });
  assert.equal(room.published[0].options.reliable, true);
});

test("the SFU dialled is the one this page was configured with, not one in the token", async () => {
  // A token minted by one deployment and offered to another deployment's SFU does not
  // error: the client joins a room the agent is not in and the caller hears silence.
  const room = roomOf([]);

  await transportOf(room, "wss://livekit.example").connect("a.b.c");

  assert.deepEqual(room.dialled, [{ url: "wss://livekit.example", token: "a.b.c" }]);
});

test("a configure that failed costs the voice that was chosen, not the call", () => {
  // By the time this can happen the room is up and the agent is in it; only the message
  // saying what the conversation is did not land, which leaves a working call running the
  // deployment's defaults. Tearing that down answers a dropdown with a hang-up, and the
  // agent the mint dispatched is in the room being paid for either way.
  const reported = [];
  const failure = new NotTold("conv_kept", new Error("the data channel closed"));

  assert.equal(keepTheCall(failure, (seen) => reported.push(seen)), "conv_kept");
  assert.deepEqual(reported, [failure], "kept is not the same as unnoticed");
});

test("a call that never opened is re-raised untouched, not kept", () => {
  // A refused mint or a connect that failed leaves no room and no agent, so there is no
  // call to keep — and `join`'s own cleanup, which releases the room and the microphone,
  // runs only if this failure reaches it. Inverting the type check would silently restore
  // the regression this function exists to end, in whichever direction it was inverted.
  const reported = [];
  const failure = new Error("the SFU refused the token");

  assert.throws(
    () => keepTheCall(failure, (seen) => reported.push(seen)),
    (thrown) => thrown === failure,
  );
  assert.deepEqual(reported, [], "re-raising is how this one is reported; twice is not");
});
