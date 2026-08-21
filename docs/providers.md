# Providers and UTXO guards

sats treats every external Bitcoin service as a typed provider bound to one
network. Providers advertise audited capabilities; commands ask for a
capability rather than depending directly on a particular API.

Metaprotocol data remains external. sats does not parse inscriptions,
runestones, alkanes, or other asset protocols. A guard answers only which of
the wallet's outpoints should be excluded from automatic coin selection.

## Capabilities

| Capability | Used for |
|---|---|
| `chain.sync` | Full or incremental BDK wallet synchronization |
| `chain.fees` | Fee-rate estimates |
| `chain.broadcast` | Raw transaction broadcast |
| `guard.ord` | Excluding outpoints reported by an ord-compatible view |
| `guard.alkanes` | Excluding outpoints reported with alkanes balances |
| `guard.native` | Deterministic native guard contract used by tests |

Configuration accepts exact names and the aliases `chain` and `guard`.
Without a `capabilities` filter, a driver advertises every capability it
implements.

## Drivers

| Driver | Chain capabilities | Guard capabilities |
|---|---|---|
| `esplora` | sync, fees, broadcast | none |
| `subfrost` | sync, fees, broadcast | ord, alkanes |

The Subfrost driver maps the provider's namespaced JSON-RPC methods onto the
fixed sats capability contract. Guard answers are presence-only; protocol
values are not interpreted by sats.

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
network its endpoint serves.

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

### Bearer authentication

Esplora providers accept an optional bearer token:

```toml
[providers.private_esplora]
driver = "esplora"
network = "mainnet"
url = "https://bitcoin.example/api"

[providers.private_esplora.auth]
bearer = "replace-with-token"
```

Keep configuration permissions restrictive. Bearer values are not displayed.
Subfrost paths are redacted to the endpoint origin because they may contain an
API key. Esplora endpoint URLs may appear in diagnostics, so put credentials
in `auth.bearer`, never in the URL path or query.

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

- Chain sync validates the provider's genesis/network before applying data.
- Planning fails rather than using stale wallet state after sync failure.
- Fee estimation failure asks a human CLI caller to supply `--fee-rate`.
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
