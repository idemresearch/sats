# Changelog

Notable changes to sats, newest first. Versions follow
`workspace.package.version` in `Cargo.toml`.

## Unreleased

### Security

- The wallet password and mnemonic are read only from a terminal.
  `SATS_PASSWORD` is refused with instructions to unset it, and restore no
  longer reads a piped phrase. Any program you start, agents included,
  inherits your environment. With `SATS_PASSWORD` set, an agent with a
  shell could run `sats agent approve --yes` and sign without you.
  Scripts that relied on either must now run at a terminal.

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
