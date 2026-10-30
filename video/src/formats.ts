import type { Launch } from "./lib/launch";

// One launch, three shapes: landscape for X and YouTube, square for
// feeds, vertical for Shorts and Stories.
export const FORMATS = [
  { name: "landscape", width: 1920, height: 1080 },
  { name: "square", width: 1080, height: 1080 },
  { name: "vertical", width: 1080, height: 1920 },
] as const;

export const compositionId = (launch: Launch, format: string) =>
  `${launch.id.replace(/\./g, "-")}-${format}`;
