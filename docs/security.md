# Security and trust model

What sats protects, how, and where that protection ends: keys, agent grants,
request execution, providers, and failure behavior.

The core rule: **no agent-originated spend reaches the signer without explicit
human authorization bound to that exact request.** An unapproved agent request
is pending or denied, never signed. Today the only path from an agent
request to the signer is `sats agent approve`. [Direction](direction.md)
explains why.

## Test networks only

sats runs on signet (the default), testnet4, and regtest, and refuses
mainnet. The refusal is one rule (`sats_core::network`) checked wherever
mainnet could enter: parsing `--network` or the config's `network`, loading a
config with a `[mainnet]` section, opening or creating a wallet, resolving a
chain provider, and constructing the local signer. The agent server takes its
network from the same flag, so it refuses to start on mainnet. Every refusal
happens before anything is written.

The reason is the signing boundary described below: the software signer runs
on the same machine, as the same user, as the agent. That is acceptable for
coins without value and not for mainnet. Treat any seed sats has held as a
development key: never send mainnet funds to it or restore it into a mainnet
wallet.

## Keys and wallet state

`sats init` creates a BIP-39 mnemonic and derives BIP-86 single-key Taproot
descriptors. Public descriptors back the persisted, watch-only BDK wallet.
Private descriptors exist only in memory, during key derivation or signing.

The mnemonic is stored in `seed.sealed`, encrypted with Argon2id and
XChaCha20-Poly1305. The format is versioned and authenticated, and its
associated data binds the blob to its purpose. The SQLite wallet holds public
descriptors and chain state, never private keys.

The password and mnemonic are entered only at a terminal, never through
arguments, environment variables, or pipes, which other programs, agents
included, can read or inherit.

Sealed keys, grants, agent requests, the event log, and finalized
transactions are written atomically and owner-only (0600). The directories
that hold them are owner-only (0700), and existing installs are re-hardened
as those directories are touched. This guards against partial writes and
other OS users, not against a process running as your user.

## Limits

- **Same-user processes.** MCP limits the tools an agent is given. It does
  not isolate a hostile process running as your OS user with shell access,
  which can tamper with wallet files or executables, read permitted memory,
  or intercept your input. sats has no independent signing hardware and
  cannot protect a compromised operating system.
- **Memory.** No long-running process keeps an unsealed seed, but `init`,
  restore, password checks, and signing briefly hold plaintext key
  material. `bip39` zeroizes its mnemonic on drop. Display strings, restore
  input buffers, compiler copies, and key material owned by BIP-32,
  secp256k1, and BDK carry no complete-erasure guarantee, and an abort skips
  drop cleanup.
- **Durability.** Atomic writes sync file contents before rename, but not
  the parent directory. Crash recovery assumes completed writes survive.
  Sudden power loss or unusual storage can violate that. The event log,
  request records, and wallet database are not one cross-file transaction.
  Keep independent backups.

## Human sends

`sats send` validates and prepares a PSBT with the watch-only wallet, shows
amount, fee, and total, and asks for confirmation unless you pass `--yes`.
It then unseals the mnemonic with your password, signs with an ephemeral
signing wallet, saves the raw finalized transaction privately, and
broadcasts. The private descriptor is never written to the wallet database.
A normal send never persists the PSBT, prepared or signed. Because the
transaction is saved before broadcast, a failure or lost response leaves an
exact retry.

## Agent grants

`sats agent grant <name>` creates bounded authority to *propose* sends, never
to sign them. Every limit is hard: approval authorizes one proposal inside the
grant and can never exceed it.

| A grant holds | Effect |
|---|---|
| Mode | `ask` (default): every request waits for you. `observe`: read-only. There is no autonomous mode. |
| `--max-tx` | Optional per-request amount cap, fee excluded; above it is `over_max_tx` |
| `--to` allowlist | Optional; any other recipient is `recipient_not_allowed` |
| Expiry | `--for`, default 24h |
| Network | The one network the grant authorizes |
| Token hash | SHA-256 of the agent's bearer token |

A grant is a credential and a scope, not an account. It holds no budget,
spending ledger, or fee cap: every execution waits for you, and you review
its real amount and fee, so cumulative accounting would only repeat that
review. The limits that remain guard what a review catches poorly. The
allowlist stops a substituted address, which nobody verifies by eye, and the
amount cap keeps absurd proposals out of your queue.

Changing a grant follows one rule: reducing authority is cheap and widening
it needs the password. Switching `ask` to `observe` or removing an allowlist
entry needs no password. Switching `observe` to `ask` or adding an entry
does. Allowlist entries change only through these commands, never through
payment history, so an agent can't launder an address into "known" by paying
it once.

Agent names are 1–32 characters of `a-z`, `0-9`, `-`, or `_`. They are path
components on disk, so sats checks them at grant creation, at the MCP
boundary, and in the store.

### The agent token

Creating a grant requires your password, but nothing derived from it enters
the grant. **A grant file holds no key material.** sats mints a random
32-byte token, prints it once inside the MCP setup commands, and stores only
its hash, compared in constant time. Re-issuing a grant mints a new token and
invalidates the old one, so rotation and revocation are the same act.

A grant is bound to its network. A grant file copied into another network's
directory is refused on load, so a signet grant can never authorize a
signature on another network.

The cost: the MCP client stores the raw token in its configuration, where the
agent can read it. A stolen token grants the authority to *ask*. Its holder
can file proposals you will see in `sats agent requests`, bounded by the grant
until it expires, and can use the grant's read tools. It can't cause a
signature, touch the seed, or reach another network. Keep expiries short,
set an amount cap and an allowlist where you can, and revoke grants you don't
need.

### Grant format

Grants use `format_version: 4`, a credential and scope with no budget.
Wrapping uses the same limits as asset transfers and requires no
protocol-specific permission. Grants of versions 1 through 3 carried a
budget, so they are refused with re-issue guidance; their limits are never
silently discarded. BTC send requests retain
version 1; frBTC wrapping requests use version 3, binding their recorded
contract facts and operation so an older binary cannot reinterpret them as
plain payments. Asset-transfer requests use version 4 and a separate digest
binding protocol, asset id, quantity, recipient, network, agent, and BTC
postage. Older binaries refuse them. Stored records are never silently
migrated. Unsupported
development shapes fail with recreate guidance, and newer grants are
refused because they may carry restrictions this build cannot see.

One older shape gets special handling. Pre-daemon development builds stored
the seed re-sealed inside the grant file, so anything that could read the file
could sign. sats recognizes those grants, reports them in `sats agent list`,
and refuses them. **If an agent with shell access ever ran while one existed,
treat the seed as disclosed and move funds to a fresh wallet**. Revoking
alone is not enough.

## The authorization ladder

`evaluate_send` is one deterministic, pure decision with typed outcomes. It
checks, in order:

1. expiry (`expired`);
2. observe mode (`observe_only`);
3. the recipient allowlist (`recipient_not_allowed`);
4. the amount cap (`over_max_tx`);
5. otherwise **ask**: the request is `pending_approval`.

Ask is the only outcome that isn't a denial, so "no unapproved request
reaches the signer" is a structural property of the ladder, not a setting.
Every denial is a grant boundary and none of them is approvable. Tests freeze
the order so a refusal names the specific boundary it crossed.

The fee is not a rung: sats computes it, not the agent, and you review it.
The ladder runs at every step that could act on a stale grant:

- **At filing**, before any network access. A proposal outside the grant is
  recorded as `denied`.
- **At execution**, on the amount derived from the PSBT, before your review
  and again under the grant lock right before `signing` is persisted. A
  grant revoked, expired, or tightened after your review still refuses.

The engine takes the current time as input. If the system clock can't be
read, filing and execution fail with `clock_unavailable` rather than treating
every grant as live.

An advisory per-network grant lock serializes decisions, request writes,
grant creation, and revocation across processes, so an in-flight request
can't undo a revocation.

## Agent requests and the audit log

Each request is a durable record under `<network>/agent-requests/<agent>/`.
It holds the canonical intent digest (network, agent, normalized recipient,
and amount, but not the fee), its state, and the `grant_id` (128 random bits)
of the grant instance that created it.

- **Ids.** A request id is `r-` plus 32 hex characters, globally unique. It
  is derived from the grant id, the agent, and the agent's required
  `idempotency_key`, so the same key from two agents, or from one agent under
  a re-issued grant, names two different requests.
- **Idempotency.** Filing the same key and intent again returns the existing
  record without writing. Reusing a key for a different intent is a typed
  error that changes nothing.
- **Only authenticated callers write.** Filing with no grant on file, or
  with a wrong token, creates no record and no log line. Filing holds the
  grant lock from the grant read through the record write, and appends the
  audit event before the record, so no request exists without its event.

Every transition (received, denied, approved, dismissed, signed, broadcast,
failed, unresolved) appends to `<network>/events/log.jsonl`, linked by
request id and intent digest. Filing and approval require their event write
to succeed; later writes are best-effort and warn on failure. Recovery relies
on request and transaction records, never on the log being complete. The
log is append-only and never pruned. Lines this build doesn't understand are
shown raw by `sats agent log`, never hidden. Finalized transactions carry an
`origin` naming the surface, agent, request, and intent digest.

`sats agent requests --watch` lets you discover requests without relying on
the agent to report them. It reads the local store only: no provider calls,
no grant writes, no events.

## Approving a request

`sats agent approve` runs in your process with your password. It is the only
place an agent request reaches the signer.

1. **Claim.** sats locks the request and checks that the grant on file is
   the instance that created it, then re-runs the ladder. A revoked or re-issued grant makes the request `denied` with
   reason `revoked`. A new grant never inherits old requests.
2. **Prepare.** sats builds the transaction on fresh chain state through the
   same pipeline as a human send. Approval has no `--allow-dust` option.
3. **Verify.** sats derives what the PSBT pays from the wallet's own
   descriptors and refuses unless it matches the recorded recipient and
   amount. A foreign input is refused outright.
4. **Re-check.** The ladder runs again on the derived amount.
5. **Review.** You see wallet, network, full recipient, amount, real fee,
   and total, then confirm and enter your password. With `--json`, the
   review goes to stderr. It is never skipped.
6. **Commit.** Under the grant lock, sats re-reads the request and the
   grant, re-runs the ladder, persists the request as `signing`, and
   records the approval in the audit log.
7. **Sign.** Only now is the signer constructed. It signs, and sats saves
   the finalized transaction.
8. **Broadcast.** The request becomes `sent`, or `broadcast_pending` if the
   provider refuses it or the response is lost.

Interactive selection opens a review and nothing else. It creates no durable
authority and still requires confirmation, even with `--yes`. Requests that
may already be signed can't be selected. Because
execution needs your password, an "approved" record forged on disk gains an
attacker nothing.

### The signing boundary

The irreversible step is the call to `Signer::sign`, not a successful write
afterwards. Before that call, a failure is provably unsigned. From that call
on, nothing the signer reports counts as proof of "no signature", because a
signer can sign, or leak a signature, and still return an error.

| State | Signer invoked? | Next step |
|---|---|---|
| `pending_approval` | No | Approve or dismiss |
| `denied` | No | Terminal |
| `failed` | Provably not | You may approve again |
| `signing` | Maybe | Reconciled, see below |
| `unresolved` | Maybe | Never signed again; check `sats status`, then dismiss |
| `broadcast_pending` | Yes, transaction saved | `sats tx broadcast <txid>` |
| `sent` | Yes, broadcast | Done |
| `dismissed` | As in the prior state | Terminal |

A failure before signing, such as an unwritable audit log or a signer that
can't be constructed, leaves the request `failed`. Sync, fee, and provider
failures during preparation also leave it `failed`. A signer error, an
unfinalized result, or a failure to finalize or save leaves the request
`unresolved`. A `failed` record never overwrites a settled or uncertain one.

### Crash recovery

Recovery is a function of the durable records and is never guessed:

- **Crash before `signing`.** Nothing was written; the request is still
  approvable.
- **Crash after `signing`.** If a saved transaction is attributed to the
  request (same agent, request id, and intent digest), the request becomes
  `broadcast_pending` or `sent`. Otherwise it becomes `unresolved` and is
  never signed again.

Under the durability assumptions above, a crash can't produce a second
signature for one approval.

## The MCP boundary

The MCP server reads the wallet and files requests under the grant its token
names. It holds no key material and never prepares, signs, or broadcasts.
Agents never receive the password, mnemonic, a signer, a PSBT-signing or
unlock tool, or the `--allow-dust` bypass.

The token authenticates every tool call, never execution. Each call re-reads
the grant, so revocation, re-issue, or expiry cuts a running session off at
its next call, reads included, and approvals under a revoked grant stop
immediately. `check_request`
never syncs, mutates records, reconciles, signs, or broadcasts. An
absent execution lock never proves a request is unsigned. The full contract
is in [MCP](mcp.md).

## Providers and inscription postage

Preparing a spend requires fresh chain state. A sync failure stops every send
mode, including `--dry-run`, and stops agent execution.
Balance, status, and history may show cached state and say so.

Coin selection excludes outputs of exactly 546 or 330 sats, common
inscription postage. This heuristic is local and does not detect assets on
other output values. An Alkanes provider adds the separate indexer-backed
exclusion described under [Alkanes](#alkanes). Only a human CLI invocation
can bypass the local heuristic with `--allow-dust`, for that invocation;
it never bypasses the Alkanes exclusion.

Provider data is untrusted. A fee rate that is non-finite, negative, or above
10,000 sat/vB is a typed error. Malformed checkpoint data fails the sync,
not the process. Diagnostics show only a provider's origin, never user-info,
paths, queries, or response bodies. Credentials still go to the configured
endpoint. Redaction does not make a provider honest.

Provider credentials live in `config.toml`, which sats writes owner-only.
`sats providers add` reads a key from a hidden prompt or stdin, never from
the command line, and checks the endpoint before saving it. A saved Esplora
token or Subfrost key never follows a new URL: a changed endpoint needs the
credential typed again, as does the first Subfrost setup on another network,
even with a built-in URL. A `--provider` override never receives one. Configuring providers is a human CLI task: no MCP
tool configures them, and the MCP server only uses what is configured.

## PSBTs

Every send, human or agent, keeps its PSBT in memory and never writes it to
disk. sats has no command to export a PSBT or to sign one built elsewhere: it
signs only transactions it prepared on fresh chain state with its own
safety checks — the postage exclusion, the Alkanes exclusion, and, for agent
requests, the derivation of what the transaction pays. A foreign PSBT would
bypass all of them.

## Alkanes

Standard builds include Alkanes inspection, advisory simulation, balance,
and human-authorized execution and transfers. Views come from the network's
provider, which must be Subfrost. MCP exposes Alkanes balance, inspection,
and advisory simulation, authenticated against the pinned network's grant
on every call. These reads never prepare, execute, sign, or broadcast.
The separate fixed `request_wrap_frbtc` capability files a request under
the ordinary BTC grant for human approval. `request_alkanes_send` files an exact asset transfer under the same grant; generic contract execution proposals remain unavailable.
Provider text is untrusted data, and asset quantities use decimal strings
to preserve u128 precision. The generic simulation request format has not been
verified against a live endpoint. Wrapping uses a separate exact-transaction
protobuf view checked against testnet4.

Generic calls require a recognized numeric status-zero
simulation response without reported errors or contradictory statuses.
Unknown responses remain displayable by the read-only command but cannot
reach preparation or signing. A successful response is still only an
advisory provider claim. The transaction checker independently decodes the
call, enforces wallet-owned pointer/refund and change outputs, rejects
edicts and unexpected outputs, and derives fees from wallet input values.
It does not prove future contract effects.

Human Alkanes execution is staged in `sats-wallet::alkanes::human`. The
surface shows its immutable review, on stderr in JSON mode, before asking
for confirmation or a password; `--yes` skips only confirmation. Commit
consumes the operation and invokes a signer factory once, then drops the
signer, persists the finalized transaction, and attempts broadcast. An
error after signer invocation never triggers an automatic retry. A failed
broadcast retains the signed record for rebroadcast. This path carries no
agent grant authority; agent requests still execute only through the
human-authorized request executor.

When the provider serves Alkanes, sends exclude outputs that carry alkanes. The
indexer is asked about every unspent output; an output it doesn't confirm
knowing (its answer must echo the outpoint and value) is excluded too, and
an unreachable indexer stops planning. The check only excludes: it never
adds a candidate or authorizes anything. The indexer is trusted for what
it reports, so a lying indexer can hide an asset and cause its loss; it
cannot redirect BTC.

`sats alkanes send` checks the transaction it built by decoding it, not by
trusting the builder: it accepts only one
alkanes protostone with no contract call, an explicit pointer, and edicts
to real outputs, and only if the recipient receives exactly the requested
amount of the requested asset and every other alkane lands on a wallet
output. Ordinals runes are not modeled; any on the spent outputs would go
to the recipient.

## Threat summary

| Threat | Boundary | Remaining risk |
|---|---|---|
| Stolen wallet database | No private descriptors in SQLite | Addresses and balances exposed |
| Stolen sealed seed | Argon2id plus authenticated encryption | Password strength; offline guessing |
| Read grant file | Limits and token hash only | Reveals limits and expiry |
| Stolen agent token | Every request waits for your password; the amount cap, allowlist, and expiry are enforced at filing and execution | Can file and read within the grant until it is revoked or expires |
| Lied-about amount or fee | Recomputed from the PSBT against the wallet's descriptors | An understated input yields an unrelayable transaction |
| Inflated fee estimate | The real fee is in every approval review | Approving a high fee without reading it |
| Compromised MCP process | Holds a token, never a key; forged approvals need the password | Same as a stolen token |
| Debugger on an approving process | Key material lives only for one approval | Seed recoverable in that window where ptrace is allowed |
| Pre-daemon wrapped-seed grant | Reported and refused | The file is a seed disclosure until the wallet is rotated |
| Revoked agent | Grant and token rechecked on every MCP tool call and under the lock at execution | Transactions signed before revocation stay valid |
| Provider outage | Planning fails closed on a failed sync | Loss of availability |
| Inscription on a non-postage value | Not detected | Can be spent by accident |
| Broadcast failure | Transaction saved first; request `broadcast_pending` | Retry with `sats tx broadcast` |
| Wrong network | Address and provider network validation | Misconfigured third-party responses |
| Malicious Alkanes view | Fixed transaction checks, request-bound facts, human review; simulations remain advisory | Can lie about contract identity and effects; an independent reference is needed |
| Alkane on a wallet output (Alkanes provider) | Indexer-backed exclusion, failing closed | A lying indexer can hide it |

## Operational guidance

- Use a fresh seed made for sats, and never move it to mainnet.
- Back up the mnemonic and verify recovery independently.
- Use a strong, unique wallet password.
- Run sats only on a machine and user account you trust.
- Prefer short expiries, and set an amount cap and a `--to` allowlist when
  you know the recipients.
- Before entering your password in `sats agent approve`, check the wallet,
  network, recipient, amount, and real fee.
- Review `sats agent list` regularly and revoke unused grants.
- Treat an agent token like the authority to ask you for payments: re-issue
  to rotate it, and never commit it.
- Treat providers and their responses as part of your trust model.
- Never paste a real mnemonic into issues, logs, screenshots, tests, or agent
  conversations.

### Agent wrapping boundary

`request_wrap_frbtc` reads public contract facts and files a proposal only.
The request records the frBTC `32:0` code hash and BTC signer address, and
its digest commits to those facts, the fixed operation, network, agent, and
gross BTC amount. The agent chooses neither calldata nor destination.
There is no protocol-specific grant flag. The single `evaluate_send`
ladder applies the normal BTC mode, expiry, recipient, and amount
boundaries at filing and execution; the recipient is the actual
BTC signer address. The request's recipient must agree with its recorded
signer. Public discovery authenticates before I/O and creation rechecks the
grant afterwards. An old or revoked token cannot file under a replacement
grant.

Contract facts are bound to each request rather than the grant. Human
approval reviews those facts and rechecks them against the provider;
changes stop the request before signing. Repeat filings observe the
original receipt without provider I/O and never replace its recorded facts.

Preparation and signing run only in the existing human approval process.
The exact unsigned transaction pays the pinned signer at output 0, places
wallet-owned postage at output 1, and carries the fixed protostone at
output 2; every remaining output belongs to the wallet. Input values come
from the wallet, not caller PSBT hints. The executor excludes asset-bearing,
unknown, and dust outputs and refuses failed sync or unrecognized/failed
simulation. Human review includes the wallet receive address, contract hash,
BTC payment, postage, actual fee, total, and advisory net frBTC prediction.
The contract's code hash and signer are rechecked before signing.
Provider claims and simulations do not prove future contract effects.

Wrapping uses the same persisted `signing` boundary, signed-transaction
persistence, unresolved recovery, and rebroadcast-only handling as ordinary
agent payments. The amount cap checks the gross BTC paid; predicted frBTC
never offsets it. A broadcast receipt does
not prove minting: confirmation and indexing must be observed separately.


### Agent asset-transfer boundary

`request_alkanes_send` files a local proposal without provider or seed access.
The core request is a generic `AssetTransfer` with a protocol-qualified asset
id and exact positive u128 quantity serialized as a decimal string.
The native executor currently accepts only canonical Alkanes transfers;
unknown protocols fail before provider access. Names and symbols never
select assets or grant authority. Request format 4 prevents older binaries
from interpreting an asset transfer as an ordinary BTC payment.

No protocol grant flags are added. The existing grant authorizes proposals;
its recipient allowlist applies to the destination and its amount cap checks
the fixed 546-sat recipient postage. **The grant does not cap asset quantity
or value.** The human must review the asset id and exact base-unit quantity
as well as recipient, returned assets, postage, fee, and total before
authorizing that request.

The shared CLI/native transfer builder syncs successfully, selects indexed
asset inputs, and excludes unrelated assets, unknown outputs, and dust from
BTC funding. Independent verification checks exact recipient assets, returns
all other indexed Alkanes to wallet-owned outputs, and rejects additional
external BTC outputs. Input BTC values come from the wallet. Fresh indexed
input balances are checked against the reviewed transaction before signing; changes or unavailable data stop before the signer.

The existing exact-request recheck, revocation rules, persisted
`signing` boundary, signature persistence, and unresolved/rebroadcast recovery
apply without a parallel execution path. A broadcast receipt proves neither
confirmation nor indexing. Indexer claims remain trusted inputs; these checks
do not model other asset protocols carried by the same outputs.
