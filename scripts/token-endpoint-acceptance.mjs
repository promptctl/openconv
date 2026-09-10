// Checks a running openconv against the contract Happy's server actually depends on.
//
//   [OPENCONV_API_KEY=...] LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
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

import { callerHeaders } from "../web/conversation.js";
import { Rooms, livekitCredentials } from "./lib/livekit.mjs";

// The one boundary: everything below runs on values this run is going to use.
function readConfig(env, argv) {
  return {
    ...livekitCredentials(env),
    // Absent against a deployment that asks callers for no credential, which is the
    // default. What this run checks about the key is read from the deployment rather
    // than from here — see the enforcement checks at the end.
    xiApiKey: env.OPENCONV_API_KEY ?? null,
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
    headers: callerHeaders(apiKey),
  });
  return { status: response.status, body: await response.text() };
}

// How Happy recovers the conversation ID — its regex, verbatim from voiceRoutes.ts.
const recoverConversationId = (room) => (room || "").match(/(conv_[a-zA-Z0-9]+)/)?.[0];

// Constructed here rather than beside its first use so the banner below can name the origin
// requests actually go to. `Rooms` derives an HTTP origin from whichever scheme it is handed
// (`wss://host` -> `https://host`), so printing the raw argument would have the banner claim
// one endpoint while every request went to another — misleading on exactly the failed run
// somebody reads a banner on. Constructing it is pure string work; nothing is dialled until
// `call`. [LAW:one-source-of-truth]
const roomService = new Rooms({
  url: config.livekit,
  apiKey: config.apiKey,
  apiSecret: config.apiSecret,
});

console.log(`openconv ${config.openconv} against LiveKit ${roomService.url}\n`);

// ---- the metered path: agent_id plus a participant_name carrying Happy's user ID ----
const minted = await mint("agent_id=agent_happy&participant_name=u_acceptance");
check("metered mint returns 200", minted.status === 200, `HTTP ${minted.status}`);

// Stop here rather than read a token out of a refusal. A 401 body is valid JSON with no
// `token` in it, so every line below would run on `undefined` and the script would die on
// `token.split` with a TypeError, before printing a single result — turning "you did not
// give me the key this deployment wants" into a stack trace. [LAW:no-silent-failure]
if (minted.status !== 200) {
  const posture = config.xiApiKey ? "the key in OPENCONV_API_KEY was refused" : "no OPENCONV_API_KEY was set";
  console.error(`\ncannot go on: the mint answered HTTP ${minted.status} and ${posture}.`);
  console.error(`  ${minted.body}`);
  process.exit(1);
}

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

// ---- the deployment's own posture is the one that is enforced ----
// Both mints below always run; what they are expected to answer is a value read from the
// deployment, not a branch in this script. [LAW:dataflow-not-control-flow] `/call/config`
// is the same answer the page shows its api key field on, so a deployment that asks for a
// key and a deployment that does not are each held to what they said about themselves —
// and a deployment that asks for one and then serves a caller who has none fails here.
const { requires_api_key: requiresKey } = await (await fetch(`${config.openconv}/call/config`)).json();
const expected = requiresKey ? 401 : 200;
const posture = `requires_api_key=${requiresKey}`;

const wrongKey = await mint("agent_id=agent_happy", "sk-not-the-key");
check(`a wrong xi-api-key gets ${expected}`, wrongKey.status === expected, `HTTP ${wrongKey.status}, ${posture}`);

const noKey = await mint("agent_id=agent_happy", null);
check(`a missing xi-api-key gets ${expected}`, noKey.status === expected, `HTTP ${noKey.status}, ${posture}`);

const noAgent = await mint("");
check("a request with no agent_id is rejected", noAgent.status >= 400, `HTTP ${noAgent.status}`);

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
