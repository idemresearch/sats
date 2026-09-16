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

If the wallet already exists, continue with `sats balance` or `sats receive`.
For a separate test wallet, set `SATS_DIR` to an empty directory before running
`sats init`.

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
- Local records from the old machine — grants, finalized transaction
  hex, agent requests — are not part of the seed and do not come back;
  on-chain history does.
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
| `sats status [TXID] [--offline]` | Show saved transactions with local broadcast status, chain observations, and sync freshness |
| `sats history [--offline]` | List the wallet's transactions, newest first |
| `sats psbt inspect <FILE>` | Decode a PSBT file offline: outputs, fee, signing state |
| `sats psbt sign <FILE> [--out FILE]` | Sign an explicit PSBT artifact |
| `sats tx broadcast <FILE\|TXID>` | Broadcast a raw hex file, or a saved transaction by txid/prefix/id |
| `sats agent grant <name>` | Create bounded authority to propose sends (never to sign) |
| `sats agent revoke <name>` | Delete an agent grant immediately |
| `sats agent mode <name> <mode>` | Switch ask/observe; widening needs the password |
| `sats agent allow <name> <addr>` | Add an allowlist recipient (password required) |
| `sats agent disallow <name> <addr>` | Remove an allowlist recipient (no password) |
| `sats agent list` | List non-expired grants and remaining budgets |
| `sats agent requests [--all\|--watch]` | Review pending/failed requests and recovery cases; `--watch` streams newly pending ones |
| `sats agent approve [id] [-y]` | Select a request in a terminal, or supply an explicit id; review, authorize, sign, persist, broadcast |
| `sats agent dismiss <id>` | Decline a pending request |
| `sats agent log [--limit N] [--request ID]` | Show the causal event log of agent activity |
| `sats agent serve <name>` | Serve the agent's wallet tools over MCP stdio: read, file requests, observe |
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
2. freshly syncs the watch-only wallet; no unspent outputs returns funding
   guidance before any guard or fee call;
3. excludes common inscription postage outputs unless `--allow-dust`;
4. queries and unions configured guards unless `--no-guards`; if all outputs
   are protected, explains that unprotected funding is needed and skips fees;
5. estimates the fee for the configured target (two blocks by default), unless
   `--fee-rate` is supplied;
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
sats status                # saved transactions and chain observations
sats status <txid>         # one transaction by txid or unique prefix
sats history               # every wallet transaction, unconfirmed first
```

Both commands sync first and tolerate sync failure with a stderr warning;
`--offline` skips sync entirely. `status <txid>` falls back to the wallet's
canonical chain view for transactions the store never saw, such as incoming
payments. A locally pending record is labeled "signed; broadcast unconfirmed":
it may already be in the mempool or confirmed after a lost response. Those
chain observations are shown even while local bookkeeping remains pending.
JSON includes `synced` and `sync_status` (`fresh`, `offline`, or `failed`);
cached observations after failed sync are never labeled fresh.

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

`tx broadcast` takes exactly one target: an existing file is read as raw
transaction hex; anything else resolves a saved transaction by full txid
or unique prefix. It rebroadcasts the saved bytes without preparing or signing
again. Repeating the command for an already-broadcast saved record repairs its
request receipt locally, without resolving providers or drawing budget again.

## Agent grants

```sh
sats agent grant <name> --budget <SATS> [--for <DURATION>] \
  [--mode ask|observe] [--to <ADDRESS>]... \
  [--max-tx <SATS>] [--max-fee <SATS>]
sats agent mode <name> <ask|observe>
sats agent allow <name> <ADDRESS>
sats agent disallow <name> <ADDRESS>
```

`--for` defaults to `24h` and accepts human-readable durations such as `30m`,
`24h`, and `7d`. Agent names are 1–32 lowercase letters, digits, hyphens, or
underscores.

Budget is amount plus fee. `--max-tx` applies to recipient amount only
and `--max-fee` applies to fee only; both are hard boundaries of the
grant. Every grant carries a fee cap: when `--max-fee` is not given it
defaults to 2% of the budget — at least 1000 sats, never above the
budget — so a single bad fee estimate cannot burn the whole budget as
miner fees. Grant creation requires the wallet password, which
authorizes the grant and is not otherwise used: the grant file holds a
budget and a token hash, never key material.

`--mode` sets how much standing authority the grant carries — and no mode
lets a request execute without you. `ask` (the default) lets the agent
file requests: every one — however small — waits as `pending_approval`
until you run `sats agent approve`, one request at a time. `observe` is
read-only: balance, addresses, grant status, and request polling keep
working, but every request is refused with `observe_only`. Revocation
remains the off switch.

There is no autonomous mode and no resident unlocked signer: an
agent-originated request reaches the signer only through your approval,
which unseals the seed with your password for exactly that execution.

`sats agent mode` switches a live grant between the modes, mid-session,
and carries the attenuation rule mechanically: tightening authority
(`ask→observe`) never asks for the password — reducing what an agent may
do stays cheap — while widening (`observe→ask`) requires it, exactly like
issuing the grant did. Every transition lands in the event log with its
direction.

Repeatable `--to` gives the grant a standing recipient allowlist — a
hard boundary. Each address is parsed and network-checked at the
boundary and stored in the same normalized spelling the intent digest
hashes. With a finite list, requests to listed recipients are routine
(`pending_approval`); any other recipient is the hard refusal
`recipient_not_allowed`: `sats agent approve` refuses it, and only
editing the list changes it — never payment history or an approval, so an
agent cannot launder an address into "known" by paying it. Without `--to` every recipient may be
proposed. `sats agent allow` adds an entry (widening — password
required); `sats agent disallow` removes one (no password), and
emptying the list means no recipient may be proposed. A grant without
an allowlist refuses `disallow` outright: a list cannot express
"everything except one address" — re-issue the grant with `--to` to
restrict it.

`--max-tx` splits amounts into exactly two outcomes. Up to the cap a
request is routine (`pending_approval`), executed only through
`sats agent approve`. Above it the refusal is the hard `over_max_tx`:
`sats agent approve` refuses it, and the only escalation is changing the
grant itself. The cap is the
pre-commitment you make while calm, so that later pressure — an agent
asking nicely fifty times — has nothing to push on.

Creating a grant prints a bearer token **once**. It is not stored — only its
SHA-256 is — so there is no way to recover it later; re-issue the grant to
mint a new one. Pass it to the served process as `SATS_AGENT_TOKEN`:

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
# SATS_AGENT_TOKEN=<token>
# Copy the exact claude mcp add command printed by grant.
```

```sh
sats agent list
sats agent revoke claude
```

The printed command supports POSIX shells and pins the effective network and
wallet/configuration locations. Relative overrides become absolute, and shell
metacharacters stay literal. Conflicting ambient defaults cannot select a
different wallet. It copies no provider credentials; the server reads the
original wallet configuration.

Expired grants are removed while listing. Revocation deletes the grant file;
an active MCP server observes the deletion on its next send call. Re-issuing
a grant replaces its token, so rotation and revocation are the same act.

An obsolete pre-release grant shape stored recoverable signing material in
the grant file. It is reported and refused with wallet-rotation guidance;
reissuing that grant alone cannot repair possible seed disclosure. See
[Security](security.md).

`sats agent serve <name>` runs the MCP server as that agent. See
[MCP and agent grants](mcp.md) for the tool-level contract.

## Reviewing agent requests

Every agent request is a durable record, and every state transition
appends to a per-network event log:

```sh
sats agent requests            # requests awaiting review or recovery
sats agent requests --all      # every recorded request, newest first
sats agent requests --watch    # stay running; print each new pending request once
sats agent log                 # the causal event chain, oldest first
sats agent log --request r-2e7d41c0a95b6f13c4d5e6f708192a3b
```

`requests` shows each request's id, agent, recipient, amount, status, and
age; `--json` returns the full records. Listing also settles any request
a crashed approve left `signing` (see below). A request is in exactly
one state:

| Status | Meaning |
|---|---|
| `pending_approval` | Inside the grant; awaiting your decision |
| `denied` | Outside a grant boundary, at filing or at execution; terminal |
| `dismissed` | You declined it; terminal |
| `signing` | You authorized it, the budget is reserved, and the signer is being invoked |
| `sent` | Broadcast; carries the txid |
| `broadcast_pending` | Signed and saved; broadcast unconfirmed locally; recover the saved transaction with `sats tx broadcast <txid>` |
| `unresolved` | A signature may exist without a durable result. Never refunded or signed again; inspect before dismissing |
| `failed` | Preparation/execution stopped before signer invocation; no new draw, or a provable pre-sign draw is refunded; may be reviewed again |

`log` renders one line per event — received, denied, approved, dismissed,
reserved, signed, broadcast, refunded, failed — and `--request` accepts
an id or unique prefix. Unreadable records and torn log lines are skipped
with a warning; both surfaces are purely local and never touch a provider.

`--watch` is the trusted discovery channel: it polls the local store and
prints each request exactly once, when it newly awaits your decision,
ending each line with the exact `sats agent approve` command. You learn
about pending requests from sats itself instead of relying on the agent
to relay (or downplay) its own status. Denied requests never appear there
— nothing is awaited — and stay reviewable with `--all`. Request ids are
global; idempotency keys are scoped to the original grant instance. With `--json` the stream is JSONL, one full record per
line. Watching is read-only; Ctrl-C stops it.

## Approving one request

```sh
sats agent approve              # terminal selection and review
sats agent approve <id> [--yes] # explicit id or unique prefix
sats agent dismiss <id>
```

Without an ID, terminal users see a numbered snapshot of pending and failed
requests. Enter/r refreshes it, w waits one second and refreshes, and q cancels.
An empty queue offers the same controls. Selection only opens review; even one
request and `--yes` still require separate confirmation. The exact selected ID
and record are revalidated, so queue reordering cannot redirect the choice.
Piped/nonterminal stdin requires an explicit ID. JSON prompts and review stay
on stderr; interactive cancellation returns `status: cancelled` on stdout,
with the selected `id` if review had begun.

The menu refreshes human reconciliation. Signing, broadcast-pending, and
unresolved rows show attention/recovery instructions and cannot be selected
for another signature. Only pending and provably pre-sign failed requests are
reviewable. Explicit IDs and unique prefixes keep their existing behavior.

`approve` is the human-authorized execution path. It prepares the
transaction on current chain state, derives what the prepared transaction
actually pays from the wallet's own descriptors and refuses if that
disagrees with the recorded request, re-runs the grant's boundaries with
the real fee, and shows you the wallet location, network, full recipient, amount,
fee, total, and remaining budget — with `--json`, on stderr, so the review always reaches
you while stdout stays the machine-readable result. It then asks for
confirmation (`--yes` skips it only with an explicit ID, never the password) and for the
wallet password: the prompt is the authorization, and the key it unseals
exists only for this one execution. Under the grant lock it draws the
budget on the grant's ledger under the request's id and records the
request as `signing` *before* the signer is invoked, then signs, saves
the finalized transaction, and broadcasts.

A request executes only under the grant that created it. A request
outside that grant's current boundaries — an amount above the cap, a fee
above the fee cap, a recipient off the allowlist — or whose grant has
been revoked, expired, or re-issued, is refused before the password and
recorded as `denied`; the only escalation is a new request under a new
grant. If execution stops before the signer is invoked — the audit log
cannot be written, the signer cannot be constructed — the draw is
returned, the request is `failed`, and you may approve it again. Once
the signer has been invoked, any failure — an error from the signer, an
unfinalized result, a signed transaction that could not be finalized or
saved — leaves the request `unresolved`: a signature may exist, so the
draw stands and sats will not sign it again; check `sats status`, then
dismiss it. If broadcast fails after signing, the request is
`broadcast_pending`: the signed transaction is saved, the draw is final,
the request is never signed again, and `sats tx broadcast <txid>`
retries it. A draw left by an approve that died before `signing`
reached disk is returned by the next listing, approve, or dismiss of
that request; a draw whose request did reach `signing` is never
returned. `dismiss`
declines a pending, failed, or unresolved request without a password:
reducing authority stays cheap, and dismissing an unresolved request
never refunds. Both accept a request id or unique prefix and support
`--json`.

Sync, guard, fee, and provider-resolution failures during preparation record a
`failed` attempt that the agent can observe and the human can review again.
Fee errors direct you to the configured provider; approval has no `--fee-rate`
option. A failed attempt cannot overwrite a settled or uncertain execution.

A saved transaction marked broadcast can repair an outdated `broadcast_pending`
receipt through listing or `sats tx broadcast <txid>`, without another network
broadcast, signature, or draw. Live execution claims are respected.

If an approve is interrupted after `signing` was recorded, the next
listing or approve reconciles the record from the durable truth: a saved
transaction attributed to the request (same agent, id, and intent) means
a signature exists and the request becomes `broadcast_pending` or
`sent`; none means the signer may or may not have run, and the request
becomes `unresolved` with nothing refunded.

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

Execution is excluded from default v0.0.1 CLI help and dispatch. The pure
composition library and execution source remain available for explicit
non-default development builds; see [Development](development.md).

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
- `agent mode`, `agent allow`, `agent disallow`;
- `agent list`;
- `agent requests`;
- `agent approve`;
- `agent dismiss`;
- `agent log`;
- `alkanes inspect`;
- `alkanes simulate`.

JSON field names are compatibility surfaces. Scripts should branch on
documented status and reason fields rather than human-readable messages.

One shape is worth calling out. `agent grant --json` includes `token`, the
only time it is ever emitted; treat that output as a secret.

`init` remains an interactive recovery-material flow and deliberately has no
JSON mode: emitting the mnemonic on a machine-readable stream invites
accidental capture.

## Configuration

The default configuration path is the platform-specific XDG config directory
for sats. A minimal configuration is:

```toml
network = "signet"
```

Fee-estimate confirmation targets are human policy, scoped per network, and
default to 2 blocks. For a low-priority signet workflow:

```toml
[fee_targets]
signet = 1008
```

Typed providers are configured under `[providers.<name>]`:

```toml
network = "mainnet"

[providers.subfrost]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
api_key = "replace-with-key"
```

See [Providers and guards](providers.md) for drivers, capability filters,
precedence, authentication, and command-line overrides.

## Exit and failure behavior

Local commands and `balance`, `history`, and `status` with `--offline` do not
resolve providers. They remain available when provider selection is ambiguous;
malformed configuration still fails at startup. Approval checks the request's
state and bound grant before resolving providers, so a settled or revoked
request is refused without requiring a usable chain provider.

Command failures print a diagnostic to stderr and exit non-zero. Balance,
status, and history are the intentionally tolerant chain-read commands: when
sync fails without `--offline`, they report cached state with a stderr
warning (`synced: false` for balance and status; status also reports
`sync_status: failed`). Planning, signing, provider
validation, and broadcast failures remain hard failures.
Invalid or ambiguous provider selection is a hard failure for online operations,
not a reason to fall back to offline output.
