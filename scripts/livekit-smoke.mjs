// Proves the deployed LiveKit accepts credentials openconv holds, by calling ListRooms
// with a token minted from them.
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

import { Rooms } from "./lib/livekit.mjs";

// The one boundary: everything below runs on values that are known to exist.
function readConfig(env, argv) {
  const missing = ["LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"].filter((name) => !env[name]);
  if (missing.length > 0) {
    throw new Error(`missing ${missing.join(" and ")} — read them from Vault at secret/livekit`);
  }
  return {
    apiKey: env.LIVEKIT_API_KEY,
    apiSecret: env.LIVEKIT_API_SECRET,
    url: argv[2] ?? "https://livekit.sanctuary.gdn",
  };
}

const { apiKey, apiSecret, url } = readConfig(process.env, process.argv);
const rooms = new Rooms({ url, apiKey, apiSecret });

// `call` throws on anything but a 2xx, naming the status and the body — a 401 means the
// SFU is up and rejected our signature, which is a different problem from it not being
// there, and the message says which one happened.
const { rooms: open = [] } = await rooms.call("ListRooms", {});
console.log(`ListRooms on ${rooms.url} as "${apiKey}": HTTP 200, ${open.length} room(s) open`);
