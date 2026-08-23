// Checks the usage endpoint against the contract Happy's gating depends on.
//
//   OPENCONV_API_KEY=... LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
//     node scripts/conversations-acceptance.mjs [openconv-url]
//
// Runs a whole conversation lifecycle: mint a token (which creates a real room on the
// SFU), deliver the room_finished webhook that ends it, then read the usage back the
// way voiceRoutes.ts reads it.
//
// The webhook deliveries here are signed exactly as LiveKit signs them — an HS256
// token whose sha256 claim is the digest of the body — using the same credentials the
// SFU holds. What this does NOT prove is that the deployment is configured to send
// them; see the note at the end of the run.

import { createHmac, createHash, randomUUID } from "node:crypto";

const b64url = (buf) =>
  Buffer.from(buf).toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");

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
  };
}

const config = readConfig(process.env, process.argv);

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push({ name, ok, detail });
  console.log(`${ok ? "  ok  " : " FAIL "} ${name}${detail ? ` — ${detail}` : ""}`);
};

/// Signs a webhook body the way LiveKit does: the token's sha256 claim is the digest of
/// the exact bytes sent, so a signature cannot be reused over a different body.
function signWebhook(body) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const payload = b64url(
    JSON.stringify({
      iss: config.apiKey,
      sub: config.apiKey,
      nbf: now,
      exp: now + 600,
      sha256: createHash("sha256").update(body).digest("base64"),
      video: {},
    }),
  );
  const signature = b64url(
    createHmac("sha256", config.apiSecret).update(`${header}.${payload}`).digest(),
  );
  return `${header}.${payload}.${signature}`;
}

async function mint(userId) {
  const response = await fetch(
    `${config.openconv}/v1/convai/conversation/token?agent_id=agent_happy&participant_name=${userId}`,
    { headers: { "xi-api-key": config.xiApiKey } },
  );
  if (!response.ok) throw new Error(`mint failed: HTTP ${response.status} ${await response.text()}`);
  const { token } = await response.json();
  const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString());
  return claims.video.room;
}

async function endConversation(room, endedAt, auth = null) {
  const body = JSON.stringify({
    event: "room_finished",
    room: { name: room },
    id: randomUUID(),
    createdAt: endedAt,
  });
  const response = await fetch(`${config.openconv}/livekit/webhook`, {
    method: "POST",
    headers: { "Content-Type": "application/webhook+json", Authorization: auth ?? signWebhook(body) },
    body,
  });
  return response.status;
}

async function usage(query) {
  const response = await fetch(`${config.openconv}/v1/convai/conversations?${query}`, {
    headers: { "xi-api-key": config.xiApiKey },
  });
  if (!response.ok) return { status: response.status, conversations: [] };
  return { status: response.status, ...(await response.json()) };
}

// A fresh user per run, so a log left over from an earlier run cannot make this pass.
const user = `u_acc${randomUUID().replaceAll("-", "").slice(0, 12)}`;
const other = `u_other${randomUUID().replaceAll("-", "").slice(0, 8)}`;
console.log(`openconv ${config.openconv}, user ${user}\n`);

// ---- two conversations for one user, both completed ----
const first = await mint(user);
const second = await mint(user);
check("two conversations minted", first !== second, `${first}, ${second}`);

// Start times come from the service, so the durations asserted below are the ones it
// actually computed rather than ones this script assumed.
const started = Object.fromEntries(
  (await usage(`user_id=${user}`)).conversations.map((c) => [
    c.conversation_id,
    c.start_time_unix_secs,
  ]),
);

check("both appear before they finish", Object.keys(started).length === 2);

const firstEnd = started[first] + 60;
const secondEnd = started[second] + 120;
check("room_finished accepted for the first", (await endConversation(first, firstEnd)) === 204);
check("room_finished accepted for the second", (await endConversation(second, secondEnd)) === 204);

// ---- the ticket's acceptance criterion ----
const all = await usage(`user_id=${user}`);
check("both completed sessions are listed", all.conversations.length === 2);
check(
  "durations are the ones the SFU reported",
  all.conversations.every((c) => [60, 120].includes(c.call_duration_secs)),
  all.conversations.map((c) => `${c.call_duration_secs}s`).join(", "),
);
check(
  "both read as done",
  all.conversations.every((c) => c.status === "done"),
);
// This is the number Happy actually computes.
const summed = all.conversations.reduce((total, c) => total + (c.call_duration_secs ?? 0), 0);
check("Happy's sum over the window", summed === 180, `${summed}s`);

// ---- a cutoff between the two returns only the later one ----
const cutoff = Math.floor((started[first] + started[second]) / 2) + 1;
const later = await usage(`user_id=${user}&created_after=${cutoff}`);
check(
  "a cutoff between them returns only the later",
  later.conversations.length === 1 && later.conversations[0].conversation_id === second,
  `${later.conversations.length} returned`,
);

// ---- created_after in the format Happy actually sends ----
const iso = new Date(cutoff * 1000).toISOString();
const isoFiltered = await usage(`user_id=${user}&created_after=${encodeURIComponent(iso)}`);
check(
  "an ISO-8601 created_after filters the same way",
  isoFiltered.conversations.length === 1 &&
    isoFiltered.conversations[0].conversation_id === second,
  iso,
);

const thirtyDaysAgo = new Date(Date.now() - 30 * 86400 * 1000).toISOString();
const happyShaped = await usage(
  `user_id=${user}&created_after=${encodeURIComponent(thirtyDaysAgo)}&page_size=100`,
);
check(
  "the exact query voiceRoutes.ts sends returns both",
  happyShaped.conversations.length === 2,
  `${happyShaped.conversations.length} returned`,
);

const badCutoff = await usage(`user_id=${user}&created_after=not-a-time`);
check("an unparseable created_after is refused", badCutoff.status === 422, `HTTP ${badCutoff.status}`);

// ---- isolation between users ----
await mint(other);
const mine = await usage(`user_id=${user}`);
check("another user's conversations stay invisible", mine.conversations.length === 2);

// ---- the webhook is authenticated ----
const unsigned = await fetch(`${config.openconv}/livekit/webhook`, {
  method: "POST",
  body: JSON.stringify({ event: "room_finished", room: { name: first } }),
});
check("an unsigned webhook is refused", unsigned.status === 401, `HTTP ${unsigned.status}`);

const forged = await endConversation(first, firstEnd, signWebhook("a completely different body"));
check("a webhook signed over another body is refused", forged === 401, `HTTP ${forged}`);

const failed = checks.filter((c) => !c.ok);
console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
console.log(
  "\nNote: this signs its own deliveries. That the deployment SENDS them depends on\n" +
    "webhook.urls in the LiveKit job config, which lives in home-infra and needs a\n" +
    "reachable openconv to point at.",
);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.map((c) => c.name).join("; ")}`);
  process.exit(1);
}
