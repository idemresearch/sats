import fs from "node:fs";
import path from "node:path";

// The docs live at the repository root, one level above the website app.
const DOCS_DIR = path.join(process.cwd(), "..", "docs");
const GITHUB_BLOB = "https://github.com/jonatns/sats/blob/main";

export function listDocSlugs(): string[] {
  return fs
    .readdirSync(DOCS_DIR)
    .filter((f) => f.endsWith(".md") && f !== "README.md")
    .map((f) => f.replace(/\.md$/, ""))
    .sort();
}

export function readDoc(slug: string): string {
  if (!/^[a-z0-9-]+$/i.test(slug)) throw new Error(`bad doc slug: ${slug}`);
  return fs.readFileSync(path.join(DOCS_DIR, `${slug}.md`), "utf8");
}

export function readDocsIndex(): string {
  return fs.readFileSync(path.join(DOCS_DIR, "README.md"), "utf8");
}

export function docTitle(markdown: string, fallback: string): string {
  const m = markdown.match(/^#\s+(.+)$/m);
  return m ? m[1].trim() : fallback;
}

// Rewrite the relative links used inside docs/*.md for the website:
// sibling docs go to /docs/<slug>, repo-root files go to GitHub.
export function rewriteHref(href: string): string {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("#")) return href;
  const [target, fragment] = href.split("#");
  const anchor = fragment ? `#${fragment}` : "";
  if (target === "README.md" || target === "./README.md") return `/docs${anchor}`;
  let m = target.match(/^(?:\.\/)?([a-z0-9-]+)\.md$/i);
  if (m) return `/docs/${m[1]}${anchor}`;
  m = target.match(/^\.\.\/(.+)$/);
  if (m) return `${GITHUB_BLOB}/${m[1]}${anchor}`;
  return `${GITHUB_BLOB}/docs/${target}${anchor}`;
}
