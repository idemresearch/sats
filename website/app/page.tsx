import Link from "next/link";
import Terminal from "@/components/Terminal";
import CopyButton from "@/components/CopyButton";

const INSTALL_CMD =
  "curl -fsSL https://raw.githubusercontent.com/jonatns/sats/main/setup.sh | sh";

export default function Home() {
  return (
    <>
      <div className="hero">
        <h1>
          A tiny Bitcoin wallet
          <br />
          for humans <span className="b">and agents</span>.
        </h1>
        <p className="tagline">
          Run an on-chain wallet from your terminal, or give an AI agent a
          budget it cannot exceed. Keys stay local, spends are prepared as
          in-memory PSBTs, and every agent spend is checked against human-set
          limits before signing.
        </p>
        <p className="netnote">
          sats is experimental. <em>Signet is the default</em>; use small
          amounts and short agent grants while evaluating it.
        </p>
      </div>

      <div className="install" id="install">
        <pre>
          <span className="p">$ </span>
          {INSTALL_CMD}
        </pre>
        <CopyButton text={INSTALL_CMD} />
      </div>

      <Terminal />

      <section id="humans">
        <h2>For humans</h2>
        <p>
          Signet is the default, so the complete flow can be tested without
          real funds. <code>send</code> prepares the transaction, shows its
          amount and fee, asks for confirmation, signs locally, saves the
          finalized transaction, and broadcasts. Amounts are integer sats with
          shorthand: <code>25k</code> is 25,000 and <code>1.5m</code> is
          1,500,000.
        </p>
        <pre className="block">
          <span className="p">$ </span>sats init{"\n"}
          <span className="p">$ </span>sats receive{"\n"}
          <span className="p">$ </span>sats balance{"\n"}
          <span className="p">$ </span>sats send tb1p... 25k{"\n"}
          <span className="p">$ </span>sats status{"\n"}
          <span className="p">$ </span>sats history
        </pre>
        <p>
          Spends stay in memory as PSBTs; the explicit PSBT lifecycle runs on
          file artifacts, step by step, and <code>sats psbt sign</code> also
          signs external PSBTs from other wallets:
        </p>
        <pre className="block">
          <span className="p">$ </span>sats send tb1p... 25k --export-psbt
          spend.psbt{"\n"}
          <span className="p">$ </span>sats psbt inspect spend.psbt{"\n"}
          <span className="p">$ </span>sats psbt sign spend.psbt{"\n"}
          <span className="p">$ </span>sats tx broadcast {"<txid>"}
        </pre>
        <p>
          See the <Link href="/docs/cli">CLI reference</Link> for all commands,
          flags, configuration, and machine-readable output.
        </p>
      </section>

      <section id="agents">
        <h2>Give an agent a budget</h2>
        <p>
          Create bounded spending authority, then launch the MCP server as
          that agent. Each send is checked against the grant&apos;s expiry,
          per-transaction amount cap, fee cap, and remaining budget —{" "}
          <strong>before</strong> a signature is produced.
        </p>
        <pre className="block">
          <span className="p">$ </span>sats agent grant claude --budget 50k
          --for 24h --max-tx 10k --max-fee 1000{"\n"}
          <span className="p">$ </span>claude mcp add sats -- sats agent serve
          claude
        </pre>
        <div className="tools">
          <span>get_balance</span>
          <span>get_receive_address</span>
          <span>get_grant</span>
          <span>send</span>
        </div>
        <p>
          Outside that authority the agent receives a deterministic refusal,
          not a signature:
        </p>
        <pre className="block deny">
          {"{\n  "}
          <span className="k">&quot;status&quot;</span>
          {": "}
          <span className="s">&quot;denied&quot;</span>
          {",\n  "}
          <span className="k">&quot;reason&quot;</span>
          {": "}
          <span className="s">&quot;over_max_tx&quot;</span>
          {",\n  "}
          <span className="k">&quot;message&quot;</span>
          {": "}
          <span className="m">
            &quot;human authorization required: requested 20,000 sat; max tx
            10,000 sat&quot;
          </span>
          {"\n}"}
        </pre>
        <p>
          <code>sats agent list</code> shows current authority.{" "}
          <code>sats agent revoke claude</code> takes effect on the
          agent&apos;s next send call, including during an existing MCP
          session. See the <Link href="/docs/mcp">MCP guide</Link> for tool
          contracts and integration details.
        </p>
      </section>

      <section id="safety">
        <h2>Safety model</h2>
        <ul className="plain">
          <li>
            <strong>Sealed seed.</strong> Argon2id and XChaCha20-Poly1305. The
            SQLite wallet database is watch-only and never contains private
            keys.
          </li>
          <li>
            <strong>Deterministic authorization.</strong> Budget is reserved
            and persisted before signing, because a signed transaction is
            already spendable.
          </li>
          <li>
            <strong>Crash-safe broadcast.</strong> Finalized transactions are
            written privately before broadcast, so a lost provider response
            cannot strand the only retry copy.
          </li>
          <li>
            <strong>Inscription-aware planning.</strong> Common inscription
            postage outputs are excluded by default and unioned with every
            configured asset guard.
          </li>
          <li>
            <strong>Fail-closed guards.</strong> A configured guard that is
            unavailable blocks planning. Agents cannot use{" "}
            <code>--allow-dust</code> or <code>--no-guards</code>.
          </li>
          <li>
            <strong>Honest trust model.</strong> An active grant enables
            unattended signing; anyone who can read the grant file as your OS
            user can recover the seed. Keep budgets small and expiries short.
          </li>
        </ul>
        <p>
          Read the full{" "}
          <Link href="/docs/security">security and trust model</Link> before
          using mainnet or unattended grants.
        </p>
      </section>

      <section id="engine">
        <h2>One engine, two native surfaces</h2>
        <p>
          <code>sats-core</code> is the portable wallet engine — transaction
          planning, authorization, seed sealing, and the signer boundary, with
          no filesystem, network, clock, or async-runtime dependencies.{" "}
          <code>sats</code> supplies native storage, providers, terminal
          output, the CLI, and the MCP server. Agents use the same preparation
          and safety path as humans, with the grant check added before
          signing.
        </p>
        <p>
          Wallets are namespaced by network and share one sealed seed:{" "}
          <code>mainnet</code>, <code>signet</code>, <code>testnet4</code>, and{" "}
          <code>regtest</code> — mainnet is always an explicit choice. With no
          provider configuration, sats uses the appropriate mempool.space
          Esplora endpoint for chain sync, fee estimates, and broadcast. See{" "}
          <Link href="/docs/architecture">Architecture</Link> and{" "}
          <Link href="/docs/providers">Providers and guards</Link>.
        </p>
      </section>
    </>
  );
}
