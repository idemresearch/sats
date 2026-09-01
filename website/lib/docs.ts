import fs from "node:fs";
import path from "node:path";

const DOCS_DIR = path.join(process.cwd(), "..", "docs");
const GITHUB_BLOB = "https://github.com/jonatns/sats/blob/main";
const PUBLIC_DOC_SLUGS = [
  "cli",
  "mcp",
  "providers",
  "security",
  "architecture",
  "direction",
] as const;

const PUBLIC_DOCS_INDEX = [
  "# Documentation",
  "",
  "Learn the wallet on signet first. Mainnet is always an explicit choice.",
  "",
  "## Use sats",
  "",
  "- [CLI reference](cli.md): commands, flags, configuration, and JSON output.",
  "- [MCP and agent grants](mcp.md): connect an agent and understand its bounded wallet tools.",
  "",
  "## Configure and trust",
  "",
  "- [Providers and guards](providers.md): chain access and optional asset protection.",
  "- [Security and trust model](security.md): keys, signing, grants, and failure behavior.",
  "",
  "## Under the hood",
  "",
  "- [Architecture](architecture.md): the portable core and shared human/agent transaction path.",
  "- [Product & security direction](direction.md): the invariant every surface serves — agents propose, humans authorize.",
  "",
  "Source code and maintainer guides live in the [GitHub repository](https://github.com/jonatns/sats).",
].join("\n");

function isPublicDocSlug(slug: string): boolean {
  return PUBLIC_DOC_SLUGS.some((candidate) => candidate === slug);
}

export function listDocSlugs(): string[] {
  return [...PUBLIC_DOC_SLUGS];
}

export function readDoc(slug: string): string {
  if (!isPublicDocSlug(slug)) throw new Error("unknown public doc: " + slug);
  return fs.readFileSync(path.join(DOCS_DIR, slug + ".md"), "utf8");
}

export function readDocsIndex(): string {
  return PUBLIC_DOCS_INDEX;
}

export function docTitle(markdown: string, fallback: string): string {
  const match = markdown.match(/^#\s+(.+)$/m);
  return match ? match[1].trim() : fallback;
}

export function docDescription(markdown: string, fallback: string): string {
  const lines = markdown.split("\n");
  let index = lines.findIndex((line) => /^#\s/.test(line)) + 1;
  const paragraph: string[] = [];
  for (; index < lines.length; index++) {
    const line = lines[index].trim();
    if (line === "") {
      if (paragraph.length > 0) break;
      continue;
    }
    if (/^(#{1,6}\s|[-*]\s|```|\||>)/.test(line)) break;
    paragraph.push(line);
  }
  const text = paragraph
    .join(" ")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
  if (text === "") return fallback;
  if (text.length <= 160) return text;
  return text.slice(0, 157).replace(/\s+\S*$/, "") + "…";
}

export function rewriteHref(href: string): string {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("#")) return href;
  const [target, fragment] = href.split("#");
  const anchor = fragment ? "#" + fragment : "";
  if (target === "README.md" || target === "./README.md") {
    return "/docs" + anchor;
  }
  let match = target.match(/^(?:\.\/)?([a-z0-9-]+)\.md$/i);
  if (match) {
    return isPublicDocSlug(match[1])
      ? "/docs/" + match[1] + anchor
      : GITHUB_BLOB + "/docs/" + target + anchor;
  }
  match = target.match(/^\.\.\/(.+)$/);
  if (match) return GITHUB_BLOB + "/" + match[1] + anchor;
  return GITHUB_BLOB + "/docs/" + target + anchor;
}
