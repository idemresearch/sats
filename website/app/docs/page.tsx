import type { Metadata } from "next";
import DocMarkdown from "@/components/DocMarkdown";
import { readDocsIndex } from "@/lib/docs";

export const metadata: Metadata = { title: "docs" };

export default function DocsIndex() {
  return <DocMarkdown markdown={readDocsIndex()} />;
}
