const SITE_URL = "https://sats.sh";

export function siteUrl(): URL {
  return new URL(process.env.NEXT_PUBLIC_SITE_URL ?? SITE_URL);
}
