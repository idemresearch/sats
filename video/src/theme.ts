import { Easing } from "remotion";

// sats.sh's palette (website/app/globals.css, dark).
export const color = {
  background: "#0a0a0a",
  surface: "#0c0c0c",
  surface2: "#141414",
  foreground: "#ededed",
  output: "#c9c9c9",
  muted: "#a1a1a1",
  subtle: "#8a8a8a",
  border: "#262626",
  borderStrong: "#3a3a3a",
  accent: "#f7931a",
  accentSoft: "rgba(247, 147, 26, 0.24)",
  success: "#5fcf8a",
  danger: "#ff7b72",
};

// ANSI names as the sats.sh playground maps them.
export const ansi: Record<string, string> = {
  green: color.success,
  yellow: color.accent,
  red: color.danger,
};

export const mono = "'Geist Mono', ui-monospace, monospace";

export const ease = {
  out: Easing.bezier(0.16, 1, 0.3, 1),
  inOut: Easing.bezier(0.65, 0, 0.35, 1),
  in: Easing.bezier(0.55, 0, 1, 0.45),
};
