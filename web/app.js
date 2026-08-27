// The page: what the button does.

import { Call } from "./caller.js";
import { cell, log, render, show } from "./transcript.js";

const els = {
  join: document.getElementById("join"),
  audio: document.getElementById("agent-audio"),
};

/** What each field is called when the page has to say it is empty. */
const FIELDS = {
  apiKey: { element: document.getElementById("api-key"), called: "the api key" },
  agentId: { element: document.getElementById("agent-id"), called: "an agent" },
  participantName: {
    element: document.getElementById("participant-name"),
    called: "a participant",
  },
};

/**
 * Reads the form, refusing anything blank.
 *
 * The boundary between what somebody typed and what gets minted, and a parser rather
 * than a check: everything downstream runs on values known to be non-empty, so nothing
 * asks again.
 *
 * Blank is refused rather than passed through, because the server takes each of these
 * as a string it does not police. An empty participant mints a metered conversation
 * attributed to nobody; an empty agent names an agent that does not exist. Dropping the
 * parameter instead would be worse than either — an absent participant is the unmetered
 * bring-your-own-key path, and choosing it by clearing a text box is a mode switch
 * nobody asked for.
 */
function requireFields() {
  const typed = Object.entries(FIELDS).map(([name, field]) => [name, field.element.value.trim()]);
  const blank = typed.filter(([, value]) => value === "");

  if (blank.length > 0) {
    const missing = blank.map(([name]) => FIELDS[name].called);
    throw new Error(`fill in ${missing.join(" and ")} before joining`);
  }

  return Object.fromEntries(typed);
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
 * The settings from last time, or none.
 *
 * A browser that refuses storage — private windows do — says so and carries on with the
 * markup's defaults rather than taking the page down over a convenience.
 */
function remembered() {
  try {
    return JSON.parse(localStorage.getItem(REMEMBERED) ?? "{}");
  } catch (error) {
    render(log("error", `could not read remembered settings: ${error.message}`));
    return {};
  }
}

/**
 * Fills the form from the URL or from the last visit, so that joining costs one click.
 *
 * These three values change rarely and the shared secret never, so retyping them every
 * visit is friction carrying no information.
 *
 * Seeds are written *into the fields* rather than read at mint time, which keeps the
 * form the only thing deciding what gets minted — a page whose box shows one key while
 * the request sends another is a bad hour. It also means `requireFields` stays the
 * single boundary: nothing routes around the parser.
 *
 * Precedence is URL, then remembered, then whatever the markup shipped, so a link can
 * always override a stale stored value.
 */
function seedFields() {
  const fromUrl = new URLSearchParams(location.search);
  const stored = remembered();

  for (const [name, field] of Object.entries(FIELDS)) {
    field.element.value = fromUrl.get(name) ?? stored[name] ?? field.element.value;
  }

  // The key does not stay in the address bar once it is in the box. A URL is the one
  // place a secret gets bookmarked, screenshotted and pasted into chat.
  history.replaceState(null, "", location.pathname);
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
  const fields = requireFields();
  showButton("joining…", false);

  call = await Call.join({
    ...fields,
    livekitUrl: await livekitUrl(),
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

seedFields();
