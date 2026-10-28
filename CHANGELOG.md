# Changelog

Notable changes to sats, newest first. Versions follow
`workspace.package.version` in `Cargo.toml`.

## 0.0.4 (2026-10-28)

0.0.4 adds Alkanes and narrows what sats will sign. Agents can propose
frBTC wraps and exact Alkanes transfers for your approval, a grant becomes a
credential and a scope with no budget, and sats signs only transactions it
built and checked itself. Agent grants from earlier builds must be
re-issued: `sats agent revoke <name> && sats agent grant <name>`. sats still
runs on test networks only.

### Alkanes

- Standard builds include `sats alkanes balance`, `inspect`, `simulate`,
  `send`, and `frbtc info`, with no separate Cargo feature flags. They need
  Subfrost as the network's provider. `simulate` is advisory and never
  prepares a spend.
- `sats alkanes send` transfers an exact amount of one alkane. It spends
  only indexed outputs holding the asset, returns every other alkane to the
  wallet, and shows its review before the password.
- With an Alkanes provider, plain BTC sends exclude asset-bearing and
  unindexed outputs, and stop if the indexer is unavailable.
- MCP `request_alkanes_send` files an exact transfer under the agent's
  existing grant. Approval reviews the asset quantity and what stays in the
  wallet separately from BTC postage and fee, and rechecks indexed balances
  before signing. The grant's amount cap checks the BTC postage; asset
  quantities are not capped, so read the quantity in every review.
- MCP `request_wrap_frbtc` files a fixed BTC-to-frBTC wrap under the
  ordinary grant. The request records the contract's code hash and BTC
  signer; approval verifies and simulates the exact transaction before your
  review. The agent supplies no destination or calldata.
- MCP `alkanes_balance`, `alkanes_inspect`, and `alkanes_simulate` read
  under the connection's pinned network and live grant. Asset amounts and
  calldata use decimal strings.
- frBTC requests use request format 3 and asset transfers format 4, so
  older binaries refuse them rather than read them as BTC payments.

### Agents

- Grants no longer carry a budget, spending ledger, or fee cap. Every agent
  request already waits for you, with its real amount and fee in the
  review, so a grant is now a credential and a scope: expiry, mode, an
  optional per-request amount cap (`--max-tx`), and an optional recipient
  allowlist (`--to`). `sats agent grant` drops `--budget` and `--max-fee`.
- Removed from JSON: `budget_sat`, `spent_sat`, `remaining_sat`,
  `max_fee_sat`, and `tx_count` from MCP `get_grant`, `sats agent list
  --json`, and `sats agent grant --json`; `remaining_budget_sat` from
  `sats agent approve --json`; the denial reasons `over_budget`,
  `over_max_fee`, and `amount_overflow`; and the `reserved` and `refunded`
  events. Older log lines still show raw in `sats agent log`.
- Grants use format version 4. Grants from earlier builds carried a budget
  and are refused with the recreate step; requests filed under them can't
  be approved.
- Approval re-checks the grant under the lock immediately before signing.
  An audit log that can't record the approval stops it before the signer
  exists, leaving the request `failed` and re-approvable.
- Printed MCP setup commands pin the running executable as well as the
  wallet and network, so an older `sats` earlier on PATH can't serve them.

### Signing

- sats signs only transactions it prepared on fresh chain state, with its
  postage and Alkanes exclusions and, for agent requests, the check of what
  the transaction pays.
- Removed `sats send --export-psbt`, `sats psbt inspect`, and
  `sats psbt sign`. Signing a PSBT built elsewhere bypassed those checks.
- Removed `sats alkanes execute`, which called any contract with any
  calldata, gated only by a provider's simulation.
- `sats tx broadcast` rebroadcasts only transactions sats signed and saved;
  it no longer broadcasts raw hex files.

### Providers

- `sats providers add subfrost --url URL` keeps the saved key on enter only
  when the endpoint is unchanged. A new URL, including another network's,
  needs the key typed again, as an Esplora token already did.
- Release builds refuse `file://` provider URLs, the undocumented test
  driver.

## 0.0.3 (2026-10-14)

sats is now a test-network developer release: mainnet is refused. If you
used mainnet with an earlier build, move those funds before upgrading (see
Security).

### Security

- sats is a test-network release: it runs on signet (the default), testnet4,
  and regtest, and refuses mainnet everywhere — `--network`, the config's
  `network`, a `[mainnet]` config section, wallet open and create, provider
  resolution, the agent server, and the local signer. The mainnet restore
  confirmation and mainnet warnings are gone with it. If you used mainnet
  with an earlier build, move those funds with that build before upgrading:
  the seed file is shared by every network. Then start sats from a fresh
  seed and never use it on mainnet.
- The wallet password and mnemonic are read only from a terminal.
  `SATS_PASSWORD` is gone: every program you start, agents included,
  inherits your environment, so an agent with a shell could have run
  `sats agent approve --yes` and signed without you. Restore no longer
  reads a piped phrase.

### Agents

- `sh scripts/demo-mcp.sh` runs the agent request loop end to end offline:
  an MCP session over stdio and the human's CLI, through approval, denial,
  expiry, revocation, and a failed broadcast, on a mock chain. It runs in
  CI as a test.

### Building from source

- The CLI crate is now `crates/sats-cli` (the command is still `sats`):
  `cargo install --locked --path crates/sats-cli`. Storage, providers, and
  the request workflow moved into a new `sats-wallet` library that future
  front ends will share.

## 0.0.2 (2026-10-07)

Provider setup is new, and a 0.0.1 provider config is refused with
instructions: remove its `[providers.<name>]`, `[esplora]`, and
`[fee_targets]` sections and run `sats providers add` again.

### Agents

- Revoking, re-issuing, or expiring a grant now cuts off a running MCP
  session at its next call, reads included. Previously only `request_send`
  re-checked the grant, so `get_balance`, `get_receive_address`, and
  `check_request` kept answering until the session was restarted. Refused
  calls carry `error_code` `no_grant` or `unauthorized`, and `get_grant`
  reports `active: false` without the grant's limits.

### Providers

- Each network has one provider for sync, fees, and broadcast: mempool.space
  (the default), Subfrost, or your own Esplora server. Capability filters
  and split providers are gone.
- `sats providers` shows the network's provider on one line, such as
  `signet  mempool.space (default)`; `--json` prints one object with
  `network`, `provider`, `url`, `auth`, and `source`.
  `sats providers add subfrost`, `add esplora --url URL`, `use`, and
  `remove` set it up. Adding or switching checks the endpoint's network
  before anything is saved, and an API key or token is read from a hidden
  prompt or stdin, never from an argument.
- The config file chooses with `chain` and `fee_target` under
  `[<network>]`, and keeps one Subfrost key under `[subfrost]`. sats writes
  it owner-only (`0600`).
- `--provider KIND=URL` can be given once and replaces the network's
  provider for one command. The saved Subfrost key is never sent to an
  override URL.
- Asset guards are removed: Subfrost's ord and Alkanes guards and
  `sats send --no-guards`. The local 546/330-sat postage check is
  unchanged.

### Alkanes

- `sats alkanes` is no longer in default builds. Build with
  `--features experimental-alkanes` to get `inspect` and `simulate`. They
  need Subfrost as the network's provider.
- `sats alkanes inspect` fetches bytecode through the indexer's
  `getbytecode` view. Subfrost's `alkanes_getbytecode` failed for every
  alkane id.

## 0.0.1 (2026-10-01)

The first release. Experimental; signet by default.

### Wallet

- `sats init` creates a wallet or restores one from a 12- or 24-word
  mnemonic. The seed is sealed with Argon2id and XChaCha20-Poly1305; the
  wallet database is watch-only.
- `receive`, `balance`, `send`, `status`, and `history` on signet, mainnet,
  testnet4, and regtest. Mainnet is always an explicit `--network` choice.
- PSBT export, `sats psbt inspect`, `sats psbt sign`, and
  `sats tx broadcast`.
- Signed transactions are saved before broadcast, so a failed broadcast is
  retried without signing again.
- Fails closed on stale chain data and on an unavailable configured asset
  guard. 546- and 330-sat outputs are not spent by default.
- `--json` output for scripts.

### Agents

- `sats agent grant` gives a named agent a budget, a per-payment cap, a fee
  cap, an optional recipient allowlist, and an expiry. Grants can be listed,
  switched between `ask` and `observe`, and revoked.
- `sats agent serve` runs an MCP server with five tools: `get_balance`,
  `get_receive_address`, `get_grant`, `request_send`, and `check_request`.
  The server holds no key material and cannot approve, sign, or broadcast.
- `sats agent requests`, `approve`, and `dismiss` review and act on agent
  requests. Approval signs exactly the approved payment, once.
- `sats agent log` shows the append-only event log for every request.

### Providers

- mempool.space for chain data by default, with other providers
  configurable.
- Optional asset guards, such as Subfrost for ord and Alkanes, never enabled
  implicitly.

### Elsewhere

- `sats alkanes inspect` and `sats alkanes simulate` (experimental).
- The browser playground at [sats.sh](https://sats.sh) runs `sats-core`
  compiled to WebAssembly against a simulated chain.
