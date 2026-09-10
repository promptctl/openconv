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

import { asksFor, AUDIBLE_MS, Caller, Checks, millis, recordSpeech } from "./lib/caller.mjs";

/**
 * The one boundary: everything downstream runs on values known to exist.
 * [LAW:parse-dont-validate] The account token is read rather than minted, because what is
 * under test is that a *real Happy account* reaches openconv — a token forged here would
 * prove only that the route parses. `~/.happy/access.key` holds the CLI's own credential,
 * whose `token` field is a valid happy-server bearer.
 *
 * The expected issuer is a value like the rest, not a literal in the check below, because
 * it is openconv's `LIVEKIT_API_KEY` — a deployment credential rather than a service name.
 * A rotation would otherwise turn this run permanently red for a reason that has nothing
 * to do with what it asserts. Reading the key itself is the one thing not done here: a
 * credential supplied by this script would prove only that openconv still mints.
 */
function readHappyEnvironment(env, argv) {
  const keyPath = env.HAPPY_ACCESS_KEY ?? join(homedir(), ".happy", "access.key");
  const raw = readFileSync(keyPath, "utf8");
  let token;
  try {
    ({ token } = JSON.parse(raw));
  } catch (cause) {
    throw new Error(`${keyPath} is not JSON`, { cause });
  }
  if (!token) throw new Error(`no bearer token in ${keyPath}`);
  return {
    token,
    happyServer: (argv[2] ?? "https://happy-server.sanctuary.gdn").replace(/\/$/, ""),
    livekitUrl: argv[3] ?? "wss://livekit.sanctuary.gdn",
    agentId: argv[4] ?? "agent_6701k211syvvegba4kt7m68nxjmw",
    issuer: env.OPENCONV_LIVEKIT_ISSUER ?? "openconv",
  };
}

/** A mint, or `null` for a body that is not one — never a mint-shaped object standing in
 *  for an answer that was not given. The raw body is what gets reported in that case. */
function mintOrNothing(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** A JWT's claims, or `null` for anything that is not one: absent, empty, the wrong number
 *  of segments, not base64url, not JSON. Asking instead whether a string *looks like* a
 *  token answers a question nothing downstream had, and throws away the one it did have. */
function claimsOf(token) {
  const [, payload, signature] = String(token).split(".");
  if (payload === undefined || signature === undefined) return null;
  try {
    return JSON.parse(Buffer.from(payload, "base64url").toString());
  } catch {
    return null;
  }
}

const { token, happyServer, livekitUrl, agentId, issuer } = readHappyEnvironment(process.env, process.argv);
const checks = new Checks();

const res = await fetch(`${happyServer}/v1/voice/conversations`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  body: JSON.stringify({ agentId }),
});
// A proxy's HTML 502, a plain-text 401, or — the one an argument typo actually produces —
// the webapp host's SPA catch-all answering 200 with index.html are all ordinary answers
// from a live deployment. Letting `res.json()` throw on one loses the body and the status,
// the two actionable facts, to a SyntaxError raised before the check meant to report them.
// The status is not the discriminator; whether the body is a mint is. [LAW:parse-dont-validate]
const body = await res.text();
const minted = mintOrNothing(body);

// One condition, read by both the check and the stop below, so the two cannot come to
// disagree about what a usable mint is — and a mint whose conversation cannot be decoded
// out of it is not one, whatever it says about itself. [LAW:one-source-of-truth]
const claims = claimsOf(minted?.conversationToken);
const usable = res.status === 200 && minted?.allowed === true && claims !== null;
checks.record(
  "happy-server minted a conversation for a real account",
  usable,
  `HTTP ${res.status} ${minted ? JSON.stringify({ ...minted, conversationToken: claims ? "<jwt>" : minted.conversationToken }) : JSON.stringify(body.slice(0, 300))}`,
);
// Nothing below can run without a token, and a caller that joined nothing would report
// its own failures as timeouts pointing at the agent. Stop where the truth is still local.
if (!usable) checks.finish();

// The room name openconv signs is the whole reason happy can name a conversation at all:
// happy pulls `conv_...` out of the JWT rather than being told it in a field. An
// ElevenLabs-signed token would clear an `allowed: true` check and fail this one.
checks.record(
  `the token happy handed back was signed by ${issuer}, for the room happy named`,
  claims.iss === issuer && claims.video?.room === minted.conversationId,
  `iss=${claims.iss} room=${claims.video?.room} conversationId=${minted.conversationId}`,
);

const caller = await Caller.at(livekitUrl, minted.conversationToken);
console.log(`joined ${minted.conversationId} at ${livekitUrl}\n`);

// The provider/SFU pairing, asserted as presence rather than as configuration: a token
// minted by one provider and offered to the other's SFU joins an empty room, quietly.
checks.record(
  "openconv's agent is in the room happy's token admits to",
  await caller.waitFor(() => caller.agentPresent(), 25_000, "the agent"),
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

checks.record(
  "the answer came back as sound in the room",
  await caller.waitFor(
    () => millis(caller.heard.audibleFrames - before.audibleFrames) >= AUDIBLE_MS,
    120_000,
    "the agent's speech",
  ),
  `${caller.heard.audibleFrames - before.audibleFrames} audible frames, peak ${caller.heard.peak}`,
);

// Zero frames from a reader that crashed and zero frames from a track nobody spoke into
// are the same number, and only this separates them.
checks.record(
  "the audio reader ran to the end of the call",
  caller.heard.error === null,
  caller.heard.error ? String(caller.heard.error) : `${caller.heard.frames} frames read`,
);

await caller.leave();
checks.finish();
