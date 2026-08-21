"use client";

import { useCallback, useEffect, useRef, useState } from "react";

// The wasm-bindgen module is loaded at runtime from /playground/ (built by
// scripts/build-playground.sh), so the bundler never sees it.
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
  agent_send(agent: string, recipient: string, amount: string, feeRate: number, now: number): string;
  history(): string;
};

type Line = { cls: string; text: string };

const WELCOME: Line[] = [
  { cls: "t-c", text: "sats playground — a real wallet in your browser" },
  { cls: "t-dim", text: "sats-core compiled to WebAssembly · simulated signet chain · nothing leaves this page" },
  { cls: "t-dim", text: "type `help`, or start with `sats init`" },
  { cls: "", text: "" },
];

const HELP: Line[] = [
  ["t-o", "  sats init                    create a wallet (12-word seed)"],
  ["t-o", "  sats receive                 show a receive address"],
  ["t-o", "  sats balance                 balance and simulated chain height"],
  ["t-o", "  sats faucet [amount]         playground faucet (default 100k)"],
  ["t-o", "  sats send <addr> <amt>       prepare · confirm · sign · broadcast"],
  ["t-o", "  sats history                 finalized transactions"],
  ["t-o", "  sats agent grant <name> --budget <sats> [--for 24h] [--max-tx n] [--max-fee n]"],
  ["t-o", "  sats agent list              current authority"],
  ["t-o", "  sats agent revoke <name>     delete a grant"],
  ["t-o", "  sats agent send <name> <addr> <amt>    what the MCP `send` tool runs"],
  ["t-o", "  clear · reset"],
  ["t-dim", "amounts are integer sats, with shorthand: 25k = 25,000 · 1.5m = 1,500,000"],
].map(([cls, text]) => ({ cls, text }));

function fmt(n: number): string {
  return Math.trunc(n).toLocaleString("en-US");
}

function parseDuration(s: string): number | null {
  const m = /^(\d+(?:\.\d+)?)(s|m|h|d)$/.exec(s.trim());
  if (!m) return null;
  const mult = { s: 1, m: 60, h: 3600, d: 86400 }[m[2] as "s" | "m" | "h" | "d"];
  const secs = Math.round(parseFloat(m[1]) * mult);
  return secs > 0 ? secs : null;
}

function humanDuration(secs: number): string {
  if (secs % 86400 === 0) return `${secs / 86400}d`;
  if (secs % 3600 === 0) return `${secs / 3600}h`;
  if (secs % 60 === 0) return `${secs / 60}m`;
  return `${secs}s`;
}

function splitFlags(tokens: string[]): { args: string[]; flags: Map<string, string> } {
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
  const walletRef = useRef<WasmWallet | null>(null);
  const loadingRef = useRef<Promise<void> | null>(null);
  const pendingSendRef = useRef<string | null>(null);
  const histRef = useRef<string[]>([]);
  const histPosRef = useRef(0);
  const bodyRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const push = useCallback((...added: Line[]) => {
    setLines((prev) => [...prev, ...added]);
  }, []);

  useEffect(() => {
    bodyRef.current?.scrollTo({ top: bodyRef.current.scrollHeight });
  }, [lines]);

  const ensureWallet = useCallback(async (): Promise<WasmWallet> => {
    if (walletRef.current) return walletRef.current;
    if (!loadingRef.current) {
      // Runtime import keeps webpack/turbopack from trying to bundle the
      // wasm-bindgen glue; the /playground files are plain static assets.
      const importUrl = new Function("u", "return import(u)") as (u: string) => Promise<{
        default: (m?: string) => Promise<unknown>;
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

  // Warm the engine while the visitor is still reading the page.
  useEffect(() => {
    const idle = (window as { requestIdleCallback?: (cb: () => void) => number })
      .requestIdleCallback ?? ((cb: () => void) => window.setTimeout(cb, 1500));
    idle(() => {
      ensureWallet().catch(() => {
        // Surfaced on first command instead.
        loadingRef.current = null;
      });
    });
  }, [ensureWallet]);

  const now = () => Date.now() / 1000;

  function kv(rows: [string, string][], cls = "t-o"): Line[] {
    const width = Math.max(...rows.map(([k]) => k.length));
    return rows.map(([k, v]) => ({ cls, text: `${k.padEnd(width + 2)}${v}` }));
  }

  async function run(raw: string): Promise<void> {
    const cmd = raw.trim();
    push({ cls: "", text: "" });

    // A pending `sats send` confirmation intercepts the next entry.
    if (pendingSendRef.current !== null) {
      const id = pendingSendRef.current;
      pendingSendRef.current = null;
      setPrompt("$ ");
      const wallet = await ensureWallet();
      if (/^y(es)?$/i.test(cmd)) {
        const sent = JSON.parse(wallet.confirm(id, now()));
        push(
          { cls: "t-g", text: "signed · saved · broadcast ✓" },
          { cls: "t-dim", text: `txid ${sent.txid} · confirmed in simulated block ${fmt(sent.height)}` }
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
      push({ cls: "t-r", text: `unknown command ${tokens[0]!} — try \`help\`` });
      return;
    }

    const wallet = await ensureWallet();
    const { args, flags } = splitFlags(tokens.slice(1));
    const sub = args[0];

    if (sub === "init") {
      if (wallet.has_wallet()) {
        push({ cls: "t-r", text: "a wallet already exists — `reset` to start over" });
        return;
      }
      const info = JSON.parse(wallet.init(now()));
      push(
        { cls: "t-g", text: "wallet created (signet) · BIP-86 taproot · watch-only + local signer" },
        { cls: "t-dim", text: "seed (playground only — never shown like this in the CLI):" },
        { cls: "t-o", text: `  ${info.mnemonic}` },
        { cls: "t-dim", text: "get simulated coins with `sats faucet`" }
      );
      return;
    }

    if (!wallet.has_wallet()) {
      push({ cls: "t-r", text: "no wallet — run `sats init` first" });
      return;
    }

    switch (sub) {
      case "receive": {
        const r = JSON.parse(wallet.receive());
        push({ cls: "t-o", text: r.address });
        return;
      }
      case "balance": {
        const b = JSON.parse(wallet.balance());
        push({ cls: "t-o", text: `${fmt(b.total_sat)} sat` });
        if (b.pending_sat > 0)
          push({ cls: "t-dim", text: `${fmt(b.pending_sat)} sat pending` });
        push({ cls: "t-dim", text: `simulated signet · block ${fmt(b.height)}` });
        return;
      }
      case "faucet": {
        const f = JSON.parse(wallet.faucet(args[1] ?? "100k", now()));
        push(
          { cls: "t-g", text: `+${fmt(f.amount_sat)} sat confirmed in simulated block ${fmt(f.height)}` },
          { cls: "t-dim", text: `txid ${f.txid.slice(0, 16)}… (faucet is playground-only; on real signet use a public faucet)` }
        );
        return;
      }
      case "send": {
        if (args.length < 3) {
          push({ cls: "t-r", text: "usage: sats send <address> <amount> [--fee-rate n]" });
          return;
        }
        const feeRate = parseInt(flags.get("fee-rate") ?? "2", 10);
        const plan = JSON.parse(wallet.prepare(args[1], args[2], feeRate, now()));
        push(
          ...kv([
            ["Amount", `${fmt(plan.amount_sat)} sat`],
            ["Fee", `${fmt(plan.fee_sat)} sat (${feeRate} sat/vB)`],
            ["Total", `${fmt(plan.total_sat)} sat`],
          ])
        );
        if (plan.excluded_utxos > 0)
          push({ cls: "t-dim", text: `${plan.excluded_utxos} inscription-suspect utxo(s) excluded from selection` });
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
        for (const r of rows)
          push({
            cls: "t-o",
            text: `${r.txid.slice(0, 12)}…  ${fmt(r.amount_sat).padStart(10)} sat  fee ${fmt(r.fee_sat)}  ${r.status}`,
          });
        return;
      }
      case "agent": {
        await runAgent(wallet, args.slice(1), flags);
        return;
      }
      default:
        push({ cls: "t-r", text: `unknown command \`sats ${sub ?? ""}\` — try \`help\`` });
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
        push({ cls: "t-r", text: "usage: sats agent grant <name> --budget <sats> [--for 24h] [--max-tx n] [--max-fee n]" });
        return;
      }
      const lifetime = parseDuration(flags.get("for") ?? "24h");
      if (lifetime === null) {
        push({ cls: "t-r", text: `invalid --for ${flags.get("for")} (try 24h, 7d)` });
        return;
      }
      const g = JSON.parse(
        wallet.grant(name, budget, flags.get("max-tx"), flags.get("max-fee"), lifetime, now())
      );
      const rows: [string, string][] = [
        ["Grant", g.agent],
        ["Budget", `${fmt(g.budget_sat)} sat`],
      ];
      if (g.max_tx_sat != null) rows.push(["Max tx", `${fmt(g.max_tx_sat)} sat`]);
      if (g.max_fee_sat != null) rows.push(["Max fee", `${fmt(g.max_fee_sat)} sat`]);
      rows.push(["For", humanDuration(lifetime)]);
      push(...kv(rows));
      push({ cls: "t-g", text: `granted  ${g.agent}${g.replaced ? " (previous grant replaced)" : ""}` });
      push({ cls: "t-dim", text: `try it:  sats agent send ${g.agent} <address> <amount>` });
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
      for (const g of rows)
        push({
          cls: g.expired ? "t-dim" : "t-o",
          text: `${g.agent.padEnd(12)} ${fmt(g.remaining_sat)}/${fmt(g.budget_sat)} sat remaining · ${g.tx_count} tx${g.expired ? " · expired" : ""}`,
        });
      return;
    }
    if (sub === "revoke") {
      if (!args[1]) {
        push({ cls: "t-r", text: "usage: sats agent revoke <name>" });
        return;
      }
      wallet.revoke(args[1]);
      push({ cls: "t-g", text: `revoked  ${args[1]} — takes effect on the agent's next send` });
      return;
    }
    if (sub === "send") {
      if (args.length < 4) {
        push({ cls: "t-r", text: "usage: sats agent send <name> <address> <amount>" });
        return;
      }
      const feeRate = parseInt(flags.get("fee-rate") ?? "2", 10);
      const result = JSON.parse(wallet.agent_send(args[1], args[2], args[3], feeRate, now()));
      if (result.status === "sent") {
        push(
          { cls: "t-g", text: `sent ${fmt(result.amount_sat)} sat · fee ${fmt(result.fee_sat)} sat · authorized by grant "${args[1]}"` },
          { cls: "t-dim", text: `budget remaining ${fmt(result.grant_remaining_sat)} sat · txid ${result.txid.slice(0, 16)}…` }
        );
      } else {
        push(
          { cls: "t-r", text: "denied — no signature was produced" },
          ...JSON.stringify(result, null, 2)
            .split("\n")
            .map((text) => ({ cls: "t-o", text: `  ${text}` }))
        );
      }
      return;
    }
    push({ cls: "t-r", text: "usage: sats agent grant|list|revoke|send" });
  }

  async function onSubmit() {
    const raw = input;
    setInput("");
    push({ cls: "", text: `${prompt}${raw}`.trimEnd() });
    try {
      await run(raw);
    } catch (e) {
      pendingSendRef.current = null;
      setPrompt("$ ");
      push({ cls: "t-r", text: e instanceof Error ? e.message : String(e) });
    }
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter") {
      e.preventDefault();
      void onSubmit();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (histPosRef.current > 0) setInput(histRef.current[--histPosRef.current] ?? "");
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (histPosRef.current < histRef.current.length)
        setInput(histRef.current[++histPosRef.current] ?? "");
      else setInput("");
    }
  }

  return (
    <div>
      <div className="term term-live" onClick={() => inputRef.current?.focus()}>
        <div className="term-bar">
          <i></i>
          <i></i>
          <i></i>
          <span>sats — signet (simulated) · live wasm</span>
        </div>
        <div className="term-body term-scroll" ref={bodyRef}>
          {lines.map((line, i) => (
            <div key={i} className={`line ${line.cls}`}>
              {line.text || " "}
            </div>
          ))}
          <div className="line term-input-line">
            <span className="t-p">{prompt}</span>
            <input
              ref={inputRef}
              className="term-input"
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={onKeyDown}
              spellCheck={false}
              autoComplete="off"
              autoCapitalize="off"
              aria-label="sats playground command input"
              placeholder={lines === WELCOME ? "sats init" : ""}
            />
          </div>
        </div>
      </div>
      <p className="term-caption">
        This terminal runs the actual <code>sats-core</code> engine — planning,
        UTXO protection, signing, and grant authorization — compiled to
        WebAssembly. Only the chain is simulated; keys and state live in this
        page and are gone when you close it.
      </p>
    </div>
  );
}
