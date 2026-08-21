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
  "amount_sat": 4500
}
```

Successful result:

```json
{
  "status": "sent",
  "txid": "...",
  "amount_sat": 4500,
  "fee_sat": 281,
  "total_sat": 4781,
  "remaining_budget_sat": 45219
}
```

Policy denial:

```json
{
  "status": "denied",
  "reason": "over_max_tx",
  "message": "human authorization required: requested 20,000 sat; max tx 10,000 sat"
}
```

Operational failure:

```json
{
  "status": "error",
  "message": "broadcast failed after signing: ... — budget reserved; a human can retry with: sats tx broadcast <txid>"
}
```

`send` does not expose fee-rate, dust, guard, signer, PSBT, or provider-bypass
parameters. The agent supplies only destination and integer satoshis.

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

An agent should relay the denial to its human and stop. Retrying the same
request cannot expand authority.

## Send lifecycle

The server executes:

1. reload the grant so revocation is current;
2. precheck expiry, amount cap, and obviously exhausted budget;
3. run shared safe preparation to sync, protect UTXOs, and learn the fee;
4. authorize the final amount plus fee;
5. reserve and persist budget;
6. unlock the grant-wrapped seed and sign;
7. privately save raw finalized transaction hex before network access;
8. broadcast, mark the transaction broadcast, and return the txid.

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

