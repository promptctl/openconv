// Verifies the metered voice path Happy's own clients take, from Happy's server outward.
//
//   node scripts/happy-metered-path-acceptance.mjs [happy-server-url] [livekit-ws-url] [agent-id]
//
// Needs @livekit/rtc-node and macOS `say`, like its siblings:
//
//   NODE_PATH=/path/to/node_modules node scripts/happy-metered-path-acceptance.mjs
//
// `live-call-acceptance.mjs` mints against openconv directly, so it proves openconv and
// nothing about who is allowed to reach it. This mints through the *deployed happy-server*
// with a real Happy account token, so the legs it adds are the ones that only exist once
// happy is pointed here: happy's `VOICE_CONVAI_ORIGIN`, its usage gate, the shared secret
// it presents as `xi-api-key`, and the `conv_` id it recovers from the JWT rather than
// being handed in a field. Only the browser SDK itself is left uncovered.
//
// It takes no API key. That is the point of the run: the credential under test is the one
// happy-server holds, and a key supplied here would prove only that openconv still mints.
//
// The three spellings of "which provider" — happy-server's origin, the native bundle's
// SFU, and the SFU baked into the webapp image — are checked nowhere by construction
// (openconv-openconv-bwy.15). A mismatch does not error: the token is a JWT signed by one
// provider's LiveKit keys, and offered to the other's SFU it joins a room the agent is not
// in. This script is what turns that silence into a failed check, because it asserts the
// agent is present in the room happy's own token admits it to.

import { mkdtempSync, readFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

import { asksFor, Caller, Checks, millis, recordSpeech } from "./lib/caller.mjs";

/**
 * The one boundary: everything downstream runs on values known to exist.
 * [LAW:parse-dont-validate] The account token is read rather than minted, because what is
 * under test is that a *real Happy account* reaches openconv — a token forged here would
 * prove only that the route parses. `~/.happy/access.key` holds the CLI's own credential,
 * whose `token` field is a valid happy-server bearer.
 */
function readHappyEnvironment(env, argv) {
  const keyPath = env.HAPPY_ACCESS_KEY ?? join(homedir(), ".happy", "access.key");
  const { token } = JSON.parse(readFileSync(keyPath, "utf8"));
  if (!token) throw new Error(`no bearer token in ${keyPath}`);
  return {
    token,
    happyServer: (argv[2] ?? "https://happy-server.sanctuary.gdn").replace(/\/$/, ""),
    livekitUrl: argv[3] ?? "wss://livekit.sanctuary.gdn",
    agentId: argv[4] ?? "agent_6701k211syvvegba4kt7m68nxjmw",
  };
}

const { token, happyServer, livekitUrl, agentId } = readHappyEnvironment(process.env, process.argv);
const checks = new Checks();

const res = await fetch(`${happyServer}/v1/voice/conversations`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  body: JSON.stringify({ agentId }),
});
const minted = await res.json();

checks.record(
  "happy-server minted a conversation for a real account",
  res.status === 200 && minted.allowed === true,
  `HTTP ${res.status} ${JSON.stringify({ ...minted, conversationToken: minted.conversationToken ? "<jwt>" : undefined })}`,
);
// Nothing below can run without a token, and a caller that joined nothing would report
// its own failures as timeouts pointing at the agent. Stop where the truth is still local.
if (!minted.allowed) checks.finish();

// The room name openconv signs is the whole reason happy can name a conversation at all:
// happy pulls `conv_...` out of the JWT rather than being told it in a field. An
// ElevenLabs-signed token would clear an `allowed: true` check and fail this one.
const claims = JSON.parse(Buffer.from(minted.conversationToken.split(".")[1], "base64").toString());
checks.record(
  "the token happy handed back was signed by openconv, for the room happy named",
  claims.iss === "openconv" && claims.video?.room === minted.conversationId,
  `iss=${claims.iss} room=${claims.video?.room} conversationId=${minted.conversationId}`,
);

const caller = await Caller.at(livekitUrl, minted.conversationToken);
console.log(`joined ${minted.conversationId} at ${livekitUrl}\n`);

// The provider/SFU pairing, asserted as presence rather than as configuration: a token
// minted by one provider and offered to the other's SFU joins an empty room, quietly.
checks.record(
  "openconv's agent is in the room happy's token admits to",
  await caller.waitFor(() => caller.roster().some((name) => name.startsWith("agent_")), 25_000, "the agent"),
  caller.roster().join(", "),
);

const before = caller.mark();
const { line, said } = asksFor();
const recording = recordSpeech(line, join(mkdtempSync(join(tmpdir(), "happy-path-")), "caller.wav"));
console.log(`the caller will say: "${line}"`);

const spoken = await caller.speak(recording);
console.log(`spoke ${spoken.toFixed(1)}s into the room, waiting to be answered\n`);

checks.record(
  "the caller's words reached speech-to-text",
  await caller.waitFor(() => caller.transcripts().some(said), 60_000, "a final transcript"),
  JSON.stringify(caller.transcripts()),
);

checks.record(
  "the agent answered what the caller actually said",
  await caller.waitFor(() => caller.replies().some(said), 60_000, "the reply"),
  JSON.stringify(caller.replies()),
);

// Two hundred milliseconds is what separates a spoken word from a click, and the same bar
// `live-call-acceptance` sets, for the same reason: the reply here is deliberately one
// word, so a bar set to the length of some particular answer fails on a short audible one.
const AUDIBLE_MS = 200;
checks.record(
  "the answer came back as sound in the room",
  await caller.waitFor(
    () => millis(caller.heard.audibleFrames - before.audibleFrames) >= AUDIBLE_MS,
    120_000,
    "the agent's speech",
  ),
  `${caller.heard.audibleFrames - before.audibleFrames} audible frames, peak ${caller.heard.peak}`,
);

await caller.leave();
checks.finish();
