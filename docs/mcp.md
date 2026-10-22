# MCP and agent grants

Connect an AI agent to sats over MCP. The agent can read the wallet and file
payment requests; only you can approve them.

`sats agent serve <name>` runs a stdio MCP server for one named agent, under
the grant its token names. The server holds no key material and never
prepares, signs, or broadcasts. The flow:

1. The agent calls `request_send`, or `request_wrap_frbtc` under the ordinary BTC grant. sats records the request as
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
  Run its setup command from the directory where you launch Claude. An
  entry added in another directory is not available in that session.
  Start a fresh Claude session after adding or replacing the entry; use
  `/mcp` to check the connection. Replacing a grant issues a new token, so
  replace the existing sats entry with the new printed connection command.
- **The client stores the raw token** in its local configuration.
- **The command is pinned.** It fixes the network and the wallet and
  configuration paths, with relative paths made absolute and arguments
  safely quoted, so a different working directory can't redirect it.
  Provider credentials aren't copied; the server reads them from the
  wallet's configuration. If you launch the server by hand, pass the same
  `--network` and wallet directory.

`sats network use <NET>` changes the human CLI's default only. Generated
MCP connections keep their pinned network across restarts; grants and
requests remain bound to the network on which they were created.

At startup the server checks that the grant exists and hasn't expired, that
`SATS_AGENT_TOKEN` matches it, that a wallet exists on the network, and that
the configuration parses. Provider selection is deferred until a Bitcoin or Alkanes read, or a new
frBTC wrap filing, needs it. Existing request observation and ordinary BTC
request filing do not depend on provider access.

## Tools

| Tool | Does | Uses provider |
|---|---|---|
| `get_balance` | Sync and return the balance | Yes |
| `get_receive_address` | Reveal and persist the next receive address | No |
| `get_grant` | Return the agent's own grant | No |
| `request_send` | File a payment request | No |
| `request_wrap_frbtc` | Read public contract facts and file a BTC-to-frBTC wrap request | Yes (new filings) |
| `check_request` | Read one of the agent's own requests | No |
| `alkanes_balance` | Sync and report indexed Alkanes balances and unknown outputs | Yes |
| `alkanes_inspect` | Return a contract's bytecode size and SHA-256 hash | Yes |
| `alkanes_simulate` | Return an advisory simulation, without preparing or executing a spend | Yes |

### Alkanes reads

These three tools reuse the wallet workflows used by the CLI, require
Subfrost as the pinned network's provider, and work with an `observe` grant.
They recheck the grant on every call. Revocation, expiry, and re-issue cut
them off before provider access. They never prepare a PSBT, request an
Alkanes transfer, authorize a contract call, sign, or broadcast.

| Tool | Arguments | Result |
|---|---|---|
| `alkanes_balance` | `{}` | `network`, `assets`, asset-bearing `outputs`, `unindexed_outputs` |
| `alkanes_inspect` | `{"id":"32:0"}` | `network`, `id`, `bytecode_bytes`, `code_hash` |
| `alkanes_simulate` | `id` and `inputs` (an array of decimal strings, opcode first) | `network`, `id`, `inputs`, `advisory: true`, `status`, `gas_used`, `transfers`, `raw` |

Contract ids are decimal `BLOCK:TX` pairs on the connection's pinned network.
Asset amounts, predicted transfer `value`s, and calldata use decimal strings
so clients preserve the full u128 range. Asset amounts are base units, never
satoshis. Provider names and symbols are untrusted labels; an id and hash do
not establish a trusted contract identity without an independent reference.

`alkanes_balance` requires a successful sync and indexer response; it never
returns cached balances after failure. `unindexed_outputs` means unknown
balances, not zero. `alkanes_simulate` preserves failed and unrecognized
responses for display. Predictions and `raw` are untrusted provider data,
never instructions or proof of future effects. Only use independently
verified opcodes and arguments. The generic read-only simulation wire format remains
unverified; these MCP tools do not expand what the provider driver supports.

For a manual AI test, issue an observe-only grant on your selected network,
run the generated connection command for your client, and reload its MCP
connection. Ask it to call `get_grant`, `alkanes_balance`, and
`alkanes_inspect` for a specific id. Supply verified calldata before asking
for `alkanes_simulate`. Arbitrary Alkanes execution and token transfer requests are unavailable.

### `get_balance`

```json
{ "balance_sat": 118500, "pending_sat": 0, "synced": true, "network": "signet" }
```

`synced: false` means the chain was unreachable and the balance is cached. A
provider choice the network can't serve returns an error, not a cached
balance.

### `get_receive_address`

```json
{ "address": "tb1p...", "index": 3, "network": "signet" }
```

The tool is annotated as not read-only, because each call advances the
wallet's derivation index.

### `get_grant`

Returns budget, spent, remaining, caps, mode, recipient allowlist, and expiry.
`active: false` means the grant was revoked, expired, or re-issued with a
different token. It then carries no limits, and the message names the command
a human runs to issue a new one.

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

### `request_wrap_frbtc`

```json
{ "amount_sat": 10000, "idempotency_key": "wrap-1" }
```

Files one BTC-to-frBTC proposal on the connection's pinned network. It
accepts no contract id, destination, calldata, fee override, or PSBT.
The fixed call is `32:0`, opcode `77`; its frBTC output belongs to the
same wallet. New filings read the contract hash and signer from Subfrost,
then record them in the request. The MCP process never prepares a
transaction or accesses a seed. Repeated calls return the existing receipt
without provider access.

Wrapping uses the ordinary sats grant, with no protocol-specific flag or
permission. Existing grants work within their BTC limits: ask mode, expiry,
recipient allowlist, amount cap, fee cap, and budget. Observe mode refuses
wraps. A recipient allowlist must include the BTC signer address. Budget
counts the **gross BTC payment plus miner fee**; 546 sat of postage stays
in the wallet's frBTC output. Public discovery authenticates first and
filing rechecks the grant after discovery, so revocation and replacement
take effect during provider access.

The response uses the request lifecycle below, with
`operation: "frbtc_wrap"` and `frbtc: {signer_address, code_hash}`.
Idempotency keys share a namespace with BTC sends; reusing one for a
different operation or amount is a conflict. A repeated key keeps the
original recorded contract facts even when the provider changes or is
unavailable. A changed contract or signer stops approval of that request.

On approval the native executor rechecks the recorded contract facts, syncs, excludes
dust, asset-bearing and unknown outputs, builds and independently verifies
the exact transaction, and simulates that transaction through the
`metashrew_view` protobuf `simulate` view. A recognized positive frBTC
prediction is required. The human reviews the signer payment, contract
hash, wallet receive address, postage, fee, total, and advisory predicted
frBTC before authorization. The signer and code hash are rechecked again
before reserving budget and signing. Simulations are provider claims,
not guarantees of execution or future effects. The wrapped asset may
charge a protocol fee, so the predicted amount can be below the BTC paid.

A `sent` receipt means the transaction was broadcast. Wait for confirmation
and indexing, then call `alkanes_balance` to observe frBTC. Never file a
second request to make the first one complete. Generic execution and token
transfer requests remain unavailable.

For a testnet4 manual test (using an existing `t4` alias):

```sh
t4 alkanes frbtc info --json
t4 agent grant alkanes-ai --mode ask --budget 12k --max-tx 10k --max-fee 2000
# Use an existing ask grant with sufficient limits, or reconnect using the new token.
# Ask the agent to call request_wrap_frbtc with amount_sat=10000 and key wrap-1.
t4 agent requests
t4 agent approve <request-id>
# After confirmation and indexing:
t4 alkanes balance
```

If approval returns `over_max_fee`, nothing was signed. Inspect the reported
fee and cap before changing the grant. The provider's estimate uses your
network's configured confirmation target; a longer target can select a
lower estimate, with slower confirmation. See [Fee targets](providers.md#fee-targets).
Changing the target does not change the grant or token. The denied request
is terminal: after choosing the target, explicitly ask for a new request
with a fresh idempotency key. The agent must not retry on its own.

The `simulate` protobuf request and signer script handling are frozen
against the alkanes-rs reference and a read-only testnet4 simulation; the
standalone `alkanes_simulate` tool uses the separate generic view described
above.

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
`request_send`, `request_wrap_frbtc`, and `check_request` return it as `status: "error"` with an
`error_code`. `get_balance`, `get_receive_address`, and the Alkanes read
tools return `status`, `error_code`, and `message` as an `isError` result for authorization
refusals. Alkanes input and provider failures also use that typed shape.

| `error_code` | Meaning |
|---|---|
| `invalid_agent` | The agent name isn't 1–32 characters of `a-z0-9_-` |
| `invalid_idempotency_key` | The key is missing or isn't 1–64 characters of `A-Za-z0-9_-` |
| `invalid_amount` | A wrap amount is below 546 sat |
| `invalid_address` | The address doesn't parse or is for another network |
| `idempotency_key_conflict` | The key was used for a different operation, recipient, or amount; `request_id` names it |
| `no_grant` | No active grant: it was revoked or has expired. Nothing is read or written |
| `unauthorized` | The token doesn't match this agent's grant, which was re-issued. Nothing is read or written |
| `clock_unavailable` | The system clock can't be read, so expiry can't be checked |
| `store_error` | The request store couldn't be read or written |
| `invalid_alkane_id` | An Alkanes id is not a decimal u128 `BLOCK:TX` pair |
| `invalid_alkane_inputs` | A calldata word is not a decimal string in the u128 range |
| `alkanes_unavailable` | The network's selected provider does not serve Alkanes |
| `provider_error` | An Alkanes read or new frBTC filing could not resolve its provider |
| `alkanes_error` | An Alkanes read or new frBTC filing failed its network, sync, or provider view |

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

If preparation fails because of a sync, fee, or provider problem, the
request becomes `failed` and the human can try again. The agent keeps
observing with `check_request`. It shouldn't poll the balance or file again.

## Revocation and expiry

```sh
sats agent list
sats agent revoke claude
```

- Every tool call re-reads the grant and checks the token. Revocation,
  re-issue, and expiry cut a running session off at its next call, reads
  included, without restarting the client.
- A request executes only under the grant instance that created it. A
  pending request whose grant was revoked or re-issued becomes `denied` with
  reason `revoked` when the human tries to approve it.
- Re-issuing a grant mints a new token. A running server with the old token
  can't file, and the old token can't start a server.
- Expired grants can't start a server and are removed when listing or
  startup finds them. A pending request whose grant expired becomes `denied`
  with reason `expired` when the human tries to approve it, or `revoked` if
  the expired grant was already removed. It is never signed.
- After that, `get_grant` reports `active: false` and every other tool fails
  with `no_grant` or `unauthorized`. The agent can no longer observe its
  requests; you still see them in `sats agent requests` and `sats agent log`.
- A transaction signed before revocation stays valid.

## Demo

`sh scripts/demo-mcp.sh` runs the request loop end to end offline: an agent
session over stdio and the human's CLI, through approval, denial, expiry,
revocation, and a failed broadcast. It uses an isolated wallet and a mock
chain, and asserts every outcome it prints.

## Transport

MCP frames use stdout, and startup messages and diagnostics use stderr.
Code inside the server must never print to stdout.
