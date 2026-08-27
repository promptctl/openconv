// The page: what the button does.

import { Call } from "./caller.js";
import { cell, log, render, show } from "./transcript.js";

const els = {
  apiKey: document.getElementById("api-key"),
  agentId: document.getElementById("agent-id"),
  participantName: document.getElementById("participant-name"),
  join: document.getElementById("join"),
  audio: document.getElementById("agent-audio"),
};

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
  showButton("joining…", false);
  call = await Call.join({
    livekitUrl: await livekitUrl(),
    apiKey: els.apiKey.value,
    agentId: els.agentId.value,
    participantName: els.participantName.value,
    onEvent: show,
    onTrack: (track) => track.attach(els.audio),
    onState: (state) => render(cell("room", state)),
  });

  render(cell("call", call.conversationId));

  // Reported rather than assumed: a browser refusing to play audio is the one failure
  // that looks exactly like an agent with nothing to say.
  const audible = await call.allowPlayback();
  render(cell("audio", audible ? "playing" : "BLOCKED — click the page"));

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
    call = null;
    showButton("join", true);
  }
});
