import type { Metadata, Viewport } from "next";
import Link from "next/link";
import { Geist_Mono } from "next/font/google";
import { siteUrl } from "@/lib/site";
import "@xterm/xterm/css/xterm.css";
import "./globals.css";

const mono = Geist_Mono({
  subsets: ["latin"],
  variable: "--font-mono",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: siteUrl(),
  title: {
    default: "sats — a self-custodial Bitcoin wallet for humans and agents",
    template: "%s · sats",
  },
  description:
    "A self-custodial Bitcoin wallet with maker-checker built in. Agents propose payments within limits you set, you approve each one. Keys stay yours.",
  alternates: {
    canonical: "./",
  },
  openGraph: {
    type: "website",
    siteName: "sats",
    url: "./",
    images: [
      {
        url: "/og.png",
        width: 1200,
        height: 630,
        alt: "sats — self-custodial Bitcoin wallet for humans and agents",
      },
    ],
  },
  twitter: {
    card: "summary_large_image",
  },
  icons: {
    icon: [
      {
        url: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%23141412'/%3E%3Ccircle cx='24' cy='8' r='3' fill='%23f7931a'/%3E%3Cpath d='M9 10.5h10v3H9zm0 5.5h10v3H9z' fill='white'/%3E%3C/svg%3E",
        type: "image/svg+xml",
      },
      { url: "/favicon.png", sizes: "32x32", type: "image/png" },
    ],
    apple: "/apple-touch-icon.png",
  },
};

export const viewport: Viewport = {
  colorScheme: "dark light",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#0a0a0a" },
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
  ],
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en" className={mono.variable}>
      <body>
        <header className="site-header">
          <Link className="wordmark" href="/" aria-label="sats home">
            <span className="wordmark-mark" aria-hidden="true">
              =
            </span>
            <span>sats</span>
          </Link>
          <nav aria-label="Main navigation">
            <Link href="/docs">docs</Link>
            <Link href="/docs/cli">cli</Link>
            <Link href="/docs/mcp">agents</Link>
            <Link href="/#try">try</Link>
            <a href="https://github.com/idemresearch/sats">source</a>
          </nav>
        </header>

        <main>{children}</main>

        <footer className="site-footer">
          <a href="https://github.com/idemresearch/sats/blob/main/CHANGELOG.md">
            changelog
          </a>
          <a href="https://github.com/idemresearch/sats">source</a>
          <Link href="/docs">docs</Link>
          <span aria-hidden="true">·</span>
          <a href="https://github.com/idemresearch">Idem Research</a>
        </footer>
      </body>
    </html>
  );
}
