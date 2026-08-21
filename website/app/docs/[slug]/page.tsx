import type { Metadata } from "next";
import Link from "next/link";
import DocMarkdown from "@/components/DocMarkdown";
import { docTitle, listDocSlugs, readDoc } from "@/lib/docs";

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
  return { title: docTitle(readDoc(slug), slug) };
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
