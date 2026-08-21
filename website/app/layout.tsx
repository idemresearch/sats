import type { Metadata } from "next";
import { IBM_Plex_Mono } from "next/font/google";
import Link from "next/link";
import "./globals.css";

const plexMono = IBM_Plex_Mono({
  weight: ["400", "500", "600"],
  subsets: ["latin"],
  display: "swap",
});

export const metadata: Metadata = {
  title: {
    default: "sats",
    template: "%s · sats",
  },
  description:
    "A tiny Bitcoin wallet for humans and agents. Run an on-chain wallet from your terminal, or give an AI agent a budget it cannot exceed.",
  icons: {
    icon: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%230b0e14'/%3E%3Ctext x='16' y='23' font-family='monospace' font-size='18' font-weight='700' fill='%23f7931a' text-anchor='middle'%3E%E2%82%BF%3C/text%3E%3C/svg%3E",
  },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body className={plexMono.className}>
        <div className="wrap">
          <header className="site">
            <div className="brand">
              <Link href="/" style={{ color: "inherit" }}>
                <span className="b">₿</span> sats
              </Link>
            </div>
            <nav>
              <Link href="/#install">install</Link>
              <Link href="/#humans">cli</Link>
              <Link href="/#agents">agents</Link>
              <Link href="/docs">docs</Link>
              <a href="https://github.com/jonatns/sats">github</a>
            </nav>
          </header>
          {children}
          <footer>
            <div>© 2026 sats · MIT-licensed Bitcoin tooling</div>
            <div className="links">
              <a href="https://github.com/jonatns/sats">github</a>
              <Link href="/docs/cli">cli</Link>
              <Link href="/docs/mcp">mcp</Link>
              <Link href="/docs/architecture">architecture</Link>
              <Link href="/docs/security">security</Link>
            </div>
          </footer>
        </div>
      </body>
    </html>
  );
}
