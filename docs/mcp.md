# MCP and agent grants

`sats agent serve` exposes a deliberately small wallet surface to one named
agent over Model Context Protocol stdio. The server does not grant authority
by itself; it starts only when a human has already created a non-expired
grant.

## Connect an agent

Initialize and fund a wallet, then create a grant:

```sh
sats agent grant claude \
  --budget 50k \
  --for 24h \
  --max-tx 10k \
  --max-fee 1000
```

Configure an MCP client to launch:

```sh
sats agent serve claude
```

For Claude Code, for example:

```sh
claude mcp add sats -- sats agent serve claude
```

Use the same `--network`, `--provider`, and `SATS_DIR` values that identify the
wallet and provider configuration the agent should use.

At startup the server verifies:

- the named grant exists and has not expired;
- the selected network wallet exists;
- provider configuration resolves without ambiguity.

Failure is reported immediately, before the client begins a conversation.

## Tool surface

### `get_balance`

Returns:

```json
{
  "balance_sat": 100000,
  "pending_sat": 0,
  "synced": true,
  "network": "signet"
}
```

The server attempts chain sync. If the provider cannot be reached, it returns
cached wallet state with `synced: false`; this read-only operation does not
pretend the cache is current.

### `get_receive_address`

Returns and persists the next external receive address:

```json
{
  "address": "tb1p...",
  "index": 4,
  "network": "signet"
}
```

### `get_grant`

Returns this server identity's current authority:

```json
{
  "active": true,
  "agent": "claude",
  "network": "signet",
  "budget_sat": 50000,
  "spent_sat": 4781,
  "remaining_sat": 45219,
  "max_tx_sat": 10000,
  "max_fee_sat": 1000,
  "tx_count": 1,
  "expires_at": 1787300000
}
```

When the grant has been revoked or expired, `active` is false, limit and
accounting fields are omitted, and `message` tells the agent to ask its human
for a new grant.

### `send`

Parameters:

```json
{
  "address": "tb1p...",
  "amount_sat": 4500,
  "request_id": "invoice-7012"
}
```

`request_id` is an optional idempotency key: 1–64 characters of
`A-Za-z0-9_-`. Every send — keyed or not — is recorded as a durable request;
the returned `request_id` is the server-assigned record id (`k-<key>` for
keyed requests) a human can review with `sats agent requests`.

Successful result:

```json
{
  "status": "sent",
  "txid": "...",
  "amount_sat": 4500,
  "fee_sat": 281,
  "total_sat": 4781,
  "remaining_budget_sat": 45219,
  "request_id": "k-invoice-7012"
}
```

Policy denial:

```json
{
  "status": "denied",
  "reason": "over_max_tx",
  "message": "human authorization required: requested 20,000 sat; max tx 10,000 sat",
  "request_id": "k-invoice-7012"
}
```

Operational failure:

```json
{
  "status": "error",
  "message": "broadcast failed after signing: ... — budget reserved; a human can retry with: sats tx broadcast <txid>",
  "request_id": "k-invoice-7012"
}
```

`send` does not expose fee-rate, dust, guard, signer, PSBT, or provider-bypass
parameters. The agent supplies only destination, integer satoshis, and
optionally its idempotency key.

## Idempotent retries

Retrying `send` with the same `request_id` and the identical address and
amount is always safe:

- if the earlier execution signed a transaction, the recorded outcome is
  returned verbatim — the same txid for a broadcast send, the same error for
  a signed-but-unbroadcast one. Nothing executes twice and no budget is
  drawn again. This replay answers even after the grant was revoked: a
  signed transaction is the truth the agent must learn.
- if the earlier execution was denied or failed before any signature
  existed, the retry re-evaluates the same request from scratch.

Reusing a key for a different address or amount never mutates anything.
Mechanical failures of the idempotency contract are `status: "error"`
results carrying a stable `error_code`:

| `error_code` | Meaning |
|---|---|
| `invalid_request_id` | The key is not 1–64 characters of `A-Za-z0-9_-` |
| `request_id_conflict` | The key was already used for a different send |
| `request_in_flight` | The same request is executing right now |
| `request_incomplete` | An earlier execution recorded neither outcome nor transaction; a human should review |

## Denial semantics

Denial is an expected successful tool result, not an MCP transport error.
Supported reason codes are:

| Reason | Meaning |
|---|---|
| `expired` | The grant's expiry has been reached |
| `over_max_tx` | Recipient amount exceeds the per-transaction cap |
| `over_max_fee` | Planned fee exceeds the fee cap |
| `over_budget` | Amount plus fee exceeds remaining budget |
| `revoked` | The grant file no longer exists |
| `approval_fee_exceeded` | The prepared fee exceeds a one-time approval's ceiling |

An agent should relay the denial — including its `request_id` — to its
human and stop. Retrying the same request unchanged cannot expand
authority; what can change the answer is a human decision.

## One-time approvals

A cap denial (`over_max_tx`, `over_max_fee`, `over_budget`) is not a dead
end: the denied request persists, and its message names the exact command —
`sats agent approve <request-id>` — that lets a human authorize precisely
that send, once. The approval binds the request's canonical intent digest
(network, agent, recipient, amount), carries its own fee ceiling and
expiry, and is consumed by the first matching send. After the human
approves, the agent retries the identical send — same address, same
amount, ideally the same `request_id` — and the result carries
`via_approval: true`.

Approvals never override revocation or grant expiry: those are the human's
kill switches, and an exception issued earlier does not survive them. A
consumed approval never authorizes a second signature; re-running
`sats agent approve` is a fresh human decision. `sats agent deny <id>`
dismisses a request and revokes its unconsumed approval.

## Send lifecycle

The server executes:

1. normalize the recipient and compute the canonical intent digest
   (network, agent, recipient, amount — the fee is excluded);
2. resolve the request id: replay a recorded keyed outcome, reject a
   conflicting key reuse, or claim a durable request record for execution;
3. reload the grant so revocation is current;
4. precheck expiry, amount cap, and obviously exhausted budget;
5. run shared safe preparation to sync, protect UTXOs, and learn the fee;
6. authorize the final amount plus fee;
7. reserve and persist budget;
8. unlock the grant-wrapped seed and sign;
9. privately save raw finalized transaction hex — attributed to the agent,
   request, and intent digest — before network access;
10. broadcast, mark the transaction broadcast, and return the txid.

Each transition is appended to the per-network event log
(`events/log.jsonl`), and the request record is resolved to its outcome, so
the causal chain from request through decision to transaction survives on
disk. Denied requests remain reviewable with `sats agent requests`.

If signing fails before a signature exists, the reservation is refunded. If
broadcast fails, budget stays reserved and the saved finalized transaction
can be retried by a human. The normal MCP path never persists the PSBT. See
[Security](security.md) for why the boundary occurs at signing rather than
broadcast.

## Revocation and expiry

```sh
sats agent list
sats agent revoke claude
```

Every send reloads the grant from disk. Revocation therefore takes effect on
the next call without restarting the MCP client. An already signed
transaction remains valid after revocation.

Expired grants cannot start a new MCP server and are removed when discovered
by grant-listing and startup paths.

## Transport rules

MCP protocol frames use stdout. Human-readable startup information and
diagnostics use stderr. Code running inside the MCP server must not print
arbitrary messages to stdout.

