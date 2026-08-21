"use client";

import { useCallback, useEffect, useRef, useState } from "react";

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
  agent_send(
    agent: string,
    recipient: string,
    amount: string,
    feeRate: number,
    now: number
  ): string;
  history(): string;
};

type Line = { cls: string; text: string };
type QuickAction = { label: string; command: string; primary?: boolean };

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
  ["t-o", "  sats agent grant <name> --budget <sats> [--for 24h]"],
  ["t-o", "  sats agent list              current authority"],
  ["t-o", "  sats agent revoke <name>     delete a grant"],
  ["t-o", "  sats agent send <name> <addr> <amt>    run a granted send"],
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
  const [lines, setLines] = useState<Line[]>(WELCOME);
  const [input, setInput] = useState("");
  const [prompt, setPrompt] = useState("$ ");
  const [isRunning, setIsRunning] = useState(false);
  const walletRef = useRef<WasmWallet | null>(null);
  const loadingRef = useRef<Promise<void> | null>(null);
  const pendingSendRef = useRef<string | null>(null);
  const histRef = useRef<string[]>([]);
  const histPosRef = useRef(0);
  const bodyRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const push = useCallback((...added: Line[]) => {
    setLines((previous) => [...previous, ...added]);
  }, []);

  useEffect(() => {
    bodyRef.current?.scrollTo({ top: bodyRef.current.scrollHeight });
  }, [lines]);

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
    push({ cls: "", text: "" });

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
    histRef.current.push(cmd);
    histPosRef.current = histRef.current.length;

    if (cmd === "clear") {
      setLines([]);
      return;
    }
    if (cmd === "help" || cmd === "sats help" || cmd === "sats") {
      push(...HELP);
      return;
    }
    if (cmd === "reset") {
      walletRef.current?.reset();
      setLines(WELCOME);
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
          (grant.replaced ? " · previous grant replaced" : ""),
      });
      push({
        cls: "t-dim",
        text:
          "Try it: sats agent send " +
          grant.agent +
          " <address> <amount>",
      });
      return;
    }
    if (sub === "list") {
      const rows = JSON.parse(wallet.grants(now())) as {
        agent: string;
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
    if (sub === "send") {
      if (args.length < 4) {
        push({
          cls: "t-r",
          text: "usage: sats agent send <name> <address> <amount>",
        });
        return;
      }
      const feeRate = parseInt(flags.get("fee-rate") ?? "2", 10);
      const result = JSON.parse(
        wallet.agent_send(args[1], args[2], args[3], feeRate, now())
      );
      if (result.status === "sent") {
        push(
          {
            cls: "t-g",
            text:
              "sent " +
              fmt(result.amount_sat) +
              " sat · fee " +
              fmt(result.fee_sat) +
              " sat · grant '" +
              args[1] +
              "'",
          },
          {
            cls: "t-dim",
            text:
              "budget remaining " +
              fmt(result.grant_remaining_sat) +
              " sat · txid " +
              result.txid.slice(0, 16) +
              "…",
          }
        );
      } else {
        push(
          { cls: "t-r", text: "denied · no signature was produced" },
          ...JSON.stringify(result, null, 2)
            .split("\n")
            .map((text) => ({ cls: "t-o", text: "  " + text }))
        );
      }
      return;
    }
    push({
      cls: "t-r",
      text: "usage: sats agent grant|list|revoke|send",
    });
  }

  async function execute(raw: string): Promise<void> {
    if (isRunning || raw.trim() === "") return;
    setInput("");
    push({ cls: "", text: (prompt + raw).trimEnd() });
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
      setIsRunning(false);
      window.setTimeout(() => inputRef.current?.focus(), 0);
    }
  }

  function onSubmit(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void execute(input);
  }

  function onKeyDown(event: React.KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowUp") {
      event.preventDefault();
      if (histPosRef.current > 0) {
        setInput(histRef.current[--histPosRef.current] ?? "");
      }
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      if (histPosRef.current < histRef.current.length) {
        setInput(histRef.current[++histPosRef.current] ?? "");
      } else {
        setInput("");
      }
    }
  }

  const actions =
    pendingSendRef.current !== null ? CONFIRM_ACTIONS : DEFAULT_ACTIONS;

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

        <div
          className="term-body"
          ref={bodyRef}
          onClick={() => inputRef.current?.focus()}
          aria-live="polite"
        >
          {lines.map((line, index) => (
            <div key={index} className={"line " + line.cls}>
              {line.text || " "}
            </div>
          ))}
          <form className="line term-input-line" onSubmit={onSubmit}>
            <span className="t-p">{prompt}</span>
            <input
              ref={inputRef}
              className="term-input"
              value={input}
              onChange={(event) => setInput(event.target.value)}
              onKeyDown={onKeyDown}
              disabled={isRunning}
              spellCheck={false}
              autoComplete="off"
              autoCapitalize="off"
              aria-label="sats playground command input"
              placeholder={isRunning ? "running…" : "type a command"}
            />
          </form>
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
              disabled={isRunning}
              onClick={() => void execute(action.command)}
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
        <span>↑/↓ history · enter to run</span>
      </p>
    </div>
  );
}
