// Checks a running openconv against the contract Happy's server actually depends on.
//
//   OPENCONV_API_KEY=... LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
//     node scripts/token-endpoint-acceptance.mjs [openconv-url] [livekit-url]
//
// This exercises the endpoint end to end against a real LiveKit deployment, which is
// the only way some of it can be exercised at all: the SFU rejecting a signature, a
// room that was never created, and a reqwest build with no TLS backend compiled in all
// look like a working service from inside a unit test.
//
// The assertions are transcribed from voiceRoutes.ts in the Happy repo rather than
// from the endpoint's own documentation, so this fails when openconv stops satisfying
// its caller — not when it stops matching what we believed its caller wanted.

import { Rooms } from "./lib/livekit.mjs";

// The one boundary: everything below runs on values known to exist.
function readConfig(env, argv) {
  const missing = ["OPENCONV_API_KEY", "LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"].filter(
    (name) => !env[name],
  );
  if (missing.length > 0) {
    throw new Error(`missing ${missing.join(", ")} — the LiveKit pair lives in Vault at secret/livekit`);
  }
  return {
    xiApiKey: env.OPENCONV_API_KEY,
    apiKey: env.LIVEKIT_API_KEY,
    apiSecret: env.LIVEKIT_API_SECRET,
    openconv: (argv[2] ?? "http://127.0.0.1:8080").replace(/\/$/, ""),
    livekit: (argv[3] ?? "https://livekit.sanctuary.gdn").replace(/\/$/, ""),
  };
}

const config = readConfig(process.env, process.argv);

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push({ name, ok, detail });
  console.log(`${ok ? "  ok  " : " FAIL "} ${name}${detail ? ` — ${detail}` : ""}`);
};

async function mint(query, apiKey = config.xiApiKey) {
  const response = await fetch(`${config.openconv}/v1/convai/conversation/token?${query}`, {
    headers: { "xi-api-key": apiKey },
  });
  return { status: response.status, body: await response.text() };
}

// How Happy recovers the conversation ID — its regex, verbatim from voiceRoutes.ts.
const recoverConversationId = (room) => (room || "").match(/(conv_[a-zA-Z0-9]+)/)?.[0];

console.log(`openconv ${config.openconv} against LiveKit ${config.livekit}\n`);

// ---- the metered path: agent_id plus a participant_name carrying Happy's user ID ----
const minted = await mint("agent_id=agent_happy&participant_name=u_acceptance");
check("metered mint returns 200", minted.status === 200, `HTTP ${minted.status}`);

const { token } = JSON.parse(minted.body);
check("response carries a token field", typeof token === "string" && token.length > 0);

const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString());
const conversationId = recoverConversationId(claims.video?.room);

check("Happy recovers a conversation id from video.room", Boolean(conversationId), conversationId);
// The near-miss this whole design exists to exclude: a room name the regex matches
// only a prefix of, yielding an id that names a room nobody created.
check(
  "the recovered id is the whole room name, not a prefix",
  claims.video.room === conversationId,
  `room=${claims.video.room} recovered=${conversationId}`,
);
check("the token names the participant", claims.name === "u_acceptance", claims.name);

// ---- the grants admit that participant to that room, and to nothing else ----
check("roomJoin granted", claims.video.roomJoin === true);
check("room scoped to this conversation", claims.video.room === conversationId);
check("can publish a microphone track", claims.video.canPublish === true);
check("can subscribe to the agent", claims.video.canSubscribe === true);
check("can publish control messages", claims.video.canPublishData === true);
check("no room creation granted", claims.video.roomCreate === false);
check("no room administration granted", claims.video.roomAdmin === false);
check("token is signed by the configured key", claims.iss === config.apiKey, claims.iss);
check("token outlives a long call", claims.exp - claims.nbf >= 5 * 3600, `${claims.exp - claims.nbf}s`);

// ---- the room exists on the SFU, because auto_create is off and joining would fail ----
const roomService = new Rooms({
  url: config.livekit,
  apiKey: config.apiKey,
  apiSecret: config.apiSecret,
});
const { rooms = [] } = await roomService.call("ListRooms", {});
const room = rooms.find((candidate) => candidate.name === conversationId);

check("the room was actually created on the SFU", Boolean(room), `${rooms.length} room(s) open`);
if (room) {
  const metadata = JSON.parse(room.metadata || "{}");
  check("room metadata names the conversation", metadata.conversation_id === conversationId);
  check("room metadata names the user, so the agent knows who it serves", metadata.happy_user === "u_acceptance");
  check("room metadata names the agent", metadata.agent_id === "agent_happy");
}

// ---- the bring-your-own-key path, which sends no participant_name at all ----
const byo = await mint("agent_id=agent_happy");
check("BYO mint (no participant_name) returns 200", byo.status === 200, `HTTP ${byo.status}`);
if (byo.status === 200) {
  const byoClaims = JSON.parse(Buffer.from(JSON.parse(byo.body).token.split(".")[1], "base64").toString());
  const byoId = recoverConversationId(byoClaims.video?.room);
  check("BYO token still yields a conversation id", Boolean(byoId), byoId);
  check("BYO conversation is distinct from the metered one", byoId !== conversationId);
}

// ---- the credential is actually enforced ----
const wrongKey = await mint("agent_id=agent_happy", "sk-not-the-key");
check("a wrong xi-api-key is refused", wrongKey.status === 401, `HTTP ${wrongKey.status}`);

const noKey = await fetch(`${config.openconv}/v1/convai/conversation/token?agent_id=agent_happy`);
check("a missing xi-api-key is refused", noKey.status === 401, `HTTP ${noKey.status}`);

const noAgent = await mint("");
check("a request with no agent_id is rejected", noAgent.status >= 400, `HTTP ${noAgent.status}`);

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
