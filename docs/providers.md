# Providers and UTXO guards

sats treats every external Bitcoin service as a typed provider bound to one
network. Providers advertise audited capabilities; commands ask for a
capability rather than depending directly on a particular API.

Guard decisions remain presence-only: sats never interprets protocol
values when deciding which outpoints to exclude from coin selection, and
it does not parse inscriptions, runestones, or other asset protocols.
The one deliberate exception is the explicit `sats alkanes` command
family, a minimal Alkanes client: sats encodes only call envelopes it
constructs itself (in the `sats-alkanes` crate, against the published
reference encoding) and displays view results without trusting them.

## Capabilities

| Capability | Used for |
|---|---|
| `chain.sync` | Full or incremental BDK wallet synchronization |
| `chain.fees` | Fee-rate estimates |
| `chain.broadcast` | Raw transaction broadcast |
| `guard.ord` | Excluding outpoints reported by an ord-compatible view |
| `guard.alkanes` | Excluding outpoints reported with alkanes balances |
| `guard.native` | Deterministic native guard contract used by tests |
| `alkanes.view` | Contract bytecode fetch and call simulation for `sats alkanes` |

Configuration accepts exact names and the aliases `chain` and `guard`.
`alkanes.view` is deliberately outside the `guard` alias — a view reads
contracts, a guard protects UTXOs. Without a `capabilities` filter, Esplora
and Subfrost provide chain operations. Subfrost guards and Alkanes views
require an explicit capability opt-in.

## Drivers

| Driver | Chain capabilities | Guard capabilities | Views |
|---|---|---|---|
| `esplora` | sync, fees, broadcast | none | none |
| `subfrost` | sync, fees, broadcast | ord, alkanes | alkanes.view |

The Subfrost driver maps the provider's namespaced JSON-RPC methods onto the
fixed sats capability contract, using `btc_sendrawtransaction` for broadcast.
On a fresh wallet, Subfrost derives and queries scripts lazily, stopping after
20 consecutive unused scripts on each of the receive and change keychains.
Activity resets that keychain's gap; transaction-history pagination is
completed for each script before advancing to the next one.
Guard answers are presence-only; protocol values are not interpreted by sats.
Like guards, `alkanes.view` never
resolves from the legacy or built-in fallback tiers: configuring one is an
explicit trust decision, and the endpoint's network is validated before any
view result is used.

## Default behavior

When no explicit provider supplies a chain capability, sats falls back to an
Esplora endpoint:

| Network | Default |
|---|---|
| `mainnet` | `https://mempool.space/api` |
| `signet` | `https://mempool.space/signet/api` |
| `testnet4` | `https://mempool.space/testnet4/api` |
| `regtest` | `http://localhost:3002` |

Defaults cover chain operations only. sats never supplies or enables a
default asset guard.

## Configuration

Configure providers under `[providers.<name>]`:

```toml
network = "mainnet"

[providers.subfrost]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
```

Provider names are local labels. Each entry must declare the one Bitcoin
network its endpoint serves. Subfrost needs no capability list for ordinary
chain synchronization, fee estimation, and broadcast.

### Fee policy

Providers report fee estimates; the human-owned configuration chooses which
confirmation target sats uses. Targets are scoped by network, accept 1–1008
blocks, and default to 2 when omitted:

```toml
[fee_targets]
signet = 1008
```

The target applies to shared transaction preparation, including MCP sends.
A human CLI `--fee-rate` remains an explicit per-invocation override. Agents
cannot choose a target or fee rate; their grant's absolute fee cap still
authorizes or refuses the fee derived from the prepared transaction.

### Restrict capabilities

Use a capability filter to split responsibilities:

```toml
network = "mainnet"

[providers.chain]
driver = "esplora"
network = "mainnet"
url = "https://mempool.space/api"
capabilities = ["chain"]

[providers.assets]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
capabilities = ["guard"]
```

Exact capability names allow finer splits:

```toml
[providers.sync]
driver = "esplora"
network = "mainnet"
url = "https://example-a.invalid/api"
capabilities = ["chain.sync"]

[providers.relay]
driver = "esplora"
network = "mainnet"
url = "https://example-b.invalid/api"
capabilities = ["chain.fees", "chain.broadcast"]
```

The resolver prefers the provider selected for sync when it also offers fees
or broadcast. Otherwise each chain capability must resolve to one candidate;
multiple equally eligible candidates are an explicit ambiguity error.

### Authentication

Esplora providers accept an optional bearer token:

```toml
[providers.private_esplora]
driver = "esplora"
network = "mainnet"
url = "https://bitcoin.example/api"

[providers.private_esplora.auth]
bearer = "replace-with-token"
```

Subfrost API keys use the provider's dedicated header:

```toml
[providers.subfrost]
driver = "subfrost"
network = "signet"
url = "https://signet.subfrost.io/v4/jsonrpc"
api_key = "replace-with-key"
```

Keep configuration permissions restrictive. Authentication values are never
displayed. Subfrost URLs are redacted to their origin because older setups may
carry a key in the path, but new configurations should use the provider's
direct `api_key` field.
Esplora endpoint URLs may appear in diagnostics, so put credentials in
`auth.bearer`, never in the URL path or query.

## Resolution precedence

For the selected network:

1. one or more CLI `--provider` values replace the configured provider set;
2. matching `[providers.*]` entries supply explicit capabilities;
3. the legacy `[esplora]` map may supply missing chain capabilities;
4. built-in Esplora defaults supply any remaining chain capabilities.

Guards never come from legacy or built-in fallback tiers.

One-shot override:

```sh
sats --provider esplora=https://mempool.space/signet/api balance
sats --provider subfrost=https://signet.subfrost.io/v4/jsonrpc balance
```

The syntax is `KIND=URL`. It is repeatable, but CLI entries have no capability
filter; supplying two drivers that both advertise `chain.sync` is ambiguous.
An explicit CLI override has no hidden fallback behind it, so its selected
driver must provide every capability needed by the command.

## UTXO protection

Before planning, sats gathers the wallet's current unspent outputs and builds
one exclusion set:

1. unless `--allow-dust`, add outputs worth exactly 546 or 330 sats;
2. unless `--no-guards`, ask every configured guard about those outpoints;
3. union all answers and pass the resulting outpoints into BDK coin
   selection as unspendable.

The local postage check is a heuristic. False positives can be bypassed by a
human for one invocation; values other than 546 and 330 require a guard for
protection.

Guards are restrictive-only. They can cause a spend to be refused by marking
too many outputs, but they cannot add a wallet UTXO, select an input, or
authorize a signature.

## Failure behavior

Esplora and Subfrost set a 30-second HTTP request timeout. Esplora retries a
retryable GET response at most twice (three attempts with backoff). Subfrost
retries one safe JSON-RPC read after an HTTP 429, honoring `Retry-After` up to
60 seconds and waiting 60 seconds when the header is absent. Transport
timeouts are not retried, and neither driver automatically retries broadcast.
For a hostname with multiple DNS addresses, each nonfinal TCP connection
attempt is capped at two seconds (or the remaining request deadline, if
shorter). The last address gets the remaining deadline. This lets a healthy
address answer when an earlier one is unreachable, without pinning IPs,
changing providers, disabling TLS verification or resending an HTTP request.
Single-address hosts retain the full remaining request timeout. Mempool
remains the default; Subfrost is used only when explicitly configured.

This is a request limit, not a deadline for an entire wallet scan: sync and
guards can require multiple requests. OS DNS resolution can also outlast the
HTTP client's timeout. A broadcast timeout is an uncertain outcome, not proof
that the transaction was rejected; the signed transaction and reserved budget
remain durable for recovery.

- Chain sync validates the provider's genesis/network before applying data.
- Planning fails rather than using stale wallet state after sync failure.
- Fee estimation failure asks a human CLI caller to supply `--fee-rate`.
- Fee estimates are bounded: a non-finite, negative, or absurd rate (above
  10,000 sat/vB) from an endpoint is a typed fee error, never a fee.
- Malformed checkpoint data from an endpoint fails the sync with a typed
  error rather than the process.
- A configured guard failure stops planning.
- `--no-guards` and `--allow-dust` are human, per-invocation escape hatches.
- MCP sends have no escape hatches and always use the safe defaults.

Balance is intentionally more tolerant than spending: if sync fails it may
show cached state and reports `synced: false`.

## Adding a driver

Provider work is security-sensitive. Follow the recipe in
[AGENTS.md](../AGENTS.md): declare an audited capability set, keep resolution
free of network I/O, validate the Bitcoin network at operation time, provide
a display-safe error URL, and test ambiguity and failure paths with
deterministic mocks.
