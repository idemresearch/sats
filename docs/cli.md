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
sats status
```

Signet is the default. `init` prints the new mnemonic once; back it up before
continuing. Mainnet requires `--network mainnet` explicitly.

`init` is the single setup entry point: run bare on a terminal it asks
whether to create a new wallet or restore one from a mnemonic backup;
`--restore` and `--words` skip the question.

## Restoring a wallet

```sh
sats init --restore [--network <NET>]
```

Restore consumes the mnemonic backup and rebuilds everything else: the
phrase is entered with hidden input (never as a CLI argument), validated
against the BIP-39 checksum before anything touches disk, sealed with a new
password, and the first sync rediscovers the wallet's history on chain via a
full scan. A parse failure names the mistyped word position when it can;
nothing is stored until the phrase validates.

Behavior to know before restoring:

- Restore requires an empty data directory. A machine that already holds a
  seed extends to new networks through plain `sats init` instead.
- sats derives BIP-86 taproot addresses only (`bc1p…`/`tb1p…`). A phrase
  from a wallet holding funds on other address types restores successfully
  but shows those coins as invisible, not gone.
- The wallet restores onto the selected network — signet unless
  `--network mainnet` is given.
- Mainnet restore turns the backup into hot key material that agent grants
  can draw on, so it requires typing an explicit confirmation phrase. To
  only watch funds, do not restore a seed onto an online machine.
- Local records from the old machine — grants, finalized transaction hex,
  PSBT sessions — are not part of the seed and do not come back; on-chain
  history does.
- Non-interactive use reads the phrase from stdin, for scripted recovery.

## Commands

Everyday commands express intent; the `psbt`, `tx`, and `agent` namespaces
hold the explicit advanced workflows.

| Command | Behavior |
|---|---|
| `sats init [--words 12\|24] [--restore]` | Set up the wallet for the selected network: create a new sealed seed, or restore one from a mnemonic backup |
| `sats balance [--offline]` | Sync and show confirmed/trusted and pending balances; `--offline` uses cached state |
| `sats receive` | Reveal and persist the next external receive address |
| `sats send <address> <amount>` | Prepare, confirm, sign, privately persist raw finalized transaction hex, then broadcast |
| `sats send ... --dry-run` | Prepare and price the send, persist nothing |
| `sats send ... --export-psbt <FILE>` | Write the unsigned PSBT to a private file artifact instead of signing |
| `sats status [TXID] [--offline]` | Show signed-but-unbroadcast and broadcast transactions with confirmation state |
| `sats history [--offline]` | List the wallet's transactions, newest first |
| `sats psbt inspect <FILE>` | Decode a PSBT file offline: outputs, fee, signing state |
| `sats psbt sign <FILE> [--out FILE]` | Sign an explicit PSBT artifact |
| `sats tx broadcast <FILE\|TXID>` | Broadcast a raw hex file, or a saved transaction by txid/prefix/id |
| `sats agent grant <name>` | Create bounded unattended signing authority |
| `sats agent revoke <name>` | Delete an agent grant immediately |
| `sats agent list` | List non-expired grants and remaining budgets |
| `sats agent requests [--all]` | Review agent send requests; denied ones await a human decision |
| `sats agent approve <id>` | Authorize one denied request exactly once (password required) |
| `sats agent deny <id>` | Dismiss a request and revoke its unconsumed approval |
| `sats agent log [--limit N] [--request ID]` | Show the causal event log of agent activity |
| `sats agent serve <name>` | Serve the four wallet tools for one granted agent over MCP stdio |
| `sats alkanes inspect <BLOCK:TX>` | Fetch a contract's bytecode and show its sha256 code hash |
| `sats alkanes simulate <BLOCK:TX> <INPUTS...>` | Simulate a contract call and show the interpreted result |

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

## Sending

```sh
sats send <address> <amount> [--fee-rate <SAT_VB>] \
  [--allow-dust] [--no-guards] [--yes | --dry-run | --export-psbt <FILE>]
```

Every send — human or agent — runs the shared preparation path:

1. validates the address against the selected network;
2. syncs the watch-only wallet;
3. excludes common inscription postage outputs unless `--allow-dust`;
4. queries and unions configured guards unless `--no-guards`;
5. estimates a roughly two-block fee unless `--fee-rate` is supplied;
6. builds the unsigned PSBT in memory.

Sync or configured-guard failure stops preparation. Both bypass flags apply only
to the current human invocation and are intentionally absent from MCP sends.

The three send modes:

- **Default**: shows the priced spend, asks for confirmation, signs, privately
  persists raw finalized transaction hex, then broadcasts. `--yes` skips the
  confirmation prompt but does not bypass UTXO safety, provider validation,
  password unlocking, or any agent authorization rule.
- **`--dry-run`**: prints or returns the priced spend and persists nothing —
  no PSBT, no transaction record, no wallet-state change, and no password
  prompt.
- **`--export-psbt <FILE>`**: writes the unsigned PSBT to `FILE` as an
  owner-only artifact and signs nothing. The change address it reserves is
  persisted so the artifact stays valid.

If broadcast fails after signing, the transaction is already saved:
`sats status` lists it and `sats tx broadcast <txid>` retries it.

## Transaction visibility

```sh
sats status                # pending (signed, unbroadcast) + broadcast with confirmations
sats status <txid>         # one transaction by txid, unique prefix, or session id
sats history               # every wallet transaction, unconfirmed first
```

Both commands sync first and tolerate sync failure with a stderr warning;
`--offline` skips sync entirely. `status <txid>` falls back to the wallet's
canonical chain view for transactions the store never saw, such as incoming
payments.

## Explicit PSBT workflow

Normal `send` never persists a PSBT. The staged lifecycle works on explicit
file artifacts:

```sh
sats send tb1p... 25k --export-psbt spend.psbt
sats psbt inspect spend.psbt
sats psbt sign spend.psbt
sats tx broadcast <txid>
```

`psbt sign FILE` accepts base64 text or binary. When the wallet's signature
finalizes the transaction, sats privately saves raw finalized transaction hex
(ready for `sats tx broadcast`); it does not retain the signed PSBT. A PSBT
that still needs other signers is written back beside the input as
`<name>.signed.psbt` and reported as partially signed. `--out <FILE>` always
writes the signed PSBT to `FILE` and persists nothing — the pure artifact
path for multi-signer flows.

`psbt sign --session <id>` signs a stored PSBT session or pre-refactor plan
from an older release. The id is always explicit: signing never consumes
hidden internal state. New sats versions no longer write stored sessions.

`tx broadcast` takes exactly one target: an existing file is read as raw
transaction hex; anything else resolves a saved transaction by full txid,
unique prefix, or the session id that produced it.

## Agent grants

```sh
sats agent grant <name> --budget <SATS> [--for <DURATION>] \
  [--max-tx <SATS>] [--max-fee <SATS>]
```

`--for` defaults to `24h` and accepts human-readable durations such as `30m`,
`24h`, and `7d`. Agent names are 1–32 lowercase letters, digits, hyphens, or
underscores.

Budget is amount plus fee. `--max-tx` applies to recipient amount only and
`--max-fee` applies to fee only. Grant creation requires the wallet password.

```sh
sats agent list
sats agent revoke claude
```

Expired grants are removed while listing. Revocation deletes the grant file;
an active MCP server observes the deletion on its next send call.

`sats agent serve <name>` runs the MCP server as that agent. See
[MCP and agent grants](mcp.md) for the tool-level contract.

## Reviewing agent activity

Every agent send is recorded as a durable request, and every state
transition appends to a per-network event log:

```sh
sats agent requests            # pending: denied requests awaiting a decision
sats agent requests --all      # every recorded request, newest first
sats agent log                 # the causal event chain, oldest first
sats agent log --request k-big-1
```

`requests` shows each request's id, agent, recipient, amount, outcome,
age, and approval state; `--json` returns the full records. `log` renders
one line per event — received, denied, reserved, signed, broadcast,
refunded, replayed — and `--request` accepts an id or unique prefix.
Unreadable records and torn log lines are skipped with a warning; both
surfaces are purely local and never touch a provider.

## Approving one request

```sh
sats agent approve <id> [--max-fee <SATS>] [--for <DURATION>]
sats agent deny <id>
```

`approve` turns one denied request into a single-use exception bound to
exactly the intent the denial recorded — same agent, recipient, and
amount. It shows the full recipient and amounts, then requires the wallet
password: the prompt is the authorization, as with grant creation. The
approval carries a fee ceiling — `--max-fee`, defaulting to twice the fee
the denial recorded when one exists — and a lifetime (`--for`, default
`1h`). The agent's next matching send consumes it; a consumed approval
never authorizes a second signature, and approvals never survive grant
revocation or expiry. `deny` dismisses the request and revokes an
unconsumed approval without a password: reducing authority stays cheap.
Both accept a request id or unique prefix and support `--json`.

## Alkanes contract tools

The `sats alkanes` namespace is an experimental, signet-first Alkanes
client over the `alkanes.view` provider capability (currently the
Subfrost driver; the JSON-RPC dialect has not been verified against a
live endpoint and is confined to the driver so corrections stay local):

```sh
sats alkanes inspect 2:1          # bytecode size + sha256 code hash
sats alkanes simulate 2:1 77      # advisory call simulation
```

`inspect` fetches the contract bytecode and prints its sha256 code hash
for comparison against a build you trust. `simulate` runs a call against
the endpoint's view and shows the recognized fields (status, gas, asset
transfers) alongside the verbatim result — it is advisory display, never
an authorization. Both are read-only, validate the endpoint's network
first, and require an explicitly configured `alkanes.view` provider:
there is no fallback, and without one they fail with a typed error.

## JSON output

`--json` is implemented for:

- `balance`;
- `receive`;
- `send` (all three modes);
- `status`;
- `history`;
- `psbt inspect`;
- `psbt sign`;
- `tx broadcast`;
- `agent grant`;
- `agent revoke`;
- `agent list`;
- `agent requests`;
- `agent approve`;
- `agent deny`;
- `agent log`;
- `alkanes inspect`;
- `alkanes simulate`.

JSON field names are compatibility surfaces. Scripts should branch on
documented status and reason fields rather than human-readable messages.

`init` remains an interactive recovery-material flow and deliberately has no
JSON mode: emitting the mnemonic on a machine-readable stream invites
accidental capture.

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

Command failures print a diagnostic to stderr and exit non-zero. Balance,
status, and history are the intentionally tolerant chain-read commands: when
sync fails without `--offline`, they report cached state with a stderr
warning (`synced: false` for balance). Planning, signing, provider
validation, and broadcast failures remain hard failures.
