// What a client says to openconv to open a conversation, and in what order it says it.
//
// Shared by every caller this repo has: the page in `caller.js`, and the acceptance runs
// through `scripts/lib/caller.mjs`. It exists because those two used to hold the same
// knowledge separately and drifted — the Node callers learned to publish the session
// configuration and the page did not, so for months every conversation a person actually
// held ran on the default prompt while a green acceptance suite reported the feature
// working. A step list written in a comment is a map only a human redraws.
// [LAW:one-source-of-truth]
//
// It lives in `web/` rather than somewhere that sounds more neutral because the browser's
// module graph is flat: the page is served as a hand-listed table of assets under
// `/call/`, so a module `caller.js` imports has to sit beside it on disk for that import
// to resolve the same way in node, where `scripts/` and the tests load it off the real
// filesystem. `web/` is therefore the JavaScript openconv client — the page is one thing
// in it, and this is what the page and the scripts both are underneath.
//
// What cannot be shared is the SDK underneath: `web/vendor/livekit-client.js` in a
// browser, `@livekit/rtc-node` under node, two type systems for one protocol and neither
// running where the other does. So the room calls sit behind a transport seam of three
// operations, with one small implementation per SDK, and everything above it — the
// sequence, and every byte of every control message — is this file.
// [LAW:locality-or-seam]

/**
 * What opening a conversation needs from a LiveKit SDK.
 *
 * Three operations, named for what the handshake wants rather than for what either SDK
 * calls them, which is what keeps this file free of both:
 *
 * - `connect(token)` — join the room the token names, resolving once joined.
 * - `participants()` — the identities in the room *now*, as an array.
 * - `publishBytes(payload)` — put one reliable data packet on the wire.
 *
 * `participants` is a reader rather than a roster handed over once, because both SDKs
 * keep their own roster correct and mutate it before they emit the event that says it
 * changed. Copying it here would create a second answer to "who is in the room" that goes
 * stale between events. [LAW:one-source-of-truth]
 *
 * @typedef {{
 *   connect: (token: string) => Promise<void>,
 *   participants: () => string[],
 *   publishBytes: (payload: Uint8Array) => Promise<void>,
 * }} Transport
 */

/**
 * Whether a participant in the room is the agent rather than another caller.
 *
 * The identity openconv mints for it — `agent_<conversation>`, from
 * `crates/openconv-server/src/livekit.rs`. One prefix, in one place, because two clients
 * disagreeing about who counts as the agent would have them disagree about the same room,
 * and this is the fact that decides who gets told what the conversation is.
 */
export const isAgent = (identity) => identity.startsWith("agent_");

/**
 * What a caller asked for, or `null` where it asked for nothing.
 *
 * Blank and absent are the same answer — a settings box nobody typed in and a field
 * nobody passed both mean "no particular value" — and `null` is how that reaches the
 * server, because serde reads an explicit null into the same `None` an omitted field
 * gives. An empty string would instead ask openconv to resolve `""` as a voice, an engine
 * or a prompt, which is a different question with a worse answer.
 */
const said = (value) => value?.trim() || null;

/**
 * The message that tells an agent what this conversation is.
 *
 * Every field of the override, every time, with `null` standing where a caller asked for
 * nothing — rather than a shape assembled out of whichever settings happen to be set.
 * That is not a stylistic choice in either direction: it is what the ElevenLabs SDK
 * actually puts on the wire, which `ConversationConfigOverride` in
 * `crates/openconv-protocol/src/client.rs` records from the far side — the SDK builds its
 * sub-objects whenever a caller overrides anything at all, so they arrive filled with nulls
 * rather than omitted. It is what lets this function have no branches at all: the caller's
 * variability is carried entirely by the values flowing through one fixed shape.
 * [LAW:dataflow-not-control-flow]
 *
 * Two of that note's three sub-objects, though, and not all three. `conversation` carries
 * `text_only` and `client_events`, which this page has no control for and `SessionConfig`
 * does not read — `settle` reads `agent` and `tts` and stops. An empty one would be a field
 * put on the wire so that a sentence about the wire came out true, and absent is the same
 * answer as all-null on the far side anyway, which
 * `a_message_that_overrides_nothing_settles_where_a_conversation_starts` pins from there.
 *
 * Which is also why an all-`null` message is a legitimate thing to send rather than a
 * message worth skipping. `SessionConfig::settle` reads every absent override as "use the
 * default", and the agent's own starting config at `crates/openconv-agent/src/lib.rs:192`
 * is settled from precisely this empty payload — so a caller that overrides nothing and a
 * caller that sends nothing at all leave the agent running the same conversation. Sending
 * it regardless is what lets every caller take one path.
 *
 * The nesting is the protocol's business and stays here. A caller names a prompt, a first
 * message, a language, a voice and an engine; that `language` travels under `agent` while
 * `voice_id` travels under `tts` is a fact about the wire that no acceptance script and no
 * page has any reason to know, and six of them knowing it is how the seventh got it wrong.
 *
 * @param {{
 *   prompt?: string, firstMessage?: string, language?: string,
 *   voiceId?: string, modelId?: string, variables?: object,
 * }} settings
 */
export const conversationInitiation = (settings) => ({
  type: "conversation_initiation_client_data",
  conversation_config_override: {
    agent: {
      prompt: { prompt: said(settings.prompt) },
      first_message: said(settings.firstMessage),
      language: said(settings.language),
    },
    tts: {
      voice_id: said(settings.voiceId),
      model_id: said(settings.modelId),
    },
  },
  dynamic_variables: settings.variables ?? null,
});

/**
 * A conversation that opened but could not be told what it is.
 *
 * A different failure from a room that never opened, and a type rather than a message to
 * match on, because what it costs is the caller's decision and not this module's: an
 * acceptance run that cannot configure its agent has nothing left to assert and should
 * fail, while the page has a call that works on the deployment's defaults and should not
 * lose the room over a dropdown. Neither can act on that difference by reading a string.
 * [LAW:parse-dont-validate]
 *
 * It carries the conversation because there is one — that is the whole of what separates
 * it from a mint or a connect that failed, and a caller that keeps the call still has to
 * be able to name it. [LAW:one-source-of-truth]
 */
export class NotTold extends Error {
  constructor(conversationId, cause) {
    // The cause's own sentence, which already names the agent that could not be reached:
    // this type says which *kind* of failure it is, and has nothing to add about what went
    // wrong that the failure underneath has not already said.
    super(cause.message, { cause });
    this.name = "NotTold";
    this.conversationId = conversationId;
  }
}

/**
 * Reads a JWT's payload.
 *
 * `atob` and node's `Buffer` both decode base64, and a JWT carries base64*url* — a
 * different alphabet in two characters. Feeding one to the other does not fail; it yields
 * bytes that are wrong only sometimes, depending on the claims, which is the worst way for
 * a bug to behave. Translated here so that neither caller has to remember, and so the two
 * cannot decode the same token differently.
 */
const claims = (token) => {
  const payload = token.split(".")[1].replaceAll("-", "+").replaceAll("_", "/");
  const bytes = Uint8Array.from(atob(payload), (character) => character.codePointAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
};

/**
 * Which conversation a token admits you to.
 *
 * The conversation ID *is* the room name, so it is recovered from the token rather than
 * carried beside it — the two cannot then disagree, and a caller handed only a token is
 * not handed a second thing to keep in step. [LAW:one-source-of-truth]
 */
export const conversationOf = (token) => claims(token).video.room;

/**
 * Mints a conversation token, which is also what dispatches the agent into the room.
 *
 * A parser rather than a check: it returns a token and the conversation that token admits
 * you to, or it throws carrying whatever the server said. There is no arm where a caller
 * receives an empty token and has to work out for itself whether the mint happened.
 * [LAW:parse-dont-validate]
 */
export async function mintConversation({ openconv, apiKey, agentId, participantName }) {
  const url = new URL(`${openconv.replace(/\/$/, "")}/v1/convai/conversation/token`);
  url.searchParams.set("agent_id", agentId);
  url.searchParams.set("participant_name", participantName);

  const response = await fetch(url, { headers: { "xi-api-key": apiKey } });
  const body = await response.text();
  if (!response.ok) {
    throw new Error(`mint failed: HTTP ${response.status} ${body}`);
  }

  const { token } = JSON.parse(body);
  return { token, conversationId: conversationOf(token) };
}

/**
 * A conversation being held over one transport, and the agents that know what it is.
 *
 * `readSettings` is a reader rather than the settings themselves, and that is what keeps a
 * control on a page and an agent in a room from ever disagreeing: read afresh at every
 * send, there is no copy here to go stale, so a conversation opened on one voice and
 * changed to another needs nothing kept in step. A caller whose settings never change
 * passes a reader that always answers the same thing, which is the same code path rather
 * than a simpler one. [LAW:one-source-of-truth]
 *
 * @param {Transport} transport
 * @param {() => object} readSettings
 */
export function conversationWith(transport, readSettings) {
  // Who has been told what this conversation is, against the publish that answers for it.
  //
  // An identity is entered the moment its publish *starts* rather than when it settles,
  // because two sweeps overlapping is the ordinary case here and not a rare one: an agent
  // that arrives while `connect` is still settling is seen both by the sweep inside `open`
  // and by the caller's own arrival handler, and the browser SDK buffers
  // `ParticipantConnected` until the room is connected (`emitWhenConnected`) so the two
  // land together. Recorded only on fulfilment, both sweeps read that agent as untold and
  // both published to it. [LAW:no-ambient-temporal-coupling]
  //
  // Holding the publish itself rather than a flag saying one is in flight is what lets the
  // second sweep *join* the first instead of skipping it, so `open` still does not return
  // until the agent it found has actually been told — the order the microphone goes live
  // behind in `caller.js`. Absent is untold, pending is being told, settled is told: one
  // record with a state for each, rather than a second set beside this one that could
  // disagree with it. [LAW:one-source-of-truth]
  let telling = new Map();

  /** Publishes the configuration to `identity`, and stands as its record while it flies. */
  const send = (identity, message) => {
    const attempt = transport.publishBytes(message).catch((failure) => {
      // Only this attempt's own claim is retracted, so an agent a send failed to reach is
      // not remembered as told and the next sweep tries it again rather than leaving it on
      // the default conversation for the rest of the call. Checked against the record
      // rather than deleted outright because `everyone` forgets the map to send afresh: a
      // stale publish failing after that would otherwise delete the newer one's claim.
      if (telling.get(identity) === attempt) telling.delete(identity);

      throw new Error(
        `${identity} could not be told what this conversation is: ${failure.message}`,
        { cause: failure },
      );
    });

    telling.set(identity, attempt);
    return attempt;
  };

  /** Publishes the configuration to each of `identities`, joining any send already flying. */
  const tell = async (identities) => {
    // Read once for the whole sweep rather than per agent, so that two agents told together
    // are told the same thing: a control moving while the sweep runs would otherwise leave
    // one agent on the old answer and the other on the new, with the record claiming both
    // hold the current one. [LAW:one-source-of-truth]
    const message = new TextEncoder().encode(JSON.stringify(conversationInitiation(readSettings())));

    // Told one at a time so that a failure names the agent it could not reach, and so that
    // a partial failure records exactly the ones that were. Telling nobody is an empty list
    // rather than a case of its own: a room with no agent in it yet, and a room whose agent
    // has left, take the same path as a room with one. [LAW:dataflow-not-control-flow]
    const attempts = identities.map(
      (identity) => telling.get(identity) ?? send(identity, message),
    );

    // Settled rather than raced, because `Promise.all` reports the first rejection and
    // drops every other one on the floor. Two agents unreachable in the same sweep is the
    // case the sentence above claims this shape handles, and under `all` the second one is
    // named nowhere at all — not thrown, not logged — while its `send` still quietly
    // un-records it for the next sweep to retry. [LAW:no-silent-failure]
    const failed = (await Promise.allSettled(attempts))
      .filter((attempt) => attempt.status === "rejected")
      .map((attempt) => attempt.reason);

    // One failure reads exactly as it did before this aggregated — `send` already put the
    // agent's identity in every sentence, so joining one of them changes nothing, and
    // `NotTold` still surfaces that sentence unaltered. `AggregateError` carries the rest
    // where a single `cause` could hold only the first. [LAW:one-source-of-truth]
    if (failed.length > 0) {
      throw new AggregateError(failed, failed.map((failure) => failure.message).join("; "));
    }
  };

  return {
    /**
     * Opens the conversation: mints, joins, and tells whoever is already there.
     *
     * The step list, as code rather than as a comment, and the only copy of it. A seventh
     * step is a line in this function, which every caller runs by calling it — where a
     * seventh step in a comment is a line one caller's author reads and another's does
     * not.
     *
     * The agent is dispatched by the mint, so it is usually in the room *before* this
     * client is, and both SDKs suppress their arrival event until the connection is
     * established — which means the common case never announces itself and a caller
     * waiting for the event alone would configure nobody. Swept here instead, off the same
     * roster the arrival event reads, so the agent that was already here and the one that
     * takes another second are each found exactly once and neither depends on which order
     * they happened in. [LAW:no-ambient-temporal-coupling]
     */
    async open(credentials) {
      const { token, conversationId } = await mintConversation(credentials);
      await transport.connect(token);
      await this.arrived().catch((failure) => {
        throw new NotTold(conversationId, failure);
      });
      return conversationId;
    },

    /**
     * Tells the agents that have arrived since the last time anyone asked.
     *
     * What an arrival handler calls. A control message reaches whoever is in the room at
     * the instant it is published and nobody else — there is no queue for a participant
     * yet to join — so a configuration sent on a fixed schedule would be right only in
     * whichever arrival order it was written against. Off the diff it is right in every
     * order. [LAW:no-ambient-temporal-coupling]
     */
    arrived() {
      const present = new Set(transport.participants().filter(isAgent));
      // An agent that has left drops out, so one that leaves and comes back is told again.
      telling = new Map([...telling].filter(([identity]) => present.has(identity)));
      return tell([...present]);
    },

    /**
     * Tells every agent in the room what the conversation is now, told before or not.
     *
     * The other half of the arrival sweep, and not a second send but the same one: what
     * changing the settings *means* is that nobody now holds what this conversation is, so
     * it forgets who was told and runs the arrival sweep, which then owes everybody. An
     * agent that arrives late and an agent whose settings were changed need the current
     * answer for the same reason, and there is one path that says it.
     * [LAW:dataflow-not-control-flow]
     *
     * Forgetting rather than re-sending over the record is also what keeps a send already
     * in flight from being joined by mistake: that one carries the settings the caller has
     * just moved off, and a sweep that reused it would report the new ones as delivered.
     *
     * The agent takes a second message the same way it took the first: openconv re-settles
     * its `SessionConfig` on every `ConversationInitiation`. Since the message names every
     * field, a re-send moves whatever changed and returns everything else to what the
     * caller is asking for now — it is not a patch, and was never treated as one.
     */
    everyone() {
      telling = new Map();
      return this.arrived();
    },
  };
}
