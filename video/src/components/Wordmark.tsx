import React from "react";
import { color, mono } from "../theme";

/** The sats.sh wordmark: a light tile holding "=", then the name. */
export const Tile: React.FC<{ size: number; style?: React.CSSProperties }> = ({
  size,
  style,
}) => (
  <span
    style={{
      display: "inline-grid",
      placeItems: "center",
      width: size,
      height: size,
      borderRadius: size * 0.24,
      background: color.foreground,
      color: color.background,
      fontFamily: mono,
      fontSize: size * 0.68,
      fontWeight: 600,
      lineHeight: 1,
      ...style,
    }}
  >
    =
  </span>
);
