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
transaction records. The boundary protects against partial writes and other
OS users, but not against a process already running as the wallet's user.

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

## Agent grants

`sats agent grant <name>` creates bounded unattended authority with:

- total budget, including transaction fees;
- optional per-transaction amount cap;
- optional per-transaction fee cap;
- expiry timestamp;
- network binding;
- running spend and transaction counts.

Creating the grant requires the human's wallet password. The mnemonic is
re-sealed under a fresh random grant key, and the wrapped seed plus key are
stored in the grant file. The password is not stored and is never sent to the
agent.

The honest cost is that the active grant file contains everything needed to
recover the seed. Its boundary is the wallet user's filesystem permission and
the grant's lifetime, not a hardware security module. A process that can read
the grant file as that user can exceed the policy by extracting the seed.

Use small budgets and short expiries. Revoke grants when they are not needed.
Hardware- or passkey-backed `Signer` implementations can strengthen this
boundary without changing transaction planning.

## Authorization order

The authorization engine is deterministic and pure. Checks occur in this
order:

1. expiry;
2. per-transaction amount cap;
3. per-transaction fee cap;
4. remaining total budget.

Budget drawdown is amount plus fee. The MCP server performs a cheap
amount-only precheck, prepares the transaction to learn the real fee, then
makes the final decision.

After approval, sats reserves and persists the budget before signing. If
signing fails and no signature exists, the reservation is refunded. Once a
signature exists, the reservation remains even if saving or broadcast fails:
the transaction is already spendable outside sats.

The decision, reservation, and persistence happen under an advisory
per-network grant lock, shared with grant creation and revocation.
Concurrent sends — in one server or across processes — therefore serialize
their budget decisions instead of double-drawing, and a revocation cannot
be undone by an in-flight send's write.

The grant file is reloaded for every send. Deleting it with `sats agent revoke`
therefore takes effect on the next send call, even in an existing MCP session.

## Agent requests, idempotency, and the event log

Every agent send is recorded as a durable request under
`<network>/agent-requests/<agent>/`, keyed by the agent's optional
`request_id`, carrying the canonical intent digest (network, agent,
normalized recipient, amount — never the fee) and the resolved outcome.
Retrying a key whose earlier execution signed a transaction replays the
recorded outcome; it can never sign twice. Reusing a key for a different
intent is a typed error that mutates nothing. Denials are side-effect
free, so a keyed retry after a denial re-evaluates the same request.

Each state transition of the agent path — request received, denial,
reservation, refund, signature, broadcast — appends one line to the
per-network event log at `<network>/events/log.jsonl`, linked by request id
and intent digest. Finalized transaction records carry an `origin` field
naming the surface, agent, request, and digest, so an agent-signed
transaction is attributable after the fact. Request records and the event
log contain recipients and amounts; both are written owner-only (0600),
like the transaction records beside them. The log is append-only and never
pruned by sats.

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
- an approval lifts only the grant's quantitative caps (per-transaction
  amount, fee, budget). It never overrides grant expiry or revocation:
  those are the kill switches that withdraw all authority at once, and an
  exception issued earlier must not survive them — mechanically it cannot,
  because the signing key lives inside the grant file;
- consumption is single-use and persisted before the budget draw, under
  the same per-network lock as every grant write. A crash between the two
  writes burns the approval without signing — the failing direction is
  always toward less authority. A signing failure refunds budget but never
  re-arms the approval; only a fresh `sats agent approve` does.

A stolen approval file entry is inert: it names no key material, binds one
exact intent, and spends nothing without a live grant. A replayed approval
is refused by its `consumed_at` mark under the grant lock.

## MCP boundary

The MCP server starts only when the named agent has a non-expired grant and
the selected wallet and provider configuration are valid. It exposes only:

- balance lookup;
- fresh receive address;
- the caller's grant status;
- a bounded send operation.

Agents never receive the password, mnemonic, grant key, raw signer, arbitrary
PSBT signing tool, or the CLI's UTXO-safety bypass flags. Expected policy
denials are machine-readable results, not errors that invite a retry.

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
| Stolen active grant | OS file permissions and expiry | File contains recoverable unattended signing material |
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
- Treat provider endpoints and their responses as part of your trust model.
- Never paste a real mnemonic into issues, logs, screenshots, tests, or agent
  conversations.
