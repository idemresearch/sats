import { isValidElement, type ComponentProps, type ReactNode } from "react";
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

// GitHub's heading slug, so `file.md#section` links work on both surfaces.
function textOf(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(textOf).join("");
  if (isValidElement<{ children?: ReactNode }>(node)) return textOf(node.props.children);
  return "";
}

function slug(children: ReactNode): string {
  return textOf(children)
    .toLowerCase()
    .replace(/[^\p{L}\p{M}\p{N}\p{Pc} -]/gu, "")
    .replace(/ /g, "-");
}

function DocH2({ children, ...rest }: ComponentProps<"h2">) {
  return <h2 id={slug(children)} {...rest}>{children}</h2>;
}

function DocH3({ children, ...rest }: ComponentProps<"h3">) {
  return <h3 id={slug(children)} {...rest}>{children}</h3>;
}

export default function DocMarkdown({ markdown }: { markdown: string }) {
  return (
    <article className="doc">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{ a: DocLink, table: DocTable, h2: DocH2, h3: DocH3 }}
      >
        {markdown}
      </ReactMarkdown>
    </article>
  );
}
