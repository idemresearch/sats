import React from "react";
import { envelope, mix, progress } from "../lib/anim";
import {
  BAR,
  CHAR,
  FONT_SIZE,
  LINE,
  PAD_BOTTOM,
  PAD_TOP,
  PAD_X,
} from "../lib/layout";
import type { Span, Style } from "../lib/screen";
import { type Timeline, screenAt } from "../lib/timeline";
import { ansi, color, ease, mono } from "../theme";

const SCROLL = 0.6;
const CARET_BLINK = 1.06;

const textColor = (style: Style) => {
  if (style.fg) return ansi[style.fg] ?? style.fg;
  if (style.dim) return color.subtle;
  if (style.bold) return color.foreground;
  return color.output;
};

// Geist Mono has no check mark; draw it in exactly one cell.
const Check: React.FC<{ fill: string }> = ({ fill }) => (
  <svg
    width={CHAR}
    height={FONT_SIZE}
    viewBox="0 0 12 20"
    style={{ display: "inline-block", verticalAlign: "-0.2em" }}
  >
    <path
      d="M2.2 10.6 L5.1 13.6 L10 6.6"
      fill="none"
      stroke={fill}
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

const SpanView: React.FC<{ span: Span }> = ({ span }) => {
  const fill = textColor(span.style);
  const style: React.CSSProperties = {
    color: fill,
    fontWeight: span.style.bold ? 600 : 400,
  };
  if (!span.text.includes("✓")) return <span style={style}>{span.text}</span>;
  return (
    <span style={style}>
      {span.text.split(/(✓)/).map((part, i) =>
        part === "✓" ? <Check key={i} fill={fill} /> : part,
      )}
    </span>
  );
};

/** The first visible line at `t`: each new prompt scrolls to the top. */
const scrollAt = (timeline: Timeline, t: number) => {
  let line = 0;
  for (const key of timeline.scroll) {
    if (key.t > t) break;
    line = mix(line, key.line, progress(t, key.t, SCROLL, ease.inOut));
  }
  return line;
};

export const Terminal: React.FC<{ timeline: Timeline; t: number }> = ({
  timeline,
  t,
}) => {
  const { layout, launch } = timeline;
  const screen = screenAt(timeline, t);
  const lines = screen.spans();
  const top = scrollAt(timeline, t);

  // The spotlight: highlighted lines stay, the rest of that command dims.
  const active = timeline.highlights
    .map((h) => ({ h, a: envelope(t, h.t, h.end, 0.3, 0.35, ease.out) }))
    .filter(({ a }) => a > 0);
  const commandOf = (line: number) =>
    timeline.commands.find((c) => line >= c.prompt && line <= c.last)?.id;
  const opacity = (line: number) => {
    let dim = 0;
    for (const { h, a } of active) {
      if (line !== h.line && commandOf(line) === h.command) dim = Math.max(dim, a);
    }
    return 1 - 0.62 * dim;
  };

  const lastKey = timeline.keys.reduce((last, k) => (k <= t ? k : last), -Infinity);
  const typing = t - lastKey < 0.45;
  const caretOn = typing || (t - Math.max(lastKey, 0)) % CARET_BLINK < CARET_BLINK / 2;
  const pulse = Math.max(0, 1 - (t - lastKey) / 0.12);

  const first = Math.max(0, Math.floor(top) - 1);
  const last = Math.ceil(top + layout.rows) + 1;

  return (
    <div
      style={{
        position: "absolute",
        width: layout.window.w,
        height: layout.window.h,
        borderRadius: 16,
        background: color.surface,
        border: `1px solid ${color.borderStrong}`,
        boxShadow:
          "0 60px 160px rgba(0,0,0,0.7), 0 18px 48px rgba(0,0,0,0.5), inset 0 1px 0 rgba(255,255,255,0.05)",
        overflow: "hidden",
        fontFamily: mono,
      }}
    >
      <div
        style={{
          position: "absolute",
          inset: 0,
          height: BAR,
          display: "grid",
          gridTemplateColumns: "1fr auto 1fr",
          alignItems: "center",
          padding: "0 22px",
          borderBottom: `1px solid ${color.border}`,
          background: "rgba(255,255,255,0.012)",
        }}
      >
        <div style={{ display: "flex", gap: 9 }}>
          {[0, 1, 2].map((i) => (
            <span
              key={i}
              style={{
                width: 13,
                height: 13,
                borderRadius: "50%",
                background: color.borderStrong,
              }}
            />
          ))}
        </div>
        <div style={{ color: color.muted, fontSize: 17 }}>{launch.terminal.title}</div>
        <div
          style={{
            justifySelf: "end",
            display: "flex",
            alignItems: "center",
            gap: 9,
            color: color.subtle,
            fontSize: 15,
          }}
        >
          <span
            style={{ width: 8, height: 8, borderRadius: "50%", background: color.accent }}
          />
          {launch.terminal.status}
        </div>
      </div>
      <div
        style={{
          position: "absolute",
          left: 0,
          right: 0,
          top: BAR,
          bottom: 0,
          overflow: "hidden",
          maskImage: `linear-gradient(transparent, black ${PAD_TOP}px, black calc(100% - ${PAD_BOTTOM}px), transparent)`,
        }}
      >
        <div
          style={{
            position: "absolute",
            left: PAD_X,
            top: PAD_TOP - top * LINE,
            fontSize: FONT_SIZE,
            lineHeight: `${LINE}px`,
            whiteSpace: "pre",
            fontVariantLigatures: "none",
            fontFeatureSettings: '"liga" 0, "calt" 0',
          }}
        >
          {active.map(({ h, a }) => {
            const grow = progress(t, h.t, 0.38, ease.out);
            const note = envelope(t, h.t + 0.12, h.end, 0.32, 0.3, ease.out);
            return (
              <React.Fragment key={`${h.command}-${h.line}-${h.t}`}>
                <div
                  style={{
                    position: "absolute",
                    left: (h.cols[0] - 0.7) * CHAR,
                    top: h.line * LINE + 2,
                    width: (h.cols[1] - h.cols[0] + 1.4) * CHAR,
                    height: LINE - 4,
                    borderRadius: 7,
                    background: `rgba(247, 147, 26, ${0.13 * a})`,
                    boxShadow: `inset 3px 0 0 rgba(247, 147, 26, ${a})`,
                    transform: `scaleX(${mix(0.2, 1, grow)})`,
                    transformOrigin: "left center",
                  }}
                />
                {h.note ? (
                  <div
                    style={{
                      position: "absolute",
                      left: (h.cols[1] + 2.2) * CHAR,
                      top: h.line * LINE + 4,
                      height: LINE - 8,
                      display: "flex",
                      alignItems: "center",
                      gap: 9,
                      padding: "0 14px",
                      borderRadius: 999,
                      border: `1px solid rgba(247, 147, 26, ${0.55 * note})`,
                      background: `rgba(247, 147, 26, ${0.1 * note})`,
                      color: color.accent,
                      fontSize: 20,
                      lineHeight: 1,
                      opacity: note,
                      transform: `translateX(${mix(-10, 0, note)}px)`,
                    }}
                  >
                    {h.note}
                  </div>
                ) : null}
              </React.Fragment>
            );
          })}
          {timeline.pastes.map((paste) => {
            const flash = envelope(t, paste.t, paste.t + 0.9, 0.05, 0.75);
            if (flash <= 0) return null;
            return (
              <div
                key={paste.t}
                style={{
                  position: "absolute",
                  left: paste.cols[0] * CHAR - 4,
                  top: paste.line * LINE + 3,
                  width: (paste.cols[1] - paste.cols[0]) * CHAR + 8,
                  height: LINE - 6,
                  borderRadius: 6,
                  background: `rgba(237, 237, 237, ${0.14 * flash})`,
                }}
              />
            );
          })}
          {lines.slice(first, last).map((spans, i) => {
            const line = first + i;
            return (
              <div
                key={line}
                style={{
                  position: "absolute",
                  top: line * LINE,
                  height: LINE,
                  opacity: opacity(line),
                }}
              >
                {spans.map((span, j) => (
                  <SpanView key={j} span={span} />
                ))}
              </div>
            );
          })}
          <div
            style={{
              position: "absolute",
              left: screen.col * CHAR,
              top: screen.row * LINE + (LINE - FONT_SIZE * 1.3) / 2,
              width: CHAR,
              height: FONT_SIZE * 1.3,
              background: color.accent,
              opacity: caretOn ? 0.9 : 0,
              boxShadow: `0 0 ${18 * pulse}px rgba(247, 147, 26, ${0.8 * pulse})`,
            }}
          />
        </div>
      </div>
    </div>
  );
};
