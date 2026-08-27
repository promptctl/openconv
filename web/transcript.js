// What a control message looks like on screen.
//
// Every message the agent publishes reaches the page. The ones worth reading as a
// conversation get a view below; everything else prints raw rather than being dropped,
// because the point of this page is to see what actually happened, and an event that
// renders as nothing is indistinguishable from one that was never sent.

const surfaces = {
  cells: document.getElementById("cells"),
  transcript: document.getElementById("transcript"),
};

/** A line of the conversation, appended in the order it arrived. */
export const log = (who, text) => ({ surface: "log", who, text });

/**
 * A named value that replaces its own previous value.
 *
 * For what arrives faster than anyone can read it — a voice-activity score lands many
 * times a second — where appending would bury the conversation in its own telemetry.
 * `level`, between 0 and 1, draws the bar; a cell with nothing to meter passes 0 and
 * draws none, so there is only one kind of cell rather than two.
 */
export const cell = (name, text, level = 0) => ({ surface: "cell", name, text, level });

/**
 * How each message the agent publishes shows up.
 *
 * A table rather than a chain of type comparisons: a new message type is a row, and
 * `show` never learns that the protocol grew.
 *
 * These are the messages `openconv-agent` actually publishes today. The rest of
 * `ServerEvent` is real protocol that nothing in this service emits, and speculative
 * views for messages that never arrive would be code no run can check — those fall
 * through to raw, which is what makes their first appearance visible rather than silent.
 */
const VIEWS = {
  conversation_initiation_metadata: (event) =>
    log("system", `conversation ${event.conversation_initiation_metadata_event.conversation_id}`),
  user_transcript: (event) => log("you", event.user_transcription_event.user_transcript),
  // The running guess, replaced as it firms up. It becomes a `user_transcript` line the
  // moment the agent decides the utterance ended.
  tentative_user_transcript: (event) =>
    cell("hearing", event.tentative_user_transcription_event.user_transcript),
  agent_response: (event) => log("agent", event.agent_response_event.agent_response),
  interruption: () => log("system", "interrupted"),
  vad_score: (event) =>
    cell("voice", event.vad_score_event.vad_score.toFixed(2), event.vad_score_event.vad_score),
  client_tool_call: (event) =>
    log(
      "tool",
      `${event.client_tool_call.tool_name}(${JSON.stringify(event.client_tool_call.parameters)})`,
    ),
};

const RENDER = {
  log: ({ who, text }) => {
    const row = document.createElement("div");
    row.className = `row row-${who}`;
    row.innerHTML = '<span class="who"></span><span class="text"></span>';
    // Set as text, never as markup: a transcript carries whatever was said, and what
    // was said is not this page's to execute.
    row.querySelector(".who").textContent = who;
    row.querySelector(".who").classList.add(`who-${who}`);
    row.querySelector(".text").textContent = text;
    surfaces.transcript.append(row);
    surfaces.transcript.scrollTop = surfaces.transcript.scrollHeight;
  },
  cell: ({ name, text, level }) => {
    const element = cellElement(name);
    element.querySelector(".cell-text").textContent = text;
    element.querySelector(".cell-bar").style.width = `${Math.round(level * 60)}px`;
  },
};

/** The status cell of a given name, created the first time that name is used. */
function cellElement(name) {
  const existing = surfaces.cells.querySelector(`[data-cell="${name}"]`);
  if (existing) return existing;

  const element = document.createElement("div");
  element.className = "cell";
  element.dataset.cell = name;
  element.innerHTML =
    '<span class="cell-name"></span><span class="cell-bar"></span><span class="cell-text"></span>';
  element.querySelector(".cell-name").textContent = name;
  surfaces.cells.append(element);
  return element;
}

/** Puts one thing on screen — a view above, or a row this page built itself. */
export function render(rendered) {
  RENDER[rendered.surface](rendered);
}

/** Puts one control event on screen, whether or not this page knows its type. */
export function show(event) {
  const view = VIEWS[event.type] ?? ((raw) => log("raw", JSON.stringify(raw)));
  render(view(event));
}
