# Providers and UTXO guards

Choose where sats gets chain data, fee estimates, and broadcast, and which
asset indexers, if any, protect your UTXOs.

A provider is an external service bound to one Bitcoin network. Commands ask
for a capability, such as "sync" or "broadcast", rather than a specific API.
You don't need any configuration to start.

## Defaults

With no provider configured, sats uses Esplora for chain sync, fees, and
broadcast:

| Network | Default endpoint |
|---|---|
| `mainnet` | `https://mempool.space/api` |
| `signet` | `https://mempool.space/signet/api` |
| `testnet4` | `https://mempool.space/testnet4/api` |
| `regtest` | `http://localhost:3002` |

sats never enables an asset guard by default. Configuring one is an explicit
trust decision.

## Managing providers

`sats providers` lists what serves the active network, and
`sats providers login` sets a provider up without editing the config file.

```sh
sats providers                          # list (same as `sats providers list`)
sats providers login subfrost           # asks for your Subfrost API key
sats providers login subfrost --assets  # also enable asset guards and Alkanes views
sats providers logout subfrost          # remove the entry and its key
```

```text
signet providers

Name      Driver    Endpoint                    Auth     Serves
subfrost  subfrost  https://signet.subfrost.io  api key  chain, guard.ord, guard.alkanes, alkanes.view
```

`Serves` is what each provider is used for right now, after
[resolution](#resolution-order). `chain` stands for `chain.sync`,
`chain.fees`, and `chain.broadcast` together. A configured entry that another
provider shadows shows `—`. The built-in default appears only while it
serves something. If the configuration can't be resolved, the list shows the
entries as written and exits with the resolution error.

### Logging in

`sats providers login <subfrost|esplora>` configures the provider for the
active network (`--network` picks another):

1. It checks that the result still resolves. A login that would make two
   chain providers ambiguous is refused before you're asked for anything.
2. It asks for the credential with hidden input. Without a terminal, it reads
   one line from stdin, so `printf '%s\n' "$KEY" | sats providers login subfrost`
   works in scripts. The credential is never a command-line argument.
3. It contacts the endpoint and checks its genesis block, so a wrong key, URL,
   or network is caught now rather than at the next send.
4. Only then does it save the entry to `config.toml`.

| Option | Meaning |
|---|---|
| `--url URL` | Endpoint. Subfrost defaults to its mainnet and signet endpoints; other networks, and every Esplora login, need one. |
| `--assets` | Subfrost only: also use it for `guard.ord`, `guard.alkanes`, and `alkanes.view`. |
| `--name NAME` | Config entry name. Defaults to the provider's name, or `<provider>-<network>` when that name is used by another network. |

On a terminal, a first Subfrost login without `--assets` asks whether to
enable asset protection, defaulting to no. Asset guards are never enabled
without that answer or the flag.

Logging in again to the same provider on the same network updates its
existing entry: enter a new key to rotate it, or press enter to keep the
stored one, for example when adding `--assets` later. An existing entry keeps
its capabilities unless `--assets` is given.

For Esplora the credential is an optional bearer token; press enter for
none. `--json` prints the saved entry's name, network, origin,
`authenticated`, `updated`, and `serves`, never the credential.

`sats providers logout <NAME>` deletes the `[providers.<NAME>]` entry and
its credential. Logging in or out rewrites `config.toml`, so comments in a
hand-edited file are not kept.

## Drivers and capabilities

| Capability | Used for | `esplora` | `subfrost` |
|---|---|---|---|
| `chain.sync` | Wallet synchronization | ✓ | ✓ |
| `chain.fees` | Fee-rate estimates | ✓ | ✓ |
| `chain.broadcast` | Raw transaction broadcast | ✓ | ✓ |
| `guard.ord` | Excluding outputs an ord-compatible index reports | | opt-in |
| `guard.alkanes` | Excluding outputs that hold Alkanes balances | | opt-in |
| `alkanes.view` | Bytecode and simulation for `sats alkanes` | | opt-in |
| `guard.native` | Deterministic guard used by tests | | |

Without a `capabilities` filter, a provider supplies chain capabilities only.
Guards and views must be listed explicitly. The aliases `chain` and `guard`
cover their groups. `alkanes.view` is not part of `guard`, because a view
reads contracts while a guard protects UTXOs.

Guard answers are presence-only: sats never parses inscriptions, runestones,
or other asset data to decide what to exclude. The one exception is
`sats alkanes`, which encodes only calls it builds itself (in the
`sats-alkanes` crate) and displays view results without trusting them.

The Subfrost driver maps Subfrost's JSON-RPC methods onto these capabilities
and broadcasts with `btc_sendrawtransaction`. On a fresh wallet it derives
scripts lazily and stops after 20 consecutive unused scripts on each of the
receive and change keychains.

## Configuration

Providers live under `[providers.<name>]` in the [config file](cli.md#configuration).
`sats providers login` writes these entries for you; you can also edit them
by hand. The name is a local label. Every entry declares the one network its
endpoint serves.

```toml
network = "mainnet"

[providers.subfrost]
driver = "subfrost"
network = "mainnet"
url = "https://mainnet.subfrost.io/v4/jsonrpc"
```

### Split responsibilities

Use `capabilities` to divide work between providers, for example Esplora for
chain access and Subfrost as an asset guard:

```toml
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

Exact names allow finer splits, such as `["chain.sync"]` on one provider and
`["chain.fees", "chain.broadcast"]` on another. sats prefers the sync
provider for fees and broadcast when it offers them. Otherwise each chain
capability must resolve to exactly one provider, and two equal candidates
are an ambiguity error.

### Authentication

```toml
[providers.private_esplora]
driver = "esplora"
network = "mainnet"
url = "https://bitcoin.example/api"

[providers.private_esplora.auth]
bearer = "replace-with-token"     # sent on reads and broadcast

[providers.subfrost]
driver = "subfrost"
network = "signet"
url = "https://signet.subfrost.io/v4/jsonrpc"
api_key = "replace-with-key"      # sent in Subfrost's API-key header
```

sats writes `config.toml` owner-only (`0600`, in an owner-only directory).
If you create or edit it yourself, keep its permissions restrictive.
Diagnostics, debug output, and `sats providers` show only a provider's
origin. User-info, paths, queries, fragments, and
response bodies are omitted, including from failures saved on agent requests.
Credentials are never copied into the MCP setup commands.

### Fee targets

Providers estimate fees; you choose the confirmation target, per network, from
1 to 1008 blocks (default 2):

```toml
[fee_targets]
signet = 1008
```

The target applies to every send, including approved agent requests. A human
`--fee-rate` overrides it for one command. Agents can't choose a target or
rate, and their grant's fee cap still applies to the real fee.

## Resolution order

For the selected network:

1. `--provider` values on the command line replace all configured providers;
2. `[providers.*]` entries supply their capabilities;
3. a legacy `[esplora]` map of network to URL fills missing chain capabilities;
4. the built-in defaults fill any chain capability still missing.

Guards and `alkanes.view` come only from `[providers.*]` entries that list
them. Resolution does no network I/O. Each operation checks the provider's
network when it runs.

## One-off overrides

```sh
sats --provider esplora=https://mempool.space/signet/api balance
sats --provider subfrost=https://signet.subfrost.io/v4/jsonrpc balance
```

The syntax is `KIND=URL`, and it's repeatable. Overrides provide chain
capabilities only and have no fallback behind them. Two overrides that both
offer `chain.sync` are ambiguous.

> **Overrides drop guards.** An override replaces every configured provider,
> including asset guards. A `send` or `agent approve` run with `--provider`
> queries no guard and relies on the 546/330-sat heuristic alone.

## UTXO protection

Before selecting coins, sats builds one exclusion set:

1. outputs worth exactly 546 or 330 sats, common inscription postage, unless
   `--allow-dust`;
2. every output a configured guard reports, unless `--no-guards`;
3. the union of both, passed to coin selection as unspendable.

The postage check is a heuristic. Assets on other values need a guard. A
human can bypass either layer for one command. Agents never can.

Guards can only remove candidates. A wrong guard can block a spend by
over-protecting, or miss an asset, but it can never add an input or authorize
a signature. A configured guard that can't answer stops the spend.

## Failure behavior

- Sync checks the provider's genesis block, and so its network, before
  using any data.
- A sync failure stops every send. `balance`, `status`, and `history` fall
  back to cached data and say so.
- A fee estimate that is non-finite, negative, or above 10,000 sat/vB is an
  error, never a fee. When estimation fails, a human can pass `--fee-rate`.
- Malformed checkpoint data fails the sync, not the process.
- Each HTTP request times out after 30 seconds. Esplora retries a retryable
  GET up to twice with backoff. Subfrost retries one safe read after HTTP
  429, honoring `Retry-After` up to 60 seconds. Timeouts and broadcasts are
  never retried automatically.
- For hosts with several DNS addresses, each attempt before the last one is
  capped at 2 seconds, so a healthy address can still answer. TLS
  verification and the request itself are unchanged.
- The 30-second limit is per request, not per wallet scan. A broadcast
  timeout is an uncertain outcome, not a rejection: the signed transaction
  stays saved for `sats tx broadcast`.

## Adding a driver

Provider code is security-sensitive. Follow the recipe in
[AGENTS.md](../AGENTS.md#new-provider-or-capability).
