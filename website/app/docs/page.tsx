import type { Metadata } from "next";
import DocMarkdown from "@/components/DocMarkdown";
import { readDocsIndex } from "@/lib/docs";

export const metadata: Metadata = {
  title: "docs",
  description:
    "Documentation for the sats Bitcoin wallet: CLI reference, MCP agent grants, providers, and the security and trust model.",
};

export default function DocsIndex() {
  return <DocMarkdown markdown={readDocsIndex()} />;
}
