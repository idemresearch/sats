import Link from "next/link";
import Playground from "@/components/Playground";
import CopyButton from "@/components/CopyButton";
import { satsVersion } from "@/lib/site";

const INSTALL_CMD = "curl -fsSL https://sats.sh/setup.sh | sh";

const features = [
  {
    title: "Self-custodial",
    body: (
      <>
        Your seed is sealed on your machine with Argon2id and
        XChaCha20-Poly1305 and unsealed only for the command you run. The
        wallet database is watch-only.
      </>
    ),
  },
  {
    title: "Agents ask, you approve",
    body: (
      <>
        Agents connect over <Link href="/docs/mcp">MCP</Link> and get five
        tools to read the wallet and file requests. None of them approve,
        sign, or broadcast.
      </>
    ),
  },
  {
    title: "Hard limits per agent",
    body: (
      <>
        <code>sats agent grant</code> sets a budget, a per-payment cap, a fee
        cap, a recipient allowlist, and an expiry. Approval can&apos;t
        override them.
      </>
    ),
  },
  {
    title: "Verified signing",
    body: (
      <>
        sats recomputes what the transaction pays and signs only if it
        matches what you approved, once. Every signed transaction is saved
        before broadcast.
      </>
    ),
  },
  {
    title: "Fails closed",
    body: (
      <>
        No spending on stale chain data or while a configured{" "}
        <Link href="/docs/providers">asset guard</Link> is down. 546- and
        330-sat outputs are left alone by default.
      </>
    ),
  },
  {
    title: "Signet first",
    body: (
      <>
        Signet is the default network, where coins have no value. Mainnet is
        always an explicit <code>--network mainnet</code>.
      </>
    ),
  },
  {
    title: "Portable core",
    body: (
      <>
        Planning, authorization, and signing live in <code>sats-core</code>,
        with no filesystem, network, or clock. The same core runs the
        terminal above.
      </>
    ),
  },
];

export default function Home() {
  const version = satsVersion();

  return (
    <>
      <section className="hero" aria-label="Install sats">
        <h1>Self-custodial Bitcoin wallet for humans and agents.</h1>

        <div className="install">
          <span className="prompt" aria-hidden="true">$</span>
          <code>{INSTALL_CMD}</code>
          <CopyButton text={INSTALL_CMD} />
        </div>

        <p className="meta">
          v{version} (
          <a href="https://github.com/idemresearch/sats/blob/main/CHANGELOG.md">
            notes
          </a>
          ) · signet by default · status:{" "}
          <Link className="status" href="/docs/security">
            experimental
          </Link>
        </p>
      </section>

      <section className="try" id="try" aria-label="Try sats in the browser">
        <Playground />
      </section>

      <section className="prose" aria-label="About sats">
        <p>
          sats is a command-line Bitcoin wallet written in Rust, with an MCP
          server for agents. You spend directly from your shell. An agent can
          only ask.
        </p>
        <p>
          An agent files a payment request within limits you set. You review
          the recipient, amount, and real fee, then enter your password, and
          sats signs exactly that payment, once. AI asks. You approve. Keys
          stay yours.
        </p>
        <p>
          The terminal above is the real wallet engine compiled to
          WebAssembly, running against a simulated chain.
        </p>
      </section>

      <section className="features" aria-label="Features">
        {features.map((feature) => (
          <div key={feature.title}>
            <h2>{feature.title}</h2>
            <p>{feature.body}</p>
          </div>
        ))}
      </section>
    </>
  );
}
