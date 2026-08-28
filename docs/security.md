# Security and trust model

sats separates watch-only wallet state from signing material, makes spending
authority explicit, and treats every signature as a point of no return. This
document describes the implemented boundaries and their costs.

## Seed and wallet state

`sats init` creates a BIP-39 mnemonic and derives BIP-86 single-key Taproot
descriptors. Two forms are used:

- public descriptors create the persisted watch-only BDK wallet;
- private descriptors are created in memory only when a signer is active.

The mnemonic is stored in `seed.sealed`, encrypted with Argon2id and
XChaCha20-Poly1305. The sealed format is versioned and authenticated; its
additional authenticated data binds the blob to its purpose. The SQLite
wallet database contains public descriptors and chain state, not private key
material.

Sensitive files are written atomically with restrictive Unix permissions.
This includes sealed keys, grants, explicit PSBT sessions, and finalized
transaction records. The data directories that hold them are owner-only
(0700) as well, so directory listings — agent names, transaction ids,
request ids — are as private as the files; existing installations are
re-hardened lazily as those directories are touched. The boundary protects
against partial writes and other OS users, but not against a process
already running as the wallet's user.

## Human signing

For a human send, sats:

1. validates and prepares a PSBT with the watch-only wallet;
2. displays amount, fee, and total;
3. asks for confirmation unless explicitly bypassed;
4. unlocks the sealed mnemonic with the supplied password;
5. builds an ephemeral signing wallet and signs the PSBT;
6. extracts and privately saves raw finalized transaction hex;
7. broadcasts it and records the final status.

The private descriptor is never written to the wallet database. Normal sends
never persist the prepared or signed PSBT. The finalized transaction is saved
before network broadcast, so a failure or lost response leaves an exact retry
without retaining PSBT derivation metadata.

## The signing daemon

`satsd` is a local, per-network process that holds the unsealed mnemonic in
memory and is the only thing that can produce a signature for an agent. It
has no chain access: callers sync, apply guards, estimate fees, and
broadcast; the daemon decides and signs.

The daemon starts locked. `sats daemon unlock` unseals the seed into its
memory with the wallet password; `sats daemon lock` drops it; an idle
timeout (default 8h, `--auto-lock`) drops it unattended. While locked the
daemon still serves, and refuses every signature with the typed error code
`wallet_locked` — distinct from any policy denial, so an agent can tell
"your budget said no" from "no human has unlocked the wallet".

Unlock attempts are throttled: they run one at a time (so parallel
connections cannot stack Argon2id derivations in memory), the first three
misses are free, and further misses back off with a doubling delay capped
at one minute. A blocked attempt is refused with the typed code
`unlock_throttled` before the password is even tried. The window is
monotonic, so rolling the wall clock back does not lift it; restarting the
daemon does, which costs an attacker more than waiting. The daemon's
status query is read-only — expired grants are pruned by `sats agent
list`, never by an unauthenticated socket call.

The socket lives at `$XDG_RUNTIME_DIR/sats/<network>.sock` (or under
`SATS_DIR`) with mode 0600, in a 0700 directory.

A private lifetime lock is acquired before stale-socket cleanup; concurrent
daemon launches cannot race to own the same socket. On macOS, optional
`daemon install` supervision uses a per-user LaunchAgent, never root. The
service definition stores executable/identity settings, not passwords or
tokens. Login and crash recovery always start locked. Availability is separate
from authority: MCP can remain connected while the daemon is missing, but
signing still requires an unlocked daemon and the existing grant checks.

What this boundary is, stated exactly: satsd runs as the wallet's own user,
so it is a **process-memory** boundary, not a privilege boundary. It defeats
reading a file, which is what a shell-capable agent actually does. It does
not defeat root, a debugger attaching to the process, or a core dump, and it
cannot stop the seed from paging to swap.

### Agent-requested unlock dialogs

On macOS an authenticated agent may call `request_unlock`, with no password
or custom prompt text. satsd re-reads the matching, unexpired grant before
opening a dialog and again under the grant lock before unlocking. A fixed,
embedded AppleScript runs through `/usr/bin/osascript` as a child of satsd;
its masked password field returns only through a private pipe to the daemon.
The MCP process never sees that pipe or password. The helper inherits no
secret environment, reads no `SATS_PASSWORD`, invokes no shell, and receives
display text as a separate argument, never executable script text. Password
buffers owned by Rust are zeroized on drop; macOS/AppleScript runtime memory
is not under Rust's zeroization control. No passwords are saved to disk or
logged by this flow.

The dialog displays the agent, network, escaped canonical wallet path and
idle auto-lock interval. Unlocking enables all valid grants for that daemon,
not just the requesting agent or a particular payment. Limits, approvals,
and accounting are unchanged. There is one prompt per daemon, a 30-second
cooldown after it completes, and a two-minute deadline. Caller cancellation
or disconnect kills the helper; human lock/unlock changes invalidate pending
consent, including at the final seed installation boundary. Cancelling after
an unlock has already committed does not retroactively lock the daemon.

This dialog is not a privilege boundary or an unspoofable OS authentication
surface. A process with the same user's UI/debugging access can still attack
it. Humans should enter the wallet password only in a dialog they requested,
check its wallet/network, and never paste it into chat. Headless or non-macOS
users keep using the terminal unlock command.

## Agent grants

`sats agent grant <name>` creates bounded unattended authority with:

- an authority mode — `auto` (sends inside the caps execute), `ask`
  (every send needs a one-time approval), or `observe` (read-only) —
  switchable live with `sats agent mode`, where tightening never needs
  the password and widening always does;
- total budget, including transaction fees;
- optional per-transaction amount cap, and an optional hard ceiling
  above which a send is never approvable;
- per-transaction fee cap — defaulted when not given to 2% of the budget,
  at least 1000 sats and never above the budget, so one bad fee estimate
  cannot burn the whole budget as miner fees; only the explicit
  `--no-max-fee` issues a grant without one;
- expiry timestamp;
- network binding;
- running spend and transaction counts;
- the SHA-256 of a bearer token.

Agent names are path components on disk, so they are restricted to 1-32
characters of `a-z`, `0-9`, `-` or `_` — enforced at grant creation, at
the daemon's socket boundary, and again inside the store.

Creating the grant requires the human's wallet password, but nothing derived
from it enters the grant: **the grant file holds no key material.** A random
32-byte token is minted, printed once, and never persisted — only its hash
is stored, compared in constant time. Reading a grant file yields a budget
and a hash, and nothing that can spend.

The token is what an agent presents. Pass it to the served process as
`SATS_AGENT_TOKEN`; `sats agent grant` prints the exact `claude mcp add`
line. Re-issuing a grant mints a new token and kills the old one, so
rotation and revocation are the same act.

A grant names its network and authorizes only that network. Loading a grant
checks the record's network against the one requested and refuses a
mismatch, so a grant file moved or copied into another network's directory
is inert — a signet grant can never authorize a mainnet signature. The token
is likewise scoped to the grant that holds its hash.

The honest cost is that a token in an agent's configuration is a secret that
agent can also read. Compromising it costs the grant's remaining budget until
its expiry — which is what a budget is for — never the seed, and never any
other network. Use small budgets and short expiries; revoke when not needed.

Hardware- or passkey-backed `Signer` implementations can strengthen the
daemon's own boundary further without changing transaction planning.

### Grant format versions

The current on-disk grant format is v3. The v3 fields are all restrictive
and all default to their permissive v2 meaning, so a v2 record deserializes
to exactly its prior behavior: mode `auto`, no hard amount ceiling, no
recipient rule, not suspended, no strikes.

- `mode`: `auto`, `ask`, or `observe` — how much standing autonomy the
  grant carries;
- `ask_max_tx_sat`: a hard per-transaction ceiling above which a send is
  never approvable;
- `allowed_recipients`: a standing recipient allowlist in the digest's
  normalized spelling (`null` means unrestricted, an empty list means every
  recipient asks);
- `suspended`: the STOP state, with its trigger and timestamp;
- `strikes`: the refusal-storm breaker's bounded, deduplicated bookkeeping.

Loading a grant refuses `format_version` values above what the build
understands, with an error naming the fix: a newer sats may have written
restrictions this build cannot see, and ignoring them would widen the
agent's authority. The honest caveat runs the other way in time: binaries
older than v3 predate that check and ignore unknown fields, so a v3 grant
carrying `mode: observe` would be enforced by such a binary as plain
auto-within-caps. Do not run pre-v3 binaries against a data directory
holding v3 grants; upgrade every binary that shares the `SATS_DIR`.

Releases before the daemon (v1) stored the master seed re-sealed under a
key in the same grant file, so any process that could read it could sign
without asking. Those records are read well enough to name themselves and
are then **refused**, with the commands that replace them; honoring one
would preserve exactly the weakness the daemon removes. `sats agent list`
reports them rather than showing an empty table.

If an agent with shell access ever ran on a machine while a v1 grant existed,
treat the seed as disclosed: move the funds to a fresh wallet rather than
only revoking.

## Authorization order

The authorization engine is deterministic and pure, and every refusal is
typed. The ladder has two halves. The **hard envelope** runs first, and
none of its refusals can be lifted by a one-time approval:

1. expiry;
2. suspension (STOP);
3. observe mode;
4. intent authority (only send is grantable today);
5. arithmetic sanity — an amount + fee that overflows is the typed denial
   `amount_overflow`, never a saturated number a budget could pass;
6. the hard amount ceiling (`over_ask_max`), when the grant carries one.

Only then the **ask band**, whose refusals a human may approve exactly
once:

7. the recipient rule (`recipient_not_allowed`), when the grant carries an
   allowlist;
8. ask mode (`ask_required`);
9. per-transaction amount cap;
10. per-transaction fee cap;
11. remaining total budget.

The order is deliberate and frozen by tests: an approvable reason must
never mask a hard one, so a request that is both outside the allowlist and
above the hard ceiling reports the ceiling. `DenyReason::approvable()` is
the one predicate every surface keys off to decide whether a refusal may
carry an approval path at all.

Budget drawdown is amount plus fee. The MCP server performs a cheap
amount-only precheck through the same full ladder — recipient rule
included — prepares the transaction to learn the real fee, then makes the
final decision.

The engine takes the current time as an input, and the daemon's clock
fails closed: if the system clock cannot be read, sends refuse with the
typed error `clock_unavailable` rather than evaluating expiry against a
1970 fallback that would treat every grant as live.

After approval, sats reserves and persists the budget before signing. If
signing fails and no signature exists, the reservation is refunded. Once a
signature exists, the reservation remains even if saving or broadcast fails:
the transaction is already spendable outside sats.

The decision, reservation, and persistence happen under an advisory
per-network grant lock, shared with grant creation and revocation.
Concurrent sends — in one server or across processes — therefore serialize
their budget decisions instead of double-drawing, and a revocation cannot
be undone by an in-flight send's write.

The grant file is reloaded for every send, and again under the lock before
the budget draw, where the presented token is re-checked. Deleting the grant
with `sats agent revoke` therefore takes effect on the next send call, even
in an existing MCP session, and a grant replaced mid-flight refuses the older
token.

## Agent requests, idempotency, and the event log

Every agent send is recorded as a durable request under
`<network>/agent-requests/<agent>/`, keyed by the agent's optional
`request_id`, carrying the canonical intent digest (network, agent,
normalized recipient, amount — never the fee) and the resolved outcome.
Retrying a key whose earlier execution signed a transaction replays the
recorded outcome; it can never sign twice. Reusing a key for a different
intent is a typed error that mutates nothing. Denials are side-effect
free, so a keyed retry after a denial re-evaluates the same request.

Only authenticated callers write: a send for an agent with no grant on
file — a name that never had one, or one already revoked — is denied
without creating a request record or a journal line, exactly like a wrong
token. Recorded truth still outranks revocation, read-only: a keyed retry
of a send that signed before the revocation replays its recorded txid
from the existing record without writing anything new.

Each state transition of the agent path — request received, denial,
reservation, refund, signature, broadcast — appends one line to the
per-network event log at `<network>/events/log.jsonl`, linked by request id
and intent digest. Finalized transaction records carry an `origin` field
naming the surface, agent, request, and digest, so an agent-signed
transaction is attributable after the fact. Request records and the event
log contain recipients and amounts; both are written owner-only (0600),
like the transaction records beside them. The log is append-only and never
pruned by sats.

The log is also read tolerantly: a syntactically valid line whose event
kind or format version this build does not understand — written by a newer
sats — is shown raw by `sats agent log` with a warning, never hidden and
never fatal to the listing. The audit must survive the future.

Discovery of pending asks does not depend on the agent's own channel:
`sats agent requests --watch` streams each newly pending approvable
request from the local store, so an agent that misrepresents, downplays,
or simply never relays a denial cannot keep the human from seeing it.
The watch is read-only — no provider access, no grant writes, no events —
and approval itself stays in `sats agent approve`, which re-reads and
renders the exact durable request before the password prompt.

The event log also makes the one irreducible crash window visible: a
`reserved` event with no following `signed` or `refunded` means the process
died between persisting the budget draw and signing — budget is held for a
transaction that never existed, and a human resolves it by re-granting or
accepting the drawdown.

## One-time approvals

`sats agent approve` converts one denied request into a single-use
exception. Its trust model:

- the approval binds the request's canonical intent digest — network,
  agent, normalized recipient, amount — so it authorizes exactly the send
  the human reviewed, nothing adjacent;
- the fee is not part of the digest (it varies per preparation), so the
  approval carries its own explicit fee ceiling instead, shown before the
  password prompt; a prepared fee above it is the typed denial
  `approval_fee_exceeded`;
- creating an approval requires the wallet password — the prompt is the
  authorization, exactly as for grant creation. Dismissing a request and
  revoking its approval (`sats agent deny`) needs no password: reducing
  authority stays cheap;
- an approval lifts only refusals whose `DenyReason::approvable()` is
  true — the ask band: the quantitative caps, ask mode, and the recipient
  rule. It never lifts the hard envelope: not grant expiry or revocation
  (the kill switches that withdraw all authority at once — mechanically it
  cannot, because the signing key lives behind the grant file), not
  suspension, not observe mode, and not the hard amount ceiling, which is
  the human's pre-commitment that no amount of asking can move;
- consumption is single-use and persisted before the budget draw, under
  the same per-network lock as every grant write. A crash between the two
  writes burns the approval without signing — the failing direction is
  always toward less authority. A signing failure refunds budget but never
  re-arms the approval; only a fresh `sats agent approve` does.

A stolen approval file entry is inert: it names no key material, binds one
exact intent, and spends nothing without a live grant. A replayed approval
is refused by its `consumed_at` mark under the grant lock.

## MCP boundary

The served process is a shim. It prepares transactions and broadcasts them,
and carries a bearer token naming the grant it acts under; it holds no key
material, so nothing in it can sign.

It starts only when the named agent has a non-expired grant, `SATS_AGENT_TOKEN`
matches that grant, the wallet and provider configuration are valid, and satsd
is reachable. Each failure names its own remedy at `claude mcp add` time
rather than mid-conversation. A daemon that is running but locked is a
warning at startup, not a refusal — a human can unlock it later.

It exposes only:

- balance lookup;
- fresh receive address;
- the caller's grant status;
- read-only status of the caller's own send requests, so an agent can
  wait for a human decision without retrying sends;
- a bounded send operation.

Agents never receive the password, mnemonic, raw signer, arbitrary PSBT
signing tool, or the CLI's UTXO-safety bypass flags. Expected policy denials
are machine-readable results, not errors that invite a retry. Operational
conditions carry a typed `error_code` instead of a denial `reason`:
`wallet_locked` when no human has unlocked satsd, `daemon_unavailable` when
it cannot be reached.

MCP uses stdout as its protocol transport. Diagnostic information goes to
stderr so logs cannot corrupt protocol frames.

## Chain providers and asset guards

Transaction preparation requires fresh wallet state. A chain sync failure
stops every send mode — including `--dry-run` and `--export-psbt` — and MCP
sends rather than allowing coin selection from stale data. Balance, status,
and history are different by design: when sync fails they may report cached
state and say so.

UTXO protection has two layers:

1. a local heuristic excludes outputs worth exactly 546 or 330 sats;
2. every configured guard is asked which wallet outpoints carry assets.

The protected sets are unioned. Guards are restrictive-only: they can exclude
an outpoint but cannot make anything spendable or authorize a transaction. A
configured guard that cannot answer fails closed. Only a human CLI invocation
can bypass a guard or the dust heuristic, and each bypass applies to that
invocation only.

sats does not enable a default asset indexer. Configuring one is an explicit
trust decision. A dishonest or incorrect guard can cause denial of service by
over-protecting outputs; it cannot directly authorize a spend. An incomplete
guard can miss an asset, which is why bypasses and third-party results require
care.

Fee estimates are remote data and are bounded before use: a non-finite,
negative, or absurd rate (above 10,000 sat/vB) is a typed fee error rather
than a number the endpoint chose. Malformed checkpoint data from a
provider fails the sync instead of the process.

Subfrost URLs may contain API keys in their paths, so that driver exposes only
a redacted origin in errors and debug output. Esplora authentication is
expected in the configured bearer header; its endpoint URL may be displayed
in diagnostics. Do not embed Esplora credentials in URL paths or queries.

## Alkanes execution

`sats alkanes execute` is human-only and deliberately narrow:

- the OP_RETURN envelope is encoded locally by the `sats-alkanes` crate,
  against the published reference encoding, frozen by byte-vector tests —
  the provider composes nothing;
- simulation and inspection results are advisory display from the
  configured `alkanes.view` endpoint, never an authorization, and the
  endpoint's network is validated before any result is shown;
- the transaction pipeline is the ordinary send tail: dust and guard
  exclusions with no escape flags, confirmation, password unlock, private
  persistence before broadcast;
- mainnet is refused in this release, and no agent surface exists — the
  MCP server cannot reach any alkanes operation, and no grant can carry
  alkanes authority.

The wire dialect for the view calls has not been verified against a live
endpoint; a wrong dialect fails closed as a view error rather than
composing a transaction from misread data.

## PSBT and finalized-transaction boundary

PSBTs are the preparation and signer contract. A normal human or agent send
keeps the PSBT in memory. `sats send --export-psbt` is the explicit
exception: it writes the unsigned PSBT to an owner-only file artifact the
user names. Successful `sats psbt sign` converts an artifact — or a stored
session from an older release, by explicit `--session` id — into a private
raw finalized-transaction record. Stored sessions and pre-refactor plan
files remain readable, permission-hardened, and are converted and deleted
when used; new releases never write them.

`sats psbt sign FILE` accepts an external base64 or binary PSBT; a PSBT that
still needs other signers is written back as a signed artifact rather than
entering sats-managed transaction state. External PSBTs are untrusted input:
`sats psbt inspect` shows destinations, amounts, fee, and signing state, and
an independent tool should confirm them before signing.

The local signer may partially sign a PSBT that requires additional signers.
That is reported as partial rather than treated as a broadcastable success.

## Threat summary

| Threat | Implemented boundary | Remaining risk |
|---|---|---|
| Stolen watch-only database | No private descriptors in SQLite | Address history and balances may be exposed |
| Stolen sealed seed | Argon2id plus authenticated encryption | Password strength and offline guessing |
| Read grant file | Holds a budget and a token hash, no key material | Reveals amounts and expiry |
| Stolen agent token | Budget, per-tx caps, and expiry enforced by satsd | Spends that grant's remaining budget until it expires |
| Lied-about send amount or fee | Recomputed from the PSBT against the wallet's descriptors | An understated input burns the caller's own budget on an unrelayable transaction |
| Compromised served process | Holds a token, never a key | Same as a stolen token |
| Debugger attached to satsd | Process memory only; same-user | Seed recoverable where ptrace is permitted |
| Grant left in v1 format | Read, reported, and refused for signing | The file itself is a seed disclosure until the wallet is rotated |
| Revoked agent session | Grant reloaded on every send | A transaction signed before revocation remains valid |
| Provider outage | Planning and configured guards fail closed | Loss of availability |
| Malicious asset guard | Restrictive-only result | Can hide funds; incomplete results can miss assets |
| Broadcast failure | Finalized raw transaction saved before the attempt; agent budget remains reserved | Manual retry or reconciliation is required |
| Wrong Bitcoin network | Address and provider network validation | Misconfigured third-party responses remain possible |
| Malicious alkanes view endpoint | Advisory display only; local encoding; mainnet refused | Can mislead the human reviewing a signet call |

## Operational guidance

- Learn the flow on signet before selecting mainnet.
- Keep wallet backups and verify recovery independently.
- Use a strong, unique wallet password.
- Run sats only on a machine and user account you trust.
- Keep grant budgets small, set fee caps, and prefer short expiries.
- Review `sats agent list` regularly and revoke unused grants.
- Lock satsd (`sats daemon lock`) when no agent needs to spend, and keep
  `--auto-lock` no longer than the work actually requires.
- Treat an agent token like the budget it unlocks: re-issue the grant to
  rotate it, and never commit one to a repository.
- Treat provider endpoints and their responses as part of your trust model.
- Never paste a real mnemonic into issues, logs, screenshots, tests, or agent
  conversations.
