# sats

**A tiny Bitcoin wallet for humans and agents.**

**AI asks. You approve. Keys stay yours.**

Run an on-chain wallet from your terminal, or let an AI agent use it
without giving it your keys: the agent can read the wallet and propose
payments inside a budget you set, and every agent send waits for your
one-time approval before a separate process — the only one holding the
key — signs it. There is no autonomous spend mode: an unapproved agent
request can never reach the signer, even if the agent misbehaves.

[Docs](docs/README.md) · [Direction](docs/direction.md) ·
[CLI](docs/cli.md) · [MCP](docs/mcp.md) ·
[Architecture](docs/architecture.md) · [Security](docs/security.md)

> [!WARNING]
> sats is experimental. Signet is the default; use small amounts and short
> agent grants while evaluating it.
>
> Grants created before the signing daemon stored recoverable key material in
> the grant file. They are now refused rather than honored. If an agent with
> shell access ever ran while one existed, move the funds to a fresh wallet —
> see [Security](docs/security.md).

## Install

Prebuilt binaries are available for macOS and Linux on x86_64 and ARM64:

```sh
curl -fsSL https://sats.sh/setup.sh | sh
```

The installer verifies the release checksum, installs `sats` to
`~/.local/bin`, and adds that directory to your shell path when needed.
Pin a release or choose another install directory with environment variables:

```sh
curl -fsSL https://sats.sh/setup.sh \
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
sats status
sats history
```

`send` prepares the transaction, shows its amount and fee, asks for
confirmation, signs locally, saves the finalized transaction, and broadcasts.
`send --dry-run` prices the same spend without persisting anything. The
explicit PSBT lifecycle runs on file artifacts, step by step:

```sh
sats send tb1p... 25k --export-psbt spend.psbt
sats psbt inspect spend.psbt
sats psbt sign spend.psbt
sats tx broadcast <txid>
```

After signing, sats keeps private raw transaction hex for broadcast retry; it
does not retain the signed PSBT. `sats psbt sign tx.psbt` also signs external
PSBTs from other wallets. Amounts are integer sats with shorthand: `25k` is
25,000 and `1.5m` is 1,500,000.

See the [CLI reference](docs/cli.md) for all commands, flags, configuration,
and machine-readable output.

## Let an agent ask

Grant an agent the authority to file requests:

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
# prints, once: SATS_AGENT_TOKEN=<token>

claude mcp add sats --env SATS_AGENT_TOKEN=<token> -- sats agent serve claude
```

The rule: **agents create requests, humans authorize requests, sats
executes requests.** The agent receives five tools: `get_balance`,
`get_receive_address`, `get_grant`, `request_send`, and `check_request`.
It can read the wallet and file a request; it cannot approve, unlock,
sign, or broadcast, and it takes no action after filing. The normal
result of `request_send` is a pending request —

```json
{
  "status": "pending_approval",
  "request_id": "k-invoice-7012",
  "recipient": "tb1p...",
  "amount_sat": 4500,
  "message": "filed for human review — the human approves with: sats agent approve k-invoice-7012; poll check_request to observe the result, and do not file it again"
}
```

You review requests with `sats agent requests` (or stream them with
`--watch`, a trusted channel that does not rely on the agent relaying its
own status), and `sats agent approve <id>` executes exactly that one:
it prepares the transaction, shows you the real fee, takes your password,
reserves the budget, signs, saves, and broadcasts. The agent observes the
result with `check_request`. `sats agent dismiss <id>` declines a request.
`sats agent log` keeps the full causal chain from request through decision
to transaction.

The grant's boundaries are hard, and approval works only inside them:
within `--max-tx` a request waits for you; above it — or over the fee cap
or budget — it is refused outright with no approval path, and only
changing the grant escalates. A `--to` allowlist refuses recipients
outside it; `--mode observe` makes a grant read-only.

The token names a policy; it opens nothing. No process holds an unsealed
seed: your password at approval time unseals it for one execution, and
sats derives what the transaction actually pays from the PSBT rather than
believing what it was told.

`sats agent list` shows current authority. `sats agent revoke claude`
takes effect on the agent's next call, including during an existing MCP
session. See the [MCP guide](docs/mcp.md) for tool contracts and
integration details, and [Direction](docs/direction.md) for the trust
model this implements.

## Safety model

- The seed is sealed with Argon2id and XChaCha20-Poly1305. The SQLite wallet
  database is watch-only and never contains private keys.
- A grant file holds a budget and the SHA-256 of a bearer token. It contains
  no key material: reading it gets you nothing that can spend.
- No agent-originated request reaches the signer without your explicit
  authorization bound to exactly that request. There is no autonomous
  mode and no resident unlocked signer; a grant file in a retired
  development shape fails closed with a recreate hint and is never
  rewritten.
- The approving process recomputes a transaction's payment and fee from
  the PSBT against the wallet's own descriptors and refuses on
  disagreement with the recorded request; a foreign input is refused
  outright.
- Agent authorization is deterministic. Budget is reserved and persisted,
  and the request recorded as executing, before signing, because a signed
  transaction is already spendable. A request that signed is never signed
  again and never refunded; one that did not is refunded.
- Finalized transactions are written privately before broadcast, by the
  process that signed them, so a crash or lost response cannot strand the
  only retry copy.
- Planning excludes common inscription postage outputs by default and unions
  those exclusions with every configured asset guard.
- A configured guard fails closed. Agents cannot use `--allow-dust` or
  `--no-guards`.
- A stolen agent token cannot spend: it can only file requests you will
  see in `sats agent requests`, bounded by the grant until it expires —
  and never touch the seed. Keep budgets small and expiries short anyway.

Read the full [security and trust model](docs/security.md) before using
mainnet.

## One engine, two native surfaces

`crates/sats-core` is the portable wallet engine: transaction planning,
authorization, seed sealing, and the signer boundary, with no filesystem,
network, clock, or async-runtime dependencies. `crates/sats` supplies native
storage, providers, terminal output, the CLI, and the MCP server.

PSBTs are the preparation and signer contract. They stay in memory for normal
sends and leave the wallet only as explicit `--export-psbt` file artifacts.
Once fully signed, sats persists private raw transaction hex instead; a
provider then broadcasts it. An approved agent request uses the same
preparation and safety path as a human send, in the human's process, with
the grant re-checked against the real fee before signing.

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
