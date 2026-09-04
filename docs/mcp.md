# MCP and agent grants

> **Testing only. Not mainnet-ready.** Use signet or regtest with a fresh
> test wallet. Do not use real funds or restore a real-funded wallet.

`sats agent serve` exposes a deliberately small wallet surface to one named
agent over Model Context Protocol stdio. The rule it implements:

> **Agents create requests. Humans authorize requests. sats executes
> requests.**

The served process reads the wallet and files requests under the grant its
bearer token names. It holds no key material and never prepares, signs, or
broadcasts a transaction. A request is executed only when a human
authorizes it with `sats agent approve`, in the human's own process, with
the wallet password; the agent takes no action after filing and observes
the result with `check_request`.

## Connect an agent

Initialize a fresh signet wallet and fund it with test coins from a signet
faucet, then create a grant:

```sh
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
- provider configuration resolves without ambiguity.

Authentication, wallet, or configuration failure is reported when the client
starts the MCP server. Registering the command with `claude mcp add` does not
by itself demonstrate that the server can start.

## Tool surface

Five tools. The agent reads, files, and observes; it cannot approve,
unlock, sign, execute, or broadcast.

### `get_balance`

Syncs the wallet through the configured provider and returns:

```json
{ "balance_sat": 118500, "pending_sat": 0, "synced": true, "network": "signet" }
```

`synced: false` means the chain could not be reached and the value is from
cache.

### `get_receive_address`

Reveals and persists the next external receive address:

```json
{ "address": "tb1p...", "index": 3, "network": "signet" }
```

It is annotated as not read-only, because revealing an address advances
the wallet's derivation index.

### `get_grant`

The agent's own grant: budget, spent, remaining, per-transaction caps,
mode, recipient allowlist, and expiry. `active: false` means the grant was
revoked or has expired, and the message names the command a human runs to
issue one.

### `request_send`

Files a request to send bitcoin.

Parameters: `address`, `amount_sat`, and `idempotency_key` (required;
1–64 characters of `A-Za-z0-9_-`). The key is the agent's retry
handle: a repeated call with the same key and the identical address and
amount returns the existing request instead of filing a second one, so
a lost response or a retried tool call never files twice; reusing the
key for a different send is a typed error. The key is scoped to the
grant it was filed under: after a revoke and re-issue, the same key
files a new request rather than returning the old grant's.

The `request_id` the result returns is the server's request id — `r-`
plus 32 hex characters, globally unique across agents — and is the
handle the human approves or dismisses by and the agent observes with.
It is never the key: passing a returned `request_id` back as an
`idempotency_key` files a new request.

The grant's full verdict ladder runs at filing, with the fee unknown. A
proposal inside every boundary is recorded as `pending_approval` and
returned as a successful result:

```json
{
  "status": "pending_approval",
  "request_id": "r-8c1f0a2b9d3e4f57a1b2c3d4e5f60718",
  "recipient": "tb1p...",
  "amount_sat": 4500,
  "message": "filed for human review — the human approves with: sats agent approve r-8c1f0a2b9d3e4f57a1b2c3d4e5f60718; poll check_request to observe the result, and do not file it again"
}
```

That is the normal result, not a failure. A proposal outside a boundary is
recorded as `denied`, with a `reason`:

```json
{
  "status": "denied",
  "request_id": "r-2e7d41c0a95b6f13c4d5e6f708192a3b4c",
  "reason": "over_max_tx",
  "recipient": "tb1p...",
  "amount_sat": 20000,
  "message": "outside the grant: requested 20,000 sat; max tx 10,000 sat — no approval lifts a grant boundary; only the human changing the grant can"
}
```

Denied is terminal. No approval lifts a grant boundary; the only
escalation is the human changing the grant.

### `check_request`

Returns the state of one of this agent's own requests, by the `request_id`
a `request_send` result returned. It deals in server request ids only:
an idempotency key is a malformed id here. It reads the durable record
only: no chain access, no side effects. The result has the same shape
as `request_send`; a `sent` request carries its `txid` and `fee_sat`. An
unknown or malformed id is `status: "not_found"`, and another agent's
requests are never visible.

## Request states

| `status` | Meaning | What the agent does |
|---|---|---|
| `pending_approval` | Inside the grant; awaiting the human | Relay the id; observe |
| `denied` | Outside a grant boundary (`reason`); terminal | Report once; stop |
| `dismissed` | The human declined; terminal | Stop; ask the human before proposing again |
| `signing` | The human authorized it; sats is signing and broadcasting | Observe |
| `sent` | Broadcast; carries `txid` | Done |
| `broadcast_pending` | Signed and persisted; the broadcast failed. The human retries it | Nothing |
| `unresolved` | The signer was invoked and did not produce a durable, broadcastable result; a signature may exist. The human resolves it | Nothing; do not file it again |
| `failed` | Execution stopped before the signer was invoked; the human may authorize again | Observe |

`error` is not a request state: it is a typed operational condition on the
call itself, with an `error_code` and no record written.

| `error_code` | Meaning |
|---|---|
| `invalid_agent` | The agent name is not 1–32 characters of `a-z0-9_-` |
| `invalid_idempotency_key` | The key is missing or not 1–64 characters of `A-Za-z0-9_-` |
| `invalid_address` | The address does not parse, or is for another network |
| `idempotency_key_conflict` | The key was already used for a different send; `request_id` names it |
| `no_grant` | No grant on file: nothing can be authenticated, so nothing is written |
| `unauthorized` | The presented token does not authorize this agent's grant |
| `clock_unavailable` | The system clock cannot be read; expiry cannot be evaluated |
| `store_error` | The request store could not be read or written |

## Denial reasons

| `reason` | Meaning |
|---|---|
| `expired` | The grant's expiry has been reached |
| `over_max_tx` | Recipient amount exceeds the per-transaction cap |
| `over_max_fee` | The real fee, at execution, exceeds the fee cap |
| `over_budget` | Amount plus fee exceeds remaining budget |
| `amount_overflow` | Amount plus fee overflows |
| `observe_only` | The grant is observe-only |
| `revoked` | The grant that created the request was revoked or re-issued; a request never executes under another grant |
| `recipient_not_allowed` | The recipient is outside the grant's standing allowlist |

Every reason is a grant boundary. An agent should report it to its human
once and stop: re-filing the same proposal cannot expand authority.

## What happens after filing

Nothing, on the agent's side. The human sees the request in
`sats agent requests` (or the `--watch` stream, a trusted channel that
does not rely on the agent relaying its own status) and runs
`sats agent approve <id>`, which:

1. prepares the transaction on current chain state, with no UTXO-safety
   bypasses;
2. derives what the prepared transaction pays from the wallet's own
   descriptors and refuses if it disagrees with the recorded request;
3. re-runs the grant's ladder with the real fee, and shows the human the
   recipient, amount, fee, and total;
4. takes the wallet password — the authorization — and, under the grant
   lock, checks that the grant on file is the one that created the
   request, draws the budget on the grant's ledger under the request's
   id, and persists the request as `signing` before the signer is
   invoked;
5. signs, persists the finalized transaction, and broadcasts.

A failure before the signer is invoked — the audit log cannot be
written, the signer cannot be constructed — returns the draw and leaves
the request `failed`, which the human may authorize again. Once the
signer has been invoked, nothing it reports is trusted to mean "no
signature": an error, an unfinalized result, or a failure to finalize or
save the transaction leaves the request `unresolved`, with the draw kept
and the request never signed again. A broadcast failure after signing is
`broadcast_pending`: the draw is final, the request is never signed
again, and `sats tx broadcast <txid>` settles it to `sent`. `sats agent
dismiss <id>` declines a pending request.

Provider HTTP requests time out after 30 seconds each; those limits apply
to the human's approve command, not to the served process, which never
touches the chain for a request.

## Revocation and expiry

```sh
sats agent list
sats agent revoke claude
```

Every request records the `grant_id` of the grant instance that
created it and executes only under that instance. Revocation takes
effect on the next call without restarting the MCP client, and a pending
request whose grant was revoked or re-issued is `denied` with reason
`revoked` when a human tries to approve it — a new grant for the same
agent never inherits old requests, and a client key reused under the new
grant files a new request. A transaction signed before revocation
remains valid.

Expired grants cannot start a new MCP server and are removed when discovered
by grant-listing and startup paths. Re-issuing a grant mints a new token, so
a server still holding the old one is refused on its next start, and a
running server with a superseded token can file nothing.

## Transport rules

MCP protocol frames use stdout. Human-readable startup information and
diagnostics use stderr. Code running inside the MCP server must not print
arbitrary messages to stdout.
