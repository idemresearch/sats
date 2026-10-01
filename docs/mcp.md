# MCP and agent grants

Connect an AI agent to sats over MCP. The agent can read the wallet and file
payment requests; only you can approve them.

`sats agent serve <name>` runs a stdio MCP server for one named agent, under
the grant its token names. The server holds no key material and never
prepares, signs, or broadcasts. The flow:

1. The agent calls `request_send`. sats records the request as
   `pending_approval`, or `denied` if it crosses a grant limit.
2. You run `sats agent approve`, review the payment, and enter your password.
   That command signs and broadcasts, in your process.
3. The agent observes the result with `check_request`. It never retries or
   files again to make a payment happen.

## Connect an agent

Create a grant on a funded wallet:

```sh
sats agent grant claude --budget 50k --for 24h --max-tx 10k --max-fee 1000
```

sats prints two setup commands that embed the same token as
`SATS_AGENT_TOKEN`. Run the one for your client:

```sh
# ChatGPT desktop, Codex CLI, and the Codex IDE extension
codex mcp add sats --env SATS_AGENT_TOKEN=<token> -- <pinned sats command>

# Claude Code, scoped to the current project
claude mcp add --transport stdio --scope local sats --env SATS_AGENT_TOKEN=<token> -- <pinned sats command>
```

- **The token is shown once.** sats stores only its hash. If you lose it,
  re-issue the grant.
- **Restart ChatGPT desktop** after adding the server. ChatGPT on the web
  can't launch a local stdio server.
- **Claude Code uses local scope** and writes no repository `.mcp.json`.
- **The client stores the raw token** in its local configuration.
- **The command is pinned.** It fixes the network and the wallet and
  configuration paths, with relative paths made absolute and arguments
  safely quoted, so a different working directory can't redirect it.
  Provider credentials aren't copied; the server reads them from the
  wallet's configuration. If you launch the server by hand, pass the same
  `--network` and wallet directory.

At startup the server checks that the grant exists and hasn't expired, that
`SATS_AGENT_TOKEN` matches it, that a wallet exists on the network, and that
the configuration parses. It doesn't select a provider until `get_balance`
needs one, so an ambiguous provider setup never blocks the other tools.

## Tools

| Tool | Does | Touches the chain |
|---|---|---|
| `get_balance` | Sync and return the balance | Yes |
| `get_receive_address` | Reveal and persist the next receive address | No |
| `get_grant` | Return the agent's own grant | No |
| `request_send` | File a payment request | No |
| `check_request` | Read one of the agent's own requests | No |

### `get_balance`

```json
{ "balance_sat": 118500, "pending_sat": 0, "synced": true, "network": "signet" }
```

`synced: false` means the chain was unreachable and the balance is cached. An
invalid or ambiguous provider configuration returns an error, not a cached
balance.

### `get_receive_address`

```json
{ "address": "tb1p...", "index": 3, "network": "signet" }
```

The tool is annotated as not read-only, because each call advances the
wallet's derivation index.

### `get_grant`

Returns budget, spent, remaining, caps, mode, recipient allowlist, and expiry.
`active: false` means the grant was revoked or expired, and the message names
the command a human runs to issue a new one.

### `request_send`

Parameters:

- `address`: the recipient, valid for the grant's network.
- `amount_sat`: the amount in sats.
- `idempotency_key`: required. 1–64 characters of `A-Za-z0-9_-`.

The key is the agent's retry handle. Repeating a call with the same key,
address, and amount returns the existing request instead of filing another,
so a lost response never files twice. Reusing a key for a different send is
the error `idempotency_key_conflict`. Keys are scoped to a grant: after a
re-issue, the same key files a new request.

sats runs the grant's full [authorization ladder](security.md#the-authorization-ladder)
with the fee unknown. A proposal inside every limit is the normal, successful
result:

```json
{
  "status": "pending_approval",
  "request_id": "r-8c1f0a2b9d3e4f57a1b2c3d4e5f60718",
  "recipient": "tb1p...",
  "amount_sat": 4500,
  "message": "filed for human review — the human approves with: sats agent approve r-8c1f0a2b9d3e4f57a1b2c3d4e5f60718; poll check_request to observe the result, and do not file it again"
}
```

A proposal outside a limit is recorded as `denied`, with a `reason`:

```json
{
  "status": "denied",
  "request_id": "r-2e7d41c0a95b6f13c4d5e6f708192a3b",
  "reason": "over_max_tx",
  "recipient": "tb1p...",
  "amount_sat": 20000,
  "message": "outside the grant: requested 20,000 sat; max tx 10,000 sat — no approval lifts a grant boundary; only the human changing the grant can"
}
```

`request_id` is the server's id: `r-` plus 32 hex characters, unique across
agents. Humans approve and dismiss by it, and the agent observes by it. It is
not an idempotency key. Passing it back as one files a new request.

### `check_request`

Takes a `request_id` and returns that request in the same shape as
`request_send`. A `sent` result adds `txid` and `fee_sat`. The tool reads the
local record only. It never contacts the chain or changes anything.

- An unknown or malformed id, including an idempotency key, returns
  `status: "not_found"`. Other agents' requests are never visible.
- An unreadable record returns `status: "error"` with
  `error_code: "store_error"` and the id. Ask the human to inspect it. Never
  file a replacement.
- A `signing` result adds `execution_active` and `needs_reconciliation`,
  read from the execution lock without changing it. An inactive lock means a
  human must reconcile, never that the request is unsigned.

## Request states

| `status` | Meaning | The agent should |
|---|---|---|
| `pending_approval` | Inside the grant, waiting for the human | Relay the id and observe |
| `denied` | Outside a grant limit (see `reason`); terminal | Report once and stop |
| `dismissed` | The human declined; terminal | Stop; ask before proposing again |
| `signing` | Approval is executing, or needs human reconciliation | Observe; relay guidance if `needs_reconciliation` |
| `sent` | Broadcast; has `txid` | Done |
| `broadcast_pending` | Signed and saved; broadcast not confirmed | Observe; the human rebroadcasts |
| `unresolved` | A signature may exist without a saved result | Don't file again; the human inspects |
| `failed` | Stopped before signing; see the diagnostic | Observe; only the human can retry |

What each state means for signing and budget is in
[Security](security.md#the-signing-boundary).

## Errors

An error is a failed call, not a request state. It never files a request.

| `error_code` | Meaning |
|---|---|
| `invalid_agent` | The agent name isn't 1–32 characters of `a-z0-9_-` |
| `invalid_idempotency_key` | The key is missing or isn't 1–64 characters of `A-Za-z0-9_-` |
| `invalid_address` | The address doesn't parse or is for another network |
| `idempotency_key_conflict` | The key was used for a different send; `request_id` names it |
| `no_grant` | No grant on file, so nothing is written |
| `unauthorized` | The token doesn't match this agent's grant |
| `clock_unavailable` | The system clock can't be read, so expiry can't be checked |
| `store_error` | The request store couldn't be read or written |

## Denial reasons

Every reason is a grant limit. Report it to the human once and stop, because
filing the same proposal again can't expand authority.

| `reason` | Meaning |
|---|---|
| `expired` | The grant has expired |
| `observe_only` | The grant is observe-only |
| `amount_overflow` | Amount plus fee overflows |
| `recipient_not_allowed` | The recipient isn't on the grant's allowlist |
| `over_max_tx` | The amount exceeds the per-transaction cap |
| `over_max_fee` | The real fee, known at approval, exceeds the fee cap |
| `over_budget` | Amount plus fee exceeds the remaining budget |
| `revoked` | The grant that created the request was revoked or re-issued |

## After filing

Nothing happens on the agent's side. The human finds the request with
`sats agent requests` (or `--watch`) and approves or dismisses it; see
[Reviewing agent requests](cli.md#reviewing-agent-requests). Approval
prepares the transaction on fresh chain state, verifies it matches the
request, re-checks the grant with the real fee, and signs only after the
human enters the wallet password.

If preparation fails because of a sync, guard, fee, or provider problem, the
request becomes `failed` and the human can try again. The agent keeps
observing with `check_request`. It shouldn't poll the balance or file again.

## Revocation and expiry

```sh
sats agent list
sats agent revoke claude
```

- Revocation takes effect on the next filing, without restarting the client.
- A request executes only under the grant instance that created it. A
  pending request whose grant was revoked or re-issued becomes `denied` with
  reason `revoked` when the human tries to approve it.
- Re-issuing a grant mints a new token. A running server with the old token
  can't file, and the old token can't start a server.
- Expired grants can't start a server and are removed when listing or
  startup finds them.
- A running session keeps `get_balance`, `get_receive_address`, `get_grant`,
  and `check_request` after revocation or expiry. Stop the session to end
  them.
- A transaction signed before revocation stays valid.

## Transport

MCP frames use stdout, and startup messages and diagnostics use stderr.
Code inside the server must never print to stdout.
