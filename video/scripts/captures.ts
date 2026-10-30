// The recordings, read from disk: what src/captures.ts bundles for the frames.
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { parseCast } from "../src/lib/cast";
import type { AgentCapture, Captures } from "../src/lib/timeline";

export const loadCaptures = (dir = join(import.meta.dirname, "..", "captures")): Captures => {
  const captures: Captures = { casts: {}, agents: {} };
  for (const file of readdirSync(dir)) {
    const text = readFileSync(join(dir, file), "utf8");
    if (file.endsWith(".cast")) captures.casts[file.slice(0, -5)] = parseCast(text);
    if (file.endsWith(".json")) captures.agents[file.slice(0, -5)] = JSON.parse(text) as AgentCapture;
  }
  return captures;
};
