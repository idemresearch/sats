import React from "react";
import { Composition, Folder } from "remotion";
import { captures } from "./captures";
import { launches } from "./demos";
import { FORMATS, compositionId } from "./formats";
import { Launch } from "./Launch";
import { buildTimeline } from "./lib/timeline";

export const RemotionRoot: React.FC = () => (
  <>
    {launches.map((launch) => {
      const timeline = buildTimeline(launch, captures);
      return (
        <Folder key={launch.id} name={launch.id.replace(/\./g, "-")}>
          {FORMATS.map((format) => (
            <Composition
              key={format.name}
              id={compositionId(launch, format.name)}
              component={Launch}
              durationInFrames={Math.ceil(timeline.duration * launch.fps)}
              fps={launch.fps}
              width={format.width}
              height={format.height}
              defaultProps={{ timeline }}
            />
          ))}
        </Folder>
      );
    })}
  </>
);
