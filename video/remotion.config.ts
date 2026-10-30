// Options for the Remotion CLI (`remotion studio`, `remotion render`).
// See https://remotion.dev/docs/config
import { Config } from "@remotion/cli/config";

Config.setEntryPoint("src/index.ts");
Config.setRspack(true);
// Lossless frames: terminal text stays crisp until the final encode.
Config.setVideoImageFormat("png");
Config.setCodec("h264");
Config.setCrf(16);
Config.setPixelFormat("yuv420p");
Config.setOverwriteOutput(true);
Config.overrideRspackConfig((config) => ({
  ...config,
  module: {
    ...config.module,
    rules: [
      ...(config.module?.rules ?? []),
      // Recordings are imported as text.
      { test: /\.cast$/, type: "asset/source" },
    ],
  },
}));
