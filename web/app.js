// The page: what the button does.

import { Call } from "./caller.js";
import { cell, log, render, show } from "./transcript.js";

const els = {
  join: document.getElementById("join"),
  audio: document.getElementById("agent-audio"),
  voice: document.getElementById("voice"),
};

/**
 * Every control on the form, under the name its value travels by — into the call, into
 * the URL that can seed it, and into what the next visit remembers.
 */
const CONTROLS = {
  apiKey: document.getElementById("api-key"),
  agentId: document.getElementById("agent-id"),
  participantName: document.getElementById("participant-name"),
  voiceId: els.voice,
};

/**
 * What the mint refuses to be without, and what to call each one when it is empty.
 *
 * A second table rather than a flag on the first, because being required is not a
 * property every control has and never was — it is a property of the three values that
 * get minted with. The voice's empty value is a real answer, "no particular voice, let
 * the deployment choose", and refusing it would take away the only state available
 * before the roster has loaded or when it cannot.
 */
const REQUIRED = {
  apiKey: "the api key",
  agentId: "an agent",
  participantName: "a participant",
};

/**
 * Reads the form, refusing a blank where blank is not an answer.
 *
 * The boundary between what somebody chose and what the call is made with, and a parser
 * rather than a check: everything downstream runs on values already known to be as
 * complete as they have to be, so nothing asks again.
 *
 * A blank among `REQUIRED` is refused rather than passed through, because the server
 * takes each of those as a string it does not police. An empty participant mints a
 * metered conversation attributed to nobody; an empty agent names an agent that does not
 * exist. Dropping the parameter instead would be worse than either — an absent
 * participant is the unmetered bring-your-own-key path, and choosing it by clearing a
 * text box is a mode switch nobody asked for.
 *
 * The voice is the control that is complete when it is empty, and it leaves here as the
 * string the form holds rather than as an absence, so that what is remembered for next
 * visit is the same kind of value every other control remembers. Turning "" into "no
 * particular voice" happens once, where the message that says it is built.
 */
function readForm() {
  const chosen = Object.fromEntries(
    Object.entries(CONTROLS).map(([name, control]) => [name, control.value.trim()]),
  );

  const missing = Object.keys(REQUIRED).filter((name) => chosen[name] === "");
  if (missing.length > 0) {
    throw new Error(
      `fill in ${missing.map((name) => REQUIRED[name]).join(" and ")} before joining`,
    );
  }

  return chosen;
}

/**
 * What to do about the failures a browser reports by name instead of by sentence.
 *
 * `NotAllowedError` is the one that will happen most, and it is genuinely ambiguous:
 * the browser raises the identical error for a refused permission prompt and for an
 * origin it considers insecure, without saying which. Safari treats `http://127.0.0.1`
 * as insecure and Chrome does not, so the same page works in one browser and is refused
 * by the other with this same word — which is a bad ten minutes unless the page says so.
 */
const ADVICE = {
  NotAllowedError:
    " — the microphone was refused. Chrome and Firefox treat http://127.0.0.1 as a secure" +
    " origin and will prompt for it; Safari does not, and refuses without asking. If you" +
    " are already in Chrome, check macOS Settings › Privacy & Security › Microphone.",
  NotFoundError: " — this machine has no microphone the browser can see.",
  NotReadableError: " — the microphone is held by another application.",
};

/** Where the last visit's settings are kept. */
const REMEMBERED = "openconv.call";

/**
 * The settings from last time: string values, for field names this page actually has.
 *
 * This is the parser for the one input on the page nobody typed, and everything about
 * it is untrusted — an older version of this page wrote it, or a second tab, or an
 * extension, or somebody in devtools. So it returns the shape `seedFields` needs on
 * every path rather than whatever `JSON.parse` happened to produce.
 * [LAW:parse-dont-validate]
 *
 * That matters more than it looks. `JSON.parse("null")` succeeds, and indexing the
 * result for a field name throws — at module scope, before the form is ever seeded.
 * A stored number or object does not throw; it stringifies into the box as `42` or
 * `[object Object]`, and since seeds are written *into the controls*, `readForm` sees a
 * non-empty string and mints with it.
 *
 * An unusable stored value is dropped quietly, because it is not a failure: it is
 * indistinguishable from a first visit, the control falls through to the markup's
 * default, and an empty box on screen is the whole of the consequence. A browser that
 * refuses storage outright is a different matter and still says so.
 */
function remembered() {
  try {
    const stored = JSON.parse(localStorage.getItem(REMEMBERED) ?? "{}");

    return Object.fromEntries(
      Object.keys(CONTROLS)
        .filter((name) => typeof stored?.[name] === "string")
        .map((name) => [name, stored[name]]),
    );
  } catch (error) {
    render(log("error", `could not read remembered settings: ${error.message}`));
    return {};
  }
}

/**
 * Fills the form from the URL or from the last visit, so that joining costs one click.
 *
 * These values change rarely and the shared secret never, so retyping them every visit
 * is friction carrying no information. The voice is seeded the same way as the rest,
 * which is what makes `?voiceId=af_heart` a bookmark and a chosen voice something that
 * survives a reload — there is no separate notion of a default voice for this page
 * because there does not need to be one.
 *
 * Seeds are written *into the controls* rather than read at mint time, which keeps the
 * form the only thing deciding what the call is made with — a page whose box shows one
 * key while the request sends another is a bad hour. It also means `readForm` stays the
 * single boundary: nothing routes around the parser.
 *
 * Precedence is URL, then remembered, then whatever the markup shipped, so a link can
 * always override a stale stored value.
 *
 * Returns what it wrote. The voice list arrives later than this runs, and a `<select>`
 * cannot hold a value it has no option for — so the one control that has to be written
 * twice is written from one read, and the two cannot disagree about what this visit
 * asked for. [LAW:one-source-of-truth]
 */
function seedFields() {
  const fromUrl = new URLSearchParams(location.search);
  const stored = remembered();

  const seeded = Object.fromEntries(
    Object.entries(CONTROLS).map(([name, control]) => [
      name,
      fromUrl.get(name) ?? stored[name] ?? control.value,
    ]),
  );

  for (const [name, control] of Object.entries(CONTROLS)) {
    control.value = seeded[name];
  }

  // The key does not stay in the address bar once it is in the box. A URL is the one
  // place a secret gets bookmarked, screenshotted and pasted into chat.
  history.replaceState(null, "", location.pathname);

  return seeded;
}

/**
 * Fills the voice list from the deployment, then puts back the voice this visit asked
 * for.
 *
 * The list is the text-to-speech server's, reached through `/call/voices` because a
 * browser has no route to that server and no business holding its address — the same
 * reason the SFU to dial is read from `./config` rather than typed.
 *
 * A failure here leaves the page on `deployment default`, which is not a substitute for
 * the voice that was wanted: it is the same request this page made before the list
 * existed, it is the one state the server is guaranteed to have an answer for, and it is
 * said out loud rather than arrived at silently. [LAW:no-silent-failure] The join is
 * untouched either way — a dropdown that could stop someone talking to their agent would
 * be a bad trade for a dropdown.
 */
async function offerVoices(wanted) {
  try {
    const response = await fetch("./voices");
    if (!response.ok) {
      throw new Error(`HTTP ${response.status} ${await response.text()}`);
    }

    for (const voice of (await response.json()).voices) {
      // The server's own sentence about the voice — "Kokoro Heart (en-us, female)" —
      // which already names the engine, the locale and the tier. Composing a label here
      // out of a voice's several `models` would mean deciding on this page which of them
      // is really its engine, and that is elvenspeak's answer to give.
      els.voice.append(new Option(voice.description, voice.voice_id));
    }
  } catch (error) {
    render(log("error", `could not read the voices this deployment serves: ${error.message}`));
  }

  // Written after the options exist, because a `<select>` silently ignores a value it
  // has no option for — and read back, because that silence is the whole failure: a
  // voice that quietly stopped being served would put the caller on the deployment's
  // default with the form agreeing that nothing was wrong.
  els.voice.value = wanted;
  if (els.voice.value !== wanted) {
    render(
      log("error", `the voice ${wanted} is not on offer here — using the deployment default`),
    );
  }
}

/**
 * Keeps the current settings for next time.
 *
 * Called only after a join that worked, so what is remembered is a set of values known
 * to mint. Note this does put the shared secret in `localStorage`: acceptable for a
 * client whose whole purpose is a fast development loop, and worth knowing before
 * pointing this page at anything you would not paste into a browser console.
 */
function remember(fields) {
  try {
    localStorage.setItem(REMEMBERED, JSON.stringify(fields));
  } catch (error) {
    render(log("error", `could not remember these settings: ${error.message}`));
  }
}

/**
 * The call this page is in, and the only mutable state it has.
 *
 * Written by `join` and `leave` and nowhere else, and the button is derived from it, so
 * the two cannot disagree about whether a call is up.
 */
let call = null;

function showButton(label, enabled) {
  els.join.textContent = label;
  els.join.disabled = !enabled;
}

/**
 * The SFU these tokens are valid for, read from the server that mints them.
 *
 * Not a field on this page, and deliberately so: a token minted by one deployment and
 * offered to a different deployment's SFU does not error. The client joins a room the
 * agent is not in, and the caller hears silence with nothing anywhere reporting a
 * problem. The server that mints already knows the answer, so it is asked rather than
 * re-typed here where the two can drift.
 */
async function livekitUrl() {
  const response = await fetch("./config");
  if (!response.ok) {
    throw new Error(`could not read the SFU to dial: HTTP ${response.status}`);
  }
  return (await response.json()).livekit_url;
}

async function join() {
  const fields = readForm();
  showButton("joining…", false);

  call = await Call.join({
    ...fields,
    livekitUrl: await livekitUrl(),
    // A reader of the control rather than the voice `fields` happens to be carrying —
    // that copy goes on to `remember` and no further. The form stays the one place that
    // says which voice this call is in, so changing it mid-call reaches the agent through
    // the same reader with nothing to keep in step. [LAW:one-source-of-truth]
    chosenVoice: () => els.voice.value,
    onEvent: show,
    onTrack: (track) => track.attach(els.audio),
    onState: (state) => render(cell("room", state)),
    // Who else is in the room, which is otherwise invisible: "I pressed join and
    // nothing happened" is almost always an agent that never arrived, and that is a
    // different problem from one that arrived and stayed quiet.
    onPresence: (identity, presence) => render(log("system", `${identity} ${presence}`)),
  });

  remember(fields);
  render(cell("call", call.conversationId));
  render(cell("audio", call.audible ? "playing" : "BLOCKED — click the page"));
  showButton("leave", true);
}

async function leave() {
  showButton("leaving…", false);
  await call.leave();
  call = null;
  showButton("join", true);
}

els.join.addEventListener("click", async () => {
  try {
    await (call ? leave() : join());
  } catch (error) {
    // On screen, not in the console alone. Someone is looking at this page *because*
    // something is wrong, and a failure only devtools can see is a page that appears to
    // do nothing at all.
    // The advice is a lookup with an empty default, so an error the page has nothing
    // extra to say about still prints its own message in full.
    render(log("error", `${error}${ADVICE[error.name] ?? ""}`));

    // The button is derived from whether a call is still held, not assumed to be gone.
    // A `leave` that failed is still in its call, and a page that says "join" over a
    // room the caller is in tells them they hung up when they did not — while
    // `Call.join` tears its own half-built call down, so a failure there really has
    // left nothing behind.
    showButton(call ? "leave" : "join", true);
  }
});

// Choosing a voice during a call changes the call. Without this the control is only read
// at the join, and the way to hear a different voice is to hang up and dial again — which
// makes comparing two voices a thing you do from memory across two conversations.
//
// Nothing happens when no call is up, and that arm is complete rather than skipped: with
// no agent anywhere there is nothing that could disagree with the form, and the next join
// reads this same control.
els.voice.addEventListener("change", async () => {
  try {
    await call?.useChosenVoice();
  } catch (error) {
    // The control keeps showing what was asked for. Putting it back would make the page
    // agree with an agent nobody can hear yet, which is the silent substitution this
    // whole control exists to prevent, one level up — so the disagreement is said out
    // loud instead of tidied away. [LAW:no-silent-failure]
    render(log("error", `${error} — the voice box is showing a voice this call is not using`));
  }
});

offerVoices(seedFields().voiceId);
