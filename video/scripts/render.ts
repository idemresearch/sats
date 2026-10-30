// Render every launch in every format to out/. Arguments pass through to
// `remotion render`, e.g. `npm run render -- --concurrency=4`; name a
// format (landscape, square, vertical) to render only that one.

import { execFileSync } from "node:child_process";
import { launches } from "../src/demos";
import { FORMATS, compositionId } from "../src/formats";

const args = process.argv.slice(2);
const only = FORMATS.map((f) => f.name as string).filter((name) => args.includes(name));
const passthrough = args.filter((arg) => !only.includes(arg));

for (const launch of launches) {
  for (const format of FORMATS) {
    if (only.length && !only.includes(format.name)) continue;
    const id = compositionId(launch, format.name);
    execFileSync("npx", ["remotion", "render", id, `out/sats-${launch.id}-${format.name}.mp4`, ...passthrough], {
      stdio: "inherit",
    });
  }
}
