const FALLBACK_SITE_URL = "https://sats-jonathan-navarretes-projects.vercel.app";

export function siteUrl(): URL {
  const explicit = process.env.NEXT_PUBLIC_SITE_URL;
  if (explicit) return new URL(explicit);
  const vercel = process.env.VERCEL_PROJECT_PRODUCTION_URL;
  if (vercel) return new URL(`https://${vercel}`);
  return new URL(FALLBACK_SITE_URL);
}
