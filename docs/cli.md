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
| `sats tx broadcast <TXID>` | Rebroadcast a saved transaction |
| `sats agent grant <name>` | Let an agent file requests you approve |
| `sats agent mode <name> <ask\|observe>` | Switch a grant's mode |
| `sats agent allow <name> <address>` | Add a recipient to a grant's allowlist |
| `sats agent disallow <name> <address>` | Remove a recipient from a grant's allowlist |
| `sats agent list` | List active grants and their limits |
| `sats agent revoke <name>` | Delete a grant immediately |
| `sats agent requests [--all\|--watch]` | List requests that need attention, or stream new ones |
| `sats agent approve [ID] [--yes]` | Review, authorize, sign, and broadcast one request |
| `sats agent dismiss <ID>` | Decline a request |
| `sats agent log [--limit N] [--request ID]` | Show the agent event log |
| `sats agent serve <name>` | Run the MCP server for an agent ([MCP](mcp.md)) |
| `sats providers [list]` | Show the network's provider |
| `sats providers add <subfrost\|esplora>` | Add a provider, asking for its key, and use it |
| `sats providers use <mempool\|subfrost\|esplora>` | Switch the network's provider |
| `sats providers remove <subfrost\|esplora>` | Remove a provider and its stored key |
| `sats alkanes balance` | Show indexed assets and unindexed outputs |
| `sats alkanes inspect <ID>` | Show bytecode size and code hash |
| `sats alkanes simulate <ID> <INPUTS>...` | Show an advisory simulation |
| `sats alkanes send <ID> <AMOUNT> <ADDRESS>` | Transfer an asset in its base units |
| `sats alkanes frbtc info` | Read the frBTC contract hash and BTC signer |
| `sats network` | Show the active network and saved default |
| `sats network use <signet\|testnet4\|regtest>` | Set the CLI's default network |

## Global options

| Option | Meaning |
|---|---|
| `--network <NET>` | `signet`, `testnet4`, or `regtest`. Overrides the config. Mainnet is refused. |
| `--provider <KIND=URL>` | Replace the network's provider for this run. See [Providers](providers.md#one-off-overrides). |
| `--json` | Machine-readable output, where supported |

| Variable | Meaning |
|---|---|
| `SATS_DIR` | Put all configuration and data under one directory. Useful for test wallets. |

**Amounts** are whole satoshis. `25000`, `25k`, and `1.5m` (1,500,000) all
work. Suffixes are case-insensitive, and shorthand must resolve to a whole
sat.

## Setting up

```sh
sats init                     # asks: create a new wallet, or restore one
sats init --words 24          # create, with a 24-word mnemonic
sats init --restore           # restore from a mnemonic backup
sats --network testnet4 init  # add another test network
```

`init` shows a new mnemonic once. Write it down before you continue. One seed
serves every network: on a machine that already has a seed, `init` with
another `--network` adds that network's wallet. If a wallet already exists,
use `sats balance` or `sats receive`. For a separate test wallet, point
`SATS_DIR` at an empty directory.

### Switching networks

```sh
sats network                   # signet (default)
sats network use testnet4       # save testnet4 as this configuration's default
sats init                      # initialize its wallet if needed
sats --network signet balance  # use signet for just this command
```

Switching updates only the saved default in `config.toml`. Each network keeps
its wallet, provider settings, grants, and requests. It needs no
password or provider connection and does not initialize a wallet. Existing
MCP connections stay on their pinned network, including after restart; an
agent needs a separate grant and token to use another network.

`sats network` respects a one-command `--network` override and shows the
saved default alongside it. `sats network use <NET>` saves its explicit
target even when `--network` is also present. Both support `--json` with
`network` (active network) and `default_network` (saved default).

### Restoring

`sats init --restore` reads the phrase with hidden input, never as an
argument. It validates the BIP-39 checksum before writing anything, names the
mistyped word when it can, seals the seed with a new password, and rescans the
chain on first sync.

- Restore needs an empty data directory.
- sats derives BIP-86 Taproot addresses only (`bc1p…`, `tb1p…`). Coins on
  other address types won't appear, but they aren't lost.
- Restore only a seed made for testing. sats refuses mainnet, and a seed it
  has held is a development key that must never hold mainnet funds.
- Grants, saved transactions, and agent requests are local records. They
  don't come back with the seed; on-chain history does.
- Restore needs a terminal for both the phrase and the new password.

## Sending

```sh
sats send <address> <amount> [--fee-rate <SAT_VB>] [--allow-dust]
          [--yes | --dry-run]
```

Every send, human or agent, takes the same path:

1. Validate the address for the selected network.
2. Sync the wallet. An empty wallet stops here with funding guidance.
3. Exclude 546- and 330-sat outputs, which may carry inscriptions, unless
   you pass `--allow-dust`. If every output is excluded, stop and say so.
4. Estimate the fee for the configured target (2 blocks by default), unless
   you pass `--fee-rate`.
5. Build the unsigned PSBT in memory.

A sync failure stops the send. `--allow-dust` applies only to that one command
and is never available to agents.

| Mode | Behavior |
|---|---|
| default | Show amount and fee, ask to confirm, sign, save the transaction, broadcast |
| `--yes` | Skip the confirmation. Password, safety checks, and grant rules still apply. |
| `--dry-run` | Price the send and change nothing. No password needed. |

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

## Rebroadcasting

sats signs only transactions it prepared and verified itself. There is no
command to sign a PSBT built elsewhere or to broadcast a transaction sats
didn't sign.

`tx broadcast` looks up a saved transaction by txid or unique prefix and
rebroadcasts the saved bytes, without signing again.

## Agent grants

```sh
sats agent grant <name> [--for <DURATION>] [--max-tx <SATS>]
                 [--mode ask|observe] [--to <ADDRESS>]...
```

| Option | Meaning |
|---|---|
| `--for` | Lifetime, such as `30m`, `24h`, or `7d`. Default `24h`. |
| `--max-tx` | Per-request amount cap, fee excluded. Anything above is refused outright. Without it, you review every amount. |
| `--mode` | `ask` (default): every request waits for your approval. `observe`: read-only, and every request is refused. |
| `--to` | Repeatable. Restricts recipients to this list; anyone else is refused. |

Agent names are 1–32 characters of lowercase letters, digits, `-`, or `_`.
Creating a grant requires your password, but the grant stores only limits and
a token hash, never key material.

Every grant limit is hard. Approval can't lift one; only changing the grant
can. Within the limits, every request waits for you, however small. There is
no autonomous mode, so a grant has no budget or fee cap: you see the real
amount and fee of every payment before you approve it.

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
signed, broadcast, failed, or unresolved. It reads local files only,
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
3. re-checks the grant;
4. shows wallet, network, full recipient, amount, fee, and total;
5. asks you to confirm, then asks for your password;
6. re-checks the grant once more, signs, saves the transaction, and
   broadcasts.

`--yes` skips the confirmation, but only with an explicit id, and never skips
the password. With `--json`, the review and prompts go to stderr and the
result goes to stdout. A cancelled selection returns `status: cancelled`.

A request outside its grant's current limits, or whose grant was revoked,
expired, or re-issued, is refused before the password prompt and recorded as
`denied`. The only way forward is a new request under a new grant. Approval
has no `--fee-rate`. If fee estimation fails, fix the configured provider.

`dismiss` declines a pending, failed, or unresolved request. It needs no
password. Dismissing an unresolved request only closes the record: a
signature may still exist. Both commands
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
`sats tx broadcast <txid>` repairs it without broadcasting or signing
again.

## Alkanes

Agent transfer proposals use `request_alkanes_send`. The approval queue shows
the protocol-qualified asset id and quantity separately from BTC postage.
`sats agent approve` reviews the exact base-unit quantity, recipient, returned
assets, 546-sat recipient postage, real fee, and total before the password,
including with `--json` (review on stderr). Existing grants apply: the
per-request cap checks the BTC postage, not asset quantities.
See [transfer requests](mcp.md#request_alkanes_send).

`sats alkanes` is included in standard builds for the supported test networks.
It needs Subfrost as the network's provider (`sats providers add subfrost`),
and there is no fallback. Generic execution requires human operation.
MCP can propose fixed BTC-to-frBTC wraps or exact Alkanes transfers under
the existing grant, without a separate protocol permission; `sats agent approve`
is their execution path. See [wrapping requests](mcp.md#request_wrap_frbtc). The corresponding
[MCP reads](mcp.md#alkanes-reads) expose balance, inspection, and advisory
simulation.

```sh
sats alkanes frbtc info            # query frBTC code hash and BTC signer (no spend)
sats alkanes balance              # alkanes held by this wallet's outputs
sats alkanes inspect 2:1          # bytecode size and sha256 code hash
```

`sats alkanes simulate <ID> <INPUTS>...` takes calldata words, opcode first.
Use the contract's verified method and arguments.

`balance` asks the indexer's `protorunesbyoutpoint` view what each wallet
output carries, after a sync. An output the indexer doesn't confirm
knowing (an unconfirmed one, for example) is listed as not yet indexed.

An asset's name and symbol are whatever its contract chose, as the indexer
reports them. sats shows them as unverified labels and identifies assets
only by id. An asset at any id other than `32:0` whose label claims frBTC
is flagged in `balance` and in the `send` review, where Enter alone
cancels.

sats knows one name itself: `frbtc` stands for `32:0`, in any case, wherever
`send`, `inspect`, or `simulate` takes an id, and that asset shows as frBTC.
Any other asset needs its id. Receipts and `--json` output carry ids.

When the selected provider serves Alkanes, every send also protects assets:
an output carrying alkanes, or one the indexer hasn't confirmed, is never used to fund a
plain BTC send, because a transaction without a protostone burns the
alkanes on its inputs. No flag bypasses this. If the indexer can't be
reached, the send fails rather than falling back to the dust heuristic.

`inspect` prints the code hash so you can compare it with a build you trust.
`simulate` shows the recognized fields (status, gas, asset transfers) next to
the raw result. It is display only, never authorization. Both commands are
read-only and check the endpoint's network first. `inspect` reads bytecode
through the indexer's `getbytecode` view, checked against Subfrost's signet
endpoint; `simulate`'s request format hasn't been verified against a live
endpoint yet, and neither has `balance`'s. There is no command to execute an
arbitrary contract call: sats signs only operations it composes and
verifies, such as `send` below.

`send` always renders its review before confirmation and
password entry, including with `--yes`. With `--json`, review, confirmation,
and cancellation go to stderr; stdout is reserved for the final JSON receipt.

`sats alkanes send` transfers an exact amount of one alkane:

```sh
sats alkanes send 32:0 40000 tb1p...   # amount in the asset's base units
sats alkanes send frbtc 40000 tb1p...  # the same: frbtc is 32:0
```

It spends indexed outputs holding the asset, sends the amount to the
recipient's output (546 sats of postage, `--postage` to change it), and
returns everything else those outputs carry to a wallet output. Before the
review, sats decodes the built transaction and replays the indexer's
transfer rules; it refuses unless the recipient gets exactly the amount and
nothing else, and every other alkane stays in the wallet. The review shows
the asset, amount, the indexer's label marked unverified, recipient, what
stays, the postage, and the fee, and it precedes the password (on stderr
with `--json`).

## JSON output

`--json` is supported by `balance`, `receive`, `send` (all modes), `status`,
`history`, `tx broadcast`, `network`, `network use`,
every `agent` subcommand except `serve`, every `providers` subcommand, and
every `alkanes` subcommand.

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

[signet]
fee_target = 1008         # confirmation target in blocks (1–1008, default 2)
```

Each network's provider lives under `[<network>]` too, written by
`sats providers` or by hand. See [Providers](providers.md#configuration). sats writes
`config.toml` owner-only, because it can hold provider API keys.

## Exit behavior

Failures print to stderr and exit non-zero. `balance`, `status`, and
`history` are the deliberate exceptions: when sync fails they show cached
data with a warning and mark the JSON result as not synced. Planning, signing,
provider validation, and broadcast failures are always hard failures. So is
a provider choice the network can't serve, for any command that needs the
network.

Local commands and `--offline` reads never resolve providers, so they keep
working while a provider choice can't be served. Malformed configuration
always fails at startup. Approval checks the request and its grant before
resolving providers, so a settled or revoked request is refused even without
a working provider.
