# sats

**The self-custodial Bitcoin wallet for humans and agents.**
AI asks. You approve. Keys stay yours.

You spend directly. Agents spend through maker-checker: an agent proposes a
payment within limits you set, you check and approve it, and sats signs
exactly that payment, once, with a record of which agent and request it came
from. Your keys never leave your machine.

Today sats ships as a command-line wallet and an MCP server for agents.

[Try it in the browser](https://sats.sh) · [Docs](docs/README.md) ·
[CLI](docs/cli.md) · [Agents](docs/mcp.md) · [Security](docs/security.md)

> [!WARNING]
> sats 0.0.1 is experimental. It defaults to signet, where coins have no
> value. Read the [security model](docs/security.md) before using mainnet, and
> keep amounts and agent budgets small.

## Install

macOS and Linux, x86_64 or ARM64:

```sh
curl -fsSL https://sats.sh/setup.sh | sh
```

The installer verifies the release checksum and installs `sats` to
`~/.local/bin`. Set `SATS_VERSION` to pin a release or `SATS_INSTALL_DIR` to
install elsewhere. To build from source:

```sh
cargo install --locked --path crates/sats
```

## Use it

```sh
sats init                # create a wallet; write down the words it shows
sats receive             # get an address, then fund it from a signet faucet
sats balance
sats send tb1p... 25k    # shows amount and fee, asks, signs, broadcasts
sats history
```

Amounts are whole sats: `25k` is 25,000 and `1.5m` is 1,500,000. Add
`--dry-run` to price a send without making it. Restore, PSBT workflows, and
JSON output are in the [CLI reference](docs/cli.md).

## Let an agent ask

**1. Grant the agent a budget.**

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k
```

sats prints a setup command for Claude Code and one for ChatGPT desktop/Codex.
Run one of them. It contains a token that is shown only once.

**2. The agent files a request.** It has five tools: `get_balance`,
`get_receive_address`, `get_grant`, `request_send`, and `check_request`.
A payment request comes back `pending_approval` and waits for you.

**3. You approve it, or don't.**

```sh
sats agent requests --watch   # see new requests as they arrive
sats agent approve            # review recipient, amount, and real fee; enter your password
```

Approval signs and broadcasts that one payment, and the agent sees the result
through `check_request`. Use `sats agent dismiss <id>` to decline a request
and `sats agent revoke claude` to cut the agent off.

Grant limits are hard. sats refuses a request outright, with no approval
path, if it is over `--max-tx`, over the fee cap, over the remaining budget,
or to a recipient outside a `--to` allowlist. See
[MCP and agent grants](docs/mcp.md) for the full tool contract.

## What makes it different

- **Self-custody.** Your seed is sealed on your machine with Argon2id and
  XChaCha20-Poly1305 and unsealed only for the command you run. The wallet
  database is watch-only. No custodian or server ever holds your keys.
- **Maker-checker for agent payments.** The agent makes a request and you
  check it. The MCP server holds no key material and has no tool to approve,
  sign, or broadcast, so an agent can never approve its own request. There
  is no autonomous spend mode, and a leaked agent token can only file
  requests you will see.
- **Hard limits per agent.** Each grant sets a budget, a per-payment cap, a
  fee cap, an optional recipient allowlist, and an expiry. sats refuses
  anything outside them, and approval can't override them. Revoking a grant
  stops new requests and approvals immediately.
- **Verified execution.** sats recomputes what the transaction actually pays
  and signs only if it matches the recipient and amount you approved, and
  only once. Every signed transaction is saved before broadcast, so a failed
  broadcast can be retried without signing again.
- **Attribution.** Each saved agent transaction names the agent and request
  behind it, and an append-only event log traces every request.

sats also fails closed. It won't spend on stale chain data or while a
configured asset guard is down, and by default it never spends 546- or
330-sat outputs, which may carry inscriptions. The
[security model](docs/security.md) covers the details and the limits.

## Networks and providers

Signet is the default. `mainnet`, `testnet4`, and `regtest` are available
with `--network`, and mainnet is always an explicit choice. Each network has
its own wallet, derived from the same seed.

sats uses mempool.space for chain data unless you configure another provider.
Optional asset guards, such as Subfrost for ord and Alkanes, are never
enabled implicitly. See [Providers and guards](docs/providers.md).

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md). [AGENTS.md](AGENTS.md) holds the
implementation rules for contributors and coding agents, and
[Architecture](docs/architecture.md) maps the code.
