# sats

**Bitcoin signing for humans and agents.**

A small, sharp Bitcoin wallet CLI. Plan a payment, see exactly what it costs,
approve it, done — and grant AI agents spending budgets that are enforced in
code, not in prompts.

```
$ sats send tb1p... 25000
Send   25,000 sats
Fee       412 sats
Total  25,412 sats
Sign? [Y/n] y
✓ signed
✓ broadcast  a1b2c3…
```

## Install

```sh
curl -fsSL https://sats.sh/install | sh     # coming soon
cargo install --path crates/sats            # today
```

## Humans

```sh
sats init                  # create a wallet (signet by default)
sats receive               # fresh address
sats balance
sats send tb1p... 25000    # plan → confirm → sign → broadcast
```

Or step by step: `sats plan tb1p... 25000` → `sats sign` → `sats broadcast`.
Every step is resumable; a failed broadcast leaves a signed plan you can retry.

## Agents

Authorize an agent with a budget, then hand it the MCP server:

```sh
$ sats authorize claude --budget 50000 --expires 24h --max-tx 10000 --max-fee 1000
Agent    claude
Budget   50,000 sats
Max tx   10,000 sats
Max fee  1,000 sats
Expires  in 1d
password: ********
✓ authorized  claude

$ claude mcp add sats -- sats mcp --agent claude
```

The agent gets four tools: `get_balance`, `get_receive_address`, `get_grant`,
and `send`. Every `send` is checked deterministically against the grant —
budget (amount + fee), per-tx cap, fee cap, expiry:

```json
{ "status": "sent", "txid": "…", "fee_sat": 281, "remaining_budget_sat": 45219 }
```

Outside its authority, the agent gets a refusal, not a signature:

```json
{ "status": "denied", "reason": "over_max_tx",
  "message": "human authorization required: requested 20,000 sats; max tx 10,000 sats" }
```

`sats grants` shows live budgets; `sats revoke claude` takes effect on the
agent's very next call, even mid-session.

## Commands

| Command | Does |
|---|---|
| `sats init` | Create a wallet (BIP-39 → BIP-86 taproot), encrypted at rest |
| `sats balance` | Sync and show the balance (`--offline` for cached) |
| `sats receive` | Fresh receive address |
| `sats plan <addr> <sats>` | Build an unsigned plan: amount / fee / total |
| `sats send <addr> <sats>` | Plan → confirm → sign → broadcast |
| `sats sign` | Sign the newest plan (or `--plan <id>`, `--psbt <file>`) |
| `sats broadcast` | Broadcast the newest signed plan (or `--tx <hex-file>`) |
| `sats authorize <agent>` | Grant a spending budget (`--budget --expires --max-tx --max-fee`) |
| `sats revoke <agent>` | Revoke a grant immediately |
| `sats grants` | List active grants and remaining budgets |
| `sats mcp --agent <name>` | Serve wallet tools to that agent over MCP stdio |

Global flags: `--network mainnet|signet|testnet4|regtest`, `--json` on read
commands. `SATS_PASSWORD` replaces the prompt for scripting; `SATS_DIR`
relocates all state.

## Trust model, honestly

- Your seed lives in one file, sealed with argon2id + XChaCha20-Poly1305.
  The wallet database is watch-only — it never contains keys.
- `sats authorize` unseals the seed once (your password is the
  authorization) and re-seals it under a fresh random key stored in the
  grant file (0600). Your password is never given to the agent.
- Budget/caps/expiry enforcement is deterministic and happens before any
  signature exists. Revocation deletes the grant file — nothing survives it.
- **The honest cost of unattended signing:** while a grant is active, an
  attacker who can read that grant file as your user can extract the seed.
  The boundaries during a grant are OS file permissions and the expiry
  window. Keep budgets small and expiries short; hardware-backed signers
  (the `Signer` trait is already there) close this gap later.

## Networks

Signet is the default — grab coins from a signet faucet and try the whole
loop for free. Mainnet is an explicit choice: `sats init --network mainnet`.
Wallets are namespaced per network and share one seed. Chain access is any
Esplora endpoint (`mempool.space` by default, configurable in
`~/.config/sats/config.toml`).

## Development

```sh
cargo test --workspace                  # offline: unit + CLI + MCP smoke tests
cargo clippy --workspace --all-targets  # lint
```

Workspace layout: `crates/sats-core` is the portable engine — wallet ops,
plans, the authorization engine, the `Signer` trait — with no filesystem,
network, or clock dependencies (a future WASM target). `crates/sats` is the
CLI and MCP server on top.

## Not yet

Lightning, coin control, multi-wallet, hardware signers, passkeys, RBF.
V1 does one thing: human-authorized Bitcoin signing that also works for
agents, anywhere.
