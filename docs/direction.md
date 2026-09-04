# Product & security direction

The stable design decisions behind sats: what the product is for, where
authority lives, and the invariant every surface serves. Shipped behavior
is documented in [Architecture](architecture.md), [Security](security.md),
[CLI](cli.md), and [MCP](mcp.md); when those change, this document is the
direction they change toward.

## The thesis

Traditional wallets assume a human directly operates the wallet UI.
Agentic software changes the interface: a human increasingly says "what's
my balance?", "prepare a payment to Alice", "pay this invoice" — and an AI
understands the request, gathers information, chooses parameters, and
prepares the action.

Planning an action must not imply authority to sign it. sats separates:

```text
intent → policy → human authorization → signing → execution
```

The AI participates in the first two stages. The human remains the
authority over the signer.

> **Agents can observe, reason, prepare, and request.
> Humans authorize money movement.**

## The invariant

> **No agent-originated spend may reach the signer without explicit,
> one-time human authorization bound to that action. An unapproved
> agent-originated spend must resolve to ASK or DENY — never directly
> to signing.**

The agent never has signing authority. An agent can read balances, derive
receive addresses, inspect its grant, and file spend proposals. The human's
approval process prepares the transaction; MCP does not expose preparation.
The agent cannot unlock the wallet for itself, obtain the private key, approve its own payment, cause an unattended signature, or
silently modify a transaction after approval.

Internal allowance *after* a valid human approval is legitimate — the
security property is whether an unapproved agent request can reach the
signer. There is no autonomous spend mode, deliberately: if unattended
agent spending is ever explored, it must be a separate security model
with explicitly delegated keys or script-enforced bounds, never a mode
bit in the process that holds the seed.

## The verdicts

Every agent action resolves through three verdicts:

- **ALLOW** — the action needs no signing authority: read balance,
  derive a receive address, inspect the grant, or poll a request. The shipped
  MCP surface does not expose history or transaction preparation.
- **ASK** — the normal verdict for a valid agent-originated request: a
  durable `pending_approval` request is filed for exactly this intent,
  and a human decides.
- **DENY** — the proposal violates a boundary the human pre-committed to:
  the grant expired or was revoked, the amount or fee exceeds a cap, the
  budget is spent, the recipient is off the allowlist, the intent is not
  grantable. Grant boundaries are never approvable, and the human is
  never bothered with them.

## Grants are not signing authority

An agent's grant defines what it may read and what it may *propose*:
expiry, a total budget, a per-transaction amount cap, a fee cap, a
recipient allowlist, a network binding. Every boundary is hard, and a
human approval authorizes one valid proposal inside them — approval
cannot exceed the grant. A grant carries no key material — only the
hash of the agent's bearer token beside the policy.

A budget therefore means: *"this agent may ask me to authorize up to this
much, under these constraints."* It does not mean the agent controls an
autonomous signing key with that amount.

Budgets still matter with a human in the loop: policy is the first line
of defense that keeps obviously invalid proposals out of the review
queue, and the budget bounds the economic actions sats will authorize. A
denied request consumes no budget; only an approved, signed send draws
amount plus fee.

## Trust zones

```text
UNTRUSTED            AI agent / MCP client — reads, prepares, requests
AUTHORIZATION        policy ladder, durable requests, PSBT verification
HUMAN APPROVAL       CLI review queue today; any trusted surface tomorrow
TRUSTED SIGNER       the signing boundary that owns key material
```

The architecture preserves these responsibilities even where several of
them currently run inside one process. MCP is an adapter over the
authorization engine — never the security boundary — and must not expose
any primitive whose semantics are "sign anything". The signer is a trait
so the boundary can strengthen (hardware-, passkey-, or device-backed
signers) without changing transaction planning.

Humans and agents share the wallet but not the authority model: a human
using the CLI authenticates and proceeds directly; an agent's proposal
always crosses the human approval boundary first.

## The approval loop

Agent requests never block waiting on a human:

```text
agent creates a request
  → policy verdict: ASK (pending) or DENY
  → durable request with an id
  → a trusted surface shows it (sats agent requests --watch)
  → the human approves or dismisses
  → the approving process prepares, re-verifies, signs, broadcasts
  → the request's status is updated
  → the agent only observes the result
```

Agents create requests. Humans authorize requests. sats executes requests.
The agent takes no action after filing — it never retries to make a
payment happen. The request is a product primitive, not incidental UI,
and the discovery channel must not depend on the agent relaying its own
status.

## Intent authorization

Never trust the caller's description of a transaction; derive it
independently. The human approves a canonical intent — network, agent,
normalized recipient, amount — and the approval binds its digest, so it
authorizes exactly the send the human saw, once. Before signing, sats
recomputes what the prepared transaction actually pays from the wallet's
own descriptors and refuses on any disagreement. An approval must never
be reusable for a materially different transaction.

PSBT is the Bitcoin-native seam between preparation and signing, and the
path to stronger signers: hardware wallets, independent-device review,
offline signing, multi-signer flows. Human approval of intent and the
cryptographic signature are distinct concepts even when one interaction
covers both.

## Current implementation, honestly

The invariant holds today; several boundaries are process-internal
rather than device-separated, and they are implementation facts, not the
model:

- the signer is an in-memory software signer constructed inside the
  approving process, from the seed the human's password unseals for that
  one execution; no process holds an unsealed seed between executions;
- the human reviews the recorded request and the prepared transaction's
  real fee, not the raw PSBT — the executor's independent derivation
  enforces the exact-transaction property instead;
- the only human-authorized execution path is the CLI; a desktop or
  mobile approval client would call the same execution layer.

These are the debt to pay down — with hardware or independent-device
signers and richer review surfaces — not properties to market away.

## Threat model and claims

Assume the agent is hostile: prompt injection, malicious clients,
request replay, recipient or amount substitution, fee manipulation,
forged approval state. The design answers with no agent-side key
material, server-side grant checks on every send, independent intent
derivation, durable digest-bound approvals, and an approval surface
outside the agent's control.

Claims sats can make: agents do not receive your keys; agents cannot
approve their own payments; agent-originated money movement requires
human authorization; sats independently verifies the transaction intent
before signing. Claims sats must not make unless they become technically
true: that the seed is never decrypted anywhere, that a compromised
operating system cannot defeat the wallet, or that every approval is a
hardware-backed signature.

## Principles

1. The agent never has signing authority.
2. Money movement requires human authorization.
3. Grants constrain requests; they do not grant keys.
4. ALLOW is for operations that need no signing authority; ASK is the
   normal verdict for valid agent-originated spends; DENY holds the hard
   boundaries.
5. Never trust the caller's interpretation of a transaction.
6. Bind approval to the exact action being authorized.
7. Keep the signer replaceable and MCP an adapter.
8. Make read-only automation frictionless; make irreversible actions
   explicit and auditable.
9. Prefer boring, inspectable security over invisible autonomy.

The one-line architecture:

```text
Agent prepares → sats verifies intent → policy asks → human approves
→ sats re-verifies → signer signs → Bitcoin
```

> **AI asks. You approve. Keys stay yours.**
