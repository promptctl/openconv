// Verifies that the agent tells background context from a typed turn.
//
//   OPENCONV_API_KEY=... node scripts/inbound-text-acceptance.mjs [openconv-url] [livekit-ws-url]
//
// Needs @livekit/rtc-node:
//
//   NODE_PATH=/path/to/node_modules node scripts/inbound-text-acceptance.mjs
//
// The app sends the agent text on two channels and the difference is the whole feature:
//
// 1. **`contextual_update` is absorbed in silence.** Happy pushes new coding-agent
//    messages, session focus changes and status updates through it continuously, several
//    per event. An agent that answers them turns a quiet background feed into a
//    monologue — and one that answers them *while the caller is talking* talks over them.
//
// 2. **`user_message` is a real turn.** It is what a queued prompt flushes into, and it
//    is owed a spoken reply exactly as speech is.
//
// The two claims are checked in one conversation and against one fact, because separately
// they are both cheap to pass wrongly. An agent that ignores contextual updates outright
// passes claim 1 perfectly and is useless: it can be told its session just failed the
// build and know nothing about it. So the burst carries a detail no model could invent,
// and the answer to the `user_message` has to contain it. Silence that still absorbed,
// rather than silence from not listening.
//
// Nothing here is spoken. That is deliberate — both messages under test arrive on the
// data channel, and routing speech through a microphone as well would put transcription
// accuracy in the failure path of a check that has nothing to do with it.

import { Caller, Checks, readEnvironment } from "./lib/caller.mjs";

/// A detail carried only by the contextual updates, which the model has no other way to
/// know. Two independent tokens — a branch name and a count — so a lucky guess has to be
/// lucky twice, and the count is the one a model summarising vaguely would drop.
const BRANCH = "peregrine-42";
const FAILING = "seventeen";

/// Shaped like Happy's own `contextFormatters` output, tags and all, so the model is
/// reading the thing it will actually be sent rather than a tidied stand-in.
const BURST = [
  "Session focus changed to the parser rewrite.",
  `Claude Code: \n<text>I pushed the work to the branch ${BRANCH}.</text>`,
  `Claude Code: \n<text>The suite is red — ${FAILING} tests failing, all in the tokenizer.</text>`,
  "# Runtime counters updated\n- voice_message_count: 3",
];

/// Says what to do with each channel and nothing else. Explicit about staying quiet on
/// context because the check must fail on the agent's *routing*, not on how talkative a
/// given model feels: an agent that starts a turn per update fails here even if the model
/// would have chosen silence, which is the failure worth catching.
const PROMPT =
  "You are a voice interface for a coding assistant. Session updates arrive in " +
  "<session_update> tags; absorb them and never speak about them unless asked. When the " +
  "user asks you something, answer in one sentence, quoting any branch name and any " +
  "count exactly as they were reported to you.";

const ASK = "What is the state of my session right now?";

/// How long the agent is watched for a reply that must never come. Long enough that
/// "it had not got round to it yet" is not the reason the check passed — a model turn is
/// seconds, and every update in the burst has had far longer than one.
const SILENCE_WINDOW_MS = 20_000;

/// How long the agent is given to answer the typed turn: one model turn, then synthesis.
const ANSWER_WITHIN_MS = 45_000;

/// How long to keep listening for the answer to arrive as sound after it arrives as text.
const AUDIO_WITHIN_MS = 20_000;

const { xiApiKey, openconv, livekitUrl } = readEnvironment(process.env, process.argv);
const checks = new Checks();

/** Everything the agent has said, in arrival order. */
const responses = (caller) =>
  caller.controlEvents
    .filter((event) => event.type === "agent_response")
    .map((event) => event.agent_response_event.agent_response);

/** Transcripts the agent published — of speech, which is the only thing it transcribes. */
const transcripts = (caller) =>
  caller.controlEvents.filter((event) =>
    event.type === "user_transcript" || event.type === "tentative_user_transcript",
  );

// No `firstMessage`: a configured greeting is a legitimate `agent_response`, and one
// arriving during the silence window would be indistinguishable from the failure.
const caller = await Caller.join({
  openconv,
  livekitUrl,
  xiApiKey,
  settings: { prompt: PROMPT },
});

// Waited for rather than assumed. This used to publish the configuration and then open
// the microphone, and reached the agent only because opening one waits for a subscriber —
// so the ordering held by way of an unrelated wait rather than by anything saying so.
// [LAW:no-ambient-temporal-coupling]
await caller.agentConfigured();

// Published before anything is sent, the way a real caller joins: it is what makes the
// silence below meaningful. An agent with no track to listen to is quiet for a reason
// that has nothing to do with this ticket.
await caller.microphone();

checks.record(
  "the conversation was announced",
  await caller.waitFor(
    () => caller.control("conversation_initiation_metadata"),
    20_000,
    "the announcement",
  ),
);

// ---------------------------------------------------------------------------
// 1. A burst of contextual updates is absorbed in silence.
// ---------------------------------------------------------------------------

const beforeBurst = caller.mark();
for (const text of BURST) {
  await caller.send({ type: "contextual_update", text });
}

// Waits for the failure rather than the success: there is nothing to observe when the
// agent correctly says nothing, so the check is that the window closes empty.
await caller.waitFor(
  () => responses(caller).length > 0,
  SILENCE_WINDOW_MS,
  "a reply to the context that must never arrive",
);

const spoke = responses(caller);
checks.record(
  "the agent said nothing about the contextual updates",
  spoke.length === 0,
  spoke.length ? `said ${JSON.stringify(spoke.join(" "))}` : "",
);

// The audible half. A reply the agent published as text but never synthesized would pass
// the check above; this one is about the caller's ear, which is what the ticket is for.
checks.record(
  "nothing came out of the agent's mouth either",
  caller.heard.audibleFrames === beforeBurst.audibleFrames,
  `${caller.heard.audibleFrames - beforeBurst.audibleFrames} audible frames`,
);

// Context is not speech, and an agent that published transcripts of it would be telling
// the app the caller said things they never said — which the app renders in their bubble.
const heardAsSpeech = transcripts(caller);
checks.record(
  "the context was never mistaken for something the caller said",
  heardAsSpeech.length === 0,
  heardAsSpeech.length ? `published ${JSON.stringify(heardAsSpeech)}` : "",
);

// ---------------------------------------------------------------------------
// 2. A user_message is a real turn — and the silence above was not deafness.
// ---------------------------------------------------------------------------

const beforeAsking = caller.mark();
await caller.send({ type: "user_message", text: ASK });

checks.record(
  "the agent answered the typed message",
  await caller.waitFor(
    () => responses(caller).length > 0,
    ANSWER_WITHIN_MS,
    "the agent to answer the typed message",
  ),
);

const answer = responses(caller).join(" ");

// The strong pair. Neither token exists anywhere but in the burst, so an agent that
// dropped the contextual updates on the floor cannot produce them here however fluently
// it answers.
checks.record(
  "the answer carries the branch only the context named",
  new RegExp(BRANCH, "i").test(answer),
  `said ${JSON.stringify(answer)}`,
);
checks.record(
  "the answer carries the count only the context named",
  new RegExp(FAILING, "i").test(answer),
  `said ${JSON.stringify(answer)}`,
);

// A typed turn is answered out loud, not just in text — the app queues prompts precisely
// so the caller hears the reply.
checks.record(
  "the answer was spoken, not only published",
  await caller.waitFor(
    () => caller.heard.audibleFrames > beforeAsking.audibleFrames,
    AUDIO_WITHIN_MS,
    "the answer to arrive as sound",
  ),
  caller.heard.error ? `the audio reader died: ${caller.heard.error}` : "",
);

await caller.leave();
checks.finish();
