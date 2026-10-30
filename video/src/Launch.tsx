import React from "react";
import { AbsoluteFill, Html5Audio, staticFile, useCurrentFrame, useVideoConfig } from "remotion";
import { Captions, EndCard, Tagline, Title } from "./components/Scenes";
import { Backdrop, Stage } from "./components/Stage";
import "./fonts";
import type { Timeline } from "./lib/timeline";
import { color } from "./theme";

export const Launch: React.FC<{ timeline: Timeline }> = ({ timeline }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const t = frame / fps;
  return (
    <AbsoluteFill style={{ background: color.background }}>
      <Backdrop timeline={timeline} t={t} />
      <Stage timeline={timeline} t={t} />
      <Captions timeline={timeline} t={t} />
      <Title timeline={timeline} t={t} />
      <Tagline timeline={timeline} t={t} />
      <EndCard timeline={timeline} t={t} />
      <Html5Audio src={staticFile(`audio/${timeline.launch.id}.wav`)} />
    </AbsoluteFill>
  );
};
