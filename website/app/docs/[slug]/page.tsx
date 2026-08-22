import type { Metadata } from "next";
import Link from "next/link";
import DocMarkdown from "@/components/DocMarkdown";
import { docDescription, docTitle, listDocSlugs, readDoc } from "@/lib/docs";

export const dynamicParams = false;

export function generateStaticParams() {
  return listDocSlugs().map((slug) => ({ slug }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const markdown = readDoc(slug);
  const title = docTitle(markdown, slug);
  const description = docDescription(
    markdown,
    "Documentation for the sats Bitcoin wallet."
  );
  return { title, description };
}

export default async function DocPage({
  params,
}: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  return (
    <>
      <nav className="doc-nav" aria-label="Breadcrumb">
        <Link href="/docs">docs</Link> / {slug}
      </nav>
      <DocMarkdown markdown={readDoc(slug)} />
    </>
  );
}
