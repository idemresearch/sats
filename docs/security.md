# Security and trust model

sats separates watch-only wallet state from signing material, makes spending
authority explicit, and treats every signature as a point of no return. Its
core invariant: **no agent-originated spend reaches the signer without
explicit human authorization bound to that exact request** — an
unapproved agent request is pending or denied, never a signature. Only a
human-authorized execution path may invoke the signer for an
agent-originated request; in v0.0.1 that path is `sats agent approve`.
This document describes the implemented boundaries and their costs; the
direction they serve is in [Direction](direction.md).

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
This includes sealed keys, grants, agent requests, and finalized
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

## Executing an agent request

Agents create requests. Humans authorize requests. sats executes
requests. There is no resident signer: no process holds an unsealed seed
between executions. `sats agent approve` is the human-authorized
execution path, and it runs in the human's own process:

1. the request is claimed (a per-request lock); the grant on file must
   be the instance that created the request (its `grant_id` — 128
   random bits, never reused by a re-issue — is recorded at filing,
   under the grant lock), and it is re-checked with the fee unknown;
2. the transaction is prepared on current chain state through the same
   pipeline as a human send, with no dust or guard bypass;
3. what the prepared PSBT actually pays is derived from the wallet's own
   descriptors and must equal the recorded request's recipient and amount;
4. the grant's ladder re-runs with the real fee;
5. the human sees recipient, amount, fee, total, and remaining budget, and
   enters the wallet password — the authorization, and the only moment
   key material is unsealed;
6. under the grant lock the budget is drawn on the grant's reservation
   ledger under the request's id and persisted, then the request is
   persisted as `signing` — before the signer is invoked, so `signing`
   on disk means the signer may have run;
7. the signer is constructed only now, signs, and the finalized
   transaction is persisted before broadcast;
8. broadcast settles the request to `sent`, or to `broadcast_pending` if
   the provider refused.

The irreversible boundary is the invocation of `Signer::sign`, not a
successful write afterwards. Before it — the audit append, the signer's
construction — a failure returns the draw and the request is `failed`
(re-approvable), because the executing process knows no signature
exists. From the invocation on, nothing the signer reports is trusted
to mean "no signature": a signer may sign, or leak a signature, and then
return an error. An error, an unfinalized result, or a failure to
finalize or save the transaction leaves the request `unresolved`: a
signature may exist, so nothing is refunded and sats never signs it
again. A human resolves it (checking `sats status`, then dismissing).
`broadcast_pending` and `sent` mean a signed transaction is persisted:
the draw is never returned, the signer is never invoked again for that
request, and only rebroadcasting the saved transaction (`sats tx
broadcast <txid>`) settles it.

The budget draw is a ledger entry on the grant, keyed by the request
id, updated in the same atomic write as the grant's totals. One request
holds at most one draw; the same request with the same amount and fee
draws nothing again, and a different amount or fee for a request that
already holds a draw fails closed. A draw is returned exactly once, and
only when the durable record proves the signer was never invoked for
the request's current attempt: the record is `pending_approval`,
`failed`, or `denied`, which `signing` and every later state follow. A
draw held by a `signing`, `unresolved`, `sent`, `broadcast_pending`, or
`dismissed` request, or by a request that cannot be read, is never
returned. Recovery from any crash is therefore a function of the two
records: before the draw is persisted, nothing exists; after the draw
and before `signing`, the draw is an orphan and the next listing,
approve, or dismiss returns it; after `signing`, the request is
`unresolved` and the draw stays; after `failed` and before the refund,
the draw is an orphan and is returned once. A request that signed
keeps its entry, so the ledger is also the grant's spend history.

An interrupted execution is reconciled from that boundary, never
guessed: a request left `signing` by a dead process becomes
`broadcast_pending` or `sent` when a persisted transaction attributed to
it — same agent, request id, and intent digest — exists, and otherwise
`unresolved`, with nothing refunded and nothing signed again. A crash can
therefore never cause a second signature for one authorized execution or
a refund after a signature may exist.

Because execution happens in the approving process with the human's
password, an approval is not a durable token another process trusts: a
forged "approved" record on disk gains nothing without the password.

## Agent grants

`sats agent grant <name>` creates bounded authority to propose — never to
sign. A grant defines what an agent is allowed to propose; every boundary
it carries is hard, and human approval authorizes one valid proposal
inside the grant, never past it. A grant carries:

- an authority mode — `ask` (the default: every request waits for the
  human) or `observe` (read-only) — switchable live with
  `sats agent mode`, where tightening never needs the password and
  widening always does. There is no autonomous mode;
- total budget, including transaction fees;
- an optional per-transaction amount cap — an amount above it is the
  hard refusal `over_max_tx`, and the only escalation is changing the
  grant;
- an optional standing recipient allowlist (`--to`, edited with
  `sats agent allow`/`disallow`): other recipients are the hard refusal
  `recipient_not_allowed`. Entries change only through the control
  plane — never through payment history, successful sends, or
  approvals, so an agent cannot launder an address into "known" by
  paying it once;
- a per-transaction fee cap, always present — chosen with `--max-fee`,
  otherwise defaulted to 2% of the budget, at least 1000 sats and never
  above the budget — so one bad fee estimate cannot burn the whole
  budget as miner fees;
- expiry timestamp;
- network binding;
- running spend and transaction counts;
- the SHA-256 of a bearer token.

Agent names are path components on disk, so they are restricted to 1-32
characters of `a-z`, `0-9`, `-` or `_` — enforced at grant creation, at
the MCP boundary, and again inside the store.

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
agent can also read. Compromising it yields the authority to *ask*: a
thief can file spend proposals — visible in `sats agent requests`, bounded
by the grant, until its expiry — and can read what the grant's tools
expose. It cannot cause a signature (every send waits for your approval),
never touches the seed, and never reaches any other network. Use small
budgets and short expiries; revoke when not needed.

Hardware- or passkey-backed `Signer` implementations can strengthen the
executor's signing boundary further without changing transaction planning.

### Grant format

The on-disk grant format is `format_version: 1` — the first released
schema. sats is pre-release, so earlier development shapes are not
migrated: a record that does not parse as the current schema (for
example, one spelling the removed development-era `auto` mode, or one
missing a required field) fails with an error naming the fix — revoke
and grant again. Loading a grant refuses `format_version` values above
what the build understands: a newer sats may have written restrictions
this build cannot see, and ignoring them would widen the agent's
authority.

The one deliberately recognized older shape is the pre-daemon grant
that stored the master seed re-sealed under a key in the same file, so
any process that could read it could sign without asking. Those records
are read well enough to name themselves and are then **refused**, with
the commands that replace them; honoring one would preserve exactly the
weakness the grant model removes. `sats agent list` reports them rather than
showing an empty table.

If an agent with shell access ever ran on a machine while such a
wrapped-seed grant existed, treat the seed as disclosed: move the funds
to a fresh wallet rather than only revoking.

## Authorization order

The authorization engine is deterministic and pure, and every refusal is
typed. The ladder never allows a spend on the grant alone: a proposal
inside every boundary is the verdict `ask`, which becomes a
`pending_approval` request, so "no unapproved agent-originated request
may reach the signer" is a structural property of the ladder, not a
policy setting. Every rung is a hard grant boundary — none of them can
be lifted by an approval:

1. expiry;
2. observe mode;
3. arithmetic sanity — an amount + fee that overflows is the typed denial
   `amount_overflow`, never a saturated number a budget could pass;
4. the recipient allowlist (`recipient_not_allowed`), when the grant
   carries one;
5. per-transaction amount cap (`over_max_tx`);
6. per-transaction fee cap (`over_max_fee`);
7. remaining total budget (`over_budget`);
8. the terminal verdict, ask: a proposal inside every boundary still
   waits for a human. It is not a denial — the request is
   `pending_approval` — and it is the only non-denial the ladder has.

The order is deliberate and frozen by tests: the specific boundary
answers before the terminal ask, so a refusal names what was crossed
instead of the generic ask. Every denial is a grant boundary; there is no
approvable denial and no denial a human is asked about.

Budget drawdown is amount plus fee, and only an authorized, signed
execution draws it: a denial consumes nothing, and so does a pending
request. Filing runs the full ladder — recipient rule included — with
the fee unknown, so a proposal outside the grant is recorded as denied
before any network access. Execution prepares the transaction to learn
the real fee and re-runs the ladder with it, under the grant lock, before
the reservation: a draw can never exceed the remaining budget, and a fee
above the cap is the denial `over_max_fee` even for a request a human
just authorized.

The engine takes the current time as an input, and the clock fails
closed: if the system clock cannot be read, filing and execution refuse
with the typed error `clock_unavailable` rather than evaluating expiry
against a 1970 fallback that would treat every grant as live.

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

## Agent requests and the event log

Every agent request is a durable record under
`<network>/agent-requests/<agent>/`, keyed by the agent's optional
`request_id`, carrying the canonical intent digest (network, agent,
normalized recipient, amount — never the fee) and its state:
`pending_approval`, `denied`, `dismissed`, `signing`, `unresolved`,
`sent`, `broadcast_pending`, or `failed` — and the `grant_id` of the
grant instance that created it. The record's id is global — `r-` plus
16 hex characters — and is what humans approve and dismiss by. A keyed
filing derives its id from the grant id, the agent, and the client key,
so the same key from two agents, or from the same agent under a
re-issued grant, names two requests; a keyless filing gets a random id.
Filing the same key with the same intent returns the existing record
without writing; reusing a key for a different intent is a typed error
that mutates nothing. The agent never executes and never retries to
make a payment happen: it observes.

Only authenticated callers write: a filing for an agent with no grant on
file — a name that never had one, or one already revoked — is refused
without creating a request record or a journal line, exactly like a wrong
token. Filing holds the grant lock from the grant read through the record
write, so a revoke or re-issue cannot interleave, and the audit line is
appended before the record is written, so no request exists on disk
without its causal event.

Each state transition — request received, denial, approval, dismissal,
reservation, refund, signature, broadcast — appends one line to the
per-network event log at `<network>/events/log.jsonl`, linked by request
id and intent digest. Finalized transaction records carry an `origin`
field naming the surface, agent, request, and digest, so an agent-
originated transaction is attributable after the fact, and so an
interrupted execution can be reconciled from the record. Request records
and the event log contain recipients and amounts; both are written
owner-only (0600), like the transaction records beside them. The log is
append-only and never pruned by sats.

The log is also read tolerantly: a syntactically valid line whose event
kind or format version this build does not understand — written by a newer
sats — is shown raw by `sats agent log` with a warning, never hidden and
never fatal to the listing. The audit must survive the future.

Discovery of pending requests does not depend on the agent's own channel:
`sats agent requests --watch` streams each newly pending request from the
local store, so an agent that misrepresents, downplays, or simply never
relays its status cannot keep the human from seeing it. The watch is
read-only — no provider access, no grant writes, no events — and
authorization stays in `sats agent approve`, which prepares and renders
the exact transaction before the password prompt.

## Human authorization

`sats agent approve` is the one place an agent-originated request reaches
the signer. Its trust model:

- the request's canonical intent digest — network, agent, normalized
  recipient, amount — is what the human reviews, and the executor refuses
  to sign a PSBT whose derived payment differs from it, so the human
  authorizes exactly the send they saw, nothing adjacent;
- the fee is not part of the digest (it varies per preparation): the
  human sees the real fee before authorizing, and the grant's
  always-present fee cap bounds it like every other boundary;
- the wallet password is the authorization, exactly as for grant
  creation, and it is also what unseals the key: there is no resident
  signer for a forged approval record to exploit. Dismissing a request
  (`sats agent dismiss`) needs no password: reducing authority stays
  cheap;
- authorization executes only a proposal inside the grant. It never lifts
  a grant boundary: not expiry or revocation (the kill switches that
  withdraw all authority at once), not observe mode, not the recipient
  allowlist, and not the amount, fee, or budget bounds, which are the
  human's pre-commitments that no amount of asking can move. The grant is
  re-read at staging and again under the grant lock before the
  reservation, so a boundary tightened between the review and the
  password still refuses;
- an authorization executes at most once: a request that signed, or may
  have signed, is terminal for the signer, and a request whose execution
  stopped before any signature could exist must be authorized again by a
  fresh human decision;
- a request is bound to the grant instance that created it. Revoking or
  re-issuing the grant makes every request filed under it `denied` with
  reason `revoked` the moment a human tries to execute one; a new grant
  for the same agent never inherits requests it did not authorize;
- the review — recipient, amount, real fee, total — is shown in every
  mode before the password, on stderr when stdout is a JSON stream, so
  the authorization is always bound to visible transaction details.

## MCP boundary

The served process reads the wallet and files requests under the grant
its bearer token names. It holds no key material, never prepares or signs
a transaction, and never broadcasts; nothing in it can move money.

It starts only when the named agent has a non-expired grant,
`SATS_AGENT_TOKEN` matches that grant, and the wallet and provider
configuration are valid. Each failure names its own remedy at
`claude mcp add` time rather than mid-conversation.

It exposes only:

- balance lookup;
- fresh receive address;
- the caller's grant status;
- filing a send request — the one tool that touches authority, and it
  cannot cause a signature: a request inside the grant waits for a human;
- read-only status of the caller's own requests, so an agent can observe
  a human decision without doing anything.

Agents never receive the password, mnemonic, raw signer, arbitrary PSBT
signing tool, an unlock tool, or the CLI's UTXO-safety bypass flags.
Expected policy denials are machine-readable `denied` requests, not
errors that invite a retry. Operational conditions carry a typed
`error_code` instead.

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
user names. Successful `sats psbt sign` converts an artifact into a
private raw finalized-transaction record.

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
| Stolen agent token | Every request waits for a human's password-gated approval; caps and expiry enforced at filing and execution | Files requests in your review queue and reads what the grant exposes, until revoked or expired |
| Lied-about send amount or fee | Recomputed from the PSBT against the wallet's descriptors | An understated input burns the caller's own budget on an unrelayable transaction |
| Compromised served process | Holds a token, never a key; a forged approval record cannot sign without the password | Same as a stolen token |
| Debugger attached to an approving process | Key material exists only for the duration of one approve | Seed recoverable during that window where ptrace is permitted |
| Pre-daemon wrapped-seed grant on disk | Read, reported, and refused for signing | The file itself is a seed disclosure until the wallet is rotated |
| Revoked agent session | Grant reloaded at every filing and under the lock at execution | A transaction signed before revocation remains valid |
| Provider outage | Planning and configured guards fail closed | Loss of availability |
| Malicious asset guard | Restrictive-only result | Can hide funds; incomplete results can miss assets |
| Broadcast failure | Finalized raw transaction saved before the attempt; the request is `broadcast_pending` with budget reserved | `sats tx broadcast` retries it |
| Wrong Bitcoin network | Address and provider network validation | Misconfigured third-party responses remain possible |
| Malicious alkanes view endpoint | Advisory display only; local encoding; mainnet refused | Can mislead the human reviewing a signet call |

## Operational guidance

- Learn the flow on signet before selecting mainnet.
- Keep wallet backups and verify recovery independently.
- Use a strong, unique wallet password.
- Run sats only on a machine and user account you trust.
- Keep grant budgets small, set fee caps, and prefer short expiries.
- Keep `sats agent requests --watch` running while an agent works, and
  review each request — recipient, amount, and the real fee — before
  entering your password.
- Review `sats agent list` regularly and revoke unused grants.
- Treat an agent token like the budget it unlocks: re-issue the grant to
  rotate it, and never commit one to a repository.
- Treat provider endpoints and their responses as part of your trust model.
- Never paste a real mnemonic into issues, logs, screenshots, tests, or agent
  conversations.
