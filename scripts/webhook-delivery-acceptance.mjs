// Proves the SFU actually posts room_finished to openconv, which is the only way a
// conversation ever gets a duration.
//
//   OPENCONV_API_KEY=... LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
//     node scripts/webhook-delivery-acceptance.mjs [openconv-url] [livekit-url]
//
// conversations-acceptance.mjs signs its own deliveries, so it passes whether or not
// anything is configured to send one — it covers openconv's half of the link. This
// covers the other half: the delivery is made by the deployed SFU, from its own
// `webhook.urls`, over the network, to the hostname in its config. Nothing else in the
// suite would notice that stanza going missing, and the symptom in production is not an
// error — it is every call reading as still running and billing the six-hour cap.
//
// The room is closed with DeleteRoom rather than by waiting out empty_timeout, because
// five minutes of sleeping is not a better test than the same event arriving now.

import { createHmac } from "node:crypto";
import { Checks, readEnvironment } from "./lib/caller.mjs";
import { livekitCredentials } from "./lib/livekit.mjs";

const b64url = (buf) =>
  Buffer.from(buf).toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");

// Plain HS256 with the API key as `iss` and grants under `video` — the shape
// livekit-server's pkg/service/auth.go reads.
function mintToken(apiKey, apiSecret, grants) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const payload = b64url(
    JSON.stringify({ iss: apiKey, sub: "openconv-webhook-probe", nbf: now, exp: now + 600, video: grants }),
  );
  const signature = b64url(createHmac("sha256", apiSecret).update(`${header}.${payload}`).digest());
  return `${header}.${payload}.${signature}`;
}

/** The one boundary: everything below runs on values known to exist. */
function readConfig(env, argv) {
  const { xiApiKey, openconv } = readEnvironment(env, argv);
  // Shared, unlike the sibling scripts that fold the LiveKit pair into one combined
  // message: this one already sources its other variables through `readEnvironment`, so
  // its LiveKit check was a standalone copy with nothing holding it here.
  const { apiKey, apiSecret } = livekitCredentials(env);
  // The SFU's HTTP origin, which is what the Twirp room service answers on. Accepts the
  // wss:// form every other script takes so one argument shape works everywhere.
  const sfu = (argv[3] ?? "wss://livekit.sanctuary.gdn")
    .replace(/^wss:/, "https:")
    .replace(/^ws:/, "http:")
    .replace(/\/$/, "");
  return { xiApiKey, openconv, sfu, apiKey, apiSecret };
}

const { xiApiKey, openconv, sfu, apiKey, apiSecret } = readConfig(process.env, process.argv);
const checks = new Checks();

console.log(`openconv ${openconv}, sfu ${sfu}\n`);

// A user id nothing else shares, so the listing below cannot read another run's rows.
const userId = `u_webhook_${Math.random().toString(36).slice(2, 12)}`;

const minted = await fetch(
  `${openconv}/v1/convai/conversation/token?agent_id=agent_probe&participant_name=${userId}`,
  { headers: { "xi-api-key": xiApiKey } },
);
if (!minted.ok) {
  console.error(`minting failed: HTTP ${minted.status} ${await minted.text()}`);
  process.exit(1);
}

// The room name is the conversation id, so the JWT's `video.room` grant is where to
// read it — the response body carries only the token.
const { token } = await minted.json();
const conversationId = JSON.parse(Buffer.from(token.split(".")[1], "base64url")).video.room;
checks.record("a conversation was minted", Boolean(conversationId), conversationId);

// Held open briefly before closing. A room created and deleted inside the same second
// yields call_duration_secs=0, which would satisfy the duration check below while
// proving nothing about it — the number has to be able to come out wrong for its being
// right to mean anything.
await new Promise((resolve) => setTimeout(resolve, 3000));

const deleted = await fetch(`${sfu}/twirp/livekit.RoomService/DeleteRoom`, {
  method: "POST",
  headers: {
    Authorization: `Bearer ${mintToken(apiKey, apiSecret, { roomCreate: true, roomAdmin: true, room: conversationId })}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({ room: conversationId }),
});
checks.record("the SFU closed the room", deleted.ok, `HTTP ${deleted.status}`);

/** The conversation as openconv now reports it, or undefined if it is not listed. */
async function readBack() {
  const listed = await fetch(`${openconv}/v1/convai/conversations?user_id=${userId}`, {
    headers: { "xi-api-key": xiApiKey },
  });
  if (!listed.ok) throw new Error(`listing failed: HTTP ${listed.status} ${await listed.text()}`);
  const { conversations } = await listed.json();
  return conversations.find((row) => row.conversation_id === conversationId);
}

// Delivery is a network round trip the SFU makes on its own schedule, so the answer is
// polled rather than read once. A bounded wait that reports what it saw beats a sleep
// long enough to "probably" be safe.
const deadline = Date.now() + 30_000;
let conversation = await readBack();
while (Date.now() < deadline && conversation?.status !== "done") {
  await new Promise((resolve) => setTimeout(resolve, 1000));
  conversation = await readBack();
}

checks.record(
  "the SFU's delivery reached openconv",
  conversation?.status === "done",
  conversation ? `status=${conversation.status}` : "conversation not listed at all",
);

// The duration is the point of the whole link. Zero would mean a delivery arrived and
// carried nothing usable, which reads to Happy as a free call — so the room was held
// open above expressly so that zero is a failure rather than a coincidence.
checks.record(
  "the conversation has a real duration",
  Number.isInteger(conversation?.call_duration_secs) && conversation.call_duration_secs > 0,
  `call_duration_secs=${conversation?.call_duration_secs}`,
);

checks.finish();
