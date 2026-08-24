// Being the caller: what the ElevenLabs SDK does to openconv, with nothing asserted.
//
// The acceptance scripts differ in what they claim about a conversation, not in how
// they hold one — so joining, the control channel, the agent's audio, and speaking
// into the room live here once. A second script growing its own idea of how to join is
// the failure this module exists to prevent: two clients drifting apart means a green
// run proves whichever one happened to be right.
//
// Needs @livekit/rtc-node, which is not a dependency of the Rust crates:
//
//   npm install @livekit/rtc-node
//   NODE_PATH=/path/to/node_modules node scripts/<script>.mjs

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

import {
  AudioFrame,
  AudioSource,
  AudioStream,
  LocalAudioTrack,
  Room,
  RoomEvent,
  TrackPublishOptions,
  TrackSource,
} from "@livekit/rtc-node";

/// Frames arrive and are captured in ten-millisecond units, on both sides of the room.
const FRAMES_PER_SECOND = 100;

/// What counts as sound rather than the silence a muted or broken track carries.
///
/// A subscribed-but-silent track is exactly what a broken pump looks like, and it is
/// indistinguishable from a working one until you measure the samples.
const AUDIBLE = 1000;

/**
 * A 16-bit mono PCM recording, which is the only thing that can be spoken into a room.
 *
 * A parser rather than a check: a `Recording` cannot be built out of a stereo file, a
 * float file, or a JPEG, so nothing downstream asks what format it holds. Anything
 * else fails here, loudly, naming what it actually found.
 */
export function readRecording(path) {
  const bytes = readFileSync(path);
  const tag = (at) => bytes.toString("ascii", at, at + 4);

  if (tag(0) !== "RIFF" || tag(8) !== "WAVE") {
    throw new Error(`${path} is not a RIFF/WAVE file (starts ${tag(0)}/${tag(8)})`);
  }

  // Walked rather than assumed at offset 44: `say` writes a WAV whose data chunk is not
  // always the first one, and reading a LIST chunk as samples yields noise that looks
  // like a broken microphone rather than a broken parser.
  let format = null;
  let samples = null;
  for (let at = 12; at + 8 <= bytes.length; ) {
    const chunk = tag(at);
    const size = bytes.readUInt32LE(at + 4);
    const body = at + 8;

    if (chunk === "fmt ") {
      format = {
        encoding: bytes.readUInt16LE(body),
        channels: bytes.readUInt16LE(body + 2),
        sampleRate: bytes.readUInt32LE(body + 4),
        bitsPerSample: bytes.readUInt16LE(body + 14),
      };
    }
    if (chunk === "data") {
      samples = new Int16Array(
        bytes.buffer.slice(bytes.byteOffset + body, bytes.byteOffset + body + size),
      );
    }
    at = body + size + (size % 2); // chunks are word-aligned
  }

  if (!format || !samples) throw new Error(`${path} has no fmt/data chunk`);
  if (format.encoding !== 1 || format.channels !== 1 || format.bitsPerSample !== 16) {
    throw new Error(
      `${path} must be 16-bit mono PCM, not encoding=${format.encoding} ` +
        `channels=${format.channels} bits=${format.bitsPerSample}`,
    );
  }
  return { sampleRate: format.sampleRate, samples };
}

/**
 * Records a line of speech with macOS `say`, in the format a room takes.
 *
 * A second voice, distinct from the agent's, so a check that the caller was heard
 * cannot be satisfied by the agent hearing itself.
 */
export function recordSpeech(line, path, { sampleRate = 48_000 } = {}) {
  execFileSync("say", ["-o", path, "--data-format", `LEI16@${sampleRate}`, line]);
  return readRecording(path);
}

/** A client that has joined a conversation. Reaching one means the join worked. */
export class Caller {
  /**
   * Mints a token — which is also what dispatches the agent — and joins the room.
   *
   * Either returns a connected caller or throws. There is no half-joined state for a
   * script to have to ask about.
   */
  static async join({
    openconv,
    livekitUrl,
    xiApiKey,
    participantName = "u_acceptance",
    agentId = "agent_happy",
  }) {
    const mint = new URL(`${openconv.replace(/\/$/, "")}/v1/convai/conversation/token`);
    mint.searchParams.set("agent_id", agentId);
    mint.searchParams.set("participant_name", participantName);

    const response = await fetch(mint, { headers: { "xi-api-key": xiApiKey } });
    if (!response.ok) {
      throw new Error(`mint failed: HTTP ${response.status} ${await response.text()}`);
    }
    const { token } = await response.json();

    // The conversation ID *is* the room name, which is what makes it recoverable from
    // the token the same way both of Happy's clients recover it.
    const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64").toString());
    const caller = new Caller(claims.video.room, livekitUrl);

    await Promise.race([
      caller.room.connect(livekitUrl, token, { autoSubscribe: true, dynacast: false }),
      rejectAfter(20_000, "the room connection"),
    ]);
    return caller;
  }

  constructor(conversationId, livekitUrl) {
    this.conversationId = conversationId;
    this.livekitUrl = livekitUrl;
    this.room = new Room();

    /** Every control event the agent published, in arrival order. */
    this.controlEvents = [];
    /** Identities seen joining. Consult `roster()` too — the agent is often here first. */
    this.participants = [];
    /**
     * What the agent's track has carried so far, and whatever stopped it carrying.
     *
     * `error` is kept rather than swallowed: a reader that died on frame one leaves the
     * same zero counts as a track that was never spoken into, and only this tells them
     * apart.
     */
    this.heard = { frames: 0, audibleFrames: 0, peak: 0, error: null };
    this.agentTrack = null;

    this.room.on(RoomEvent.DataReceived, (payload) => {
      const text = new TextDecoder().decode(payload);
      try {
        this.controlEvents.push(JSON.parse(text));
      } catch {
        // Kept as a value rather than dropped: a malformed control message is a finding,
        // and a script asserting on `type` will name it rather than time out.
        this.controlEvents.push({ type: "<not json>", raw: text });
      }
    });
    this.room.on(RoomEvent.ParticipantConnected, (p) => this.participants.push(p.identity));

    // Read from the moment the track is subscribed, the way a client that is playing
    // audio does. Reading later instead — after the assertions that come first in a
    // script — misses whatever the agent said on subscribe, and reports a working track
    // as silent.
    this.room.on(RoomEvent.TrackSubscribed, (track) => {
      if (this.agentTrack) return;
      this.agentTrack = track;
      (async () => {
        for await (const frame of new AudioStream(track)) {
          this.heard.frames += 1;
          let peak = 0;
          for (const sample of frame.data) peak = Math.max(peak, Math.abs(sample));
          this.heard.peak = Math.max(this.heard.peak, peak);
          if (peak > AUDIBLE) this.heard.audibleFrames += 1;
        }
      })().catch((error) => {
        this.heard.error = error;
      });
    });
  }

  /** The remote participants currently in the room. */
  roster() {
    return Array.from(this.room.remoteParticipants.values()).map((p) => p.identity);
  }

  /** True once an agent is present, whether it joined before or after this caller. */
  agentPresent() {
    const isAgent = (identity) => identity.startsWith("agent_");
    return this.participants.some(isAgent) || this.roster().some(isAgent);
  }

  /** Publishes one client control event, as the SDK does over the data channel. */
  async send(event) {
    await this.room.localParticipant.publishData(
      new TextEncoder().encode(JSON.stringify(event)),
      { reliable: true },
    );
  }

  /** A copy of `heard`, so a script can measure what arrived after a given moment. */
  mark() {
    return { ...this.heard };
  }

  /**
   * Says a recording into the room on a microphone track, and returns once the last
   * sample has actually played out.
   *
   * Publishing a track and being subscribed to it are different moments, and speaking
   * into the gap loses the opening words — which looks exactly like a transcription
   * error and is not one. So the wait here is for the subscription itself, which the
   * room reports, rather than for a duration long enough that it has probably happened.
   *
   * Silence is part of the utterance, not padding around it. The agent decides a turn
   * ended by counting 600 ms of quiet frames, so a recording that stops when the words
   * do leaves the endpointer waiting for frames that never come, and the caller is
   * never answered. The lead-in is what the adaptive noise floor settles against.
   */
  async speak(recording, { leadInMs = 1000, tailOffMs = 1500 } = {}) {
    const { sampleRate, samples } = recording;
    const silence = (ms) => new Int16Array((sampleRate * ms) / 1000);

    const utterance = concat([silence(leadInMs), samples, silence(tailOffMs)]);
    const source = new AudioSource(sampleRate, 1);
    const track = LocalAudioTrack.createAudioTrack("caller-mic", source);

    // Registered before publishing: the agent can subscribe before `publishTrack`
    // resolves, and a listener attached afterwards would wait for an event that has
    // already fired.
    const subscribed = new Promise((resolve) =>
      this.room.once(RoomEvent.LocalTrackSubscribed, resolve),
    );

    await this.room.localParticipant.publishTrack(
      track,
      new TrackPublishOptions({ source: TrackSource.SOURCE_MICROPHONE }),
    );
    await Promise.race([subscribed, rejectAfter(20_000, "the agent to subscribe to the caller")]);

    // `captureFrame` resolves when the queue has room, so awaiting it is what paces the
    // track at real time — there is no sleep here guessing at the frame rate.
    const perFrame = sampleRate / FRAMES_PER_SECOND;
    for (let at = 0; at < utterance.length; at += perFrame) {
      // Copied, not sliced. `AudioFrame.protoInfo` hands the FFI `this.data.buffer` and
      // ignores the view's byteOffset, so a subarray sends the start of the whole
      // recording every time — a track that carries the opening silence for its entire
      // length, and an agent that reports hearing nothing at all.
      const chunk = Int16Array.from(utterance.subarray(at, Math.min(at + perFrame, utterance.length)));
      await source.captureFrame(new AudioFrame(chunk, sampleRate, 1, chunk.length));
    }
    await source.waitForPlayout();

    return utterance.length / sampleRate;
  }

  async leave() {
    await this.room.disconnect();
  }

  /** The first control event of a given type, or undefined. */
  control(type) {
    return this.controlEvents.find((event) => event.type === type);
  }

  /**
   * Polls until `predicate` holds, and says so when it never does.
   *
   * Returns a boolean rather than throwing because every caller is a check: a script
   * reports "the agent never answered" as a failed assertion, not as a stack trace.
   */
  async waitFor(predicate, ms, what) {
    const until = Date.now() + ms;
    while (Date.now() < until) {
      if (predicate()) return true;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    console.error(`  (gave up after ${ms / 1000}s waiting for ${what})`);
    return false;
  }
}

function concat(parts) {
  const total = parts.reduce((sum, part) => sum + part.length, 0);
  const out = new Int16Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function rejectAfter(ms, what) {
  return new Promise((_, reject) =>
    setTimeout(() => reject(new Error(`timed out waiting for ${what}`)), ms),
  );
}

/** Collects pass/fail so a script's exit code says what its output said. */
export class Checks {
  constructor() {
    this.results = [];
  }

  record(name, ok, detail = "") {
    this.results.push({ name, ok });
    console.log(`${ok ? "  ok  " : " FAIL "} ${name}${detail ? ` — ${detail}` : ""}`);
    return ok;
  }

  /** Prints the tally and exits non-zero if anything failed. */
  finish() {
    const failed = this.results.filter((result) => !result.ok);
    console.log(`\n${this.results.length - failed.length}/${this.results.length} checks passed`);
    if (failed.length > 0) {
      console.error(`FAILED: ${failed.map((result) => result.name).join("; ")}`);
      process.exit(1);
    }
    process.exit(0);
  }
}

/** The one boundary: everything downstream runs on values known to exist. */
export function readEnvironment(env, argv) {
  const xiApiKey = env.OPENCONV_API_KEY;
  if (!xiApiKey) throw new Error("missing OPENCONV_API_KEY");
  return {
    xiApiKey,
    openconv: (argv[2] ?? "http://127.0.0.1:8080").replace(/\/$/, ""),
    livekitUrl: argv[3] ?? "wss://livekit.sanctuary.gdn",
  };
}
