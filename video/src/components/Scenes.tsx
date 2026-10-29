import React from "react";
import { AbsoluteFill, useVideoConfig } from "remotion";
import { envelope, mix, progress } from "../lib/anim";
import { captionLines, framing } from "../lib/camera";
import type { Timeline } from "../lib/timeline";
import { color, ease, mono } from "../theme";
import { Tile } from "./Wordmark";

type Props = { timeline: Timeline; t: number };

/** The largest size at which `chars` monospace characters fit `width`. */
const fit = (width: number, chars: number, max: number) =>
  Math.min(max, width / (chars * 0.6));

/** Words rise into place one after another, out of a soft blur. */
const Words: React.FC<{
  text: string;
  t: number;
  start: number;
  stagger?: number;
  style?: React.CSSProperties;
}> = ({ text, t, start, stagger = 0.075, style }) => (
  <span style={{ display: "inline-flex", flexWrap: "wrap", columnGap: "0.6em", ...style }}>
    {text.split(" ").map((word, i) => {
      const p = progress(t, start + i * stagger, 0.75, ease.out);
      return (
        <span
          key={i}
          style={{
            display: "inline-block",
            opacity: p,
            transform: `translateY(${mix(0.42, 0, p)}em)`,
            filter: `blur(${mix(10, 0, p)}px)`,
          }}
        >
          {word}
        </span>
      );
    })}
  </span>
);

const Glow: React.FC<{ strength: number; size: number }> = ({ strength, size }) => (
  <div
    style={{
      position: "absolute",
      left: "50%",
      top: "50%",
      width: size,
      height: size * 0.62,
      transform: "translate(-50%, -50%)",
      background:
        "radial-gradient(closest-side, rgba(247,147,26,0.12), rgba(247,147,26,0.07) 30%, rgba(247,147,26,0.03) 60%, rgba(247,147,26,0.01) 80%, transparent)",
      opacity: strength,
      filter: "blur(30px)",
    }}
  />
);

export const Title: React.FC<Props> = ({ timeline, t }) => {
  const { width, height } = useVideoConfig();
  const { title } = timeline.launch;
  const [start, end] = timeline.acts.title;
  if (t > end + 0.1) return null;
  const leave = progress(t, end - 0.5, 0.5, ease.in);
  const headline = fit(width * 0.84, title.headline.length, 108);
  const tile = headline * 0.62;
  // The wordmark is up on the first frame, which feeds show as the thumbnail.
  const settle = progress(t, start, 0.9, ease.out);
  const chip = progress(t, start + 0.2, 0.6, ease.out);
  return (
    <AbsoluteFill
      style={{
        alignItems: "center",
        justifyContent: "center",
        opacity: 1 - leave,
        transform: `translateY(${mix(0, -height * 0.09, leave)}px) scale(${mix(1, 0.96, leave)})`,
        filter: `blur(${mix(0, 14, leave)}px)`,
        fontFamily: mono,
      }}
    >
      <Glow strength={progress(t, start - 0.2, 1.4, ease.out)} size={width * 0.9} />
      <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: headline * 0.52 }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: tile * 0.36,
            transform: `scale(${mix(1.06, 1, settle)})`,
          }}
        >
          <Tile size={tile} style={{ transform: `rotate(${mix(-6, 0, settle)}deg)` }} />
          <span
            style={{
              color: color.foreground,
              fontSize: tile * 0.78,
              fontWeight: 700,
              letterSpacing: "-0.02em",
            }}
          >
            sats
          </span>
          <span
            style={{
              marginLeft: tile * 0.1,
              padding: `${tile * 0.07}px ${tile * 0.22}px`,
              borderRadius: 999,
              border: `1px solid ${color.borderStrong}`,
              color: color.muted,
              fontSize: tile * 0.3,
              opacity: chip,
              transform: `translateY(${mix(8, 0, chip)}px)`,
            }}
          >
            v{title.version}
          </span>
        </div>
        <Words
          text={title.headline}
          t={t}
          start={start + 0.3}
          style={{
            color: color.foreground,
            fontSize: headline,
            fontWeight: 600,
            letterSpacing: "-0.035em",
            justifyContent: "center",
          }}
        />
      </div>
    </AbsoluteFill>
  );
};

export const Captions: React.FC<Props> = ({ timeline, t }) => {
  const viewport = useVideoConfig();
  const framed = framing(viewport, timeline.captions.map((c) => c.text));
  const { size, columns, top } = framed.caption;
  const m = framed.margin;
  const portrait = viewport.height > viewport.width;
  return (
    <AbsoluteFill style={{ pointerEvents: "none" }}>
      {timeline.captions.map((caption) => {
        if (t < caption.t - 0.1 || t > caption.end + 0.5) return null;
        const out = progress(t, caption.end, 0.45, ease.in);
        const lines = captionLines(caption.text, columns);
        const height = lines.length * size * 1.2;
        return (
          <React.Fragment key={caption.t}>
            <div
              style={{
                position: "absolute",
                left: 0,
                right: 0,
                top: portrait ? top - m : 0,
                height: portrait ? height + 2 * m : top + height + m,
                background: portrait
                  ? `linear-gradient(transparent, ${color.background}d9 ${m}px, ${color.background}d9 calc(100% - ${m}px), transparent)`
                  : `linear-gradient(${color.background} ${top + height - m * 0.5}px, transparent)`,
                opacity: envelope(t, caption.t - 0.1, caption.end + 0.45, 0.5, 0.45),
              }}
            />
            <div
              style={{
                position: "absolute",
                left: m,
                top,
                right: m,
                fontFamily: mono,
                fontSize: size,
                lineHeight: 1.2,
                fontWeight: 500,
                letterSpacing: "-0.03em",
                color: color.foreground,
                opacity: 1 - out,
                transform: `translateY(${mix(0, -12, out)}px)`,
              }}
            >
              {lines.map((line, i) => (
                <div key={i}>
                  <Words
                    text={line}
                    t={t}
                    start={caption.t + i * 0.18}
                    stagger={0.05}
                    style={{ columnGap: "0.55em" }}
                  />
                </div>
              ))}
            </div>
          </React.Fragment>
        );
      })}
    </AbsoluteFill>
  );
};

export const Tagline: React.FC<Props> = ({ timeline, t }) => {
  const { width } = useVideoConfig();
  const [start, end] = timeline.acts.tagline;
  if (t < start - 0.1 || t > end + 0.1) return null;
  const { lines } = timeline.launch.tagline;
  const size = fit(width * 0.84, Math.max(...lines.map((l) => l.length)), 112);
  const leave = progress(t, end - 0.45, 0.45, ease.in);
  return (
    <AbsoluteFill
      style={{
        alignItems: "center",
        justifyContent: "center",
        fontFamily: mono,
        opacity: 1 - leave,
        filter: `blur(${mix(0, 10, leave)}px)`,
        transform: `scale(${mix(1, 0.97, leave)})`,
      }}
    >
      <Glow strength={0.7 * progress(t, start, 1.2, ease.out)} size={width * 0.8} />
      <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: size * 0.18 }}>
        {lines.map((line, i) => (
          <Words
            key={line}
            text={line}
            t={t}
            start={start + 0.35 + i * 0.42}
            style={{
              color: i === lines.length - 1 ? color.foreground : color.muted,
              fontSize: size,
              fontWeight: 600,
              letterSpacing: "-0.04em",
            }}
          />
        ))}
      </div>
    </AbsoluteFill>
  );
};

export const EndCard: React.FC<Props> = ({ timeline, t }) => {
  const { width } = useVideoConfig();
  const [start] = timeline.acts.end;
  if (t < start - 0.1) return null;
  const { end } = timeline.launch;
  const tile = Math.min(150, width * 0.11);
  const tileIn = progress(t, start + 0.05, 0.8, ease.out);
  const word = progress(t, start + 0.22, 0.7, ease.out);
  const install = fit(width * 0.84, end.install.length + 2, 30);
  const row = (delay: number) => progress(t, start + delay, 0.7, ease.out);
  return (
    <AbsoluteFill style={{ alignItems: "center", justifyContent: "center", fontFamily: mono }}>
      <Glow strength={progress(t, start, 1.4, ease.out)} size={width * 0.85} />
      <div style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
        <div style={{ display: "flex", alignItems: "center", gap: tile * 0.34 }}>
          <Tile
            size={tile}
            style={{
              opacity: tileIn,
              transform: `scale(${mix(0.55, 1, tileIn)}) rotate(${mix(-14, 0, tileIn)}deg)`,
              filter: `blur(${mix(8, 0, tileIn)}px)`,
            }}
          />
          <span
            style={{
              color: color.foreground,
              fontSize: tile * 0.84,
              fontWeight: 700,
              letterSpacing: "-0.02em",
              clipPath: `inset(0 ${mix(100, 0, word)}% 0 0)`,
            }}
          >
            sats
          </span>
        </div>
        <div
          style={{
            marginTop: tile * 0.52,
            color: color.foreground,
            fontSize: tile * 0.46,
            fontWeight: 600,
            opacity: row(0.45),
            transform: `translateY(${mix(14, 0, row(0.45))}px)`,
          }}
        >
          {end.url}
        </div>
        <div
          style={{
            marginTop: tile * 0.36,
            display: "flex",
            gap: install * 0.6,
            padding: `${install * 0.55}px ${install * 0.9}px`,
            borderRadius: install * 0.6,
            border: `1px solid ${color.border}`,
            background: "rgba(255,255,255,0.02)",
            fontSize: install,
            opacity: row(0.62),
            transform: `translateY(${mix(14, 0, row(0.62))}px)`,
          }}
        >
          <span style={{ color: color.accent, fontWeight: 700 }}>$</span>
          <span style={{ color: color.foreground, fontWeight: 600 }}>{end.install}</span>
        </div>
        <div
          style={{
            marginTop: tile * 0.42,
            color: color.subtle,
            fontSize: install * 0.8,
            opacity: row(0.85),
          }}
        >
          {end.footer}
        </div>
      </div>
    </AbsoluteFill>
  );
};
