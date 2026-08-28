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
| `sats daemon run [--auto-lock D]` | Run the signing daemon in the foreground (for a supervisor) |
| `sats daemon start [--auto-lock D]` | Start the signing daemon in the background |
| `sats daemon install [--auto-lock D]` | macOS: install and start a locked user service with login/crash supervision |
| `sats daemon uninstall` | macOS: stop and remove the matching service, preserving wallet data |
| `sats daemon status` | Show whether the daemon is running, and whether it can sign |
| `sats daemon unlock` | Unseal the wallet into the daemon so agent sends can be signed |
| `sats daemon lock` | Drop the seed from the daemon's memory, without stopping it |
| `sats daemon stop` | Stop the daemon |
| `sats agent grant <name>` | Create bounded unattended signing authority |
| `sats agent revoke <name>` | Delete an agent grant immediately |
| `sats agent list` | List non-expired grants and remaining budgets |
| `sats agent requests [--all]` | Review agent send requests; denied ones await a human decision |
| `sats agent approve <id>` | Authorize one denied request exactly once (password required) |
| `sats agent deny <id>` | Dismiss a request and revoke its unconsumed approval |
| `sats agent log [--limit N] [--request ID]` | Show the causal event log of agent activity |
| `sats agent serve <name>` | Serve five wallet tools for one granted agent over MCP stdio |
| `sats alkanes inspect <BLOCK:TX>` | Fetch a contract's bytecode and show its sha256 code hash |
| `sats alkanes simulate <BLOCK:TX> <INPUTS...>` | Simulate a contract call and show the interpreted result |
| `sats alkanes execute <BLOCK:TX> <INPUTS...>` | Simulate, confirm, sign, and broadcast a contract call (refuses mainnet) |

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

## The signing daemon

Agent sends are signed by `satsd`, a local per-network process that holds the
seed in memory. The served MCP process holds only a bearer token, so nothing
an agent can read can produce a signature.

```sh
sats daemon start          # background; logs to <network>/satsd.log
sats daemon unlock         # password prompt, or SATS_PASSWORD
sats daemon status
```

The daemon starts **locked** and signs nothing until a human unlocks it.
On macOS, an agent can also request a local password dialog with the MCP
`request_unlock` tool after you agree. Enter the password in that dialog,
never in chat. The dialog is launched by satsd and identifies the agent,
wallet, network, and idle timeout. It enables all active grants for that
daemon within their existing limits; it does not approve or send a payment.
No terminal command is required for this path, and `SATS_PASSWORD` is ignored.
Cancel closes the request; the agent must not automatically prompt again.
The terminal `sats daemon unlock` command remains available on every platform.

`--auto-lock` (default `8h`) drops the seed after that much inactivity;
`sats daemon lock` drops it immediately without stopping the process. While
locked, agent sends return the typed error code `wallet_locked` rather than a
policy denial.

Repeated failed unlocks throttle: the first three misses are free, then
each further miss doubles a required wait (capped at one minute) during
which every attempt — right password included — is refused with the typed
code `unlock_throttled` and the remaining wait.

For reliable operation on macOS, opt into a per-user launchd service:

```sh
sats daemon install --auto-lock 8h
sats daemon unlock
# To remove the service, preserving all wallet state:
sats daemon uninstall
```

Installation requires a logged-in macOS GUI session, not root. It records the
absolute executable path, network, wallet directory and required socket setting
in a private `~/Library/LaunchAgents/sh.sats.satsd.<network>.<id>.plist`.
No password or agent token is recorded. Keep the binary at that path; after
moving it, stop the service and reinstall using the new binary.

The service starts **locked** at login and restarts **locked** after unexpected
failure. `daemon stop` exits cleanly and remains stopped until `daemon start`
or the next login. Installation with identical settings is idempotent; changing
a running service's settings or replacing an unmanaged daemon requires an
explicit stop first. `daemon start` uses the installed service and its configured
auto-lock duration; a conflicting explicit `--auto-lock` is refused. Reinstall
after stopping to change that duration. `daemon uninstall` removes only the
matching wallet/network service, not wallet data, grants, or logs.

Without an installed service, `daemon start` retains its session-dependent
background behavior. On Linux, run `sats daemon run` under your own supervisor;
managed install/uninstall are macOS-only. Installing or starting a service
does not grant authority or unlock the wallet. Closing Claude does not stop a
managed daemon, but an in-flight MCP send is not a durable background job.

The socket is `$XDG_RUNTIME_DIR/sats/<network>.sock`, mode 0600, or
`<network>/d.sock` under `SATS_DIR`. One daemon serves one network; a second
on the same socket is refused. A private lifetime lock prevents concurrent
starts from racing during stale-socket recovery.

Human commands never use the daemon. `sats send`, `sats psbt sign`, and
`sats init` unseal the seed for the duration of one command, as before.

## Agent grants

```sh
sats agent grant <name> --budget <SATS> [--for <DURATION>] \
  [--max-tx <SATS>] [--max-fee <SATS> | --no-max-fee]
```

`--for` defaults to `24h` and accepts human-readable durations such as `30m`,
`24h`, and `7d`. Agent names are 1–32 lowercase letters, digits, hyphens, or
underscores.

Budget is amount plus fee. `--max-tx` applies to recipient amount only and
`--max-fee` applies to fee only. When `--max-fee` is not given, the grant
gets a default fee cap of 2% of the budget — at least 1000 sats, never above
the budget — so a single bad fee estimate cannot burn the whole budget as
miner fees; `--no-max-fee` issues a grant without any fee cap. Grant
creation requires the wallet password, which authorizes the grant and is not
otherwise used: the grant file holds a budget and a token hash, never key
material.

Creating a grant prints a bearer token **once**. It is not stored — only its
SHA-256 is — so there is no way to recover it later; re-issue the grant to
mint a new one. Pass it to the served process as `SATS_AGENT_TOKEN`:

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
# SATS_AGENT_TOKEN=<token>
claude mcp add sats --env SATS_AGENT_TOKEN=<token> -- sats agent serve claude
```

```sh
sats agent list
sats agent revoke claude
```

Expired grants are removed while listing. Revocation deletes the grant file;
an active MCP server observes the deletion on its next send call. Re-issuing
a grant replaces its token, so rotation and revocation are the same act.

Grants written by releases before the daemon stored recoverable signing
material in the grant file. They are reported by `sats agent list` and
refused for signing; re-issue them, and read
[Security](security.md) about rotating the wallet.

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

### Executing a call

```sh
sats alkanes execute <BLOCK:TX> <INPUTS...> [--fee-rate <SAT_VB>] \
  [--postage <SATS>] [-y]
```

`execute` composes the call transaction — output 0 the runestone
OP_RETURN, output 1 a postage output (default 546 sats) back to the
wallet that the protostone's pointer and refund both target — then shows
the simulation, postage, fee, and total, asks for confirmation, signs
with the wallet password, privately persists the raw transaction, and
broadcasts. The pipeline fails closed at every step: an unavailable
simulation or guard stops it, sync failure stops it, and there are no
`--allow-dust`/`--no-guards` escapes on this command at all. It refuses
mainnet in this release — the encoding is young; dogfood on signet. The
546-sat postage lands on a wallet address at a value the dust heuristic
protects from later coin selection automatically.

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
- `daemon start`, `daemon status`, `daemon unlock`, `daemon lock`, `daemon stop`;
- `daemon install`, `daemon uninstall` (macOS);
- `agent grant`;
- `agent revoke`;
- `agent list`;
- `agent requests`;
- `agent approve`;
- `agent deny`;
- `agent log`;
- `alkanes inspect`;
- `alkanes simulate`;
- `alkanes execute`.

JSON field names are compatibility surfaces. Scripts should branch on
documented status and reason fields rather than human-readable messages.

Two shapes are worth calling out. `agent grant --json` includes `token`, the
only time it is ever emitted; treat that output as a secret. `daemon status
--json` reports `{"running": false}` and exits 0 when no daemon is running,
so a script can check without treating absence as failure — the text form
fails instead, because a human asking for status wants to be told.

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
