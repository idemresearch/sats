"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import type { IMarker, ITheme, Terminal as XTerm } from "@xterm/xterm";

type WasmWallet = {
  init(now: number): string;
  reset(): void;
  has_wallet(): boolean;
  receive(): string;
  balance(): string;
  faucet(amount: string, now: number): string;
  prepare(recipient: string, amount: string, feeRate: number, now: number): string;
  confirm(id: string, now: number): string;
  cancel(id: string): string;
  grant(
    agent: string,
    budget: string,
    maxTx: string | undefined,
    maxFee: string | undefined,
    lifetimeSecs: number,
    now: number
  ): string;
  grants(now: number): string;
  revoke(agent: string): string;
  request_send(
    agent: string,
    recipient: string,
    amount: string,
    now: number
  ): string;
  requests(): string;
  approve(id: string, feeRate: number, now: number): string;
  dismiss(id: string, now: number): string;
  history(): string;
};

type Line = { cls: string; text: string };
type QuickAction = { label: string; command: string; primary?: boolean };

const RESET = "\x1b[0m";
const BOLD = "\x1b[1m";
const PROMPT = "\x1b[38;2;247;147;26m";

const LINE_STYLE: Record<string, string> = {
  "": "",
  "t-c": BOLD,
  "t-o": "",
  "t-g": "\x1b[32m",
  "t-r": "\x1b[31m",
  "t-dim": "\x1b[2m",
};

const COMPLETIONS = [
  "help",
  "clear",
  "reset",
  "sats init",
  "sats receive",
  "sats balance",
  "sats faucet 100k",
  "sats send ",
  "sats history",
  "sats agent grant ",
  "sats agent list",
  "sats agent revoke ",
  "sats agent request ",
  "sats agent requests",
  "sats agent approve ",
  "sats agent dismiss ",
];

function cssValue(name: string, fallback: string): string {
  return (
    getComputedStyle(document.documentElement).getPropertyValue(name).trim() ||
    fallback
  );
}

function terminalTheme(): ITheme {
  const background = cssValue("--surface", "#11110f");
  const foreground = cssValue("--foreground", "#f1f0ea");
  const output = cssValue("--output", "#c8c7c0");
  const subtle = cssValue("--subtle", "#73736c");
  const accent = cssValue("--accent", "#f7931a");
  const success = cssValue("--success", "#67d391");
  const danger = cssValue("--danger", "#ff8178");

  return {
    background,
    foreground,
    cursor: accent,
    cursorAccent: background,
    selectionBackground: cssValue("--accent-soft", "rgba(247, 147, 26, 0.22)"),
    selectionInactiveBackground: cssValue("--surface-secondary", "#151513"),
    black: background,
    brightBlack: subtle,
    red: danger,
    brightRed: danger,
    green: success,
    brightGreen: success,
    yellow: accent,
    brightYellow: accent,
    blue: output,
    brightBlue: foreground,
    magenta: output,
    brightMagenta: foreground,
    cyan: output,
    brightCyan: foreground,
    white: output,
    brightWhite: foreground,
  };
}

function styledLine(line: Line): string {
  return `${LINE_STYLE[line.cls] ?? ""}${line.text}${RESET}`;
}

function styledPrompt(prompt: string, input: string): string {
  return `${PROMPT}${prompt}${RESET}${BOLD}${input}\x1b[22m`;
}

const WELCOME: Line[] = [
  { cls: "t-c", text: "sats playground" },
  {
    cls: "t-dim",
    text: "real sats-core · WebAssembly · simulated signet · local to this tab",
  },
  { cls: "t-o", text: "Start with 'sats init' or run 'help'." },
  { cls: "", text: "" },
];

const HELP: Line[] = [
  ["t-o", "  sats init                    create a wallet (12-word seed)"],
  ["t-o", "  sats receive                 show a receive address"],
  ["t-o", "  sats balance                 balance and simulated chain height"],
  ["t-o", "  sats faucet [amount]         playground faucet (default 100k)"],
  ["t-o", "  sats send <addr> <amt>       prepare · confirm · sign · broadcast"],
  ["t-o", "  sats history                 finalized transactions"],
  ["t-dim", "the agent surface — what an AI can do:"],
  ["t-o", "  sats agent request <name> <addr> <amt>  file a request (it waits for you)"],
  ["t-dim", "your control plane — the agent cannot call these:"],
  ["t-o", "  sats agent grant <name> --budget <sats> [--for 24h]"],
  ["t-o", "  sats agent requests          review pending requests"],
  ["t-o", "  sats agent approve <id>      authorize one request: it signs and sends"],
  ["t-o", "  sats agent dismiss <id>      decline a request"],
  ["t-o", "  sats agent list              current authority"],
  ["t-o", "  sats agent revoke <name>     delete a grant"],
  ["t-o", "  clear · reset"],
  ["t-dim", "amount shorthand: 25k = 25,000 · 1.5m = 1,500,000"],
].map(([cls, text]) => ({ cls, text }));

const DEFAULT_ACTIONS: QuickAction[] = [
  { label: "create wallet", command: "sats init", primary: true },
  { label: "add 100k signet sats", command: "sats faucet 100k" },
  { label: "check balance", command: "sats balance" },
  { label: "show help", command: "help" },
];

const CONFIRM_ACTIONS: QuickAction[] = [
  { label: "confirm send", command: "y", primary: true },
  { label: "cancel", command: "n" },
];

function fmt(n: number): string {
  return Math.trunc(n).toLocaleString("en-US");
}

function parseDuration(s: string): number | null {
  const match = /^(\d+(?:\.\d+)?)(s|m|h|d)$/.exec(s.trim());
  if (!match) return null;
  const mult = { s: 1, m: 60, h: 3600, d: 86400 }[
    match[2] as "s" | "m" | "h" | "d"
  ];
  const secs = Math.round(parseFloat(match[1]) * mult);
  return secs > 0 ? secs : null;
}

function humanDuration(secs: number): string {
  if (secs % 86400 === 0) return secs / 86400 + "d";
  if (secs % 3600 === 0) return secs / 3600 + "h";
  if (secs % 60 === 0) return secs / 60 + "m";
  return secs + "s";
}

function splitFlags(tokens: string[]): {
  args: string[];
  flags: Map<string, string>;
} {
  const args: string[] = [];
  const flags = new Map<string, string>();
  for (let i = 0; i < tokens.length; i++) {
    if (tokens[i].startsWith("--")) {
      flags.set(tokens[i].slice(2), tokens[i + 1] ?? "");
      i++;
    } else {
      args.push(tokens[i]);
    }
  }
  return { args, flags };
}

export default function Playground() {
  const [prompt, setPromptState] = useState("$ ");
  const [isRunning, setIsRunning] = useState(false);
  const [terminalReady, setTerminalReady] = useState(false);
  const [terminalError, setTerminalError] = useState<string | null>(null);
  const walletRef = useRef<WasmWallet | null>(null);
  const loadingRef = useRef<Promise<void> | null>(null);
  const pendingSendRef = useRef<string | null>(null);
  const histRef = useRef<string[]>([]);
  const histPosRef = useRef(0);
  const histDraftRef = useRef("");
  const terminalHostRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<XTerm | null>(null);
  const inputMarkerRef = useRef<IMarker | null>(null);
  const inputBufferRef = useRef("");
  const inputCursorRef = useRef(0);
  const promptRef = useRef("$ ");
  const runningRef = useRef(false);
  const outputQueueRef = useRef<Line[]>([]);
  const executeRef = useRef<(raw: string) => Promise<void>>(async () => {});
  const inputHandlerRef = useRef<(data: string) => void>(() => {});

  const setPrompt = useCallback((value: string) => {
    promptRef.current = value;
    setPromptState(value);
  }, []);

  const push = useCallback((...added: Line[]) => {
    const terminal = terminalRef.current;
    if (!terminal) {
      outputQueueRef.current.push(...added);
      return;
    }
    for (const line of added) terminal.writeln(styledLine(line));
  }, []);

  const ensureWallet = useCallback(async (): Promise<WasmWallet> => {
    if (walletRef.current) return walletRef.current;
    if (!loadingRef.current) {
      const importUrl = new Function("u", "return import(u)") as (
        url: string
      ) => Promise<{
        default: (module?: string) => Promise<unknown>;
        Playground: new () => WasmWallet;
      }>;
      loadingRef.current = (async () => {
        const mod = await importUrl("/playground/sats_web.js");
        await mod.default("/playground/sats_web_bg.wasm");
        walletRef.current = new mod.Playground();
      })();
    }
    await loadingRef.current;
    return walletRef.current!;
  }, []);

  useEffect(() => {
    const idle =
      (
        window as {
          requestIdleCallback?: (callback: () => void) => number;
        }
      ).requestIdleCallback ??
      ((callback: () => void) => window.setTimeout(callback, 1500));
    idle(() => {
      ensureWallet().catch(() => {
        loadingRef.current = null;
      });
    });
  }, [ensureWallet]);

  const now = () => Date.now() / 1000;

  function kv(rows: [string, string][], cls = "t-o"): Line[] {
    const width = Math.max(...rows.map(([key]) => key.length));
    return rows.map(([key, value]) => ({
      cls,
      text: key.padEnd(width + 2) + value,
    }));
  }

  async function run(raw: string): Promise<void> {
    const cmd = raw.trim();

    if (pendingSendRef.current !== null) {
      const id = pendingSendRef.current;
      pendingSendRef.current = null;
      setPrompt("$ ");
      const wallet = await ensureWallet();
      if (/^y(es)?$/i.test(cmd)) {
        const sent = JSON.parse(wallet.confirm(id, now()));
        push(
          { cls: "t-g", text: "signed · saved · broadcast ✓" },
          {
            cls: "t-dim",
            text:
              "txid " +
              sent.txid +
              " · confirmed in simulated block " +
              fmt(sent.height),
          }
        );
      } else {
        wallet.cancel(id);
        push({ cls: "t-dim", text: "cancelled — nothing was signed" });
      }
      return;
    }

    if (cmd === "") return;
    if (histRef.current.at(-1) !== cmd) histRef.current.push(cmd);
    histPosRef.current = histRef.current.length;
    histDraftRef.current = "";

    if (cmd === "clear") {
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = null;
      terminalRef.current?.clear();
      terminalRef.current?.write("\x1b[2J\x1b[H");
      return;
    }
    if (cmd === "help" || cmd === "sats help" || cmd === "sats") {
      push(...HELP);
      return;
    }
    if (cmd === "reset") {
      walletRef.current?.reset();
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = null;
      terminalRef.current?.clear();
      terminalRef.current?.write("\x1b[2J\x1b[H");
      push(...WELCOME);
      return;
    }

    const tokens = cmd.split(/\s+/);
    if (tokens[0] !== "sats") {
      push({
        cls: "t-r",
        text: "unknown command " + tokens[0] + " — try 'help'",
      });
      return;
    }

    const wallet = await ensureWallet();
    const { args, flags } = splitFlags(tokens.slice(1));
    const sub = args[0];

    if (sub === "init") {
      if (wallet.has_wallet()) {
        push({
          cls: "t-r",
          text: "a wallet already exists — use 'reset' to start over",
        });
        return;
      }
      const info = JSON.parse(wallet.init(now()));
      push(
        {
          cls: "t-g",
          text: "wallet created · signet · BIP-86 taproot · local signer",
        },
        {
          cls: "t-dim",
          text: "seed (playground only — the native CLI shows this once):",
        },
        { cls: "t-o", text: "  " + info.mnemonic },
        { cls: "t-dim", text: "Next: add simulated coins with 'sats faucet'." }
      );
      return;
    }

    if (!wallet.has_wallet()) {
      push({ cls: "t-r", text: "no wallet — run 'sats init' first" });
      return;
    }

    switch (sub) {
      case "receive": {
        const result = JSON.parse(wallet.receive());
        push({ cls: "t-o", text: result.address });
        return;
      }
      case "balance": {
        const balance = JSON.parse(wallet.balance());
        push({ cls: "t-o", text: fmt(balance.total_sat) + " sat" });
        if (balance.pending_sat > 0) {
          push({
            cls: "t-dim",
            text: fmt(balance.pending_sat) + " sat pending",
          });
        }
        push({
          cls: "t-dim",
          text: "simulated signet · block " + fmt(balance.height),
        });
        return;
      }
      case "faucet": {
        const faucet = JSON.parse(
          wallet.faucet(args[1] ?? "100k", now())
        );
        push(
          {
            cls: "t-g",
            text:
              "+" +
              fmt(faucet.amount_sat) +
              " sat · confirmed in simulated block " +
              fmt(faucet.height),
          },
          {
            cls: "t-dim",
            text:
              "txid " +
              faucet.txid.slice(0, 16) +
              "… · playground faucet only",
          }
        );
        return;
      }
      case "send": {
        if (args.length < 3) {
          push({
            cls: "t-r",
            text: "usage: sats send <address> <amount> [--fee-rate n]",
          });
          return;
        }
        const feeRate = parseInt(flags.get("fee-rate") ?? "2", 10);
        const plan = JSON.parse(
          wallet.prepare(args[1], args[2], feeRate, now())
        );
        push(
          ...kv([
            ["Amount", fmt(plan.amount_sat) + " sat"],
            ["Fee", fmt(plan.fee_sat) + " sat (" + feeRate + " sat/vB)"],
            ["Total", fmt(plan.total_sat) + " sat"],
          ])
        );
        if (plan.excluded_utxos > 0) {
          push({
            cls: "t-dim",
            text:
              plan.excluded_utxos +
              " inscription-suspect utxo(s) excluded from selection",
          });
        }
        pendingSendRef.current = plan.id;
        setPrompt("confirm send? [y/N] ");
        return;
      }
      case "history": {
        const rows = JSON.parse(wallet.history()) as {
          txid: string;
          amount_sat: number;
          fee_sat: number;
          status: string;
        }[];
        if (rows.length === 0) {
          push({ cls: "t-dim", text: "no transactions yet" });
          return;
        }
        for (const row of rows) {
          push({
            cls: "t-o",
            text:
              row.txid.slice(0, 12) +
              "…  " +
              fmt(row.amount_sat).padStart(10) +
              " sat  fee " +
              fmt(row.fee_sat) +
              "  " +
              row.status,
          });
        }
        return;
      }
      case "agent": {
        await runAgent(wallet, args.slice(1), flags);
        return;
      }
      default:
        push({
          cls: "t-r",
          text: "unknown command 'sats " + (sub ?? "") + "' — try 'help'",
        });
    }
  }

  async function runAgent(
    wallet: WasmWallet,
    args: string[],
    flags: Map<string, string>
  ): Promise<void> {
    const sub = args[0];
    if (sub === "grant") {
      const name = args[1];
      const budget = flags.get("budget");
      if (!name || !budget) {
        push({
          cls: "t-r",
          text:
            "usage: sats agent grant <name> --budget <sats> " +
            "[--for 24h] [--max-tx n] [--max-fee n]",
        });
        return;
      }
      const lifetime = parseDuration(flags.get("for") ?? "24h");
      if (lifetime === null) {
        push({
          cls: "t-r",
          text: "invalid --for " + flags.get("for") + " (try 24h or 7d)",
        });
        return;
      }
      const grant = JSON.parse(
        wallet.grant(
          name,
          budget,
          flags.get("max-tx"),
          flags.get("max-fee"),
          lifetime,
          now()
        )
      );
      const rows: [string, string][] = [
        ["Grant", grant.agent],
        ["Budget", fmt(grant.budget_sat) + " sat"],
      ];
      if (grant.max_tx_sat != null) {
        rows.push(["Max tx", fmt(grant.max_tx_sat) + " sat"]);
      }
      if (grant.max_fee_sat != null) {
        rows.push(["Max fee", fmt(grant.max_fee_sat) + " sat"]);
      }
      rows.push(["For", humanDuration(lifetime)]);
      push(...kv(rows));
      push({
        cls: "t-g",
        text:
          "granted " +
          grant.agent +
          " · mode ask — every request waits for your approval" +
          (grant.replaced ? " · previous grant replaced" : ""),
      });
      push(
        {
          cls: "t-dim",
          text:
            "the agent files:  sats agent request " +
            grant.agent +
            " <address> <amount>",
        },
        {
          cls: "t-dim",
          text: "you decide:       sats agent requests · sats agent approve <id>",
        }
      );
      return;
    }
    if (sub === "list") {
      const rows = JSON.parse(wallet.grants(now())) as {
        agent: string;
        mode: string;
        remaining_sat: number;
        budget_sat: number;
        tx_count: number;
        expired: boolean;
      }[];
      if (rows.length === 0) {
        push({ cls: "t-dim", text: "no grants" });
        return;
      }
      for (const grant of rows) {
        push({
          cls: grant.expired ? "t-dim" : "t-o",
          text:
            grant.agent.padEnd(12) +
            " " +
            grant.mode.padEnd(8) +
            fmt(grant.remaining_sat) +
            "/" +
            fmt(grant.budget_sat) +
            " sat remaining · " +
            grant.tx_count +
            " tx" +
            (grant.expired ? " · expired" : ""),
        });
      }
      return;
    }
    if (sub === "requests") {
      const rows = JSON.parse(wallet.requests()) as {
        id: string;
        agent: string;
        recipient: string;
        amount_sat: number;
        status: string;
      }[];
      if (rows.length === 0) {
        push({ cls: "t-dim", text: "no pending requests" });
        return;
      }
      for (const row of rows) {
        push({
          cls: "t-o",
          text:
            row.id.padEnd(12) +
            row.agent.padEnd(10) +
            fmt(row.amount_sat).padStart(10) +
            " sat → " +
            row.recipient.slice(0, 16) +
            "… · " +
            row.status,
        });
      }
      push({
        cls: "t-dim",
        text: "approve one: sats agent approve <id> · decline: sats agent dismiss <id>",
      });
      return;
    }
    if (sub === "approve") {
      if (!args[1]) {
        push({ cls: "t-r", text: "usage: sats agent approve <id>" });
        return;
      }
      const feeRate = parseInt(flags.get("fee-rate") ?? "2", 10);
      const sent = JSON.parse(wallet.approve(args[1], feeRate, now()));
      push(
        {
          cls: "t-g",
          text:
            "approved " +
            sent.approved +
            " · sent " +
            fmt(sent.amount_sat) +
            " sat · fee " +
            fmt(sent.fee_sat) +
            " sat · authorized by you, signed by the wallet",
        },
        {
          cls: "t-dim",
          text:
            "budget remaining " +
            fmt(sent.grant_remaining_sat) +
            " sat · txid " +
            String(sent.txid).slice(0, 16) +
            "…",
        }
      );
      return;
    }
    if (sub === "dismiss") {
      if (!args[1]) {
        push({ cls: "t-r", text: "usage: sats agent dismiss <id>" });
        return;
      }
      const dismissed = JSON.parse(wallet.dismiss(args[1], now()));
      push({ cls: "t-g", text: "dismissed " + dismissed.dismissed });
      return;
    }
    if (sub === "revoke") {
      if (!args[1]) {
        push({ cls: "t-r", text: "usage: sats agent revoke <name>" });
        return;
      }
      wallet.revoke(args[1]);
      push({
        cls: "t-g",
        text:
          "revoked " +
          args[1] +
          " · effective on the agent's next send",
      });
      return;
    }
    if (sub === "request") {
      if (args.length < 4) {
        push({
          cls: "t-r",
          text: "usage: sats agent request <name> <address> <amount>",
        });
        return;
      }
      const result = JSON.parse(
        wallet.request_send(args[1], args[2], args[3], now())
      );
      if (result.status === "pending_approval") {
        push(
          {
            cls: "t-o",
            text:
              "pending_approval · request " +
              result.request_id +
              " — no signature was produced",
          },
          {
            cls: "t-dim",
            text:
              "the agent is blocked here; as the human: sats agent approve " +
              result.request_id +
              " · sats agent dismiss " +
              result.request_id,
          }
        );
      } else {
        push(
          {
            cls: "t-r",
            text:
              "denied · " +
              result.reason +
              " — outside the grant; only changing the grant lifts it",
          },
          { cls: "t-dim", text: "  " + result.message }
        );
      }
      return;
    }
    push({
      cls: "t-r",
      text: "usage: sats agent grant|list|revoke|request|requests|approve|dismiss",
    });
  }

  const writePrompt = useCallback((after?: () => void) => {
    const terminal = terminalRef.current;
    if (!terminal) return;

    terminal.write("", () => {
      if (terminalRef.current !== terminal) return;
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = terminal.registerMarker(0) ?? null;
      terminal.write(
        styledPrompt(promptRef.current, inputBufferRef.current),
        after
      );
    });
  }, []);

  const redrawInput = useCallback((after?: () => void) => {
    const terminal = terminalRef.current;
    const marker = inputMarkerRef.current;
    if (!terminal || !marker || marker.line < 0) return;

    const buffer = inputBufferRef.current;
    const cursor = inputCursorRef.current;
    const promptText = promptRef.current;
    const currentLine = terminal.buffer.active.baseY + terminal.buffer.active.cursorY;
    const lineDelta = currentLine - marker.line;
    let redraw = "\x1b[?25l\r";
    if (lineDelta > 0) redraw += `\x1b[${lineDelta}A`;
    else if (lineDelta < 0) redraw += `\x1b[${-lineDelta}B`;
    redraw += `\x1b[0J${styledPrompt(promptText, buffer)}`;

    terminal.write(redraw, () => {
      if (terminalRef.current !== terminal || marker.line < 0) return;
      if (cursor === buffer.length) {
        terminal.write("\x1b[?25h", after);
        return;
      }

      const columns = Math.max(1, terminal.cols);
      const targetOffset = promptText.length + cursor;
      const targetLine = marker.line + Math.floor(targetOffset / columns);
      const targetColumn = targetOffset % columns;
      const renderedLine =
        terminal.buffer.active.baseY + terminal.buffer.active.cursorY;
      const rowDelta = renderedLine - targetLine;
      let movement = "";
      if (rowDelta > 0) movement += `\x1b[${rowDelta}A`;
      else if (rowDelta < 0) movement += `\x1b[${-rowDelta}B`;
      movement += `\x1b[${targetColumn + 1}G\x1b[?25h`;
      terminal.write(movement, after);
    });
  }, []);

  const submitCurrentLine = useCallback(() => {
    if (runningRef.current) return;
    const terminal = terminalRef.current;
    if (!terminal) return;

    const raw = inputBufferRef.current;
    inputBufferRef.current = "";
    inputCursorRef.current = 0;
    inputMarkerRef.current?.dispose();
    inputMarkerRef.current = null;
    terminal.write("\r\n");

    if (raw.trim() === "") {
      writePrompt(() => terminal.focus());
      return;
    }
    void executeRef.current(raw);
  }, [writePrompt]);

  function replaceInput(value: string, cursor = value.length): void {
    inputBufferRef.current = value;
    inputCursorRef.current = Math.max(0, Math.min(cursor, value.length));
    redrawInput();
  }

  function moveHistory(direction: -1 | 1): void {
    const history = histRef.current;
    if (history.length === 0) {
      terminalRef.current?.write("\x07");
      return;
    }

    if (direction === -1) {
      if (histPosRef.current === history.length) {
        histDraftRef.current = inputBufferRef.current;
      }
      if (histPosRef.current > 0) histPosRef.current--;
    } else if (histPosRef.current < history.length) {
      histPosRef.current++;
    }

    replaceInput(
      histPosRef.current === history.length
        ? histDraftRef.current
        : history[histPosRef.current] ?? ""
    );
  }

  function insertInput(value: string): void {
    const safe = value
      .replace(/\r?\n/g, " ")
      .replace(/[\x00-\x1f\x7f]/g, "");
    if (!safe) return;
    const buffer = inputBufferRef.current;
    const cursor = inputCursorRef.current;
    histPosRef.current = histRef.current.length;
    histDraftRef.current = "";
    replaceInput(
      buffer.slice(0, cursor) + safe + buffer.slice(cursor),
      cursor + safe.length
    );
  }

  function completeInput(): void {
    const buffer = inputBufferRef.current;
    if (inputCursorRef.current !== buffer.length) {
      terminalRef.current?.write("\x07");
      return;
    }
    const matches = COMPLETIONS.filter((command) => command.startsWith(buffer));
    if (matches.length === 0) {
      terminalRef.current?.write("\x07");
      return;
    }
    let completion = matches[0];
    for (const match of matches.slice(1)) {
      let index = 0;
      while (index < completion.length && completion[index] === match[index]) {
        index++;
      }
      completion = completion.slice(0, index);
    }
    if (completion === buffer) terminalRef.current?.write("\x07");
    else replaceInput(completion);
  }

  function handleTerminalData(data: string): void {
    if (runningRef.current) return;

    if (data === "\r" || data === "\n") {
      submitCurrentLine();
      return;
    }
    if (data === "\x7f" || data === "\b") {
      const buffer = inputBufferRef.current;
      const cursor = inputCursorRef.current;
      if (cursor === 0) {
        terminalRef.current?.write("\x07");
        return;
      }
      replaceInput(
        buffer.slice(0, cursor - 1) + buffer.slice(cursor),
        cursor - 1
      );
      return;
    }
    if (data === "\x1b[3~") {
      const buffer = inputBufferRef.current;
      const cursor = inputCursorRef.current;
      if (cursor < buffer.length) {
        replaceInput(buffer.slice(0, cursor) + buffer.slice(cursor + 1), cursor);
      }
      return;
    }
    if (data === "\x1b[A") {
      moveHistory(-1);
      return;
    }
    if (data === "\x1b[B") {
      moveHistory(1);
      return;
    }
    if (data === "\x1b[D") {
      if (inputCursorRef.current > 0) {
        inputCursorRef.current--;
        redrawInput();
      }
      return;
    }
    if (data === "\x1b[C") {
      if (inputCursorRef.current < inputBufferRef.current.length) {
        inputCursorRef.current++;
        redrawInput();
      }
      return;
    }
    if (data === "\x01" || data === "\x1b[H" || data === "\x1b[1~") {
      inputCursorRef.current = 0;
      redrawInput();
      return;
    }
    if (data === "\x05" || data === "\x1b[F" || data === "\x1b[4~") {
      inputCursorRef.current = inputBufferRef.current.length;
      redrawInput();
      return;
    }
    if (data === "\x15") {
      replaceInput(inputBufferRef.current.slice(inputCursorRef.current), 0);
      return;
    }
    if (data === "\x0b") {
      replaceInput(
        inputBufferRef.current.slice(0, inputCursorRef.current),
        inputCursorRef.current
      );
      return;
    }
    if (data === "\x17") {
      const buffer = inputBufferRef.current;
      const cursor = inputCursorRef.current;
      let start = cursor;
      while (start > 0 && /\s/.test(buffer[start - 1])) start--;
      while (start > 0 && !/\s/.test(buffer[start - 1])) start--;
      replaceInput(buffer.slice(0, start) + buffer.slice(cursor), start);
      return;
    }
    if (data === "\x03") {
      terminalRef.current?.write("^C\r\n");
      inputBufferRef.current = "";
      inputCursorRef.current = 0;
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = null;
      writePrompt(() => terminalRef.current?.focus());
      return;
    }
    if (data === "\x0c") {
      const terminal = terminalRef.current;
      if (!terminal) return;
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = null;
      terminal.clear();
      terminal.write("\x1b[2J\x1b[H", () => writePrompt());
      return;
    }
    if (data === "\t") {
      completeInput();
      return;
    }
    if (data.startsWith("\x1b")) return;
    insertInput(data);
  }

  async function execute(raw: string): Promise<void> {
    if (runningRef.current || raw.trim() === "") return;
    runningRef.current = true;
    setIsRunning(true);
    try {
      await run(raw);
    } catch (error) {
      pendingSendRef.current = null;
      setPrompt("$ ");
      push({
        cls: "t-r",
        text: error instanceof Error ? error.message : String(error),
      });
    } finally {
      writePrompt(() => {
        runningRef.current = false;
        setIsRunning(false);
        terminalRef.current?.focus();
      });
    }
  }

  executeRef.current = execute;
  inputHandlerRef.current = handleTerminalData;

  useEffect(() => {
    const host = terminalHostRef.current;
    if (!host) return;

    let cancelled = false;
    let terminal: XTerm | null = null;
    let resizeObserver: ResizeObserver | null = null;
    let fitFrame: number | null = null;
    let media: MediaQueryList | null = null;
    const disposables: Array<{ dispose(): void }> = [];

    void Promise.all([import("@xterm/xterm"), import("@xterm/addon-fit")])
      .then(([{ Terminal }, { FitAddon }]) => {
        if (cancelled) return;

        terminal = new Terminal({
          cursorBlink: true,
          cursorStyle: "block",
          fontFamily: cssValue(
            "--mono",
            '"SFMono-Regular", "Cascadia Code", Consolas, monospace'
          ),
          fontSize: host.clientWidth < 520 ? 11 : 13,
          fontWeight: "400",
          fontWeightBold: "600",
          letterSpacing: 0,
          lineHeight: 1.35,
          rightClickSelectsWord: true,
          screenReaderMode: true,
          scrollback: 5000,
          scrollOnUserInput: true,
          theme: terminalTheme(),
        });
        const fitAddon = new FitAddon();
        terminal.loadAddon(fitAddon);
        terminal.open(host);
        terminalRef.current = terminal;

        const fit = () => {
          if (fitFrame !== null) return;
          fitFrame = window.requestAnimationFrame(() => {
            fitFrame = null;
            if (!terminal || cancelled || !host.isConnected) return;
            const fontSize = host.clientWidth < 520 ? 11 : 13;
            if (terminal.options.fontSize !== fontSize) {
              terminal.options.fontSize = fontSize;
            }
            fitAddon.fit();
            if (inputMarkerRef.current) redrawInput();
          });
        };

        fitAddon.fit();
        resizeObserver = new ResizeObserver(fit);
        resizeObserver.observe(host);
        void document.fonts.ready.then(fit);

        media = window.matchMedia("(prefers-color-scheme: light)");
        const applyTheme = () => {
          if (!terminal) return;
          terminal.options.theme = terminalTheme();
          fit();
        };
        media.addEventListener("change", applyTheme);
        disposables.push({
          dispose: () => media?.removeEventListener("change", applyTheme),
        });

        disposables.push(
          terminal.onData((data) => inputHandlerRef.current(data))
        );
        terminal.attachCustomKeyEventHandler((event) => {
          if (
            event.type === "keydown" &&
            (event.ctrlKey || event.metaKey) &&
            event.key.toLowerCase() === "c" &&
            terminal?.hasSelection() &&
            navigator.clipboard?.writeText
          ) {
            void navigator.clipboard
              .writeText(terminal.getSelection())
              .catch(() => {});
            return false;
          }
          return true;
        });

        for (const line of [...WELCOME, ...outputQueueRef.current.splice(0)]) {
          terminal.writeln(styledLine(line));
        }
        writePrompt(() => {
          if (cancelled || !terminal) return;
          setTerminalReady(true);
          terminal.focus();
          fit();
        });
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setTerminalError(
            error instanceof Error ? error.message : String(error)
          );
        }
      });

    return () => {
      cancelled = true;
      resizeObserver?.disconnect();
      if (fitFrame !== null) window.cancelAnimationFrame(fitFrame);
      for (const disposable of disposables) disposable.dispose();
      inputMarkerRef.current?.dispose();
      inputMarkerRef.current = null;
      terminal?.dispose();
      if (terminalRef.current === terminal) terminalRef.current = null;
    };
  }, [redrawInput, writePrompt]);

  function executeAction(command: string): void {
    if (!terminalReady || runningRef.current) return;
    inputBufferRef.current = command;
    inputCursorRef.current = command.length;
    redrawInput(submitCurrentLine);
  }

  const actions = prompt === "$ " ? DEFAULT_ACTIONS : CONFIRM_ACTIONS;

  return (
    <div>
      <div className="term">
        <div className="term-bar">
          <div className="term-title">
            <span className="term-title-mark" aria-hidden="true">$</span>
            <span>sats</span>
          </div>
          <span className="term-status">signet · live wasm</span>
        </div>

        <div className="term-screen">
          <div
            className="term-host"
            ref={terminalHostRef}
            role="application"
            aria-label="Interactive sats terminal playground"
          />
          {!terminalReady && (
            <div className="term-loading" role="status">
              {terminalError ?? "starting terminal…"}
            </div>
          )}
        </div>

        <div className="term-actions" aria-label="Suggested commands">
          {actions.map((action) => (
            <button
              key={action.command}
              type="button"
              className={
                "term-action" +
                (action.primary ? " term-action-primary" : "")
              }
              disabled={isRunning || !terminalReady}
              onClick={() => executeAction(action.command)}
            >
              {action.label}
            </button>
          ))}
        </div>
      </div>

      <p className="term-caption">
        <span>
          Keys and state stay in this tab and disappear when you close it.
        </span>
        <span>tab complete · ↑/↓ history · ctrl+l clear</span>
      </p>
    </div>
  );
}
