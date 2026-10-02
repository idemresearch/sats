# Changelog

Notable changes to sats, newest first. Versions follow
`workspace.package.version` in `Cargo.toml`.

## 0.0.2 (unreleased)

### Agents

- Revoking, re-issuing, or expiring a grant now cuts off a running MCP
  session at its next call, reads included. Previously only `request_send`
  re-checked the grant, so `get_balance`, `get_receive_address`, and
  `check_request` kept answering until the session was restarted. Refused
  calls carry `error_code` `no_grant` or `unauthorized`, and `get_grant`
  reports `active: false` without the grant's limits.

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
