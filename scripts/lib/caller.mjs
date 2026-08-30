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
 * The line the acceptance runs speak into a room.
 *
 * [LAW:one-source-of-truth] One sentence, because two scripts assert on it *being the
 * same one*: `stt-acceptance` claims the agent transcribes it, and `loopback-acceptance`
 * claims the transport carries it, and the second only bounds the first if they are
 * speaking the same words. Two copies could drift into a probe that clears a sentence
 * nobody sends while still describing itself as testing what the acceptance runs test.
 *
 * Its words are chosen for the speech model, not for the reader: base.en hears them all
 * correctly, which a nonce word is not guaranteed to be (it hears "penguin" as "pen win").
 */
export const SPOKEN = "Hello, can you hear me? This is a test of the voice agent.";

/** What a count of frames is worth in milliseconds, at the one rate a room works at. */
export const millis = (frames) => (frames * 1000) / FRAMES_PER_SECOND;

/**
 * What a run of samples sounds like: how long it is, how much of it is sound rather than
 * silence, and the loudest it ever gets.
 *
 * [LAW:one-source-of-truth] One reading, because the two questions it answers only mean
 * something against each other — what a track delivered, and what the recording put on
 * it. A subscriber that measured "audible" on a different threshold, or over windows of a
 * different length, would report a healthy track as a lossy one and no assertion built on
 * the pair could tell which side was wrong.
 *
 * Counted in 10 ms windows rather than over the whole run, so `audibleFrames` is a
 * duration and not a verdict: a track that carried the first second of a four-second
 * sentence is the symptom being chased here, and a peak taken across the whole thing
 * cannot see it.
 */
export function sounding(samples, sampleRate) {
  // Rounded, because a window has to be a whole number of samples: 22 050 Hz divides into
  // 220.5, and a fractional stride walks off the end of the array into `undefined`, whose
  // absolute value is NaN — a peak that compares false against every threshold and reports
  // a loud track as silent. Every rate the recordings use divides evenly; the one that
  // does not is the one that would have been believed.
  const perFrame = Math.round(sampleRate / FRAMES_PER_SECOND);

  // [LAW:no-defensive-null-guards] exception: an inland check, and named as one. A rate
  // that rounds to zero samples per window makes the loop below step by zero and run
  // forever, and `readRecording` — the parser that would properly refuse it — is not the
  // only source here; `frame.sampleRate` arrives off the SFU with no parser of ours in
  // front of it. A hang is the one failure that reaches no log at all, so it is worth an
  // unproven guard to turn it into a sentence.
  if (!(perFrame > 0)) {
    throw new RangeError(`a ${sampleRate} Hz rate is less than one sample per 10 ms window`);
  }

  const reading = { frames: 0, audibleFrames: 0, peak: 0 };

  for (let at = 0; at < samples.length; at += perFrame) {
    const end = Math.min(at + perFrame, samples.length);
    let peak = 0;
    for (let sample = at; sample < end; sample += 1) {
      peak = Math.max(peak, Math.abs(samples[sample]));
    }
    reading.frames += 1;
    reading.peak = Math.max(reading.peak, peak);
    if (peak > AUDIBLE) reading.audibleFrames += 1;
  }
  return reading;
}

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
  // The one field of `fmt ` this parser used to read without constraining, which left a
  // zero rate representable in the `Recording` it hands out — and downstream that is not a
  // wrong number but a hung process, since it divides into zero samples per window.
  if (!(format.sampleRate > 0)) {
    throw new Error(`${path} declares a ${format.sampleRate} Hz sample rate`);
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
    return Caller.at(livekitUrl, token);
  }

  /**
   * Joins whatever room a token already names.
   *
   * Split from `join` so that being a client in a room does not require openconv to have
   * minted the token — a probe measuring the transport itself needs a room with no agent
   * in it, and building a second client to get one would mean measuring a different
   * program than the acceptance scripts run.
   *
   * The room name is read from the token rather than passed beside it, because the two
   * cannot then disagree. For a conversation that name *is* the conversation ID, which is
   * how both of Happy's clients recover it.
   */
  static async at(livekitUrl, token) {
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
    this.heard = { frames: 0, audibleFrames: 0, peak: 0, lastAudibleAt: 0, error: null };
    this.remoteTrack = null;
    this.mic = null;

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

    /**
     * Who the SFU has said is talking.
     *
     * A different fact from `heard`, and the only other one available: LiveKit decides
     * this from the audio-level header the publisher's own libwebrtc stamps on each RTP
     * packet, so it is a reading taken upstream of this client's decoder and upstream of
     * the network between them. A participant the SFU calls loud whose track arrives here
     * silent, and one the SFU never mentions at all, are the same symptom with opposite
     * causes, and nothing in the decoded audio can tell them apart.
     */
    this.reportedSpeaking = new Set();
    this.room.on(RoomEvent.ActiveSpeakersChanged, (speakers) => {
      for (const speaker of speakers) this.reportedSpeaking.add(speaker.identity);
    });

    // Read from the moment the track is subscribed, the way a client that is playing
    // audio does. Reading later instead — after the assertions that come first in a
    // script — misses whatever arrived on subscribe, and reports a working track as
    // silent. Whoever is publishing: an agent in the acceptance runs, another probe
    // client in `loopback-acceptance`, which subscribes through this same handler.
    this.room.on(RoomEvent.TrackSubscribed, (track) => {
      if (this.remoteTrack) return;
      this.remoteTrack = track;
      (async () => {
        for await (const frame of new AudioStream(track)) {
          const arrived = sounding(frame.data, frame.sampleRate);
          this.heard.frames += arrived.frames;
          this.heard.audibleFrames += arrived.audibleFrames;
          this.heard.peak = Math.max(this.heard.peak, arrived.peak);
          if (arrived.audibleFrames > 0) {
            // When the remote was last actually making a sound. What "stopped talking"
            // is measured against — the frames keep arriving after it stops, they just
            // carry silence, so counting frames cannot tell the two apart.
            this.heard.lastAudibleAt = Date.now();
          }
        }
      })().catch((error) => {
        this.heard.error = error;
      });
    });
  }

  /**
   * True once this client has a remote track to listen to.
   *
   * Being in a room and being subscribed to what is published in it are different
   * moments, and a script that speaks in the gap between them is heard by nobody — which
   * looks exactly like the transport losing the audio, and is not.
   */
  subscribed() {
    return this.remoteTrack !== null;
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
   * The caller's microphone, published and subscribed to, ready to speak into.
   *
   * Opened once and kept: a caller who talks twice has one mouth, and republishing a
   * track per utterance is both unlike any real client and unable to express the one
   * thing barge-in needs — starting to talk while the agent still is.
   *
   * Publishing a track and being subscribed to it are different moments, and speaking
   * into the gap loses the opening words, which looks exactly like a transcription error
   * and is not one. So the wait is for the subscription itself, which the room reports,
   * rather than for a duration long enough that it has probably happened.
   */
  async microphone(sampleRate = 48_000) {
    if (this.mic) return this.mic;

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
    // Named at the level of what is actually awaited — `LocalTrackSubscribed`, i.e. some
    // remote participant — rather than "the agent". Who that participant turns out to be is
    // the caller's topology, not this method's: `loopback-acceptance` publishes into a room
    // with two listeners and no agent at all, and a message naming the agent would send the
    // one probe built to stop misattribution off blaming a participant that does not exist.
    await Promise.race([
      subscribed,
      rejectAfter(20_000, "a remote participant to subscribe to the caller's microphone"),
    ]);

    this.mic = new Microphone(source, sampleRate);
    return this.mic;
  }

  /**
   * Says a recording into the room, and returns once the last sample has played out.
   *
   * Silence is part of the utterance, not padding around it. The agent decides a turn
   * ended by counting 600 ms of quiet frames, so a recording that stops when the words
   * do leaves the endpointer waiting for frames that never come, and the caller is never
   * answered. The lead-in gives the agent a stretch of established silence to open an
   * utterance against.
   */
  async speak(recording, options = {}) {
    const mic = await this.microphone(recording.sampleRate);
    return mic.say(recording, options);
  }

  async leave() {
    await this.room.disconnect();
  }

  /** Every control event of a given type, in arrival order. */
  events(type) {
    return this.controlEvents.filter((event) => event.type === type);
  }

  /** The first control event of a given type, or undefined. */
  control(type) {
    return this.events(type)[0];
  }

  /**
   * What openconv wraps each settled transcript in: the words, and the id a client
   * correlates a turn on.
   *
   * [LAW:one-source-of-truth] `user_transcription_event` is spelled here and nowhere
   * else, so a rename on the Rust side has one place to break. Handing back the payload
   * rather than one field of it means a script asserting on some other part of it needs
   * nothing new here. `inbound-text-acceptance` also selects transcript events, but only
   * on `type` — it never reaches into the payload, so this key genuinely has one owner.
   *
   * Unparsed, unlike `transcripts()`: whether a transcript carries an `event_id` is a
   * claim `stt-acceptance` exists to make, and refusing here would crash the script that
   * came to ask rather than let it report the answer.
   */
  transcriptEvents() {
    return this.events("user_transcript").map((event) => event.user_transcription_event);
  }

  /** What the caller has been heard to say — settled transcripts, not tentative ones. */
  transcripts() {
    return this.transcriptEvents().map((payload) => requireText(payload, "user_transcript"));
  }

  /**
   * What the agent has said, as text.
   *
   * Published before the words are synthesized, so this answers "did it reply" and never
   * "was the reply audible" — that claim is `heard`, and the two fail separately.
   *
   * [LAW:one-source-of-truth] `tools-` and `inbound-text-acceptance` still spell
   * `agent_response_event.agent_response` out themselves and are the remaining copies of
   * this read.
   */
  replies() {
    return this.events("agent_response").map((event) =>
      requireText(event.agent_response_event, "agent_response"),
    );
  }

  /**
   * Polls until `predicate` holds, and says so when it never does.
   *
   * The thing not happening returns `false` rather than throwing, because every caller is
   * a check: a script reports "the agent never answered" as a failed assertion, not as a
   * stack trace.
   *
   * A predicate that meets a malformed control event still throws, and is meant to. That
   * is a different fact with a different cause — openconv published something the
   * protocol does not allow, rather than the agent staying quiet — and reporting it as a
   * failed check would send the next reader to the wrong end of the system.
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

/**
 * A published microphone track. Everything said into the room goes through one.
 *
 * Separate from `Caller` because a caller has one and a script may hold it across
 * several utterances — which is what talking over the agent requires.
 */
class Microphone {
  constructor(source, sampleRate) {
    this.source = source;
    this.sampleRate = sampleRate;
  }

  /** Speaks one recording, and resolves when the last sample has played out. */
  async say(recording, { leadInMs = 1000, tailOffMs = 1500 } = {}) {
    if (recording.sampleRate !== this.sampleRate) {
      throw new Error(
        `recording is ${recording.sampleRate} Hz but the microphone is ${this.sampleRate} Hz`,
      );
    }
    await this.emit(
      concat([this.silence(leadInMs), recording.samples, this.silence(tailOffMs)]),
    );
    return recording.samples.length / this.sampleRate;
  }

  silence(ms) {
    return new Int16Array((this.sampleRate * ms) / 1000);
  }

  /**
   * Pushes samples onto the track in real time.
   *
   * `captureFrame` resolves when the queue has room, so awaiting it is what paces the
   * track — there is no sleep here guessing at the frame rate.
   */
  async emit(samples) {
    const perFrame = this.sampleRate / FRAMES_PER_SECOND;
    for (let at = 0; at < samples.length; at += perFrame) {
      // Copied, not sliced. `AudioFrame.protoInfo` hands the FFI `this.data.buffer` and
      // ignores the view's byteOffset, so a subarray sends the start of the whole
      // recording every time — a track that carries the opening silence for its entire
      // length, and an agent that reports hearing nothing at all.
      const chunk = Int16Array.from(samples.subarray(at, Math.min(at + perFrame, samples.length)));
      await this.source.captureFrame(new AudioFrame(chunk, this.sampleRate, 1, chunk.length));
    }
    await this.source.waitForPlayout();
  }
}

/**
 * The words a control event carries, or a refusal naming what was missing instead.
 *
 * [LAW:parse-dont-validate] The one place an event becomes a string. Everything
 * downstream is handed a `string`, so no script checks — there is nothing left to check.
 *
 * Absent is not empty, and the whole point is to keep them apart: a settled transcript of
 * silence *is* `""` and means the caller said nothing, while a missing field means
 * openconv published a malformed event. Collapsing the two would let a protocol bug reach
 * a script as a quiet transcription failure and be reported as one.
 */
function requireText(payload, field) {
  const text = payload?.[field];
  if (typeof text !== "string") {
    throw new TypeError(`control event carried no ${field} — found ${JSON.stringify(text)}`);
  }
  return text;
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
