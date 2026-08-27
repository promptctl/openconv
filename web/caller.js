// Being the caller, in a browser: mint a token, join the room, publish the microphone,
// and hand back everything the agent sends. Nothing here touches the page — this
// decides what a call *is*, and `app.js` decides what one looks like.
//
// This is knowingly a second implementation of the handshake in
// `scripts/lib/caller.mjs`, whose own header warns that exactly this is how two clients
// drift apart. It is unavoidable rather than careless: that file drives
// `@livekit/rtc-node` and this drives `livekit-client` — two SDKs with different types
// for the same protocol, neither of which runs where the other does. What it costs is
// real, so the mitigation is shape: this follows the Node caller step for step (mint,
// recover the room from the token, connect, subscribe, publish one microphone) instead
// of finding its own way, which keeps any drift between them legible as a difference
// rather than buried in a different design. The Node scripts remain the acceptance
// authority; this page is for hearing what they can only assert.

// Vendored rather than fetched from a CDN at load time. An ES module import has no
// Subresource Integrity mechanism, so a pinned URL constrains which release is asked
// for and not which bytes come back — and this page holds an API key and an open
// microphone. It also means a deployment with no route to the public internet serves a
// page that works. See `vendor/PROVENANCE.md` for the version, source and digest.
import {
  createLocalAudioTrack,
  Room,
  RoomEvent,
  Track,
} from "./vendor/livekit-client.js";

/**
 * Mints a conversation token, which is also what dispatches the agent into the room.
 *
 * The same endpoint Happy's server calls, at the origin serving this page — a parallel
 * test-only mint would prove a path nobody in production takes.
 *
 * A parser rather than a check: it returns a token and the conversation that token
 * admits you to, or it throws carrying whatever the server said. There is no arm where
 * a caller receives an empty token and has to work out for itself whether the mint
 * happened.
 */
async function mint({ apiKey, agentId, participantName }) {
  const url = new URL("/v1/convai/conversation/token", location.origin);
  url.searchParams.set("agent_id", agentId);
  url.searchParams.set("participant_name", participantName);

  const response = await fetch(url, { headers: { "xi-api-key": apiKey } });
  const body = await response.text();
  if (!response.ok) {
    throw new Error(`mint failed: HTTP ${response.status} ${body}`);
  }

  const { token } = JSON.parse(body);

  // The conversation ID *is* the room name, which is what makes it recoverable from the
  // token the same way both of Happy's clients recover it.
  return { token, conversationId: decodeClaims(token).video.room };
}

/**
 * Reads a JWT's payload.
 *
 * `atob` decodes base64, and a JWT carries base64*url* — a different alphabet in two
 * characters. Feeding one to the other does not fail; it yields bytes that are wrong
 * only sometimes, depending on the claims, which is the worst way for a bug to behave.
 */
function decodeClaims(token) {
  const payload = token.split(".")[1].replaceAll("-", "+").replaceAll("_", "/");
  const bytes = Uint8Array.from(atob(payload), (character) => character.codePointAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
}

/** A conversation this browser has joined. Holding one means the join worked. */
export class Call {
  /**
   * Opens the microphone, mints, joins, starts talking, and starts listening.
   *
   * Either returns a joined call or throws, so the page never has a half-joined state
   * to ask about.
   *
   * The microphone is opened *first*, before anything is minted. Minting creates a room,
   * dispatches an agent into it and records a conversation that usage is billed
   * against — so prompting for the microphone afterwards means a denied permission
   * leaves an agent sitting alone in a room that will be charged for.
   */
  static async join({ livekitUrl, apiKey, agentId, participantName, onEvent, onTrack, onState }) {
    const microphone = await createLocalAudioTrack();
    const room = new Room();

    // Listeners are attached before connecting: a track can be subscribed and a control
    // event delivered during `connect`, and a handler registered afterwards waits for
    // events that have already fired — which reads as an agent that never spoke.
    room.on(RoomEvent.DataReceived, (payload) => onEvent(decodeEvent(payload)));
    room.on(RoomEvent.ConnectionStateChanged, onState);
    room.on(RoomEvent.TrackSubscribed, (track) => {
      // Video is not part of this protocol, but subscribing to one and attaching it to
      // an `<audio>` element would silently play nothing at all.
      if (track.kind === Track.Kind.Audio) onTrack(track);
    });

    try {
      const { token, conversationId } = await mint({ apiKey, agentId, participantName });
      await room.connect(livekitUrl, token);
      await room.localParticipant.publishTrack(microphone, {
        source: Track.Source.Microphone,
      });

      // Letting the agent's audio out of the browser's autoplay jail belongs inside the
      // join, on the click that caused it, which is the gesture browsers require.
      // Outside it there is a window in which a call is joined but not yet audible, and
      // a failure in that window leaves a live room nobody holds a handle to.
      //
      // A refusal is a value rather than a failure: a call whose transcript works and
      // whose audio is muted is still a call, and the page says which it got.
      const audible = await room.startAudio().then(() => room.canPlaybackAudio, () => false);

      return new Call(room, microphone, conversationId, audible);
    } catch (error) {
      // The room and the microphone are both live by now on some paths and not others,
      // and a page left holding either one has an open capture light and a participant
      // in a room it believes it left. Both are released unconditionally.
      microphone.stop();

      // A disconnect that fails while cleaning up must not become the story — "mint
      // failed: HTTP 401" is the useful sentence, not something about a socket, and a
      // room that never connected rejects here rather than being inert. So its outcome
      // is a value, empty when it worked, appended to the cause that actually brought
      // us here. Neither failure is dropped and neither hides the other.
      const alsoFailed = await room.disconnect().then(
        () => "",
        (failure) => ` (the room also failed to disconnect: ${failure.message})`,
      );
      throw new Error(`${error.message}${alsoFailed}`, { cause: error });
    }
  }

  constructor(room, microphone, conversationId, audible) {
    this.room = room;
    this.microphone = microphone;
    /** What the server logs this call under, so a call on screen can be found in a log. */
    this.conversationId = conversationId;
    /**
     * Whether the browser will actually play what arrives.
     *
     * Carried rather than assumed: false here is precisely the state where every track
     * is arriving correctly and the room is silent, which is indistinguishable from an
     * agent with nothing to say unless the page reports it.
     */
    this.audible = audible;
  }

  async leave() {
    this.microphone.stop();
    await this.room.disconnect();
  }
}

/**
 * Reads one control message off the data channel.
 *
 * A message that is not JSON comes back as a value rather than being dropped, because a
 * malformed control message is a finding: rendered, it names itself; discarded, it is
 * indistinguishable from an agent that said nothing.
 */
function decodeEvent(payload) {
  const text = new TextDecoder().decode(payload);
  try {
    return JSON.parse(text);
  } catch {
    return { type: "<not json>", raw: text };
  }
}
