# sats

Self-custodial Bitcoin wallet for humans and agents.

```sh
curl -fsSL https://sats.sh/setup.sh | sh
```

> [!WARNING]
> sats 0.0.1 is experimental. It defaults to signet, where coins have no
> value. Read the [security model](docs/security.md) before using mainnet, and
> keep amounts and agent budgets small.

sats is a command-line Bitcoin wallet written in Rust, with an MCP server for
agents. You spend directly. An agent can only ask: it proposes a payment
within limits you set, you check and approve it, and sats signs exactly that
payment, once. AI asks. You approve. Keys stay yours.

<p>
  <a href="https://sats.sh"><img alt="Try it in the browser" src="https://img.shields.io/badge/try-sats.sh-0a0a0a.svg?style=for-the-badge&amp;labelColor=000000" height="28"></a>
  <a href="https://github.com/idemresearch/sats/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/idemresearch/sats/ci.yml?branch=main&amp;style=for-the-badge&amp;labelColor=000000&amp;label=ci" height="28"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/github/license/idemresearch/sats.svg?style=for-the-badge&amp;labelColor=000000" height="28"></a>
</p>

## Highlights

- **Self-custody:** your seed is sealed on your machine with Argon2id and
  XChaCha20-Poly1305 and unsealed only for the command you run. The wallet
  database is watch-only. No custodian or server ever holds your keys.
- **Maker-checker for agents:** the agent makes a request and you check it.
  The MCP server holds no key material and has no tool to approve, sign, or
  broadcast, so a leaked agent token can only file requests you will see.
- **Hard limits per agent:** a budget, a per-payment cap, a fee cap, an
  optional recipient allowlist, and an expiry. Approval can't override them.
- **Verified execution:** sats recomputes what the transaction pays and signs
  only if it matches what you approved, and only once. Every signed
  transaction is saved before broadcast, so a failed broadcast can be retried
  without signing again.
- **Attribution:** each saved agent transaction names the agent and request
  behind it, and an append-only event log traces every request.

## Install

macOS and Linux, x86_64 or ARM64:

```sh
curl -fsSL https://sats.sh/setup.sh | sh
```

The installer verifies the release checksum and installs `sats` to
`~/.local/bin`. Set `SATS_VERSION` to pin a release or `SATS_INSTALL_DIR` to
install elsewhere.

## Get started

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

Grant the agent a budget:

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k
```

sats prints a setup command for Claude Code and one for ChatGPT desktop/Codex.
Run one of them. It contains a token that is shown only once.

The agent gets five tools: `get_balance`, `get_receive_address`, `get_grant`,
`request_send`, and `check_request`. A payment request comes back
`pending_approval` and waits for you:

```sh
sats agent requests --watch   # see new requests as they arrive
sats agent approve            # review recipient, amount, and real fee; enter your password
```

Approval signs and broadcasts that one payment, and the agent sees the result
through `check_request`. Use `sats agent dismiss <id>` to decline a request
and `sats agent revoke claude` to cut the agent off.

sats refuses a request outright, with no approval path, if it is over
`--max-tx`, over the fee cap, over the remaining budget, or to a recipient
outside a `--to` allowlist. Revoking a grant cuts the agent off at its next call
and stops approvals of its pending requests. See [MCP and agent grants](docs/mcp.md) for the full tool
contract.

## Networks and providers

Signet is the default. `mainnet`, `testnet4`, and `regtest` are available
with `--network`, and mainnet is always an explicit choice. Each network has
its own wallet, derived from the same seed.

sats uses mempool.space for chain data unless you configure another provider.
It fails closed: it won't spend on stale chain data or while a configured
asset guard is down, and by default it never spends 546- or 330-sat outputs,
which may carry inscriptions. Optional asset guards, such as Subfrost for ord
and Alkanes, are never enabled implicitly. See
[Providers and guards](docs/providers.md).

## Documentation

Visit [sats.sh/docs](https://sats.sh/docs) for the manual: the
[CLI reference](docs/cli.md), [MCP and agent grants](docs/mcp.md),
[providers and guards](docs/providers.md), and the
[security and trust model](docs/security.md). The same pages live in
[`docs/`](docs/README.md).

## Build from source

```sh
cargo install --locked --path crates/sats
```

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md). [AGENTS.md](AGENTS.md) holds the
implementation rules for contributors and coding agents, and
[Architecture](docs/architecture.md) maps the code. Notable changes are listed
in [CHANGELOG.md](CHANGELOG.md).

## License

sats is built by [Idem Research](https://github.com/idemresearch) and licensed
under the [Apache License, Version 2.0](LICENSE). Copyright 2026 Idem Research.
