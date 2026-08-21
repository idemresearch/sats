# CLI reference

The `sats` CLI manages one sealed seed and a watch-only wallet per Bitcoin
network. Run `sats --help` or `sats <command> --help` for the exact option
list supported by the installed version.

## Quickstart

```sh
sats init
sats receive
# Fund the printed address from a signet faucet, then:
sats balance
sats send tb1p... 25k
```

Signet is the default. `init` prints the new mnemonic once; back it up before
continuing. Mainnet requires `--network mainnet` explicitly.

## Commands

| Command | Behavior |
|---|---|
| `sats init [--words 12|24]` | Create the sealed seed and the watch-only wallet for the selected network |
| `sats balance [--offline]` | Sync and show confirmed/trusted and pending balances; `--offline` uses cached state |
| `sats receive` | Reveal and persist the next external receive address |
| `sats plan <address> <amount>` | Explicitly create a private, resumable unsigned PSBT session |
| `sats send <address> <amount>` | Prepare, confirm, sign, privately persist raw finalized transaction hex, then broadcast |
| `sats sign [FILE]` | Finalize the newest saved PSBT session or sign an external base64/binary PSBT |
| `sats broadcast` | Broadcast the newest pending finalized transaction or a raw transaction file |
| `sats grant <agent>` | Create bounded unattended signing authority |
| `sats revoke <agent>` | Delete an agent grant immediately |
| `sats grants` | List non-expired grants and remaining budgets |
| `sats mcp --agent <name>` | Serve the four wallet tools for one granted agent over MCP stdio |

## Global flags

Global flags select the environment for one invocation:

| Flag | Meaning |
|---|---|
| `--network <NET>` | `mainnet`, `signet`, `testnet4`, or `regtest`; overrides config |
| `--provider <KIND=URL>` | Replace configured providers for this invocation; repeatable |
| `--json` | Use machine-readable output where the command exposes it |

`SATS_DIR` relocates configuration and data under one directory. The
corresponding `--dir` flag exists for internal/testing use and is hidden from
help. `SATS_PASSWORD` supplies the wallet password non-interactively; avoid
putting it in shell history or process-inspection surfaces.

## Amounts

Amounts are integer satoshis. Case-insensitive suffixes are accepted:

- `25k` = 25,000 sats;
- `1.5m` = 1,500,000 sats.

Fractional shorthand must resolve to a whole satoshi. Plain values such as
`25000` are interpreted directly as sats.

## Preparation and sending

```sh
sats plan <address> <amount> [--fee-rate <SAT_VB>] \
  [--allow-dust] [--no-guards]

sats send <address> <amount> [--fee-rate <SAT_VB>] \
  [--allow-dust] [--no-guards] [--yes]
```

The shared preparation path:

1. validates the address against the selected network;
2. syncs the watch-only wallet;
3. excludes common inscription postage outputs unless `--allow-dust`;
4. queries and unions configured guards unless `--no-guards`;
5. estimates a roughly two-block fee unless `--fee-rate` is supplied;
6. builds the unsigned PSBT in memory.

Sync or configured-guard failure stops preparation. Both bypass flags apply only
to the current human invocation and are intentionally absent from MCP sends.

`send --yes` skips the confirmation prompt but does not bypass UTXO safety,
provider validation, password unlocking, or any agent authorization rule.

## Explicit PSBT workflow

Normal `send` does not persist a PSBT. `plan` explicitly creates a private
unsigned session, and the newest session is used when no ID is supplied:

```sh
sats plan tb1p... 25k
sats sign
sats broadcast
```

Select a specific saved session:

```sh
sats sign --plan <id>
```

After successful signing, sats privately saves raw finalized transaction hex
and removes the PSBT session. Broadcast the newest pending transaction, or
select one by full/unique-prefix txid:

```sh
sats broadcast
sats broadcast --transaction <txid>
```

`sats broadcast --plan <id>` remains a compatibility alias for records
created from a session and migrates pre-refactor signed plan files on use.

Sign an external PSBT:

```sh
sats sign transaction.psbt
```

The input may be base64 text or binary. Output is written beside it as
`transaction.signed.psbt`. A PSBT that still needs other signers is reported
as partially signed.

Broadcast a raw transaction hex file:

```sh
sats broadcast --tx transaction.hex
```

Finalized transaction state is written before broadcast. If broadcast fails,
the pending raw transaction can be retried with `sats broadcast`; sats does
not retain the signed PSBT.

## Agent grants

```sh
sats grant <agent> --budget <SATS> [--for <DURATION>] \
  [--max-tx <SATS>] [--max-fee <SATS>]
```

`--for` defaults to `24h` and accepts human-readable durations such as `30m`,
`24h`, and `7d`. Agent names are 1–32 lowercase letters, digits, hyphens, or
underscores.

Budget is amount plus fee. `--max-tx` applies to recipient amount only and
`--max-fee` applies to fee only. Grant creation requires the wallet password.

```sh
sats grants
sats revoke claude
```

Expired grants are removed while listing. Revocation deletes the grant file;
an active MCP server observes the deletion on its next send call.

See [MCP and agent grants](mcp.md) for the tool-level contract.

## JSON output

`--json` is implemented for:

- `balance`;
- `receive`;
- `plan`;
- `send`;
- saved-session `sign`;
- `broadcast`;
- `grant`;
- `revoke`;
- `grants`.

JSON field names are compatibility surfaces. Scripts should branch on
documented status and reason fields rather than human-readable messages.

`init` remains an interactive recovery-material flow. External-file signing
writes a file and reports its path rather than returning a JSON document.

## Configuration

The default configuration path is the platform-specific XDG config directory
for sats. A minimal configuration is:

```toml
network = "signet"
```

Typed providers are configured under `[providers.<name>]`:

```toml
network = "mainnet"

[providers.subfrost]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
capabilities = ["chain", "guard"]
```

See [Providers and guards](providers.md) for drivers, capability filters,
precedence, authentication, and command-line overrides.

## Exit and failure behavior

Command failures print a diagnostic to stderr and exit non-zero. Balance is
the one intentionally tolerant chain-read command: when sync fails without
`--offline`, it reports the cached balance with `synced: false`. Planning,
signing, provider validation, and broadcast failures remain hard failures.
