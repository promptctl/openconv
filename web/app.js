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
  });

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
    render(log("error", String(error)));

    // The button is derived from whether a call is still held, not assumed to be gone.
    // A `leave` that failed is still in its call, and a page that says "join" over a
    // room the caller is in tells them they hung up when they did not — while
    // `Call.join` tears its own half-built call down, so a failure there really has
    // left nothing behind.
    showButton(call ? "leave" : "join", true);
  }
});
