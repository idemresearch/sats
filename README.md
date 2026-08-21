# sats

**A tiny Bitcoin wallet for humans and agents.**

Run an on-chain wallet from your terminal, or give an AI agent a budget it
cannot exceed. Keys stay local, transaction plans are PSBTs, and every agent
spend is checked against human-set limits before signing.

[Docs](docs/README.md) · [CLI](docs/cli.md) · [MCP](docs/mcp.md) ·
[Architecture](docs/architecture.md) · [Security](docs/security.md)

> [!WARNING]
> sats is experimental. Signet is the default; use small amounts and short
> agent grants while evaluating it.

## Install

Prebuilt binaries are available for macOS and Linux on x86_64 and ARM64:

```sh
curl -fsSL https://raw.githubusercontent.com/jonatns/sats/main/setup.sh | sh
```

The installer verifies the release checksum, installs `sats` to
`~/.local/bin`, and adds that directory to your shell path when needed.
Pin a release or choose another install directory with environment variables:

```sh
curl -fsSL https://raw.githubusercontent.com/jonatns/sats/main/setup.sh \
  | SATS_VERSION=0.1.0 SATS_INSTALL_DIR="$HOME/bin" sh
```

To build from a checkout instead:

```sh
cargo install --locked --path crates/sats
```

## Try it

Signet is the default, so the complete flow can be tested without real funds.
Initialize, print a receive address, and fund it from a signet faucet:

```sh
sats init
sats receive
sats balance
sats send tb1p... 25k
```

`send` plans the transaction, shows its amount and fee, asks for confirmation,
signs locally, and broadcasts. The same lifecycle can be run step by step:

```sh
sats plan tb1p... 25k
sats sign
sats broadcast
```

Every step is resumable. A failed broadcast leaves a signed plan that can be
retried, and `sats sign tx.psbt` signs an external PSBT file. Amounts are
integer sats with shorthand: `25k` is 25,000 and `1.5m` is 1,500,000.

See the [CLI reference](docs/cli.md) for all commands, flags, configuration,
and machine-readable output.

## Give an agent a budget

Create bounded spending authority, then launch the MCP server as that agent:

```sh
sats grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
claude mcp add sats -- sats mcp --agent claude
```

The agent receives four tools: `get_balance`, `get_receive_address`,
`get_grant`, and `send`. Each send is checked against the grant's expiry,
per-transaction amount cap, fee cap, and remaining budget. Outside that
authority the agent receives a deterministic refusal, not a signature.

```json
{
  "status": "denied",
  "reason": "over_max_tx",
  "message": "human authorization required: requested 20,000 sat; max tx 10,000 sat"
}
```

`sats grants` shows current authority. `sats revoke claude` takes effect on
the agent's next send call, including during an existing MCP session. See the
[MCP guide](docs/mcp.md) for tool contracts and integration details.

## Safety model

- The seed is sealed with Argon2id and XChaCha20-Poly1305. The SQLite wallet
  database is watch-only and never contains private keys.
- Agent authorization is deterministic. Budget is reserved and persisted
  before signing because a signed transaction is already spendable.
- Planning excludes common inscription postage outputs by default and unions
  those exclusions with every configured asset guard.
- A configured guard fails closed. Agents cannot use `--allow-dust` or
  `--no-guards`.
- An active grant enables unattended signing. Anyone who can read the grant
  file as your OS user can recover the seed; keep budgets small and expiries
  short.

Read the full [security and trust model](docs/security.md) before using
mainnet or unattended grants.

## One engine, two native surfaces

`crates/sats-core` is the portable wallet engine: transaction planning,
authorization, seed sealing, and the signer boundary, with no filesystem,
network, clock, or async-runtime dependencies. `crates/sats` supplies native
storage, providers, terminal output, the CLI, and the MCP server.

PSBTs are the transaction contract at every stage. The watch-only wallet
plans, a signer implementation signs, and a provider broadcasts. Agents use
the same planning and safety path as humans, with the grant check added before
signing.

See [Architecture](docs/architecture.md) for module ownership and end-to-end
flows, or [AGENTS.md](AGENTS.md) for the implementation rules used by coding
agents and contributors.

## Networks and providers

Wallets are namespaced by network and share one sealed seed. Supported
networks are `mainnet`, `signet`, `testnet4`, and `regtest`; mainnet is always
an explicit choice.

Chain access and optional asset protection come from typed providers.
With no provider configuration, sats uses the appropriate mempool.space
Esplora endpoint for chain sync, fee estimates, and broadcast. Guards are
never enabled implicitly.

```toml
# ~/.config/sats/config.toml
network = "mainnet"

[providers.subfrost]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
```

See [Providers and guards](docs/providers.md) for capabilities, resolution
precedence, split-provider configuration, and fail-closed behavior.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo check -p sats-core --target wasm32-unknown-unknown
```

Start with [CONTRIBUTING.md](CONTRIBUTING.md). Release maintainers should also
read [docs/releasing.md](docs/releasing.md).
