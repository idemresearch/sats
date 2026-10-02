# Providers and asset protection

Each network makes two choices: where its chain data comes from, and whether
its assets are protected. You don't need any configuration to start.

| Provider | Connect with | Command |
|---|---|---|
| mempool.space | Nothing: the default | — |
| Subfrost | API key | `sats providers add subfrost` |
| Your own Esplora server | URL, optional bearer token | `sats providers add esplora --url URL` |

```sh
sats providers                    # what this network uses (same as `sats providers list`)
sats providers add subfrost       # save and check your key, use Subfrost for chain data
sats providers protect on         # ask Subfrost about every output before a send
sats providers use mempool        # switch chain data: mempool, subfrost, or esplora
sats providers remove subfrost    # delete the key
```

```text
signet providers

Chain data        subfrost  https://signet.subfrost.io  api key
Asset protection  on        subfrost checks every output before a send
Alkanes views     subfrost  https://signet.subfrost.io  api key
```

Every command acts on the active network; `--network` picks another. With
`--json`, each prints the network's resulting overview: `chain`,
`asset_protection`, and `alkanes_views`, each an object with `provider`,
`url` (the origin only), `auth` (`none`, `api_key`, or `bearer`), and
`source` (`default`, `config`, or `override`), or `null` when off.

## Chain data

Exactly one source serves a network's wallet sync, fee estimates, and
broadcast.

| Source | Endpoint |
|---|---|
| `mempool` | The default: `https://mempool.space/api`, `/signet/api`, or `/testnet4/api` |
| `subfrost` | Subfrost's mainnet or signet endpoint, or a `--url` you give |
| `esplora` | The server you add. Regtest defaults to `http://localhost:3002`. |

`sats providers add` makes the added provider the network's chain source, and
`sats providers use` switches between them. Both check the endpoint's genesis
block, and so its network, before saving. If the check fails, nothing is
saved.

## Asset protection

Asset protection is off by default. Turning it on is a trust decision, so it
happens only when you run `sats providers protect on`, pass `--protect` to
`sats providers add subfrost`, or answer yes when adding Subfrost on a
terminal (the question defaults to no). Nothing turns it off as a side
effect: `sats providers remove subfrost` refuses while any network's
protection uses it.

With protection on, sats asks Subfrost's ord index (inscriptions and runes)
and Alkanes index about every candidate output before selecting coins. It
builds one exclusion set:

1. outputs worth exactly 546 or 330 sats, common inscription postage, unless
   `--allow-dust`;
2. every output Subfrost reports, unless `--no-guards`;
3. the union of both, passed to coin selection as unspendable.

The postage check runs with protection off too, but it is a heuristic: assets
on other values need protection. A human can bypass either layer for one
command. Agents never can.

Protection can only remove candidates. A wrong answer can block a spend by
over-protecting, or miss an asset, but it can never add an input or authorize
a signature. When protection is on and Subfrost can't answer, the spend
stops. Guard answers are presence-only: sats never parses inscriptions,
runestones, or other asset data to decide what to exclude.

Protection doesn't depend on where chain data comes from. mempool.space for
chain data and Subfrost for protection is a normal setup.

## Alkanes views

`sats alkanes` uses Subfrost whenever it is set up for the network, whatever
the chain source. Views are read-only and never authorize anything. The one
exception to presence-only answers is `sats alkanes`, which encodes only
calls it builds itself (in the `sats-alkanes` crate) and displays view results
without trusting them.

## Adding a provider

`sats providers add subfrost` asks for your API key with hidden input. Without
a terminal it reads one line from stdin, so
`printf '%s\n' "$KEY" | sats providers add subfrost` works in scripts. A
credential is never a command-line argument. One saved key serves every
network: on another network, run `sats --network mainnet providers add subfrost`
and press enter to keep it.

`sats providers add esplora --url URL` asks for an optional bearer token;
press enter for none. A saved token is kept on enter only for the same URL:
it never follows a new one.

| Option | Meaning |
|---|---|
| `--url URL` | Endpoint. Required for Esplora, and for Subfrost on testnet4 and regtest. |
| `--protect` | Subfrost only: also turn on asset protection. |

Subfrost's JSON-RPC methods map onto sats's sync, fee, broadcast, guard, and
view operations, and broadcast uses `btc_sendrawtransaction`. On a fresh
wallet the driver derives scripts lazily and stops after 20 consecutive
unused scripts on each of the receive and change keychains.

## Removing a provider

`sats providers remove subfrost` deletes the key and every Subfrost endpoint
setting. Any network that used Subfrost for chain data goes back to its
default, and the command says so. `sats providers remove esplora` removes the
active network's Esplora server, and the network goes back to its default if
it was using it.

## Configuration

The commands above write the [config file](cli.md#configuration). You can
also edit it by hand:

```toml
network = "signet"

[subfrost]
api_key = "replace-with-key"        # shared by every network

[signet]
chain = "subfrost"                  # mempool (default), subfrost, or esplora
protect_assets = true
fee_target = 1008                   # confirmation target in blocks

[mainnet]
chain = "esplora"

[mainnet.esplora]
url = "https://bitcoin.example/api"
bearer = "replace-with-token"       # optional

[regtest]
subfrost_url = "http://localhost:18888/v4/jsonrpc"
```

`subfrost_url` replaces the built-in Subfrost endpoint for one network. It is
how Subfrost is set up where it has no public endpoint.

sats writes `config.toml` owner-only (`0600`, in an owner-only directory). If
you edit it yourself, keep its permissions restrictive. Every change made
with `sats providers` rewrites the file, so comments are not kept. Diagnostics, debug output, and
`sats providers` show only an endpoint's origin. User-info, paths, queries,
fragments, and response bodies are omitted, including from failures saved on
agent requests. Credentials are never copied into the MCP setup commands.

A config file in the earlier `[providers.<name>]`, `[esplora]`, or
`[fee_targets]` format is refused with instructions: remove those sections and
run `sats providers add` again.

A choice the network can't serve, such as `chain = "esplora"` with no URL or
protection with no Subfrost, is an error for every command that needs the
chain. Local commands keep working while you fix it, and `sats providers`
shows the settings as written next to the error.

### Fee targets

Providers estimate fees; you choose the confirmation target, per network, from
1 to 1008 blocks (default 2), with `fee_target` under `[<network>]`. The
target applies to every send, including approved agent requests. A human
`--fee-rate` overrides it for one command. Agents can't choose a target or
rate, and their grant's fee cap still applies to the real fee.

## One-off overrides

```sh
sats --provider esplora=https://mempool.space/signet/api balance
sats --provider subfrost=https://signet.subfrost.io/v4/jsonrpc balance
```

`--provider KIND=URL` replaces the chain source for one command, and can be
given once. Asset protection and Alkanes views stay as configured. The saved
Subfrost key is never sent to an override URL.

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
[AGENTS.md](../AGENTS.md#new-provider).
