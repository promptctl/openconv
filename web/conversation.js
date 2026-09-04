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
 * actually puts on the wire (`ConversationConfigOverride` in
 * `crates/openconv-protocol/src/client.rs` notes that `tts` and `conversation` routinely
 * arrive as empty objects for exactly this reason), and it means this function has no
 * branches at all — the caller's variability is carried entirely by the values flowing
 * through one fixed shape. [LAW:dataflow-not-control-flow]
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
  // Who has actually been reached, written only where a publish resolved. An agent a send
  // failed to reach is therefore not remembered as told, and the next sweep tries it again
  // rather than leaving it running the default conversation for the rest of the call.
  let told = new Set();

  /** Publishes the configuration to each of `identities`, one at a time. */
  const tell = (identities) => {
    const message = new TextEncoder().encode(JSON.stringify(conversationInitiation(readSettings())));

    // Told one at a time so that a failure names the agent it could not reach, and so that
    // a partial failure records exactly the ones that were. Telling nobody is an empty list
    // rather than a case of its own: a room with no agent in it yet, and a room whose agent
    // has left, take the same path as a room with one. [LAW:dataflow-not-control-flow]
    //
    // Two arms of one `then` rather than a `then` followed by a `catch`, so that the
    // failure arm sees only a failed publish — a `catch` downstream of the record would
    // also swallow anything the record itself threw and report it as an unreachable agent.
    return Promise.all(
      identities.map((identity) =>
        transport.publishBytes(message).then(
          () => told.add(identity),
          (failure) => {
            throw new Error(
              `${identity} could not be told what this conversation is: ${failure.message}`,
              { cause: failure },
            );
          },
        ),
      ),
    );
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
      await this.arrived();
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
      told = new Set([...told].filter((identity) => present.has(identity)));
      return tell([...present].filter((identity) => !told.has(identity)));
    },

    /**
     * Tells every agent in the room what the conversation is now, told before or not.
     *
     * The other half of the arrival sweep, and deliberately the same send: an agent that
     * arrives late and an agent whose settings were changed both need the current answer,
     * and there is only one way to say it. What differs is which agents — the ones that
     * just arrived, or everyone — so that difference is a list of identities rather than
     * two code paths. [LAW:dataflow-not-control-flow]
     *
     * The agent takes a second message the same way it took the first: openconv re-settles
     * its `SessionConfig` on every `ConversationInitiation`. Since the message names every
     * field, a re-send moves whatever changed and returns everything else to what the
     * caller is asking for now — it is not a patch, and was never treated as one.
     */
    everyone() {
      return tell(transport.participants().filter(isAgent));
    },
  };
}
