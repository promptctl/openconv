// What `livekit.mjs` promises about the tokens it signs, checked without an SFU.
//
//   node --test scripts/lib/livekit.test.mjs
//
// No `NODE_PATH`, unlike `caller.test.mjs`: this module imports `node:crypto` and nothing
// else, so there is no `@livekit/rtc-node` to resolve. That is the point of testing here
// rather than against a deployment — a wrong claim name reaches a reader as a 401 from a
// live SFU, which reads exactly like a credential problem and is not one.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";

import { joinToken, Rooms, sign } from "./livekit.mjs";

const KEY = "APIkey123";
const SECRET = "secret-that-is-long-enough-to-be-a-real-one";

/// The three segments, with the payload decoded. Written out rather than pulled from a JWT
/// library on purpose: a library that agreed with a wrong implementation would hide exactly
/// the drift these tests exist to catch.
const parse = (token) => {
  const [header, payload, signature] = token.split(".");
  return {
    header: JSON.parse(Buffer.from(header, "base64url")),
    payload: JSON.parse(Buffer.from(payload, "base64url")),
    signature,
    signingInput: `${header}.${payload}`,
  };
};

test("a signed token carries the claims livekit-server reads, and an expiry", () => {
  const before = Math.floor(Date.now() / 1000);
  const { header, payload } = parse(sign({ apiKey: KEY, apiSecret: SECRET, sub: "probe", claims: {} }));
  const after = Math.floor(Date.now() / 1000);

  assert.deepEqual(header, { alg: "HS256", typ: "JWT" });
  assert.equal(payload.iss, KEY, "the API key is the issuer");
  assert.equal(payload.sub, "probe", "the participant identity is the subject");

  // `nbf` is "now" and `exp` is the documented ten minutes past it. Bounded rather than
  // pinned to a single second, because the clock moves between the two reads above.
  assert.ok(payload.nbf >= before && payload.nbf <= after, `nbf ${payload.nbf} outside [${before}, ${after}]`);
  assert.equal(payload.exp - payload.nbf, 600, "TTL_SECONDS");
});

test("the signature is HS256 over the encoded header and payload", () => {
  const { signature, signingInput } = parse(sign({ apiKey: KEY, apiSecret: SECRET, sub: "probe", claims: {} }));
  const expected = createHmac("sha256", SECRET).update(signingInput).digest("base64url");
  assert.equal(signature, expected);
});

test("every segment is base64url, so a token survives an Authorization header intact", () => {
  // The hand-rolled encoder replaces `+` and `/` and strips `=`. A token carrying any of
  // the three is a token some HTTP layer is entitled to mangle, and the failure would land
  // at the SFU as an opaque 401 rather than here.
  const token = sign({
    apiKey: KEY,
    apiSecret: SECRET,
    // Multi-byte and `?`/`>` bytes, chosen because they push the base64 alphabet into the
    // `+` and `/` range that the plain encoder would emit.
    sub: "probe-ÿ?>ø",
    claims: { video: { room: "römm-ÿ?>" } },
  });
  assert.match(token, /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/);
});

test("claims are spread over the base, so a caller can shadow iss, sub and exp", () => {
  // Pinning what the code does, not endorsing it. The spread order in `sign` puts `claims`
  // last, which makes shadowing reachable: a future claim named `exp` or `iss` would
  // silently replace the expiry or the API key and fail at the SFU as a credential error.
  // If the order is ever reversed, this test breaks loudly and on purpose.
  const { payload } = parse(
    sign({
      apiKey: KEY,
      apiSecret: SECRET,
      sub: "probe",
      claims: { iss: "shadowed", exp: 1, video: { roomList: true } },
    }),
  );
  assert.equal(payload.iss, "shadowed");
  assert.equal(payload.exp, 1);
  assert.deepEqual(payload.video, { roomList: true }, "ordinary claims still land");
});

test("a join token carries exactly the grants openconv's own mint_participant_token issues", () => {
  // The docstring is explicit that this mirrors `crates/openconv-server/src/livekit.rs` by
  // hand and nothing derives one from the other. This pins the JS side so a change to it is
  // deliberate; it cannot notice the Rust side moving, which is the asymmetry to remember.
  const { payload } = parse(joinToken({ apiKey: KEY, apiSecret: SECRET, room: "conv_1", identity: "caller-a" }));

  assert.equal(payload.sub, "caller-a", "the identity is the subject, not a claim");
  assert.deepEqual(payload.video, {
    roomJoin: true,
    room: "conv_1",
    canPublish: true,
    canSubscribe: true,
    canPublishData: true,
  });
});

test("the room service derives its HTTP origin from whichever scheme a script already holds", () => {
  // `wss://` -> `https://` works only because the two-character `ws` match is consumed and
  // the trailing `s` survives to meet the replacement. That is correct and non-obvious, so
  // all four schemes are pinned rather than the two that were in front of whoever last read
  // it. A rewrite to `^wss?` that forgot to re-add the `s` passes the `ws://` case alone.
  const origins = Object.fromEntries(
    ["ws://sfu.example:7880", "wss://sfu.example", "http://sfu.example:7880", "https://sfu.example"].map((url) => [
      url,
      new Rooms({ url, apiKey: KEY, apiSecret: SECRET }).url,
    ]),
  );

  assert.deepEqual(origins, {
    "ws://sfu.example:7880": "http://sfu.example:7880",
    "wss://sfu.example": "https://sfu.example",
    "http://sfu.example:7880": "http://sfu.example:7880",
    "https://sfu.example": "https://sfu.example",
  });
});

test("a trailing slash is dropped, so a method path never doubles it", () => {
  // `call` builds `${this.url}/twirp/...`; a URL kept as `https://host/` would request
  // `https://host//twirp/...`, which some proxies serve and some 404.
  assert.equal(new Rooms({ url: "wss://sfu.example/", apiKey: KEY, apiSecret: SECRET }).url, "https://sfu.example");
});

/// Replaces `globalThis.fetch` and records what it was handed. Every test that installs one
/// restores it in a `finally`: a stub that survived a failing assertion would make every
/// later test in the file measure the stub instead of the module, and the suite would go on
/// reporting confidently about nothing.
const stubFetch = (respond) => {
  const original = globalThis.fetch;
  const calls = [];
  globalThis.fetch = async (url, init) => {
    calls.push({ url, init });
    return respond();
  };
  return { calls, restore: () => (globalThis.fetch = original) };
};

const sfu = () => new Rooms({ url: "wss://sfu.example", apiKey: KEY, apiSecret: SECRET });

test("call() hands back the parsed body of a 2xx", async () => {
  const stub = stubFetch(() => ({ ok: true, status: 200, text: async () => '{"rooms":[{"name":"probe_1"}]}' }));
  try {
    // The shape `livekit-smoke` destructures: `const { rooms = [] } = await ...call("ListRooms", {})`.
    assert.deepEqual(await sfu().call("ListRooms", {}), { rooms: [{ name: "probe_1" }] });
  } finally {
    stub.restore();
  }
});

test("call() addresses the Twirp method and authorizes it with a room-service token", async () => {
  const stub = stubFetch(() => ({ ok: true, status: 200, text: async () => "{}" }));
  try {
    await sfu().call("DeleteRoom", { room: "probe_1" });
  } finally {
    stub.restore();
  }

  assert.equal(stub.calls.length, 1);
  const { url, init } = stub.calls[0];
  assert.equal(url, "https://sfu.example/twirp/livekit.RoomService/DeleteRoom");
  assert.equal(init.method, "POST");
  assert.equal(init.headers["Content-Type"], "application/json");
  assert.equal(init.body, JSON.stringify({ room: "probe_1" }));

  // The token is the one `sign()` builds, not a second rendering of it: same issuer, the
  // operator subject, and only the grants the room service needs.
  const { payload, signature, signingInput } = parse(init.headers.Authorization.replace(/^Bearer /, ""));
  assert.equal(payload.iss, KEY);
  assert.equal(payload.sub, "openconv-scripts");
  assert.deepEqual(payload.video, { roomCreate: true, roomList: true });
  assert.equal(signature, createHmac("sha256", SECRET).update(signingInput).digest("base64url"));
});

test("call() throws on a non-2xx, naming the method, the origin, the status and the reason", async () => {
  let bodyRead = false;
  const stub = stubFetch(() => ({
    ok: false,
    status: 401,
    text: async () => {
      bodyRead = true;
      return "invalid API key";
    },
  }));

  try {
    await assert.rejects(() => sfu().call("ListRooms", {}), (error) => {
      // All four, because this message is the whole diagnostic three scripts get: an SFU
      // that refused the signature and an SFU that is not there must not read alike.
      assert.match(error.message, /ListRooms/);
      assert.match(error.message, /https:\/\/sfu\.example/);
      assert.match(error.message, /401/);
      assert.match(error.message, /invalid API key/);
      return true;
    });
  } finally {
    stub.restore();
  }

  // The ordering that message depends on. An implementation that branched on `.ok` before
  // awaiting `text()` would still throw, still name the status, and have no reason in it —
  // a regression that looks like a working error path right up until you need it.
  assert.ok(bodyRead, "the body is read before the status is judged");
});

test("a failure before there is a response still names the method and the origin", async () => {
  // `AbortSignal.timeout` rejects with a bare "The operation was aborted due to timeout",
  // and a refused connection with a bare "fetch failed". Neither says which RPC against
  // which SFU — the one question this module exists to answer, and the reason the non-2xx
  // arm is so careful about it. Both are checked because they reach the caller by the same
  // path, so a branch introduced later would break one of them silently.
  for (const cause of [
    Object.assign(new Error("The operation was aborted due to timeout"), { name: "TimeoutError" }),
    Object.assign(new Error("fetch failed"), { name: "TypeError" }),
  ]) {
    const stub = stubFetch(() => {
      throw cause;
    });
    try {
      await assert.rejects(() => sfu().call("ListRooms", {}), (error) => {
        assert.match(error.message, /ListRooms/, `${cause.name} names the method`);
        assert.match(error.message, /https:\/\/sfu\.example/, `${cause.name} names the origin`);
        assert.match(error.message, new RegExp(cause.name), "and what went wrong");
        assert.equal(error.cause, cause, "the original is kept for the stack");
        return true;
      });
    } finally {
      stub.restore();
    }
  }
});
