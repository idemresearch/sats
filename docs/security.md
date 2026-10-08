# Security and trust model

What sats protects, how, and where that protection ends: keys, agent grants,
request execution, providers, and failure behavior.

The core rule: **no agent-originated spend reaches the signer without explicit
human authorization bound to that exact request.** An unapproved agent request
is pending or denied, never signed. In v0.0.1 the only path from an agent
request to the signer is `sats agent approve`. [Direction](direction.md)
explains why.

## Keys and wallet state

`sats init` creates a BIP-39 mnemonic and derives BIP-86 single-key Taproot
descriptors. Public descriptors back the persisted, watch-only BDK wallet.
Private descriptors exist only in memory, during key derivation or signing.

The mnemonic is stored in `seed.sealed`, encrypted with Argon2id and
XChaCha20-Poly1305. The format is versioned and authenticated, and its
associated data binds the blob to its purpose. The SQLite wallet holds public
descriptors and chain state, never private keys.

The password and mnemonic are entered only at a terminal. sats never reads
them from arguments, environment variables, or pipes: argument lists and
shell history leak, and every program you start, agents included, inherits
your environment. With an environment password, an agent with a shell could
run `sats agent approve --yes` and sign with no one at the keyboard.

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
| Budget | Total for amounts plus fees |
| `--max-tx` | Per-transaction amount cap; above it is `over_max_tx` |
| `--max-fee` | Per-transaction fee cap, always present. Default: 2% of the budget, at least 1,000 sats, never above the budget |
| `--to` allowlist | Optional; any other recipient is `recipient_not_allowed` |
| Expiry | `--for`, default 24h |
| Network | The one network the grant authorizes |
| Accounting | Spend, transaction count, and a per-request reservation ledger |
| Token hash | SHA-256 of the agent's bearer token |

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
directory is refused on load, so a signet grant can never authorize a mainnet
signature.

The cost: the MCP client stores the raw token in its configuration, where the
agent can read it. A stolen token grants the authority to *ask*. Its holder
can file proposals you will see in `sats agent requests`, bounded by the grant
until it expires, and can use the grant's read tools. It can't cause a
signature, touch the seed, or reach another network. Keep budgets small and
expiries short, and revoke grants you don't need.

### Grant format

Grants carry `format_version: 1`. sats is pre-release, so earlier development
shapes are not migrated: they fail with an error that says to revoke and grant
again. A grant claiming a newer version is refused, because it may carry
restrictions this build can't see.

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
3. arithmetic: an amount plus fee that overflows is `amount_overflow`,
   never a saturated value that could pass a budget check;
4. the recipient allowlist (`recipient_not_allowed`);
5. the amount cap (`over_max_tx`);
6. the fee cap (`over_max_fee`);
7. the remaining budget (`over_budget`);
8. otherwise **ask**: the request is `pending_approval`.

Ask is the only outcome that isn't a denial, so "no unapproved request
reaches the signer" is a structural property of the ladder, not a setting.
Every denial is a grant boundary and none of them is approvable. Tests freeze
the order so a refusal names the specific boundary it crossed.

The ladder runs twice:

- **At filing**, with the fee unknown, before any network access. A proposal
  outside the grant is recorded as `denied`.
- **At execution**, with the real fee, under the grant lock and before the
  budget reservation. A draw can never exceed the remaining budget, and a fee
  above the cap is `over_max_fee` even after you have authorized the request.

A denied or pending request consumes no budget. The engine takes the current
time as input. If the system clock can't be read, filing and execution fail
with `clock_unavailable` rather than treating every grant as live.

An advisory per-network grant lock serializes decisions, reservations, grant
creation, and revocation across processes. Concurrent requests can't
double-draw, and an in-flight request can't undo a revocation.

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

Every transition (received, denied, approved, dismissed, reserved, signed,
broadcast, refunded, failed) appends to `<network>/events/log.jsonl`, linked
by request id and intent digest. Filing requires its event write to succeed;
some later writes are best-effort and warn on failure. Recovery relies on
request, grant, and transaction records, never on the log being complete. The
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
   the instance that created it, then re-runs the ladder with the fee
   unknown. A revoked or re-issued grant makes the request `denied` with
   reason `revoked`. A new grant never inherits old requests.
2. **Prepare.** sats builds the transaction on fresh chain state through the
   same pipeline as a human send. Approval has no `--allow-dust` option.
3. **Verify.** sats derives what the PSBT pays from the wallet's own
   descriptors and refuses unless it matches the recorded recipient and
   amount. A foreign input is refused outright.
4. **Re-check.** The ladder runs again with the real fee.
5. **Review.** You see wallet, network, full recipient, amount, real fee,
   total, and remaining budget, then confirm and enter your password. With
   `--json`, the review goes to stderr. It is never skipped.
6. **Reserve.** Under the grant lock, sats re-reads the grant, draws amount
   plus fee on the grant's ledger under the request id, and persists the
   request as `signing`.
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

| State | Signer invoked? | Budget draw | Next step |
|---|---|---|---|
| `pending_approval` | No | None | Approve or dismiss |
| `denied` | No | None | Terminal |
| `failed` | Provably not | Returned once | You may approve again |
| `signing` | Maybe | Kept | Reconciled, see below |
| `unresolved` | Maybe | Kept, never refunded | Never signed again; check `sats status`, then dismiss |
| `broadcast_pending` | Yes, transaction saved | Kept | `sats tx broadcast <txid>` |
| `sent` | Yes, broadcast | Kept | Done |
| `dismissed` | As in the prior state | Kept if one was held | Terminal |

A failure before signing, such as an unwritable audit log or a signer that
can't be constructed, returns the draw and leaves the request `failed`. Sync,
fee, and provider failures during preparation also leave it `failed`.
They happen before any draw. A signer error, an unfinalized result, or a failure to
finalize or save leaves the request `unresolved`. A `failed` record never
overwrites a settled or uncertain one.

### Budget reservations

The draw is an entry in the grant's ledger, keyed by request id and written in
the same atomic update as the grant's totals. A request holds at most one
draw. Repeating the same amount and fee draws nothing; a different amount or
fee fails closed. A draw is returned exactly once, and only while the request
record is `pending_approval`, `failed`, or `denied`, the states that prove the
signer never ran. A request that signed keeps its entry, so the ledger doubles
as the grant's spend history.

### Crash recovery

Recovery is a function of the durable records and is never guessed:

- **Crash before the draw.** Nothing was written.
- **Crash after the draw, before `signing`.** The draw is an orphan. The next
  listing, approve, or dismiss of that request returns it.
- **Crash after `signing`.** If a saved transaction is attributed to the
  request (same agent, request id, and intent digest), the request becomes
  `broadcast_pending` or `sent`. Otherwise it becomes `unresolved` and
  nothing is refunded.
- **Crash after `failed`, before the refund.** The orphaned draw is
  returned once.

Under the durability assumptions above, a crash can't produce a second
signature for one approval, or a refund after a signature may exist.

## The MCP boundary

The MCP server reads the wallet and files requests under the grant its token
names. It holds no key material and never prepares, signs, or broadcasts.
Agents never receive the password, mnemonic, a signer, a PSBT-signing or
unlock tool, or the `--allow-dust` bypass.

The token authenticates every tool call, never execution. Each call re-reads
the grant, so revocation, re-issue, or expiry cuts a running session off at
its next call, reads included, and approvals under a revoked grant stop
immediately. `check_request`
never syncs, mutates records, reconciles, refunds, signs, or broadcasts. An
absent execution lock never proves a request is unsigned. The full contract
is in [MCP](mcp.md).

## Providers and inscription postage

Preparing a spend requires fresh chain state. A sync failure stops every send
mode, including `--dry-run` and `--export-psbt`, and stops agent execution.
Balance, status, and history may show cached state and say so.

Coin selection excludes outputs of exactly 546 or 330 sats, common
inscription postage. The check is local: no provider is asked which outputs
carry assets, so an asset on any other value is not detected. Only a human
CLI invocation can bypass the check, with `--allow-dust`, and only for that
invocation.

Provider data is untrusted. A fee rate that is non-finite, negative, or above
10,000 sat/vB is a typed error. Malformed checkpoint data fails the sync,
not the process. Diagnostics show only a provider's origin, never user-info,
paths, queries, or response bodies. Credentials still go to the configured
endpoint. Redaction does not make a provider honest.

Provider credentials live in `config.toml`, which sats writes owner-only.
`sats providers add` reads a key from a hidden prompt or stdin, never from
the command line, and checks the endpoint before saving it. A saved Esplora
token never follows a new URL, and the Subfrost key is never sent to a
`--provider` override. Configuring providers is a human CLI task: no MCP
tool configures them, and the MCP server only uses what is configured.

## PSBTs

A normal send, human or agent, keeps its PSBT in memory. `--export-psbt` is the
exception: it writes the unsigned PSBT to an owner-only file you name, and
leaves the directory's permissions alone. `sats psbt sign` turns a finalized
artifact into a private transaction record. It also signs external PSBTs,
which are untrusted input. Check them with `sats psbt inspect` and an
independent tool before signing. A PSBT that still needs other signers is
reported as partially signed and written back as an artifact.

## Alkanes

Default builds include no Alkanes commands. Inspection and advisory
simulation are compiled only with the non-default `experimental-alkanes`
feature, and execution only with the development-only
`experimental-alkanes-execute` feature, which also refuses mainnet. Views
come from the network's provider, which must be Subfrost. There is no
Alkanes MCP tool or grant authority. The simulation request format has not
been verified against a live endpoint.

## Threat summary

| Threat | Boundary | Remaining risk |
|---|---|---|
| Stolen wallet database | No private descriptors in SQLite | Addresses and balances exposed |
| Stolen sealed seed | Argon2id plus authenticated encryption | Password strength; offline guessing |
| Read grant file | Budget and token hash only | Reveals amounts and expiry |
| Stolen agent token | Every request waits for your password; caps and expiry enforced at filing and execution | Can file and read within the grant until it is revoked or expires |
| Lied-about amount or fee | Recomputed from the PSBT against the wallet's descriptors | An understated input wastes the agent's own budget on an unrelayable transaction |
| Compromised MCP process | Holds a token, never a key; forged approvals need the password | Same as a stolen token |
| Debugger on an approving process | Key material lives only for one approval | Seed recoverable in that window where ptrace is allowed |
| Pre-daemon wrapped-seed grant | Reported and refused | The file is a seed disclosure until the wallet is rotated |
| Revoked agent | Grant and token rechecked on every MCP tool call and under the lock at execution | Transactions signed before revocation stay valid |
| Provider outage | Planning fails closed on a failed sync | Loss of availability |
| Inscription on a non-postage value | Not detected | Can be spent by accident |
| Broadcast failure | Transaction saved first; request `broadcast_pending` | Retry with `sats tx broadcast` |
| Wrong network | Address and provider network validation | Misconfigured third-party responses |
| Malicious Alkanes view (experimental builds) | Advisory display only | Can mislead inspection or simulation |

## Operational guidance

- Learn the flow on signet before using mainnet.
- Back up the mnemonic and verify recovery independently.
- Use a strong, unique wallet password.
- Run sats only on a machine and user account you trust.
- Keep grant budgets small, set fee caps, and prefer short expiries.
- Before entering your password in `sats agent approve`, check the wallet,
  network, recipient, amount, and real fee.
- Review `sats agent list` regularly and revoke unused grants.
- Treat an agent token like the budget it unlocks: re-issue to rotate it, and
  never commit it.
- Treat providers and their responses as part of your trust model.
- Never paste a real mnemonic into issues, logs, screenshots, tests, or agent
  conversations.
