// Small animation helpers in seconds, so scenes read like the timeline.

export const clamp01 = (x: number) => Math.min(1, Math.max(0, x));

/** 0 before `start`, 1 after `start + duration`, eased in between. */
export const progress = (
  t: number,
  start: number,
  duration: number,
  ease: (x: number) => number = (x) => x,
) => (duration <= 0 ? (t >= start ? 1 : 0) : ease(clamp01((t - start) / duration)));

/** Rises over `fadeIn` from `start`, holds, and falls over `fadeOut` before `end`. */
export const envelope = (
  t: number,
  start: number,
  end: number,
  fadeIn: number,
  fadeOut: number,
  ease: (x: number) => number = (x) => x,
) =>
  Math.min(
    progress(t, start, fadeIn, ease),
    1 - progress(t, end - fadeOut, fadeOut, ease),
  );

export const mix = (a: number, b: number, p: number) => a + (b - a) * p;
