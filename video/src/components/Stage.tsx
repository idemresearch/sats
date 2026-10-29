import React from "react";
import { AbsoluteFill, useVideoConfig } from "remotion";
import { mix, progress } from "../lib/anim";
import { cameraAt, framing } from "../lib/camera";
import type { Timeline } from "../lib/timeline";
import { color, ease } from "../theme";
import { AgentCard } from "./AgentCard";
import { Terminal } from "./Terminal";

type Props = { timeline: Timeline; t: number };

/** The world the camera films: the terminal window, and the agent's card beside it. */
export const Stage: React.FC<Props> = ({ timeline, t }) => {
  const { width, height } = useVideoConfig();
  const [start, end] = timeline.acts.terminal;
  if (t < start - 0.05 || t > end + 1.2) return null;
  const framed = framing({ width, height }, timeline.captions.map((c) => c.text));
  const camera = cameraAt(timeline.shots, t, { width, height }, framed);
  const anchor = (framed.area.top + framed.area.bottom) / 2;
  const rise = progress(t, start, 1.0, ease.out);
  const recede = progress(t, end - 0.15, 1.0, ease.inOut);
  const { window } = timeline.layout;
  return (
    <AbsoluteFill
      style={{
        opacity: 1 - recede,
        filter: recede > 0 ? `blur(${14 * recede}px)` : undefined,
      }}
    >
      <div
        style={{
          position: "absolute",
          left: 0,
          top: 0,
          transformOrigin: "0 0",
          transform: `translate(${width / 2}px, ${anchor}px) scale(${
            camera.zoom * mix(1, 0.82, recede)
          }) translate(${-camera.x}px, ${-camera.y}px)`,
        }}
      >
        <div
          style={{
            position: "absolute",
            width: window.w,
            height: window.h,
            opacity: rise,
            transformOrigin: "50% 100%",
            transform: `perspective(2600px) translateY(${mix(260, 0, rise)}px) rotateX(${mix(
              20,
              0,
              rise,
            )}deg) scale(${mix(0.92, 1, rise)})`,
          }}
        >
          <Terminal timeline={timeline} t={t} />
        </div>
        <AgentCard timeline={timeline} t={t} />
      </div>
    </AbsoluteFill>
  );
};

// A tile of film grain: keeps the dark gradients from banding once encoded.
const GRAIN = `url("data:image/svg+xml,${encodeURIComponent(
  '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="240"><filter id="n"><feTurbulence type="fractalNoise" baseFrequency="0.85" numOctaves="2" stitchTiles="stitch"/><feColorMatrix values="0 0 0 0 1  0 0 0 0 1  0 0 0 0 1  0 0 0 0.55 0"/></filter><rect width="240" height="240" filter="url(#n)"/></svg>',
)}")`;

export const Backdrop: React.FC<Props> = ({ timeline, t }) => {
  const { width, height, fps } = useVideoConfig();
  const [start, end] = timeline.acts.terminal;
  const inTerminal = t >= start && t <= end + 1.2;
  const framed = framing({ width, height }, timeline.captions.map((c) => c.text));
  const camera = inTerminal ? cameraAt(timeline.shots, t, { width, height }, framed) : null;
  const shiftX = camera ? -camera.x * 0.05 * camera.zoom : 0;
  const shiftY = camera ? -camera.y * 0.05 * camera.zoom : 0;
  const frame = Math.round(t * fps);
  return (
    <AbsoluteFill style={{ background: color.background }}>
      <AbsoluteFill
        style={{
          backgroundImage: "radial-gradient(circle, rgba(255,255,255,0.075) 1.1px, transparent 1.6px)",
          backgroundSize: "36px 36px",
          backgroundPosition: `${shiftX}px ${shiftY}px`,
          maskImage: "radial-gradient(ellipse 70% 65% at 50% 50%, black, transparent)",
          opacity: 0.55,
        }}
      />
      <AbsoluteFill
        style={{
          background:
            "radial-gradient(ellipse 80% 70% at 50% 45%, transparent 40%, rgba(0,0,0,0.55))",
        }}
      />
      <AbsoluteFill
        style={{
          backgroundImage: GRAIN,
          backgroundPosition: `${(frame * 73) % 240}px ${(frame * 151) % 240}px`,
          opacity: 0.075,
          mixBlendMode: "screen",
        }}
      />
    </AbsoluteFill>
  );
};
