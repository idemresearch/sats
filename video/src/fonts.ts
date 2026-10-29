import { loadFont } from "@remotion/fonts";
// The geist package exports no font files, so import past its exports map.
import geistMono from "../node_modules/geist/dist/fonts/geist-mono/GeistMono-Variable.woff2";

export const fontsLoaded = loadFont({
  family: "Geist Mono",
  url: geistMono,
  weight: "100 900",
});
