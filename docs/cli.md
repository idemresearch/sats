# CLI reference

Every sats command, flag, and setting. Run `sats <command> --help` for the
exact options in your installed version.

## Commands

| Command | Does |
|---|---|
| `sats init [--words 12\|24] [--restore]` | Create a wallet, or restore one from a mnemonic backup |
| `sats balance [--offline]` | Sync and show confirmed and pending balance |
| `sats receive` | Reveal and save the next receive address |
| `sats send <address> <amount>` | Prepare, confirm, sign, save, and broadcast a payment |
| `sats status [TXID] [--offline]` | Show saved transactions and their chain status |
| `sats history [--offline]` | List the wallet's transactions, newest first |
| `sats psbt inspect <FILE>` | Decode a PSBT offline: outputs, fee, signing state |
| `sats psbt sign <FILE> [--out FILE]` | Sign a PSBT file |
| `sats tx broadcast <FILE\|TXID>` | Broadcast a raw hex file or a saved transaction |
| `sats agent grant <name>` | Give an agent a budget to propose sends |
| `sats agent mode <name> <ask\|observe>` | Switch a grant's mode |
| `sats agent allow <name> <address>` | Add a recipient to a grant's allowlist |
| `sats agent disallow <name> <address>` | Remove a recipient from a grant's allowlist |
| `sats agent list` | List active grants and remaining budgets |
| `sats agent revoke <name>` | Delete a grant immediately |
| `sats agent requests [--all\|--watch]` | List requests that need attention, or stream new ones |
| `sats agent approve [ID] [--yes]` | Review, authorize, sign, and broadcast one request |
| `sats agent dismiss <ID>` | Decline a request |
| `sats agent log [--limit N] [--request ID]` | Show the agent event log |
| `sats agent serve <name>` | Run the MCP server for an agent ([MCP](mcp.md)) |
| `sats providers [list]` | Show which provider serves each capability |
| `sats providers login <subfrost\|esplora>` | Set up a provider, asking for its API key |
| `sats providers logout <NAME>` | Remove a provider and its stored key |
| `sats alkanes inspect <BLOCK:TX>` | Show a contract's bytecode hash (experimental) |
| `sats alkanes simulate <BLOCK:TX> <INPUTS...>` | Simulate a contract call (experimental) |

## Global options

| Option | Meaning |
|---|---|
| `--network <NET>` | `mainnet`, `signet`, `testnet4`, or `regtest`. Overrides the config. |
| `--provider <KIND=URL>` | Replace configured providers for this run. Repeatable. See [Providers](providers.md#one-off-overrides). |
| `--json` | Machine-readable output, where supported |

| Variable | Meaning |
|---|---|
| `SATS_DIR` | Put all configuration and data under one directory. Useful for test wallets. |
| `SATS_PASSWORD` | Supply the wallet password non-interactively. Keep it out of shell history. |

**Amounts** are whole satoshis. `25000`, `25k`, and `1.5m` (1,500,000) all
work. Suffixes are case-insensitive, and shorthand must resolve to a whole
sat.

## Setting up

```sh
sats init                     # asks: create a new wallet, or restore one
sats init --words 24          # create, with a 24-word mnemonic
sats init --restore           # restore from a mnemonic backup
sats --network mainnet init   # mainnet is always explicit
```

`init` shows a new mnemonic once. Write it down before you continue. One seed
serves every network: on a machine that already has a seed, `init` with
another `--network` adds that network's wallet. If a wallet already exists,
use `sats balance` or `sats receive`. For a separate test wallet, point
`SATS_DIR` at an empty directory.

### Restoring

`sats init --restore` reads the phrase with hidden input, never as an
argument. It validates the BIP-39 checksum before writing anything, names the
mistyped word when it can, seals the seed with a new password, and rescans the
chain on first sync.

- Restore needs an empty data directory.
- sats derives BIP-86 Taproot addresses only (`bc1p…`, `tb1p…`). Coins on
  other address types won't appear, but they aren't lost.
- Mainnet restore puts the backup online as a hot key, so it asks you to type
  a confirmation phrase. To only watch funds, don't restore the seed on an
  online machine.
- Grants, saved transactions, and agent requests are local records. They
  don't come back with the seed; on-chain history does.
- Without a terminal, restore reads the phrase from stdin.

## Sending

```sh
sats send <address> <amount> [--fee-rate <SAT_VB>] [--allow-dust] [--no-guards]
          [--yes | --dry-run | --export-psbt <FILE>]
```

Every send, human or agent, takes the same path:

1. Validate the address for the selected network.
2. Sync the wallet. An empty wallet stops here with funding guidance.
3. Exclude 546- and 330-sat outputs, which may carry inscriptions, unless
   you pass `--allow-dust`.
4. Exclude outputs that configured asset guards protect, unless you pass
   `--no-guards`. If every output is protected, stop and say so.
5. Estimate the fee for the configured target (2 blocks by default), unless
   you pass `--fee-rate`.
6. Build the unsigned PSBT in memory.

A sync failure or an unavailable guard stops the send. `--allow-dust` and
`--no-guards` apply only to that one command and are never available to
agents.

| Mode | Behavior |
|---|---|
| default | Show amount and fee, ask to confirm, sign, save the transaction, broadcast |
| `--yes` | Skip the confirmation. Password, safety checks, and grant rules still apply. |
| `--dry-run` | Price the send and change nothing. No password needed. |
| `--export-psbt <FILE>` | Write the unsigned PSBT to an owner-only file and sign nothing. The file's directory must exist, and sats leaves its permissions alone. The reserved change address is saved so the PSBT stays valid. |

If a broadcast fails after signing, the transaction is already saved.
`sats status` lists it, and `sats tx broadcast <txid>` retries it.

## Status and history

```sh
sats status           # saved transactions and their chain status
sats status <txid>    # one transaction, by txid or unique prefix
sats history          # every wallet transaction, newest first
```

Both sync first. If sync fails, they show cached data with a warning. Use
`--offline` to skip sync. `status <txid>` also finds transactions sats didn't
create, such as incoming payments. A transaction whose broadcast wasn't
confirmed shows as "signed; broadcast unconfirmed", alongside whatever the
chain reports for it. In JSON, `sync_status` is `fresh`, `offline`, or
`failed`.

## PSBT workflow

A normal `send` never writes a PSBT. To work step by step:

```sh
sats send tb1p... 25k --export-psbt spend.psbt
sats psbt inspect spend.psbt
sats psbt sign spend.psbt
sats tx broadcast <txid>
```

`psbt sign` accepts base64 or binary PSBTs, including ones from other
wallets. If the wallet's signature completes the transaction, sats saves it
for `sats tx broadcast` and doesn't keep the signed PSBT. If other signers
are still needed, it writes `<name>.signed.psbt` beside the input and reports
the PSBT as partially signed. `--out <FILE>` always writes the signed PSBT to
`FILE` and saves nothing else.

`tx broadcast` reads an existing file as raw transaction hex. Otherwise it
looks up a saved transaction by txid or unique prefix and rebroadcasts the
saved bytes, without signing again.

## Agent grants

```sh
sats agent grant <name> --budget <SATS> [--for <DURATION>] [--max-tx <SATS>]
                 [--max-fee <SATS>] [--mode ask|observe] [--to <ADDRESS>]...
```

| Option | Meaning |
|---|---|
| `--budget` | Required. Total for amounts plus fees. |
| `--for` | Lifetime, such as `30m`, `24h`, or `7d`. Default `24h`. |
| `--max-tx` | Per-transaction amount cap. Anything above is refused outright. |
| `--max-fee` | Per-transaction fee cap. Default: 2% of the budget, at least 1,000 sats, never above the budget. |
| `--mode` | `ask` (default): every request waits for your approval. `observe`: read-only, and every request is refused. |
| `--to` | Repeatable. Restricts recipients to this list; anyone else is refused. |

Agent names are 1–32 characters of lowercase letters, digits, `-`, or `_`.
Creating a grant requires your password, but the grant stores only limits and
a token hash, never key material.

Every grant limit is hard. Approval can't lift one; only changing the grant
can. Within the limits, every request waits for you, however small. There is
no autonomous mode.

The output includes a one-time token and two setup commands, one for Claude
Code and one for ChatGPT desktop/Codex. See
[Connect an agent](mcp.md#connect-an-agent).

### Changing a grant

```sh
sats agent mode claude observe        # tighten: no password
sats agent mode claude ask            # widen: password required
sats agent allow claude <address>     # widen: password required
sats agent disallow claude <address>  # tighten: no password
sats agent revoke claude
sats agent grant claude ...           # re-issue: new limits and a new token
```

Reducing authority is free and widening it needs the password. Every mode
change is logged with its direction. Allowlist addresses are checked against
the grant's network. Removing the last entry means no recipient is allowed.
`disallow` is refused on a grant with no allowlist, because a list can't
express "everyone except one address". Re-issue the grant with `--to`
instead.

Revocation deletes the grant file, and a running MCP server notices on the
agent's next request. Re-issuing replaces the token, so rotating a token and
revoking it are the same act. Expired grants are removed when listed.

## Reviewing agent requests

```sh
sats agent requests              # requests that need you
sats agent requests --all        # every request, newest first
sats agent requests --watch      # stay running; print each new pending request
sats agent log                   # event log, oldest first
sats agent log --request r-2e7d  # one request, by id or unique prefix
```

`requests` shows id, agent, recipient, amount, status, and age. Listing also
settles any request a crashed approval left in `signing`.

`--watch` is the trusted way to learn about requests: it reads the local
store, not anything the agent says. Each line ends with the exact approve
command. Denied requests never appear there, because nothing is waiting on
you; `--all` shows them. With `--json` the output is JSONL. Press Ctrl-C to
stop.

`log` shows one line per event: received, denied, approved, dismissed,
reserved, signed, broadcast, refunded, or failed. It reads local files only,
and skips unreadable lines with a warning.

## Approving a request

```sh
sats agent approve               # pick from a list in your terminal
sats agent approve <id> [--yes]  # approve a specific id or unique prefix
sats agent dismiss <id>
```

Without an id, sats shows a numbered list of pending and failed requests.
Press Enter or `r` to refresh, `w` to wait a second and refresh, or `q` to
cancel. Choosing a request opens its review and approves nothing. sats
re-checks that exact request, so a reordered list can't redirect your choice.
Requests that may already be signed appear with recovery steps but can't be
chosen. Without a terminal, pass an id.

To approve, sats:

1. prepares the transaction on fresh chain state;
2. checks that it pays exactly the recipient and amount the agent requested;
3. re-checks the grant with the real fee;
4. shows wallet, network, full recipient, amount, fee, total, and remaining
   budget;
5. asks you to confirm, then asks for your password;
6. reserves the budget, signs, saves the transaction, and broadcasts.

`--yes` skips the confirmation, but only with an explicit id, and never skips
the password. With `--json`, the review and prompts go to stderr and the
result goes to stdout. A cancelled selection returns `status: cancelled`.

A request outside its grant's current limits, or whose grant was revoked,
expired, or re-issued, is refused before the password prompt and recorded as
`denied`. The only way forward is a new request under a new grant. Approval
has no `--fee-rate`. If fee estimation fails, fix the configured provider.

`dismiss` declines a pending, failed, or unresolved request. It needs no
password and never refunds an unresolved request's budget. Both commands
accept an id or unique prefix.

### When an approval doesn't finish

| Status | What happened | What to do |
|---|---|---|
| `failed` | Stopped before signing, for example a sync or fee error. Nothing was spent. | Fix the cause, then approve again, or dismiss. |
| `broadcast_pending` | Signed and saved, but the broadcast wasn't confirmed. | `sats tx broadcast <txid>` |
| `unresolved` | Something failed after signing started, so a signature may exist. sats will never sign it again. | Check `sats status`, then dismiss. |
| `signing` | An approval was interrupted. | Run `sats agent requests`. sats settles it from the saved records. |

The rules behind these states are in
[Security](security.md#the-signing-boundary). If a broadcast succeeded but
its receipt wasn't recorded, `sats agent requests` or
`sats tx broadcast <txid>` repairs it without broadcasting, signing, or
spending budget again.

## Alkanes

`sats alkanes` is an experimental, signet-first client for Alkanes contracts.
It needs an explicitly configured provider with the `alkanes.view`
capability, currently Subfrost, and there is no fallback.

```sh
sats alkanes inspect 2:1          # bytecode size and sha256 code hash
sats alkanes simulate 2:1 77      # advisory call simulation
```

`inspect` prints the code hash so you can compare it with a build you trust.
`simulate` shows the recognized fields (status, gas, asset transfers) next to
the raw result. It is display only, never authorization. Both commands are
read-only and check the endpoint's network first. The provider's JSON-RPC
dialect hasn't been verified against a live endpoint. Default v0.0.1 builds
don't include Alkanes execution.

## JSON output

`--json` is supported by `balance`, `receive`, `send` (all modes), `status`,
`history`, `psbt inspect`, `psbt sign`, `tx broadcast`, every `agent`
subcommand except `serve`, every `providers` subcommand, and both `alkanes`
subcommands.

Field names are a compatibility surface. Branch on the documented `status`
and `reason` fields, not on messages. `agent grant --json` includes `token`,
the only time it is ever shown, so treat that output as a secret. `init` has
no JSON mode, so the mnemonic never lands on a machine-readable stream.

## Configuration

| Platform | Config file | Data |
|---|---|---|
| Linux | `~/.config/sats/config.toml` | `~/.local/share/sats/` |
| macOS | `~/Library/Application Support/sats/config.toml` | same directory |

`SATS_DIR` replaces both with one directory.

```toml
network = "signet"        # default network

[fee_targets]             # confirmation target in blocks, per network (1–1008, default 2)
signet = 1008
```

Providers are configured under `[providers.<name>]`, by hand or with
`sats providers login`. See [Providers and guards](providers.md#managing-providers).
sats writes `config.toml` owner-only, because it can hold provider API keys.

## Exit behavior

Failures print to stderr and exit non-zero. `balance`, `status`, and
`history` are the deliberate exceptions: when sync fails they show cached
data with a warning and mark the JSON result as not synced. Planning, signing,
provider validation, and broadcast failures are always hard failures. So is
an invalid or ambiguous provider configuration for any command that needs
the network.

Local commands and `--offline` reads never resolve providers, so they keep
working when provider configuration is ambiguous. Malformed configuration
always fails at startup. Approval checks the request and its grant before
resolving providers, so a settled or revoked request is refused even without
a working provider.
