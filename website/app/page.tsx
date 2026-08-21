import Link from "next/link";
import Playground from "@/components/Playground";
import CopyButton from "@/components/CopyButton";

const INSTALL_CMD =
  "curl -fsSL https://raw.githubusercontent.com/jonatns/sats/main/setup.sh | sh";

const humanCommands = [
  "sats init",
  "sats receive",
  "sats balance",
  "sats send tb1p... 25k",
];

const agentCommands = [
  "sats agent grant claude \\",
  "  --budget 50k --for 24h \\",
  "  --max-tx 10k --max-fee 1000",
  "sats agent serve claude",
];

export default function Home() {
  return (
    <>
      <section className="hero" aria-labelledby="hero-title">
        <div className="eyebrow">
          <span className="status-dot" aria-hidden="true" />
          experimental · signet by default
        </div>

        <h1 id="hero-title">
          Bitcoin for humans
          <br />
          <span>and agents.</span>
        </h1>

        <p className="hero-copy">
          A small, native wallet that lives in your terminal. Spend directly,
          or give an AI agent a budget it cannot exceed.
        </p>

        <div className="hero-actions">
          <Link className="button button-primary" href="#playground">
            Try the wallet
          </Link>
          <Link className="button button-secondary" href="/docs/cli">
            Read the CLI docs
          </Link>
        </div>

        <div className="install" id="install" aria-label="Install sats">
          <span className="install-prompt" aria-hidden="true">$</span>
          <code>{INSTALL_CMD}</code>
          <CopyButton text={INSTALL_CMD} />
        </div>

        <dl className="product-facts">
          <div>
            <dt>Keys</dt>
            <dd>local only</dd>
          </div>
          <div>
            <dt>Default network</dt>
            <dd>signet</dd>
          </div>
          <div>
            <dt>Surfaces</dt>
            <dd>CLI + MCP</dd>
          </div>
          <div>
            <dt>Core</dt>
            <dd>Rust + WASM</dd>
          </div>
        </dl>
      </section>

      <section className="playground-section" id="playground" aria-labelledby="playground-title">
        <div className="section-heading">
          <div>
            <p className="section-kicker">Live playground</p>
            <h2 id="playground-title">The real wallet, in your browser.</h2>
          </div>
          <p>
            The portable engine is compiled to WebAssembly. The chain is
            simulated; the planning, signing, UTXO protection, and grant
            authorization are not.
          </p>
        </div>
        <Playground />
      </section>

      <section className="operators" id="operators" aria-labelledby="operators-title">
        <div className="section-heading compact">
          <div>
            <p className="section-kicker">Two native surfaces</p>
            <h2 id="operators-title">One wallet. Two operators.</h2>
          </div>
        </div>

        <div className="operator-grid">
          <article>
            <div className="operator-number">01</div>
            <h3>You, in a shell.</h3>
            <p>
              Intent-first commands handle the normal path. Every send shows
              the amount and fee before the seed is unlocked and the
              transaction is signed.
            </p>
            <CommandBlock commands={humanCommands} />
            <Link className="text-link" href="/docs/cli">
              Explore the CLI <span aria-hidden="true">→</span>
            </Link>
          </article>

          <article>
            <div className="operator-number">02</div>
            <h3>An agent, inside a budget.</h3>
            <p>
              MCP exposes only balance, receive, grant status, and send. The
              human sets expiry, transaction, fee, and total-budget limits
              before unattended signing begins.
            </p>
            <CommandBlock commands={agentCommands} />
            <Link className="text-link" href="/docs/mcp">
              Read the agent contract <span aria-hidden="true">→</span>
            </Link>
          </article>
        </div>
      </section>

      <section className="authority" id="safety" aria-labelledby="authority-title">
        <div className="section-heading">
          <div>
            <p className="section-kicker">Bounded by design</p>
            <h2 id="authority-title">Authority stops before signing.</h2>
          </div>
          <p>
            Agent policy is deterministic and checked before a signature is
            produced. A request outside the grant gets a stable refusal—not a
            retry path.
          </p>
        </div>

        <div className="denial" aria-label="Example denied agent spend">
          <div className="denial-request">
            <span>agent request</span>
            <strong>send 20,000 sat</strong>
          </div>
          <div className="denial-arrow" aria-hidden="true">→</div>
          <div className="denial-result">
            <span>denied</span>
            <code>over_max_tx</code>
          </div>
        </div>

        <div className="principles">
          <article>
            <h3>Watch-only at rest</h3>
            <p>
              SQLite stores public descriptors. The seed stays sealed with
              Argon2id and XChaCha20-Poly1305.
            </p>
          </article>
          <article>
            <h3>Fail closed</h3>
            <p>
              Stale chain state or an unavailable configured asset guard stops
              planning instead of weakening it.
            </p>
          </article>
          <article>
            <h3>Recoverable broadcast</h3>
            <p>
              Finalized transaction hex is saved before broadcast, so a lost
              provider response never strands the retry.
            </p>
          </article>
        </div>

        <Link className="text-link" href="/docs/security">
          Read the security and trust model <span aria-hidden="true">→</span>
        </Link>
      </section>

      <section className="engine" aria-labelledby="engine-title">
        <p className="section-kicker">Portable core</p>
        <h2 id="engine-title">
          The same transaction engine runs the CLI, the MCP server, and this
          playground.
        </h2>
        <p>
          <code>sats-core</code> owns deterministic planning, authorization,
          seed sealing, and signing boundaries—with no filesystem, network,
          clock, terminal, or async-runtime dependencies.
        </p>
        <div className="engine-links">
          <Link className="text-link" href="/docs/architecture">
            Architecture <span aria-hidden="true">→</span>
          </Link>
          <Link className="text-link" href="/docs/providers">
            Providers and guards <span aria-hidden="true">→</span>
          </Link>
        </div>
      </section>
    </>
  );
}

function CommandBlock({ commands }: { commands: string[] }) {
  return (
    <pre className="command-block">
      {commands.map((command, index) => (
        <span key={command}>
          <span className="command-prompt" aria-hidden="true">
            {command.startsWith("  ") ? " " : "$"}
          </span>
          {command}
          {index < commands.length - 1 ? "\n" : ""}
        </span>
      ))}
    </pre>
  );
}
