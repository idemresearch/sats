# Product & security direction

Why sats separates asking from signing: the one invariant every surface
serves, and the design decisions that follow from it.

What is implemented today is documented in [Security](security.md),
[Architecture](architecture.md), [CLI](cli.md), and [MCP](mcp.md). When those
change, this page is the direction they change toward.

## The thesis

Wallets have assumed a human operates the interface. Increasingly, a human
says "pay this invoice" and an AI gathers the details and prepares the
action. Preparing an action must not imply the authority to sign it:

```text
intent → policy → human authorization → signing → execution
```

The agent takes part in the first two stages. The human remains the authority
over the signer.

This is maker-checker, the separation of duties that finance uses for money
movement, applied to agents. The agent is the maker and the human is the
checker, and the maker can never check its own work. sats goes one step
further than classic maker-checker. The checker's decision binds to an
intent, and sats verifies that the transaction it signs matches that intent,
rather than trusting the maker's description. The pattern covers
agent-originated payments. A human spending directly is both maker and
checker, and the checker is always one person: sats is not multi-person
approval or multisig.

> **Agents create requests. Humans authorize requests. sats executes
> requests.**

## The invariant

> **No agent-originated spend may reach the signer without explicit,
> one-time human authorization bound to that action. An unapproved
> agent-originated spend resolves to ASK or DENY, never to signing.**

The agent never has signing authority. It can read balances, derive receive
addresses, inspect its grant, and file proposals. It cannot unlock the
wallet, obtain a key, approve its own payment, cause an unattended signature,
or change a transaction after approval.

There is no autonomous spend mode, deliberately. If unattended agent spending
is ever explored, it must be a separate security model with delegated keys or
script-enforced limits, never a mode bit in the process that holds the seed.

## Three verdicts

Every agent action resolves to one of:

- **ALLOW**: the action needs no signing authority, such as reading the
  balance, deriving an address, inspecting the grant, or observing a request.
- **ASK**: the normal verdict for a valid spend proposal. sats files a
  durable `pending_approval` request, and a human decides.
- **DENY**: the proposal crosses a boundary the human committed to in
  advance, such as expiry, an amount or fee cap, the budget, or the
  recipient allowlist. Denials are never approvable, so the human is never
  asked about them.

## The request is the product

A request is a durable record, not a terminal prompt. It outlives the agent's
session and belongs to no single client, so how a human discovers, reviews,
and decides it is independent of the CLI. Discovery must never depend on the
agent reporting its own status.

Any surface that executes a request is held to the same rules as
`sats agent approve`: fresh human authorization bound to that exact request,
and no long-lived process that keeps an unsealed seed.

## Grants bound proposals, not keys

A grant defines what an agent may *propose*: expiry, total budget,
per-transaction amount and fee caps, a recipient allowlist, and a network.
Approval authorizes one proposal inside those limits and can never exceed
them. A grant holds no key material, only the hash of the agent's token.

A budget therefore means "this agent may ask me to authorize up to this
much", not "this agent controls a key holding this much". The budget still
matters with a human in the loop: it keeps invalid proposals out of the
review queue and bounds what sats will ever authorize.

## Trust zones

```text
UNTRUSTED            AI agent / MCP client: reads, requests
AUTHORIZATION        policy ladder, durable requests, PSBT verification
HUMAN APPROVAL       local CLI review and wallet password
TRUSTED SIGNER       the signing boundary that owns key material
```

These responsibilities stay separate even where several of them run in one
process today. MCP is an adapter over the authorization engine, never the
security boundary, and it must never expose a "sign anything" primitive. The
signer is a trait, so the boundary can strengthen to hardware, passkey, or
device-backed signers without changing transaction planning.

## Never trust the caller

The human approves a canonical intent: network, agent, normalized recipient,
and amount. Before signing, sats derives what the prepared transaction
actually pays from the wallet's own descriptors and refuses any mismatch. An
approval can never be reused for a different transaction.

PSBT is the seam between preparation and signing, and the path to stronger
signers: hardware wallets, independent-device review, offline and
multi-signer flows.

## What is true today

The invariant holds. Some boundaries are process-internal rather than
device-separated:

- The signer is an in-memory software signer, built inside the approving CLI
  process from the seed your password unseals for that one execution. No
  long-running process keeps an unsealed seed.
- The human reviews the recorded request and the real fee, not the raw PSBT.
  Independent derivation of what the PSBT pays enforces the exact-transaction
  property.
- The only execution path for an agent request is `sats agent approve`.
  There is no daemon and no durable approval another process can reuse.

## What sats claims, and what it doesn't

sats claims that agents do not receive your keys, agents cannot approve their
own payments, agent-originated money movement requires human authorization,
and sats verifies a transaction's intent independently before signing.

sats does not claim that the seed is never decrypted, that the wallet
survives a compromised operating system, or that approval is a
hardware-backed signature. An agent with shell access as your user is outside
the MCP boundary. See [Security](security.md#limits).

## Principles

1. The agent never has signing authority.
2. Money movement requires human authorization.
3. Grants constrain requests; they do not grant keys.
4. ALLOW needs no signing authority, ASK is the normal verdict for a spend,
   and DENY holds the hard boundaries.
5. Never trust the caller's interpretation of a transaction.
6. Bind approval to the exact action being authorized.
7. Keep the signer replaceable and MCP an adapter.
8. Make read-only automation frictionless and irreversible actions explicit.
9. Prefer boring, inspectable security over invisible autonomy.
