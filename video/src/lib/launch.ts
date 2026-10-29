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

export type Step = Command | AgentBeat;

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
  tagline: { lines: string[]; hold: number };
  end: { url: string; install: string; footer: string; hold: number };
};

export const isAgent = (step: Step): step is AgentBeat => step.id === "agent";
