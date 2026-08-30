// Talking to LiveKit as an operator rather than as a caller: signing the JWTs the SFU
// accepts, and calling the room service with them.
//
// Four scripts here each carried their own copy of the same twenty-line HS256 signer,
// differing only in `sub` and which claims they set. Four renderings of one wire format
// is four places a claim name can drift from what livekit-server's pkg/service/auth.go
// actually reads, and the drift shows up as a 401 in whichever copy was not updated —
// which reads exactly like a credential problem and is not one.
//
// **Two of those four still carry their own, and they are not the same case.**
// `livekit-smoke`, `token-endpoint-acceptance` and `loopback-acceptance` sign here.
//
// `conversations-acceptance` (`signWebhook`) does not, and could not: it signs a webhook
// *body*, carrying a top-level `sha256` digest over the bytes and an empty `video`. That
// is a different wire format which happens to share an algorithm, and folding it in here
// would consolidate a resemblance rather than a duplicate.
//
// `webhook-delivery-acceptance` (`mintToken`) is the real remaining copy. It signs a
// room-service token exactly as `sign` does — `video: { roomCreate, roomAdmin, room }`,
// used to authorize a DeleteRoom against the same Twirp service `Rooms.call` talks to —
// and it has *already* drifted: its `sub` is `openconv-webhook-probe` where `Rooms.call`
// sends `openconv-scripts`, and it asks for `roomAdmin` where `Rooms.call` asks for
// `roomList`. What keeps it out of here is not its shape but its reporting — it reads an
// `ok` off a raw `fetch` to tally as a check, where `Rooms.call` throws, so moving it
// changes what that script claims and not merely where its signer lives.
//
// So the honest count of this module's own pattern living elsewhere is one, not zero, and
// it is written here rather than only in a pull request because this is where a reader
// meets it.
//
// Separate from caller.mjs because that module is the client side: what the ElevenLabs
// SDK does to openconv. Nothing a caller does requires the API secret.

import { createHmac } from "node:crypto";

const b64url = (buf) =>
  Buffer.from(buf).toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");

/// Long enough for a probe or an acceptance run, short enough that a leaked token from a
/// script's stdout is worthless by the time anyone reads the log.
const TTL_SECONDS = 600;

/// Far outside a healthy Twirp round-trip and far inside a reader's patience. Every other
/// wait in these scripts is bounded — `SETUP_MS`, `SETTLE_MS`, `rejectAfter` — and an
/// unbounded one here is the worst place for it: `loopback-acceptance` calls `CreateRoom`
/// just before its `try` and `DeleteRoom` inside its `finally`, so an SFU that accepts the
/// connection and never answers hangs the run either before the cleanup is armed or inside
/// it, past the point anything is printed. A probe that hangs silently is worse than one
/// that fails. [LAW:no-silent-failure]
const REQUEST_TIMEOUT_MS = 30_000;

/**
 * The LiveKit credentials, or a refusal naming what is missing and where to get it.
 *
 * One fact — these two variables must exist, and they live in Vault at secret/livekit —
 * held once. `livekit-smoke` and `loopback-acceptance` both need exactly it and had
 * separate copies of the check.
 *
 * `token-endpoint-acceptance` deliberately does NOT use this. Its check covers
 * `OPENCONV_API_KEY` as well and reports every missing variable in a single message, so
 * routing the LiveKit half through here would split that into two throws — an operator
 * missing both would learn about one, fix it, rerun, and learn about the other. Same
 * shape, different fact, and the diagnostic is the reason the difference is worth keeping.
 */
export function livekitCredentials(env) {
  const missing = ["LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"].filter((name) => !env[name]);
  if (missing.length > 0) {
    throw new Error(`missing ${missing.join(" and ")} — read them from Vault at secret/livekit`);
  }
  return { apiKey: env.LIVEKIT_API_KEY, apiSecret: env.LIVEKIT_API_SECRET };
}

/**
 * Signs one token for LiveKit.
 *
 * Plain HS256 with the API key as `iss` and the participant identity as `sub`; every
 * grant LiveKit reads lives under `video`, except the webhook body digest, which is a
 * top-level `sha256`. Both arrive here as `claims` rather than as parameters, because
 * the difference between a room-list token, a join token and a webhook signature is
 * which claims they carry — not which function built them.
 */
export function sign({ apiKey, apiSecret, sub, claims }) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const payload = b64url(JSON.stringify({ iss: apiKey, sub, nbf: now, exp: now + TTL_SECONDS, ...claims }));
  const signature = b64url(createHmac("sha256", apiSecret).update(`${header}.${payload}`).digest());
  return `${header}.${payload}.${signature}`;
}

/**
 * A token admitting one participant to one room, with a microphone and an ear.
 *
 * These mirror the grants openconv's own `mint_participant_token` issues, because a script
 * joining a room it created itself has to be the same kind of participant as one joining a
 * conversation, or it is measuring a different client. **The mirror is maintained by hand
 * and nothing checks it.** The grants themselves live in Rust
 * (`crates/openconv-server/src/livekit.rs`), and a JS constant cannot be derived from them.
 *
 * What would notice production moving is `token-endpoint-acceptance`, which asserts on a
 * token minted by the running server — deliberately against literals transcribed from
 * Happy's voiceRoutes.ts rather than against this function, so that a wrong `joinToken`
 * cannot make it pass. What nothing notices is *this* copy going stale, which is the
 * asymmetry to keep in mind before trusting a probe's "same kind of participant" claim.
 */
export function joinToken({ apiKey, apiSecret, room, identity }) {
  return sign({
    apiKey,
    apiSecret,
    sub: identity,
    claims: {
      video: {
        roomJoin: true,
        room,
        canPublish: true,
        canSubscribe: true,
        canPublishData: true,
      },
    },
  });
}

/**
 * LiveKit's room service, over Twirp.
 *
 * One `call` rather than a named method per RPC: the service's methods differ only in a
 * path segment and a JSON body, so they are values crossing one boundary. `ListRooms`,
 * `CreateRoom` and `DeleteRoom` need no code here to exist.
 */
export class Rooms {
  /**
   * `url` is whatever URL the scripts already hold, in either scheme — a caller dials
   * `wss://host` and the room service lives at `https://host`, and deriving one from the
   * other keeps a deployment one string rather than two that can point at different SFUs.
   */
  constructor({ url, apiKey, apiSecret }) {
    this.url = url.replace(/^ws/, "http").replace(/\/$/, "");
    this.apiKey = apiKey;
    this.apiSecret = apiSecret;
  }

  /**
   * Calls one room-service method and returns its parsed response.
   *
   * Throws on anything but a 2xx, naming the status and the body: an SFU that refused
   * the signature and an SFU that is not there produce very different messages, and a
   * script that swallowed either would go on to report the *next* step as the failure.
   */
  async call(method, body) {
    const token = sign({
      apiKey: this.apiKey,
      apiSecret: this.apiSecret,
      sub: "openconv-scripts",
      claims: { video: { roomCreate: true, roomList: true } },
    });

    const response = await fetch(`${this.url}/twirp/livekit.RoomService/${method}`, {
      method: "POST",
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify(body),
      // An SFU that accepts the connection and then says nothing is otherwise indefinite.
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });

    const text = await response.text();
    if (!response.ok) {
      throw new Error(`${method} on ${this.url} failed: HTTP ${response.status} ${text}`);
    }
    return JSON.parse(text);
  }
}
