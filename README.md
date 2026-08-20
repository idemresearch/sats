# sats

**Tiny, open Bitcoin wallet.**

A Bitcoin wallet runtime: one core, many surfaces. Run it natively in your
terminal, hand it to an AI agent over MCP, and — next — embed the same core
as WebAssembly. Humans sign locally; agents get bounded spending authority
without ever touching keys.

```
$ sats send tb1p... 25k
Send   25,000 sat
Fee       412 sat
Total  25,412 sat
Sign? [Y/n] y
✓ signed
✓ broadcast  a1b2c3…
```

## Install

```sh
curl -fsSL https://sats.sh/install | sh     # coming soon
cargo install --path crates/sats            # today
```

## The runtime

```
                sats
        Bitcoin wallet runtime
      ┌──────────┬──────────┐
      │          │          │
     CLI        MCP        lib
      │          │          │
      └──────────┼──────────┘
                 │
             sats-core
                 │
       ┌─────────┴─────────┐
     wallet             signing
       │                   │
  PSBT / UTXO      human authorization
```

`crates/sats-core` is the portable engine — wallet ops, transaction plans,
the authorization engine, the `Signer` trait — with no filesystem, network,
or clock dependencies. `crates/sats` is the CLI and MCP server on top.
Agents are downstream of the wallet, not the other way around.

**PSBT in. PSBT out.** Plans are PSBTs at every stage; signing is the
standard watch-only + external-signer flow.

## Humans

```sh
sats init                # create a wallet (signet by default)
sats receive             # fresh address
sats balance
sats send tb1p... 25k    # plan → confirm → sign → broadcast
```

Or step by step: `sats plan tb1p... 25k` → `sats sign` → `sats broadcast`.
Every step is resumable; a failed broadcast leaves a signed plan you can
retry. `sats sign tx.psbt` signs an external PSBT file.

Amounts are integer sats, with shorthand: `25k` = 25,000 · `1.5m` = 1,500,000.

## Agents

Grant an agent a budget, then hand it the MCP server:

```sh
$ sats grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
Grant    claude
Budget   50,000 sat
Max tx   10,000 sat
Max fee  1,000 sat
For      24h
password: ********
✓ granted  claude

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
  "message": "human authorization required: requested 20,000 sat; max tx 10,000 sat" }
```

The user learns one rule: **whenever authority changes, sats asks you.**
`sats grants` shows live budgets; `sats revoke claude` takes effect on the
agent's very next call, even mid-session.

## Commands

| Command | Does |
|---|---|
| `sats init` | Create a wallet (BIP-39 → BIP-86 taproot), encrypted at rest |
| `sats balance` | Sync and show the balance (`--offline` for cached) |
| `sats receive` | Fresh receive address |
| `sats plan <addr> <amount>` | Build an unsigned plan: amount / fee / total |
| `sats send <addr> <amount>` | Plan → confirm → sign → broadcast |
| `sats sign [FILE]` | Sign the newest plan, a `--plan <id>`, or a PSBT file |
| `sats broadcast` | Broadcast the newest signed plan (or `--tx <hex-file>`) |
| `sats grant <agent>` | Grant a spending budget (`--budget --for --max-tx --max-fee`) |
| `sats revoke <agent>` | Revoke a grant immediately |
| `sats grants` | List active grants and remaining budgets |
| `sats mcp --agent <name>` | Serve wallet tools to that agent over MCP stdio |

Global flags: `--network mainnet|signet|testnet4|regtest`, `--json` on read
commands. `SATS_PASSWORD` replaces the prompt for scripting; `SATS_DIR`
relocates all state.

## Trust model, honestly

- Your seed lives in one file, sealed with argon2id + XChaCha20-Poly1305.
  The wallet database is watch-only — it never contains keys.
- `sats grant` unseals the seed once (your password is the authorization)
  and re-seals it under a fresh random key stored in the grant file (0600).
  Your password is never given to the agent.
- Budget/caps/expiry enforcement is deterministic and happens before any
  signature exists. Revocation deletes the grant file — nothing survives it.
- **The honest cost of unattended signing:** while a grant is active, an
  attacker who can read that grant file as your user can extract the seed.
  The boundaries during a grant are OS file permissions and the expiry
  window. Keep budgets small and expiries short; hardware-backed signers
  (the `Signer` trait is already there) close this gap properly.

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

## Next

The runtime grows outward from the same core: a bare `sats` wallet shell,
`sats psbt inspect/create/finalize` as composable primitives,
`sats-core.wasm` + `@sats/core` for browsers and apps, and hardware/passkey
`Signer` backends.

**Not planned for V1:** Lightning, coin control, multi-wallet, RBF.
