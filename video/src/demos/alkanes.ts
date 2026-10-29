import type { Launch } from "../lib/launch";

// sats 0.0.4: Alkanes. Every terminal line is from captures/, recorded
// from the real binary by scripts/capture.py.
export const alkanes: Launch = {
  id: "alkanes-0.0.4",
  fps: 60,
  title: { version: "0.0.4", headline: "Now with Alkanes.", hold: 2.4 },
  terminal: {
    columns: 110,
    rows: 20,
    title: "sats",
    status: "signet",
    steps: [
      { id: "balance", capture: "alkanes-balance", type: ["sats alkanes balance"], hold: 1.6 },
      {
        id: "send",
        capture: "alkanes-send",
        type: ["sats alkanes send 32:0 40000 ", { paste: true }],
        think: [2.25, 0.35],
        wait: 0.5,
        hold: 0.85,
      },
      {
        id: "agent",
        capture: "agent-mcp",
        hold: 2.4,
        update: { at: "approve.done", delay: 0.55 },
      },
      {
        id: "approve",
        capture: "agent-approve",
        type: ["sats agent approve ", { paste: true }],
        think: [1.85, 0.3],
        wait: 0.5,
        hold: 1.8,
      },
    ],
    shots: [
      { at: "terminal.start", move: 0, target: { kind: "window" } },
      {
        at: "balance.start",
        delay: 0.1,
        move: 1.1,
        target: { kind: "lines", command: "balance", from: 0, cols: [0, 30], zoom: 2.3 },
      },
      {
        at: "balance.output",
        delay: 0.1,
        move: 1.0,
        target: { kind: "lines", command: "balance", from: "2:0", to: "32:0", cols: [0, 44], zoom: 2.4 },
      },
      {
        at: "send.start",
        delay: 0.05,
        move: 0.9,
        target: { kind: "lines", command: "send", from: 0, cols: [0, 40], zoom: 2.1 },
      },
      {
        at: "send.typed",
        delay: -0.25,
        move: 0.8,
        target: { kind: "lines", command: "send", from: 0, cols: [0, 94] },
      },
      {
        at: "send.output",
        delay: 0.1,
        move: 0.8,
        target: { kind: "lines", command: "send", from: "Wallet", to: "Sign?" },
      },
      {
        at: "send.output",
        delay: 0.8,
        move: 0.9,
        target: { kind: "lines", command: "send", from: "Send", to: "Kept", cols: [0, 56], zoom: 2.2 },
      },
      {
        at: "send.input.0",
        delay: -0.5,
        move: 0.75,
        target: { kind: "lines", command: "send", from: "Postage", to: "Sign?", cols: [0, 40], zoom: 2.0 },
      },
      {
        at: "send.input.1.enter",
        delay: 0.05,
        move: 0.7,
        target: { kind: "lines", command: "send", from: "password:", to: 11, cols: [0, 78], zoom: 1.6 },
      },
      { at: "agent.enter", delay: -0.15, move: 1.0, target: { kind: "card" } },
      {
        at: "approve.start",
        delay: -0.1,
        move: 1.0,
        target: { kind: "lines", command: "approve", from: 0, cols: [0, 56], zoom: 1.9 },
      },
      {
        at: "approve.output",
        delay: 0.1,
        move: 0.8,
        target: { kind: "lines", command: "approve", from: "Approve", to: "Recipient", cols: [0, 52], zoom: 2.0 },
      },
      {
        at: "approve.output",
        delay: 0.85,
        move: 0.8,
        target: { kind: "lines", command: "approve", from: "Operation", to: "Returned assets", cols: [0, 48], zoom: 2.0 },
      },
      {
        at: "approve.input.0",
        delay: -0.3,
        move: 0.7,
        target: { kind: "lines", command: "approve", from: "Returned assets", to: "password:", cols: [0, 48], zoom: 1.9 },
      },
      {
        at: "approve.input.1.enter",
        delay: 0.05,
        move: 0.6,
        target: { kind: "lines", command: "approve", from: "password:", to: 17, cols: [0, 56], zoom: 1.7 },
      },
      { at: "approve.done", delay: 0.3, move: 1.0, target: { kind: "card", result: true } },
    ],
    highlights: [
      { at: "balance.output", delay: 0.7, command: "balance", line: "32:0", hold: 1.15, note: "frBTC" },
      { at: "send.output", delay: 1.05, command: "send", line: "Send", hold: 0.7 },
      { at: "send.output", delay: 1.7, command: "send", line: "Kept", hold: 1.05, note: "stays with you" },
      { at: "approve.output", delay: 0.35, command: "approve", line: "Agent", hold: 0.6 },
      { at: "approve.output", delay: 1.15, command: "approve", line: "Asset amount", hold: 0.65 },
      { at: "approve.output", delay: 1.7, command: "approve", line: "Returned assets", hold: 0.7 },
    ],
    captions: [
      {
        at: "balance.start",
        delay: 0.15,
        text: "See every alkane you hold.",
        until: { at: "send.start", delay: -0.3 },
      },
      {
        at: "send.start",
        delay: 0.15,
        text: "Send an exact amount.\nKeep the rest.",
        until: { at: "agent.enter", delay: -0.3 },
      },
      {
        at: "agent.enter",
        delay: 0.15,
        text: "Agents can ask\nfor a transfer.",
        until: { at: "approve.start", delay: -0.3 },
      },
      {
        at: "approve.start",
        delay: 0.15,
        text: "Nothing moves\nuntil you approve.",
        until: { at: "terminal.end", delay: -0.2 },
      },
    ],
  },
  tagline: { lines: ["AI asks.", "You approve.", "Keys stay yours."], hold: 2.5 },
  end: {
    url: "sats.sh",
    install: "curl -fsSL https://sats.sh/setup.sh | sh",
    footer: "v0.0.4 · signet · test networks only",
    hold: 2.9,
  },
};
