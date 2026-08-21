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

The grant file is reloaded for every send. Deleting it with `sats agent revoke`
therefore takes effect on the next send call, even in an existing MCP session.

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
