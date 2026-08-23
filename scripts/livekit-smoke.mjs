// Proves the deployed LiveKit accepts credentials openconv holds, by minting a
// room-list token and calling ListRooms with it.
//
//   LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... node scripts/livekit-smoke.mjs [url]
//
// Both values live in Vault at secret/livekit. Read them on the ops VM — the Mac
// blanks secret-shaped values out of JSON API responses:
//
//   ssh ops 'cd ~/homelab-infra && VAULT_ADDR=http://192.168.7.217:8200 \
//     VAULT_TOKEN=<root> nix develop .# --command vault kv get -field=api_secret secret/livekit'
//
// A failure here separates two things that otherwise look alike from the app: the
// SFU rejecting our signature, and the SFU not being reachable at all.

import { createHmac } from "node:crypto";

const b64url = (buf) =>
  Buffer.from(buf).toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");

// LiveKit's server API is plain HS256 with the API key as `iss` and the grants
// under `video` — the key names come from EnsureListPermission in the server's
// pkg/service/auth.go, which reads claims.Video.RoomList and nothing else.
function mintToken(apiKey, apiSecret, grants) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const payload = b64url(
    JSON.stringify({ iss: apiKey, sub: "openconv-smoke", nbf: now, exp: now + 600, video: grants }),
  );
  const signature = b64url(createHmac("sha256", apiSecret).update(`${header}.${payload}`).digest());
  return `${header}.${payload}.${signature}`;
}

// The one boundary: everything below runs on values that are known to exist.
function readConfig(env, argv) {
  const apiKey = env.LIVEKIT_API_KEY;
  const apiSecret = env.LIVEKIT_API_SECRET;
  const missing = ["LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"].filter((name) => !env[name]);
  if (missing.length > 0) {
    throw new Error(`missing ${missing.join(" and ")} — read them from Vault at secret/livekit`);
  }
  return { apiKey, apiSecret, url: (argv[2] ?? "https://livekit.sanctuary.gdn").replace(/\/$/, "") };
}

const { apiKey, apiSecret, url } = readConfig(process.env, process.argv);
const token = mintToken(apiKey, apiSecret, { roomList: true });

const response = await fetch(`${url}/twirp/livekit.RoomService/ListRooms`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  body: "{}",
});

const body = await response.text();
if (!response.ok) {
  // 401 here means the SFU is up and rejected our signature — a different problem
  // from a connection error, so say which one happened.
  console.error(`ListRooms on ${url} failed: HTTP ${response.status} ${body}`);
  process.exit(1);
}

const rooms = JSON.parse(body).rooms ?? [];
console.log(`ListRooms on ${url} as "${apiKey}": HTTP 200, ${rooms.length} room(s) open`);
