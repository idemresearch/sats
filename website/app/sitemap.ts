import type { MetadataRoute } from "next";
import { listDocSlugs } from "@/lib/docs";
import { siteUrl } from "@/lib/site";

export const dynamic = "force-static";

export default function sitemap(): MetadataRoute.Sitemap {
  const base = siteUrl();
  const paths = ["/", "/docs", ...listDocSlugs().map((slug) => `/docs/${slug}`)];
  return paths.map((path) => ({ url: new URL(path, base).toString() }));
}
