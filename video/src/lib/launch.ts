// The shape of a launch video: what it says, which recordings it plays,
// and where the camera looks. A release's video is one value of this type
// (see src/demos/); everything else is shared.

/** A moment on the timeline: a named mark, plus seconds. */
export type At = { at: string; delay?: number };

/**
 * A line of one command's screen: an offset from its prompt line (0), or
 * the first line, from the prompt on, whose text starts with this string
 * once leading spaces are dropped.
 */
export type LineRef = number | string;

export type Target =
  | { kind: "window"; zoom?: number }
  // The agent's card; `result` frames it grown to show the outcome.
  | { kind: "card"; result?: boolean; zoom?: number }
  | { kind: "stage"; zoom?: number }
  // The chat window, or messages `from` through `to` as they sit once
  // `to` has arrived; `input` adds the input box under them.
  | { kind: "chat"; from?: string; to?: string; input?: boolean; zoom?: number }
  | {
      kind: "lines";
      command: string;
      from: LineRef;
      to?: LineRef;
      // Columns to frame; the lines' text extent by default.
      cols?: [number, number];
      zoom?: number;
    };

export type Shot = At & { target: Target; move?: number };

export type Highlight = At & {
  command: string;
  line: LineRef;
  cols?: [number, number];
  hold: number;
  note?: string;
};

/** On screen from `at` until `until` (or for `hold` seconds). */
export type Caption = At & { text: string; until?: At; hold?: number };

/** Typed text, or text pasted at once; `paste: true` is the rest of the command. */
export type Typed = string | { paste: string | true };

export type Command = {
  id: string;
  /** A recording in captures/, without the .cast extension. */
  capture: string;
  /** Typed and pasted parts; together they must equal the recorded command. */
  type: Typed[];
  /** Seconds the person reads before each recorded input. */
  think?: number[];
  /** The longest the screen waits on the program (unlock, broadcast). */
  wait?: number;
  hold: number;
};

export type AgentBeat = {
  id: "agent";
  /** The MCP exchange in captures/, without the .json extension. */
  capture: string;
  hold: number;
  /** When the card shows the outcome the agent observes. */
  update: At;
};

/**
 * A turn in the assistant's chat. `user` is typed into the input box and
 * sent; `tool` shows `call` from the chat's MCP recording; `reply` streams
 * in. A reply's text and command may name a recorded value as
 * `{call.field}`, or `{call.field|short}` to keep only its ends; `copiedBy`
 * is the command that pastes it.
 */
export type ChatTurn =
  | { id: string; chat: "user"; text: string; hold?: number }
  | { id: string; chat: "tool"; call: number; hold?: number }
  | { id: string; chat: "reply"; text: string; command?: string; copiedBy?: string; hold?: number };

export type Step = Command | AgentBeat | ChatTurn;

export type Launch = {
  id: string;
  fps: number;
  title: { version: string; headline: string; hold: number };
  terminal: {
    columns: number;
    rows: number;
    title: string;
    status: string;
    steps: Step[];
    shots: Shot[];
    highlights: Highlight[];
    captions: Caption[];
  };
  /** An assistant beside the terminal, its MCP exchange recorded in captures/. */
  chat?: { capture: string; name: string; label: string; placeholder: string };
  /** Where the soundtrack's groove starts, and its hats join. */
  music: { groove: At; hats: At };
  tagline: { lines: string[]; hold: number };
  end: { url: string; install: string; footer: string; hold: number };
};

export const isChat = (step: Step): step is ChatTurn => "chat" in step;
export const isAgent = (step: Step): step is AgentBeat => !isChat(step) && step.id === "agent";
