// Being the caller, in a browser: mint a token, join the room, publish the microphone,
// and hand back everything the agent sends. Nothing here touches the page — this decides
// what a call *is*, and `app.js` decides what one looks like.
//
// The handshake itself is not here. Minting, joining and telling the agent what the
// conversation is live in `conversation.js`, which the acceptance runs drive too; this
// file is the browser half of the transport seam that module names, plus the things only
// a browser has — a microphone to open, an autoplay policy to get past, and a person who
// may change their mind about the voice while the call is running.
//
// That split is the whole point. This page used to hold its own copy of the handshake and
// follow the Node caller "step for step" by the diligence of whoever edited both, which
// worked until it didn't: the scripts learned to publish the session configuration and
// the page did not, so every conversation a person actually held ran on the default
// prompt while a green acceptance suite reported otherwise. [LAW:one-source-of-truth]

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

import { NotTold, conversationWith } from "./conversation.js";

/**
 * Reports a failure nobody is waiting on, by letting the page's own reporter have it.
 *
 * Re-raised rather than swallowed: a caller who is about to hear the wrong voice should
 * be told, and the banner in `index.html` is where this page says so — reached as an
 * uncaught exception, so by that page's capturing `error` listener and not its
 * `unhandledrejection` one. Detached because the awaiting code has decided the failure is
 * not worth its own operation. [LAW:no-silent-failure]
 */
const reportDetached = (failure) =>
  queueMicrotask(() => {
    throw failure;
  });

/**
 * Lets a configure that failed cost this call the voice that was chosen, not the call.
 *
 * The room is up and the agent is in it by the time this can happen — only the message
 * saying what the conversation is did not land, which leaves a working call running the
 * deployment's defaults. Tearing that down answers a dropdown with a hang-up, and the
 * agent the mint dispatched is in the room being paid for either way.
 *
 * The conversation comes off the failure rather than being recovered from somewhere else,
 * because a `NotTold` is precisely the failure that has one. [LAW:one-source-of-truth]
 *
 * Every other failure is a call that never opened and is re-raised untouched, for `join`'s
 * own cleanup to release the room and the microphone. Reported either way, never swallowed.
 * [LAW:no-silent-failure]
 */
const keepTheCall = (failure) => {
  if (!(failure instanceof NotTold)) throw failure;

  reportDetached(failure);
  return failure.conversationId;
};

/**
 * The three room operations `conversation.js` drives, in this SDK's terms.
 *
 * The whole of what the shared handshake knows about being in a browser. Everything else
 * livekit-client offers — tracks, autoplay, the roster as objects rather than identities —
 * stays on this side of the seam, because none of it is part of saying what a conversation
 * is. [LAW:locality-or-seam]
 *
 * Exported because it is the browser half of that seam and the half that can be wrong on
 * its own: a `publishData` sent unreliably, or a roster read as objects where identities
 * were wanted, breaks every caller on this page while `conversation.js` stays provably
 * correct. `caller.test.mjs` holds it to that without a browser.
 */
export const transportOf = (room, livekitUrl) => ({
  connect: (token) => room.connect(livekitUrl, token),
  participants: () => [...room.remoteParticipants.keys()],
  publishBytes: (payload) => room.localParticipant.publishData(payload, { reliable: true }),
});

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
   *
   * `settings` is a reader rather than settings, and that is what keeps the controls on
   * the page and the agent in the room from ever disagreeing: read afresh at each send,
   * there is no copy here to go stale, so a call started on one voice and changed to
   * another needs nothing kept in step. [LAW:one-source-of-truth]
   */
  static async join({
    livekitUrl,
    apiKey,
    agentId,
    participantName,
    settings,
    onEvent,
    onTrack,
    onState,
    onPresence,
  }) {
    const microphone = await createLocalAudioTrack();
    const room = new Room();
    const conversation = conversationWith(transportOf(room, livekitUrl), settings);

    // Who is in the room has one source — the room's own roster — and the presence rows
    // are its diff. [LAW:one-source-of-truth] The events say *when* to look, never what
    // is true: livekit mutates the roster before it emits, `set` then `emitWhenConnected`
    // on arrival and `delete` then `emit` on departure, so the roster is already correct
    // inside every handler.
    //
    // Reporting from the event's own payload instead would announce twice: the sweep
    // inside `open` and the handler both see anyone who arrives while `connect` is
    // settling, and no ordering of the two removes that overlap. [LAW:no-ambient-temporal-coupling]
    // A diff is right whatever the arrival order, where a snapshot taken at a cleverer
    // moment would only make the overlap rarer.
    let reported = new Set();
    const reportPresence = () => {
      const present = new Set(room.remoteParticipants.keys());

      for (const identity of [...present].filter((who) => !reported.has(who))) {
        onPresence(identity, "joined");
      }
      for (const identity of [...reported].filter((who) => !present.has(who))) {
        onPresence(identity, "left");
      }
      reported = present;

      // Every agent that just arrived is told what this conversation is, off the shared
      // module's own diff rather than off this one — the rows above are about everybody
      // in the room and that is about the agents, two questions the same roster answers.
      // Returned rather than dropped so `join` can order its own failure reporting:
      // dropped from an event handler a rejection reaches the page's failure banner
      // unhandled. [LAW:no-silent-failure]
      return conversation.arrived().catch(reportDetached);
    };

    // Listeners are attached before connecting: a track can be subscribed and a control
    // event delivered during `connect`, and a handler registered afterwards waits for
    // events that have already fired — which reads as an agent that never spoke.
    room.on(RoomEvent.DataReceived, (payload) => onEvent(decodeEvent(payload)));
    room.on(RoomEvent.ConnectionStateChanged, onState);
    room.on(RoomEvent.ParticipantConnected, reportPresence);
    room.on(RoomEvent.ParticipantDisconnected, reportPresence);
    room.on(RoomEvent.TrackSubscribed, (track) => {
      // Video is not part of this protocol, but subscribing to one and attaching it to
      // an `<audio>` element would silently play nothing at all.
      if (track.kind === Track.Kind.Audio) onTrack(track);
    });

    try {
      // Mints, connects, and configures whoever is already in the room — the shared
      // sequence, which the acceptance runs execute line for line because it is the same
      // lines. The configure inside it lands before the microphone goes live on the next
      // statement, which is the ordering that matters: an agent told which voice to use
      // after the caller could already be speaking is an agent that changes voice partway
      // through a reply.
      //
      // Awaited for that ordering and not for its success. A configure that fails costs
      // this call the voice that was chosen, where failing the join would cost the call
      // itself over a dropdown — so the one failure that leaves a usable call is caught
      // here and every other one is not.
      const conversationId = await conversation
        .open({
          openconv: location.origin,
          apiKey,
          agentId,
          participantName,
        })
        .catch(keepTheCall);

      // The rows for anyone already here, which `open`'s own sweep configured but did not
      // announce. Its diff is over agents and this one is over everybody.
      reportPresence();

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

      return new Call(room, microphone, conversationId, audible, conversation);
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

  constructor(room, microphone, conversationId, audible, conversation) {
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
    /**
     * The conversation these agents are holding, which knows how to say what it is.
     *
     * Kept rather than the settings it was opened with. A call that remembered the
     * answers would be a second copy of what the page already holds, and the two would
     * part company the instant anyone touched a control — which is precisely the bug the
     * reader inside it exists to make unrepresentable. [LAW:one-source-of-truth]
     */
    this.conversation = conversation;
  }

  /**
   * Makes the agents in this call run on the settings the page is showing *now*.
   *
   * Failure is the caller's to report, and it is worth reporting loudly: what it means is
   * that the agent is still on the previous settings while the page shows the new ones,
   * and that gap is invisible from either end without someone saying so.
   * [LAW:no-silent-failure]
   */
  useChosenSettings() {
    return this.conversation.everyone();
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
