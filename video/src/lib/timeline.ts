// Turns a launch and its recordings into one schedule, in seconds: the
// terminal's byte stream, every keystroke, camera shots, highlights, and
// the sounds that go with them. Pure, so the soundtrack script
// (scripts/sound.ts) computes exactly what the frames show.

import type { Cast } from "./cast";
import {
  CHAT_HEADER,
  CHAT_INPUT,
  type ChatItem,
  ITEM_GAP,
  chatLayout,
  height,
  middle,
} from "./chat";
import {
  type AgentBeat,
  type At,
  type ChatTurn,
  type Command,
  type Launch,
  type LineRef,
  type Target,
  isAgent,
  isChat,
} from "./launch";
import { type Layout, type Rect, layout, lineRect } from "./layout";
import { rng } from "./random";
import { Screen } from "./screen";

export type McpCall = {
  tool: string;
  arguments: Record<string, string | number>;
  result: Record<string, unknown>;
};

export type AgentCapture = { sats: string; agent: string; calls: McpCall[] };

export type Captures = {
  casts: Record<string, Cast>;
  agents: Record<string, AgentCapture>;
};

export type SoundKind =
  | "key"
  | "space"
  | "enter"
  | "paste"
  | "whoosh"
  | "tick"
  | "pop"
  | "chime"
  | "logo";

export type Sound = { t: number; kind: SoundKind; seed: number };

/** `crop` shots frame terminal lines, which may run off the right edge. */
export type Shot = { t: number; move: number; rect: Rect; zoom?: number; crop: boolean };

export type Timeline = {
  launch: Launch;
  layout: Layout;
  duration: number;
  marks: Record<string, number>;
  acts: Record<"title" | "terminal" | "tagline" | "end", [number, number]>;
  stream: { t: number; data: string }[];
  keys: number[];
  pastes: { t: number; line: number; cols: [number, number] }[];
  scroll: { t: number; line: number }[];
  commands: { id: string; prompt: number; last: number; start: number }[];
  shots: Shot[];
  highlights: {
    t: number;
    end: number;
    command: string;
    line: number;
    cols: [number, number];
    note?: string;
  }[];
  captions: { t: number; end: number; text: string }[];
  agent: null | {
    capture: AgentCapture;
    enter: number;
    call: number;
    result: number;
    update: number;
  };
  chat: null | { capture: AgentCapture; items: ChatItem[] };
  sounds: Sound[];
};

// The shell around sats: an orange prompt and bold input, as on sats.sh.
export const PROMPT = "\x1b[38;2;247;147;26m$\x1b[0m ";
const BOLD = "\x1b[1m";
const UNBOLD = "\x1b[22m";

// A status line that erases itself, like sats' "syncing…".
const TRANSIENT = /^([^\r\n]+)\r +\r/;

const WINDOW_IN = 0.9;
const STATUS_HOLD = 0.42;
const LINE_STAGGER = 0.028;

export const buildTimeline = (launch: Launch, captures: Captures): Timeline => {
  const { terminal } = launch;
  const random = rng(launch.id);
  const marks: Record<string, number> = {};
  const sounds: Sound[] = [];
  const stream: Timeline["stream"] = [];
  const keys: number[] = [];
  const pastes: Timeline["pastes"] = [];
  const scroll: Timeline["scroll"] = [];
  const commands: Timeline["commands"] = [];
  const screen = new Screen(terminal.columns);
  let agent: Timeline["agent"] = null;
  let agentStep: AgentBeat | null = null;
  const chatCapture = launch.chat ? captures.agents[launch.chat.capture] : undefined;
  if (launch.chat && !chatCapture) throw new Error(`no recording ${launch.chat.capture}.json`);
  const chatItems: ChatItem[] = [];
  const copies: { item: ChatItem & { kind: "reply" }; by: string }[] = [];

  const mark = (name: string, t: number) => {
    if (name in marks) throw new Error(`mark ${name} is set twice`);
    marks[name] = t;
  };
  const sound = (t: number, kind: SoundKind) =>
    sounds.push({ t, kind, seed: Math.floor(random() * 1e9) });
  const emit = (t: number, data: string) => {
    stream.push({ t, data });
    screen.write(data);
  };
  // Print a block of output line by line, a few milliseconds apart.
  const print = (t: number, data: string) => {
    const lines = data.split(/(?<=\r\n)/);
    lines.forEach((line, i) => emit(t + i * LINE_STAGGER, line));
    if (data.includes("✓")) sound(t, "chime");
    return t + (lines.length - 1) * LINE_STAGGER;
  };
  const keystroke = (t: number, ch: string) => {
    keys.push(t);
    sound(t, ch === " " ? "space" : "key");
  };
  // A quick, slightly uneven typist; a beat longer between words.
  const typingGap = (ch: string) =>
    0.03 + random() * 0.022 + (ch === " " ? 0.045 : 0);

  // `promptAt` is when the prompt appears; the first one waits in the window as it rises.
  const command = (step: Command, start: number, promptAt = start): number => {
    const cast = captures.casts[step.capture];
    if (!cast) throw new Error(`no recording ${step.capture}.cast`);
    let typed = "";
    const parts = step.type.map((part) => {
      const text =
        typeof part === "string"
          ? part
          : part.paste === true
            ? cast.command.slice(typed.length)
            : part.paste;
      typed += text;
      return { text, paste: typeof part !== "string" };
    });
    if (typed !== cast.command) {
      throw new Error(
        `${step.id}: types "${typed}" but the recording ran "${cast.command}"`,
      );
    }
    if (cast.width !== terminal.columns) {
      throw new Error(`${step.capture} was recorded ${cast.width} columns wide`);
    }
    if (screen.col !== 0) {
      throw new Error(`${step.id}: the previous command left the cursor mid-line`);
    }
    const prompt = screen.row;
    mark(`${step.id}.start`, start);
    scroll.push({ t: promptAt, line: prompt });
    emit(promptAt, PROMPT);

    let t = start + 0.32;
    for (const part of parts) {
      if (part.paste) {
        t += 0.14;
        pastes.push({ t, line: screen.row, cols: [screen.col, screen.col + part.text.length] });
        emit(t, BOLD + part.text + UNBOLD);
        sound(t, "paste");
        t += 0.3;
      } else {
        for (const ch of part.text) {
          emit(t, BOLD + ch + UNBOLD);
          keystroke(t, ch);
          t += typingGap(ch);
        }
      }
    }
    mark(`${step.id}.typed`, t);
    t += 0.2;
    emit(t, "\r\n");
    sound(t, "enter");
    mark(`${step.id}.enter`, t);

    let real = 0;
    let inputs = 0;
    let output = false;
    const events = cast.events;
    for (let i = 0; i < events.length; i++) {
      const event = events[i];
      if (event.kind === "i") {
        const keysTyped = event.data.replace(/\r$/, "");
        const next = events[i + 1];
        if (!next || next.kind !== "o") {
          throw new Error(`${step.capture}: input without the terminal's echo`);
        }
        const echoed = `${keysTyped}\r\n`;
        const visible = next.data.startsWith(echoed);
        if (!visible && !next.data.startsWith("\r\n")) {
          throw new Error(`${step.capture}: unexpected echo ${JSON.stringify(next.data)}`);
        }
        t += step.think?.[inputs] ?? 0.5;
        mark(`${step.id}.input.${inputs}`, t);
        for (const ch of keysTyped) {
          if (visible) emit(t, ch);
          keystroke(t, ch);
          t += visible ? 0.09 : 0.05 + random() * 0.025;
        }
        t += 0.14;
        emit(t, "\r\n");
        sound(t, "enter");
        mark(`${step.id}.input.${inputs}.enter`, t);
        const rest = next.data.slice(visible ? echoed.length : 2);
        if (rest) {
          t += Math.min(next.t - event.t, step.wait ?? 0.6) + 0.06;
          t = print(t, rest);
        }
        mark(`${step.id}.response.${inputs}`, t);
        real = next.t;
        inputs += 1;
        i += 1;
        continue;
      }
      let data = event.data;
      t += Math.max(0.06, Math.min(event.t - real, step.wait ?? 0.6));
      real = event.t;
      const status = TRANSIENT.exec(data);
      if (status) {
        emit(t, status[0]);
        // Shown, then erased: the program's own `\r spaces \r`.
        stream[stream.length - 1] = { t, data: status[1] };
        stream.push({ t: t + STATUS_HOLD, data: status[0].slice(status[1].length) });
        if (!(`${step.id}.status` in marks)) mark(`${step.id}.status`, t);
        t += STATUS_HOLD;
        data = data.slice(status[0].length);
        if (!data) continue;
      }
      if (!output) mark(`${step.id}.output`, t);
      output = true;
      t = print(t, data);
    }
    mark(`${step.id}.done`, t);
    // The cursor's empty line belongs to the next prompt.
    const last = screen.col === 0 ? screen.row - 1 : screen.row;
    commands.push({ id: step.id, prompt, last, start });
    return t + step.hold;
  };

  // A recorded value in a reply: `{1.txid}`, or `{1.txid|short}` for its ends.
  const fill = (text: string) =>
    text.replace(/\{(\d+)\.(\w+)(\|short)?\}/g, (_, call: string, field: string, short?: string) => {
      const value = chatCapture?.calls[Number(call)]?.result[field];
      if (typeof value !== "string") throw new Error(`no recorded value ${call}.${field}`);
      return short ? middle(value, 4) : value;
    });

  const chatTurn = (step: ChatTurn, start: number): number => {
    if (!chatCapture) throw new Error(`${step.id}: this launch has no chat`);
    mark(`${step.id}.start`, start);
    if (step.chat === "user") {
      let t = start + 0.25;
      const keys: number[] = [];
      for (const ch of step.text) {
        keys.push(t);
        sound(t, ch === " " ? "space" : "key");
        t += typingGap(ch);
      }
      mark(`${step.id}.typed`, t);
      t += 0.22;
      sound(t, "enter");
      mark(`${step.id}.sent`, t);
      chatItems.push({ kind: "user", id: step.id, t, text: step.text, keys });
      return t + (step.hold ?? 0.45);
    }
    if (step.chat === "tool") {
      const call = chatCapture.calls[step.call];
      if (!call) throw new Error(`${step.id}: no recorded call ${step.call}`);
      // A status check shows only its answer; a request, what the agent asked for.
      const rows: [string, string][] =
        call.tool === "check_request"
          ? []
          : Object.entries(call.arguments)
              .filter(([key]) => key !== "idempotency_key")
              .map(([key, value]) => [key, key === "address" ? middle(String(value), 12) : String(value)]);
      sound(start, "tick");
      const result = start + (rows.length ? 0.75 + rows.length * 0.08 : 0.5);
      sound(result, "pop");
      mark(`${step.id}.result`, result);
      chatItems.push({ kind: "tool", id: step.id, t: start, result, call, rows });
      return result + (step.hold ?? 0.4);
    }
    const text = fill(step.text);
    const words: number[] = [];
    let t = start;
    for (const word of text.split(/[ \n]/)) {
      words.push(t);
      t += 0.04 + Math.min(word.length, 10) * 0.004;
    }
    const item: ChatItem & { kind: "reply" } = { kind: "reply", id: step.id, t: start, text, words };
    if (step.command) {
      t += 0.12;
      item.command = { text: fill(step.command), t };
      sound(t, "tick");
      t += 0.3;
      if (step.copiedBy) copies.push({ item, by: step.copiedBy });
    }
    mark(`${step.id}.done`, t);
    chatItems.push(item);
    return t + (step.hold ?? 0.5);
  };

  // Title, then the terminal rising as the title leaves.
  mark("title.start", 0);
  sound(0.3, "logo");
  const titleEnd = launch.title.hold;
  mark("title.end", titleEnd);
  const terminalStart = titleEnd - 0.2;
  mark("terminal.start", terminalStart);
  sound(terminalStart, "whoosh");

  let t = terminalStart + WINDOW_IN;
  for (const step of terminal.steps) {
    const first = commands.length === 0;
    if (isChat(step)) {
      t = chatTurn(step, t);
    } else if (isAgent(step)) {
      const capture = captures.agents[step.capture];
      if (!capture) throw new Error(`no recording ${step.capture}.json`);
      mark("agent.enter", t);
      sound(t, "whoosh");
      mark("agent.call", t + 0.55);
      mark("agent.result", t + 1.5);
      sound(t + 1.5, "pop");
      agent = { capture, enter: t, call: t + 0.55, result: t + 1.5, update: 0 };
      agentStep = step;
      t += step.hold;
      mark("agent.end", t);
    } else {
      t = command(step, t, first ? terminalStart : t);
    }
  }
  const terminalEnd = t;
  mark("terminal.end", terminalEnd);

  const at = (moment: At) => {
    const base = marks[moment.at];
    if (base === undefined) throw new Error(`no mark ${moment.at}`);
    return base + (moment.delay ?? 0);
  };
  if (agent && agentStep) {
    agent.update = at(agentStep.update);
    sound(agent.update, "pop");
  }
  // The person copies a command from the chat until they paste it.
  for (const { item, by } of copies) {
    if (item.command) item.command.copied = [at({ at: `${by}.start` }), at({ at: `${by}.enter` })];
  }

  const taglineStart = terminalEnd;
  mark("tagline.start", taglineStart);
  sound(taglineStart, "whoosh");
  const endStart = taglineStart + launch.tagline.hold;
  mark("end.start", endStart);
  sound(endStart, "logo");
  const duration = endStart + launch.end.hold;
  mark("end", duration);

  const geometry = layout(terminal.columns, terminal.rows, Boolean(launch.chat));
  const lines = (id: string) => {
    const found = commands.find((c) => c.id === id);
    if (!found) throw new Error(`no command ${id}`);
    return found;
  };
  const resolve = (id: string, ref: LineRef): number => {
    const cmd = lines(id);
    if (typeof ref === "number") return cmd.prompt + ref;
    for (let line = cmd.prompt; line <= cmd.last; line++) {
      if (screen.text(line).trimStart().startsWith(ref)) return line;
    }
    throw new Error(`${id}: no line starts with "${ref}"`);
  };
  const extent = (from: number, to: number): [number, number] => {
    let start = Infinity;
    let end = 0;
    for (let line = from; line <= to; line++) {
      const text = screen.text(line);
      const trimmed = text.trimEnd();
      if (!trimmed) continue;
      start = Math.min(start, text.length - text.trimStart().length);
      end = Math.max(end, trimmed.length);
    }
    return start === Infinity ? [0, 1] : [start, end];
  };
  const chatRect = (target: Target & { kind: "chat" }, t: number): Rect => {
    const chat = geometry.chat;
    if (!chat) throw new Error("a chat shot in a launch without a chat");
    const bottom = chat.y + chat.h;
    if (!target.from) {
      return target.input ? { x: chat.x, y: bottom - CHAT_INPUT - 150, w: chat.w, h: CHAT_INPUT + 150 } : chat;
    }
    const index = (id: string) => {
      const i = chatItems.findIndex((item) => item.id === id);
      if (i < 0) throw new Error(`no chat message ${id}`);
      return i;
    };
    const first = index(target.from);
    const last = index(target.to ?? target.from);
    // Framed as the messages sit once the last of them has scrolled in.
    const { tops, offset } = chatLayout(chatItems, chat.h, Math.max(t, chatItems[last].t) + 0.6);
    const area = chat.y + CHAT_HEADER;
    const top = Math.max(area, area + tops[first] - offset - ITEM_GAP);
    const end = target.input
      ? bottom
      : Math.min(bottom - CHAT_INPUT, area + tops[last] - offset + height(chatItems[last]) + ITEM_GAP);
    return { x: chat.x, y: top, w: chat.w, h: end - top };
  };
  const rect = (target: Target, t: number): { rect: Rect; zoom?: number; crop: boolean } => {
    if (target.kind === "chat") return { rect: chatRect(target, t), zoom: target.zoom, crop: false };
    if (target.kind === "window") return { rect: geometry.window, zoom: target.zoom, crop: false };
    if (target.kind === "card") {
      return { rect: target.result ? geometry.cardResult : geometry.card, zoom: target.zoom, crop: false };
    }
    if (target.kind === "stage") return { rect: geometry.stage, zoom: target.zoom, crop: false };
    const from = resolve(target.command, target.from);
    const to = target.to === undefined ? from : resolve(target.command, target.to);
    const cols = target.cols ?? extent(from, to);
    const top = lines(target.command).prompt;
    return { rect: lineRect(from, to - from + 1, cols, top), zoom: target.zoom, crop: true };
  };

  const shots = terminal.shots
    .map((shot) => ({ t: at(shot), move: shot.move ?? 0.8, ...rect(shot.target, at(shot)) }))
    .sort((a, b) => a.t - b.t);
  const highlights = terminal.highlights.map((h) => {
    const line = resolve(h.command, h.line);
    const t = at(h);
    sound(t, "tick");
    return {
      t,
      end: t + h.hold,
      command: h.command,
      line,
      cols: h.cols ?? extent(line, line),
      note: h.note,
    };
  });
  const captions = terminal.captions.map((c) => ({
    t: at(c),
    end: c.until ? at(c.until) : at(c) + (c.hold ?? 2),
    text: c.text,
  }));

  sounds.sort((a, b) => a.t - b.t);
  return {
    launch,
    layout: geometry,
    duration,
    marks,
    acts: {
      title: [0, titleEnd],
      terminal: [terminalStart, terminalEnd],
      tagline: [taglineStart, endStart],
      end: [endStart, duration],
    },
    stream,
    keys,
    pastes,
    scroll,
    commands,
    shots,
    highlights,
    captions,
    agent,
    chat: chatCapture ? { capture: chatCapture, items: chatItems } : null,
    sounds,
  };
};

/** The terminal as it stands at `t`. */
export const screenAt = (timeline: Timeline, t: number): Screen => {
  const screen = new Screen(timeline.launch.terminal.columns);
  for (const chunk of timeline.stream) {
    if (chunk.t > t) break;
    screen.write(chunk.data);
  }
  return screen;
};
