// The camera over the world: each shot frames a rect, and moves between
// shots ease from wherever the camera was, so an interrupted move stays
// continuous. Zoom interpolates in log space, which reads as even speed.

import { mix } from "./anim";
import { CHAR } from "./layout";
import type { Shot } from "./timeline";

export type Camera = { x: number; y: number; zoom: number };
export type Viewport = { width: number; height: number };

/** Where shots and captions go in a frame of this shape. */
export type Framing = {
  margin: number;
  caption: { top: number; size: number; columns: number };
  // The band shots are framed in.
  area: { top: number; bottom: number };
  // Terminal lines never shrink below this, so text stays legible on a
  // phone; wider lines are cropped on the right instead.
  minZoom: number;
};

// After a move lands, the camera keeps pushing in, slowly.
const DRIFT = 0.011;

const ease = (x: number) =>
  x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;

/** Lines a caption takes: "\n" breaks only when the whole text doesn't fit. */
export const captionLines = (text: string, columns: number): string[] => {
  const whole = text.replace(/\n/g, " ");
  const parts = whole.length <= columns ? [whole] : text.split("\n");
  const lines: string[] = [];
  for (const part of parts) {
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
  }
  return lines;
};

export const framing = ({ width, height }: Viewport, captions: string[]): Framing => {
  const margin = Math.min(width, height) * 0.075;
  const size = Math.min(64, width * 0.046);
  const columns = Math.floor((width - 2 * margin) / (size * 0.6));
  const lines = Math.max(1, ...captions.map((text) => captionLines(text, columns).length));
  const band = margin * 1.5 + lines * size * 1.2;
  const maxColumns = width > height ? 112 : width === height ? 58 : 44;
  const minZoom = (width - 2 * margin) / (CHAR * maxColumns);
  if (height > width) {
    // Portrait: shots in the upper part, captions under them, clear of
    // the controls apps lay over the bottom of a vertical video.
    const area = { top: margin * 1.2, bottom: height * 0.64 };
    return { margin, caption: { top: area.bottom + margin * 0.6, size, columns }, area, minZoom };
  }
  return {
    margin,
    caption: { top: margin, size, columns },
    area: { top: captions.length ? band : 0, bottom: height },
    minZoom,
  };
};

export const frame = (
  shot: Pick<Shot, "rect" | "zoom" | "crop">,
  viewport: Viewport,
  framed: Framing,
): Camera => {
  const { rect } = shot;
  const m = framed.margin;
  const areaHeight = framed.area.bottom - framed.area.top;
  const fit = Math.min((viewport.width - 2 * m) / rect.w, (areaHeight - 2 * m) / rect.h);
  let zoom = Math.min(shot.zoom ?? fit, fit, 3);
  let x = rect.x + rect.w / 2;
  if (shot.crop && zoom < framed.minZoom) {
    // Keep the start of the lines in view; let the ends run off the right.
    zoom = framed.minZoom;
    x = rect.x + (viewport.width / 2 - m) / zoom;
  }
  return { x, y: rect.y + rect.h / 2, zoom };
};

const at = (from: Camera, to: Camera, elapsed: number, move: number): Camera => {
  const p = move <= 0 ? 1 : ease(Math.min(1, elapsed / move));
  const settled = Math.max(0, elapsed - move);
  const push = 1 + DRIFT * Math.min(settled, 4);
  return {
    x: mix(from.x, to.x, p),
    y: mix(from.y, to.y, p),
    zoom: Math.exp(mix(Math.log(from.zoom), Math.log(to.zoom), p)) * push,
  };
};

/** Where the camera is at `t`. */
export const cameraAt = (
  shots: Shot[],
  t: number,
  viewport: Viewport,
  framed: Framing,
): Camera => {
  let from = frame(shots[0], viewport, framed);
  let to = from;
  let start = shots[0].t;
  let move = 0;
  for (let i = 1; i < shots.length && shots[i].t <= t; i++) {
    from = at(from, to, shots[i].t - start, move);
    to = frame(shots[i], viewport, framed);
    start = shots[i].t;
    move = shots[i].move;
  }
  return at(from, to, Math.max(0, t - start), move);
};
