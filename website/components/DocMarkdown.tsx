import type { ComponentProps } from "react";
import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { rewriteHref } from "@/lib/docs";

function DocLink({ href = "", children, ...rest }: ComponentProps<"a">) {
  const target = rewriteHref(href);
  if (target.startsWith("/") || target.startsWith("#")) {
    return (
      <Link href={target} {...rest}>
        {children}
      </Link>
    );
  }
  return (
    <a href={target} {...rest}>
      {children}
    </a>
  );
}

function DocTable(props: ComponentProps<"table">) {
  return (
    <div className="table-wrap">
      <table {...props} />
    </div>
  );
}

export default function DocMarkdown({ markdown }: { markdown: string }) {
  return (
    <article className="doc">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{ a: DocLink, table: DocTable }}
      >
        {markdown}
      </ReactMarkdown>
    </article>
  );
}
