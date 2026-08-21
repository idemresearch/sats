import type { Metadata, Viewport } from "next";
import Link from "next/link";
import "./globals.css";

export const metadata: Metadata = {
  title: {
    default: "sats — a tiny Bitcoin wallet",
    template: "%s · sats",
  },
  description:
    "A tiny on-chain Bitcoin wallet for humans and agents. Keys stay local and agent spending stays inside human-set limits.",
  icons: {
    icon: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%23141412'/%3E%3Ccircle cx='24' cy='8' r='3' fill='%23f7931a'/%3E%3Cpath d='M9 10.5h10v3H9zm0 5.5h10v3H9z' fill='white'/%3E%3C/svg%3E",
  },
};

export const viewport: Viewport = {
  colorScheme: "dark light",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#0b0b0a" },
    { media: "(prefers-color-scheme: light)", color: "#f6f6f2" },
  ],
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body>
        <div className="shell">
          <header className="site-header">
            <Link className="wordmark" href="/" aria-label="sats home">
              <span className="wordmark-mark" aria-hidden="true">
                =
              </span>
              <span>sats</span>
            </Link>
            <nav aria-label="Main navigation">
              <Link href="/#playground">playground</Link>
              <Link href="/docs/cli">cli</Link>
              <Link href="/docs/mcp">agents</Link>
              <Link href="/docs">docs</Link>
              <a href="https://github.com/jonatns/sats">source ↗</a>
            </nav>
          </header>

          <main>{children}</main>

          <footer>
            <div>
              <span className="footer-mark" aria-hidden="true">=</span>
              sats · MIT
            </div>
            <div className="footer-links">
              <Link href="/docs/security">security</Link>
              <Link href="/docs/architecture">architecture</Link>
              <a href="https://github.com/jonatns/sats">github</a>
            </div>
          </footer>
        </div>
      </body>
    </html>
  );
}
