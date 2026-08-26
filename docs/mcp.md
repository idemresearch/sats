# MCP and agent grants

`sats agent serve` exposes a deliberately small wallet surface to one named
agent over Model Context Protocol stdio. The server does not grant authority
by itself, and it cannot sign: it is a shim that prepares and broadcasts
transactions, carrying a bearer token that names the grant it acts under.
Signatures come from `satsd` (see [CLI](cli.md#the-signing-daemon)).

## Connect an agent

Initialize and fund a wallet, start and unlock the daemon, then create a
grant:

```sh
sats daemon start
sats daemon unlock

sats agent grant claude \
  --budget 50k \
  --for 24h \
  --max-tx 10k \
  --max-fee 1000
```

The grant prints a bearer token once. It is never stored, so copy it now;
re-issue the grant if you lose it. The served process reads it from
`SATS_AGENT_TOKEN`:

```sh
SATS_AGENT_TOKEN=<token> sats agent serve claude
```

For Claude Code, for example:

```sh
claude mcp add sats --env SATS_AGENT_TOKEN=<token> -- sats agent serve claude
```

Use the same `--network`, `--provider`, and `SATS_DIR` values that identify the
wallet and provider configuration the agent should use.

At startup the server verifies:

- the named grant exists and has not expired;
- `SATS_AGENT_TOKEN` is present and matches that grant;
- the selected network wallet exists;
- provider configuration resolves without ambiguity;
- satsd is reachable.

Failure is reported immediately, before the client begins a conversation. A
daemon that is running but locked is a warning rather than a refusal — a
human can unlock it while the client is already connected.

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

Locked wallet — an error, deliberately not a denial:

```json
{
  "status": "error",
  "error_code": "wallet_locked",
  "message": "the wallet is locked — a human must run: sats daemon unlock",
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
Mechanical and operational failures are `status: "error"` results carrying a
stable `error_code`. An `error_code` and a denial `reason` never appear
together: a denial is a policy decision a human can approve, an error is a
condition to fix.

| `error_code` | Meaning |
|---|---|
| `invalid_agent` | The agent name is not 1–32 characters of `a-z0-9_-` |
| `invalid_request_id` | The key is not 1–64 characters of `A-Za-z0-9_-` |
| `request_id_conflict` | The key was already used for a different send |
| `request_in_flight` | The same request is executing right now |
| `request_incomplete` | An earlier execution recorded neither outcome nor transaction; a human should review |
| `wallet_locked` | satsd holds no seed; a human must run `sats daemon unlock` |
| `daemon_unavailable` | satsd could not be reached, or could not be told a send's outcome |
| `unauthorized` | The presented token does not authorize this agent's grant |
| `clock_unavailable` | The system clock cannot be read; expiry cannot be evaluated, so the send refuses |

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
| `amount_overflow` | Amount plus fee overflows; not approvable |

An agent should relay the denial — including its `request_id` — to its
human and stop. Retrying the same request unchanged cannot expand
authority; what can change the answer is a human decision.

A `revoked` denial for an agent with no grant on file carries a
`request_id` only when an earlier authenticated execution already recorded
the request: with no grant there is nobody to authenticate, so the daemon
records nothing new. A keyed retry of a send that signed before the
revocation still replays its recorded txid.

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

A send spans one connection to the daemon; the request claim is released if
that connection closes, so a caller that dies mid-send strands nothing.

The shim normalizes the recipient, then the daemon executes:

1. verify the bearer token against the grant, and compute the canonical
   intent digest (network, agent, recipient, amount — the fee is excluded);
2. resolve the request id: replay a recorded keyed outcome, reject a
   conflicting key reuse, or claim a durable request record for execution;
3. reload the grant so revocation is current;
4. precheck expiry, amount cap, and obviously exhausted budget.

The shim then runs shared safe preparation to sync, protect UTXOs, and build
the PSBT, and hands it back. The daemon:

5. derives the payment and fee **from the PSBT** against the wallet's own
   descriptors, and refuses if they disagree with the request it claimed —
   nothing the shim reports about the transaction is trusted;
6. re-reads the grant under its lock, re-checks the token, and authorizes
   the derived amount plus fee;
7. reserves and persists budget;
8. signs with the seed held in its memory;
9. privately saves raw finalized transaction hex — attributed to the agent,
   request, and intent digest — before returning. A signed transaction never
   crosses the socket.

The shim broadcasts the saved transaction and reports the result back, which
marks the record and resolves the request.

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
by grant-listing and startup paths. Re-issuing a grant mints a new token, so
a server still holding the old one is refused on its next start.

A locked daemon is not a denial. `send` returns `status: "error"` with
`error_code: "wallet_locked"` and no `reason`, so an agent can distinguish a
policy refusal — which a human may approve — from a wallet nobody has
unlocked yet, which they simply need to unlock. `daemon_unavailable` means
satsd could not be reached at all.

## Transport rules

MCP protocol frames use stdout. Human-readable startup information and
diagnostics use stderr. Code running inside the MCP server must not print
arbitrary messages to stdout.

