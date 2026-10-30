// The assistant's chat window: what each message holds and where it sits.
// Pure, like the timeline, so the camera can frame the messages the
// component draws. The chat is monospace too, so a message's height
// follows from its text.

import { mix, progress } from "./anim";
import { ease } from "../theme";
import type { McpCall } from "./timeline";

export const CHAT_WIDTH = 900;
export const CHAT_GAP = 84;
export const CHAT_HEADER = 68;
export const CHAT_INPUT = 124;
export const CHAT_PAD = 36;
export const CHAT_FONT = 24;
export const CHAT_CHAR = CHAT_FONT * 0.6;
export const CHAT_LINE = 36;
export const ITEM_GAP = 24;
export const BUBBLE_MAX = 640;
export const BUBBLE_PAD_X = 22;
export const BUBBLE_PAD_Y = 14;
export const TOOL_HEAD = 54;
export const TOOL_ROW = 36;
export const TOOL_RESULT = 46;
export const TOOL_COMPACT = 60;
export const CHIP_GAP = 14;
export const CHIP_HEIGHT = 60;
const SCROLL = 0.55;

export type ChatItem =
  | { kind: "user"; id: string; t: number; text: string; keys: number[] }
  | { kind: "tool"; id: string; t: number; result: number; call: McpCall; rows: [string, string][] }
  | {
      kind: "reply";
      id: string;
      t: number;
      text: string;
      // When each word appears: replies stream in.
      words: number[];
      command?: { text: string; t: number; copied?: [number, number] };
    };

/** Long values keep their ends, as `sats agent requests` shortens them. */
export const middle = (value: string, keep: number) =>
  value.length <= keep * 2 + 1 ? value : `${value.slice(0, keep)}…${value.slice(-keep)}`;

/** Greedy word wrap at `columns`; "\n" always breaks. */
export const wrap = (text: string, columns: number): string[] =>
  text.split("\n").flatMap((part) => {
    const lines: string[] = [];
    let line = "";
    for (const word of part.split(" ")) {
      if (line && line.length + 1 + word.length > columns) {
        lines.push(line);
        line = word;
      } else {
        line = line ? `${line} ${word}` : word;
      }
    }
    lines.push(line);
    return lines;
  });

export const INNER = CHAT_WIDTH - 2 * CHAT_PAD;
export const BUBBLE_COLUMNS = Math.floor((BUBBLE_MAX - 2 * BUBBLE_PAD_X) / CHAT_CHAR);
export const REPLY_COLUMNS = Math.floor(INNER / CHAT_CHAR);

export const height = (item: ChatItem): number => {
  switch (item.kind) {
    case "user":
      return wrap(item.text, BUBBLE_COLUMNS).length * CHAT_LINE + 2 * BUBBLE_PAD_Y;
    case "tool":
      return item.rows.length
        ? TOOL_HEAD + item.rows.length * TOOL_ROW + 8 + TOOL_RESULT + 14
        : TOOL_COMPACT;
    case "reply":
      return (
        wrap(item.text, REPLY_COLUMNS).length * CHAT_LINE +
        (item.command ? CHIP_GAP + CHIP_HEIGHT : 0)
      );
  }
};

/** Message tops in the scrolled message area, and its scroll offset, at `t`. */
export const chatLayout = (items: ChatItem[], windowHeight: number, t: number) => {
  const area = windowHeight - CHAT_HEADER - CHAT_INPUT;
  const tops: number[] = [];
  let y = ITEM_GAP;
  let offset = 0;
  for (const item of items) {
    tops.push(y);
    y += height(item) + ITEM_GAP;
    // Each message scrolls the newest one into view as it arrives.
    if (item.t <= t) offset = mix(offset, Math.max(0, y - area), progress(t, item.t, SCROLL, ease.inOut));
  }
  return { area, tops, offset };
};
