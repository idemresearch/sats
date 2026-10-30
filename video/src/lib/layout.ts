// World geometry, in pixels before the camera scales anything: the
// terminal window at the origin and the agent card to its right.

export const FONT_SIZE = 24;
// Geist Mono advances every glyph 600/1000 em.
export const CHAR = FONT_SIZE * 0.6;
export const LINE = 35;
export const BAR = 52;
export const PAD_X = 32;
export const PAD_TOP = 22;
export const PAD_BOTTOM = 28;
export const CARD_GAP = 84;
export const CARD_WIDTH = 760;
// The card holds the request; it grows when the agent sees the outcome.
export const CARD_HEIGHT = 530;
export const CARD_COMPACT = 425;

export type Rect = { x: number; y: number; w: number; h: number };

export type Layout = {
  columns: number;
  rows: number;
  window: Rect;
  card: Rect;
  cardResult: Rect;
  stage: Rect;
};

export const layout = (columns: number, rows: number): Layout => {
  const window = {
    x: 0,
    y: 0,
    w: PAD_X * 2 + columns * CHAR,
    h: BAR + PAD_TOP + rows * LINE + PAD_BOTTOM,
  };
  const cardResult = {
    x: window.w + CARD_GAP,
    y: (window.h - CARD_HEIGHT) / 2,
    w: CARD_WIDTH,
    h: CARD_HEIGHT,
  };
  const card = { ...cardResult, h: CARD_COMPACT };
  return {
    columns,
    rows,
    window,
    card,
    cardResult,
    stage: { x: 0, y: 0, w: card.x + card.w, h: window.h },
  };
};

/** Where screen line `line`, columns [c0, c1), sit while `top` is the first visible line. */
export const lineRect = (
  line: number,
  lines: number,
  cols: [number, number],
  top: number,
): Rect => ({
  x: PAD_X + cols[0] * CHAR,
  y: BAR + PAD_TOP + (line - top) * LINE,
  w: (cols[1] - cols[0]) * CHAR,
  h: lines * LINE,
});
