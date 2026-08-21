"use client";

import { useEffect, useRef } from "react";

type Step = { cmd: string; out: [string, string][] };

const SCRIPT: Step[] = [
  { cmd: "sats init", out: [["t-g", "wallet created (signet) · seed sealed with passphrase"]] },
  { cmd: "sats receive", out: [["t-o", "tb1p6h5fuzmnvpdthf5shf0qqjzwy7wsqc5rhmgq2ks9xrak4ry6mtrscsqvzp"]] },
  { cmd: "sats balance", out: [["t-o", "100,000 sat"]] },
  {
    cmd: "sats send tb1p... 25k",
    out: [
      ["t-o", "amount 25,000 sat · fee 302 sat · confirm? y"],
      ["t-g", "signed · saved · broadcast ✓"],
    ],
  },
  {
    cmd: "sats agent grant claude --budget 50k --for 24h",
    out: [["t-o", "grant “claude” · budget 50,000 sat · expires in 24h"]],
  },
  {
    cmd: "sats agent serve claude",
    out: [
      ["t-dim", "mcp server listening on stdio — tools: get_balance,"],
      ["t-dim", "get_receive_address, get_grant, send"],
    ],
  },
];

export default function Terminal() {
  const termRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.textContent = "";
    const timers: ReturnType<typeof setTimeout>[] = [];

    function addLine(cls: string, text: string) {
      const d = document.createElement("div");
      d.className = "line" + (cls ? " " + cls : "");
      d.textContent = text;
      term!.appendChild(d);
      return d;
    }
    function promptLine() {
      const d = document.createElement("div");
      d.className = "line";
      const p = document.createElement("span");
      p.className = "t-p";
      p.textContent = "$ ";
      const c = document.createElement("span");
      c.className = "t-c";
      d.appendChild(p);
      d.appendChild(c);
      term!.appendChild(d);
      return c;
    }

    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduced) {
      for (const step of SCRIPT) {
        promptLine().textContent = step.cmd;
        for (const [cls, text] of step.out) addLine(cls, text);
      }
      return;
    }

    const cursor = document.createElement("span");
    cursor.className = "cursor";

    function typeStep(i: number) {
      if (i >= SCRIPT.length) {
        promptLine().parentNode!.appendChild(cursor);
        return;
      }
      const step = SCRIPT[i];
      const target = promptLine();
      target.parentNode!.appendChild(cursor);
      let j = 0;
      const typeChar = () => {
        if (j < step.cmd.length) {
          target.textContent += step.cmd.charAt(j++);
          timers.push(setTimeout(typeChar, 26 + Math.min(j % 7, 3) * 9));
        } else {
          timers.push(
            setTimeout(() => {
              cursor.remove();
              for (const [cls, text] of step.out) addLine(cls, text);
              timers.push(setTimeout(() => typeStep(i + 1), 520));
            }, 260)
          );
        }
      };
      typeChar();
    }
    timers.push(setTimeout(() => typeStep(0), 400));

    return () => timers.forEach(clearTimeout);
  }, []);

  return (
    <div className="term" aria-label="Terminal demo of the sats quickstart">
      <div className="term-bar">
        <i></i>
        <i></i>
        <i></i>
        <span>sats — signet</span>
      </div>
      <div className="term-body" ref={termRef}>
        {SCRIPT.map((step) => (
          <div key={step.cmd}>
            <div className="line">
              <span className="t-p">$ </span>
              <span className="t-c">{step.cmd}</span>
            </div>
            {step.out.map(([cls, text]) => (
              <div key={text} className={`line ${cls}`}>
                {text}
              </div>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
