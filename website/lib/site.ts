import fs from "node:fs";
import path from "node:path";

const SITE_URL = "https://sats.sh";

export function siteUrl(): URL {
  return new URL(process.env.NEXT_PUBLIC_SITE_URL ?? SITE_URL);
}

// `workspace.package.version` is the single source for every surface's version.
export function satsVersion(): string {
  const manifest = fs.readFileSync(
    path.join(process.cwd(), "..", "Cargo.toml"),
    "utf8"
  );
  const match = manifest.match(
    /\[workspace\.package\][^[]*?^version\s*=\s*"([^"]+)"/m
  );
  if (!match) throw new Error("workspace.package.version not found in Cargo.toml");
  return match[1];
}
