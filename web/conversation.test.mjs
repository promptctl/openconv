// What `conversation.js` promises about opening a conversation, checked without a browser,
// without an SFU and without either LiveKit SDK.
//
//   node --test web/conversation.test.mjs
//
// The transport is a fake here on purpose: this module's whole reason to exist is that it
// is the half of both callers that does *not* depend on which SDK is underneath, so a test
// that needed one would be testing the wrong thing. What each real transport does with the
// three operations is its own file's business — `web/caller.test.mjs` for the browser.

import { test } from "node:test";
import assert from "node:assert/strict";

import { conversationInitiation, conversationWith, isAgent } from "./conversation.js";

/** A transport that records what was published rather than reaching an SFU. */
const transportOf = (identities, publishBytes) => {
  const published = [];
  const connected = [];

  return {
    published,
    connected,
    // Reassignable, so a test can have an agent arrive between two sweeps.
    identities,
    connect: async (token) => connected.push(token),
    participants() {
      return this.identities;
    },
    publishBytes:
      publishBytes ??
      (async (payload) => published.push(JSON.parse(new TextDecoder().decode(payload)))),
  };
};

const voiced = (voiceId) => ({ voiceId });

test("the message names every field, so nothing a caller did not set goes unsaid", () => {
  // The exact shape `crates/openconv-agent/src/session.rs` pins from the far side. A
  // field that stops being sent does not fail anywhere — it silently reverts to the
  // deployment default, which is this ticket's own bug.
  assert.deepEqual(conversationInitiation({}), {
    type: "conversation_initiation_client_data",
    conversation_config_override: {
      agent: { prompt: { prompt: null }, first_message: null, language: null },
      tts: { voice_id: null, model_id: null },
    },
    dynamic_variables: null,
  });
});

test("every setting a caller can express lands in the half of the message that carries it", () => {
  // That `language` travels under `agent` while `voice_id` travels under `tts` is the
  // protocol's shape, and the reason no caller builds this by hand any more.
  assert.deepEqual(
    conversationInitiation({
      prompt: "you are a voice interface",
      firstMessage: "ready when you are",
      language: "es",
      voiceId: "bm_george",
      modelId: "piper",
      variables: { sessionId: "sess_42" },
    }),
    {
      type: "conversation_initiation_client_data",
      conversation_config_override: {
        agent: {
          prompt: { prompt: "you are a voice interface" },
          first_message: "ready when you are",
          language: "es",
        },
        tts: { voice_id: "bm_george", model_id: "piper" },
      },
      dynamic_variables: { sessionId: "sess_42" },
    },
  );
});

test("a blank setting asks for nothing rather than for a value named the empty string", () => {
  // `null` and `""` are different questions to the far side: serde reads null as "no
  // particular voice", where "" asks openconv to resolve an empty voice id.
  const override = conversationInitiation({ voiceId: "  ", prompt: "" })
    .conversation_config_override;

  assert.equal(override.tts.voice_id, null);
  assert.equal(override.agent.prompt.prompt, null);
});

test("a setting is trimmed, so a stray space is not a different voice", () => {
  assert.equal(
    conversationInitiation({ voiceId: " bm_george " }).conversation_config_override.tts.voice_id,
    "bm_george",
  );
});

test("only the agents in the room are told", async () => {
  const transport = transportOf(["agent_one", "u_browser", "agent_two"]);

  await conversationWith(transport, () => voiced("bm_george")).arrived();

  assert.equal(transport.published.length, 2, "one message per agent, and none for the caller");
  for (const message of transport.published) {
    assert.equal(message.conversation_config_override.tts.voice_id, "bm_george");
  }
});

test("a room with no agent in it is not a case of its own", async () => {
  const transport = transportOf(["u_browser"]);

  await conversationWith(transport, () => voiced("bm_george")).arrived();

  assert.deepEqual(transport.published, []);
});

test("an agent already told is not told again when someone else arrives", async () => {
  // The diff is what makes "told exactly once" true in both arrival orders — the sweep
  // after connecting and the arrival event see the same roster, and only one of them may
  // speak to any given agent.
  const transport = transportOf(["agent_one"]);
  const conversation = conversationWith(transport, () => voiced("af_heart"));

  await conversation.arrived();
  transport.identities = ["agent_one", "agent_two"];
  await conversation.arrived();

  assert.equal(transport.published.length, 2, "agent_one told once, agent_two told once");
});

test("an agent that left and came back is told again", async () => {
  // It is a new participant holding none of what the last one was told, and a room that
  // remembered otherwise would leave it running the deployment default in silence.
  const transport = transportOf(["agent_one"]);
  const conversation = conversationWith(transport, () => voiced("af_heart"));

  await conversation.arrived();
  transport.identities = [];
  await conversation.arrived();
  transport.identities = ["agent_one"];
  await conversation.arrived();

  assert.equal(transport.published.length, 2);
});

test("the settings are read when they are sent, not copied when the conversation opened", async () => {
  // The whole point of the reader. A copy taken up front would make both sends name the
  // first voice, which is the bug this shape exists to make unrepresentable.
  let showing = "af_heart";
  const transport = transportOf(["agent_one"]);
  const conversation = conversationWith(transport, () => voiced(showing));

  await conversation.arrived();
  showing = "bm_george";
  await conversation.everyone();

  assert.deepEqual(
    transport.published.map((message) => message.conversation_config_override.tts.voice_id),
    ["af_heart", "bm_george"],
  );
});

test("telling everyone reaches an agent that was already told", async () => {
  const transport = transportOf(["agent_one", "agent_two"]);
  const conversation = conversationWith(transport, () => voiced("af_heart"));

  await conversation.arrived();
  await conversation.everyone();

  assert.equal(transport.published.length, 4, "both agents told on arrival and told again");
});

test("telling everyone leaves nobody owed a second copy on the next arrival", async () => {
  // `everyone` and `arrived` share one record of who holds what, so a change of settings
  // followed by a new arrival sends one message, not a duplicate to the room.
  const transport = transportOf(["agent_one"]);
  const conversation = conversationWith(transport, () => voiced("af_heart"));

  await conversation.everyone();
  await conversation.arrived();

  assert.equal(transport.published.length, 1);
});

test("a send that fails names the agent it could not be delivered to", async () => {
  const transport = transportOf(["agent_one"], async () => {
    throw new Error("data channel closed");
  });

  await assert.rejects(conversationWith(transport, () => voiced("bm_george")).arrived(), (failure) => {
    assert.match(failure.message, /agent_one could not be told what this conversation is/);
    assert.match(failure.message, /data channel closed/);
    return true;
  });
});

test("opening connects before it configures, and configures before it returns", async () => {
  // The sequence, asserted as a sequence. An agent told which voice to use after the
  // caller's microphone is live is an agent that changes voice partway through a reply,
  // and the only thing keeping that from happening is this order.
  const order = [];
  const transport = transportOf(["agent_one"]);
  const connect = transport.connect;
  transport.connect = async (token) => {
    order.push("connect");
    await connect(token);
  };
  const publishBytes = transport.publishBytes;
  transport.publishBytes = async (payload) => {
    order.push("configure");
    await publishBytes(payload);
  };

  // The mint is the one step that reaches the network, so it is the one thing stubbed.
  const realFetch = globalThis.fetch;
  const claims = Buffer.from(JSON.stringify({ video: { room: "conv_abc" } })).toString("base64url");
  globalThis.fetch = async (url) => {
    order.push("mint");
    assert.match(String(url), /\/v1\/convai\/conversation\/token\?agent_id=agent_happy/);
    return { ok: true, text: async () => JSON.stringify({ token: `header.${claims}.signature` }) };
  };

  try {
    const conversation = conversationWith(transport, () => voiced("bm_george"));
    const conversationId = await conversation.open({
      openconv: "http://127.0.0.1:8080",
      apiKey: "secret",
      agentId: "agent_happy",
      participantName: "u_test",
    });

    assert.equal(conversationId, "conv_abc", "the conversation is recovered from the token");
    assert.deepEqual(order, ["mint", "connect", "configure"]);
  } finally {
    globalThis.fetch = realFetch;
  }
});

test("a mint that is refused throws carrying what the server said", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => ({ ok: false, status: 401, text: async () => "bad api key" });

  try {
    await assert.rejects(
      conversationWith(transportOf([]), () => ({})).open({
        openconv: "http://127.0.0.1:8080",
        apiKey: "wrong",
        agentId: "agent_happy",
        participantName: "u_test",
      }),
      /mint failed: HTTP 401 bad api key/,
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

test("the agent is recognised by the identity openconv actually mints", () => {
  // Matched rather than re-decided: two clients disagreeing about who counts as the agent
  // would disagree about the same room. `crates/openconv-server/src/livekit.rs` mints it.
  assert.ok(isAgent("agent_conv_01J8"));
  assert.ok(!isAgent("u_browser"));
});
