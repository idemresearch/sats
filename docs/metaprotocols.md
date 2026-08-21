# metaprotocols: bring your own indexer

sats stays a basic Bitcoin wallet. Metaprotocols — ordinals, runes,
alkanes, BRC-20, whatever comes next — are supported the way chain access
already is: **you point the wallet at a URL.** The wallet never parses,
indexes, or interprets metaprotocol data. It only learns which of its
UTXOs an external indexer says are carrying something, and refuses to
spend those automatically.

Chain truth comes from a configurable Esplora URL today. Asset truth
comes from a configurable indexer URL — a subfrost RPC, a sandshrew
endpoint, an ord server, your own adapter. Capability is an endpoint you
supply, not code we ship.

Nothing in this document is implemented yet, deliberately. The wallet
gets stable first; this is the strategy that stability work must not
paint out.

## Principles

**1. Protocol ignorance.** sats contains zero metaprotocol code: no
envelope parsing, no runestone decoding, no inventory of protocols. A new
metaprotocol requires no sats release — only an indexer that answers the
one question below. The trust story stays auditable: the wallet cannot
misinterpret an inscription because it never interprets one.

**2. Indexer as URL.** Asset awareness is opt-in configuration, mirroring
`[esplora]`. No URL configured → no asset features, and no network calls
to anyone but your Esplora endpoint. The indexer is a *view*, never an
authority over spending: it can only make the wallet more conservative
(mark UTXOs as off-limits), never authorize anything.

**3. A safety floor without any indexer.** Even a metaprotocol-ignorant
wallet shouldn't casually burn someone's inscription. Default coin
selection will exclude UTXOs at the classic postage values — 546 and
330 sats — overridable with `--allow-dust`. Honest cost: this is a
heuristic. False positives (real dust you can't auto-spend) are
recoverable with the flag; false negatives (a 10,000-sat inscription)
are exactly what indexers are for.

**4. Core stays pure.** `sats-core` keeps its no-filesystem, no-network,
no-clock discipline. Core never fetches from an indexer; it accepts
*facts* — a set of outpoints to avoid — as plain inputs. This is the same
split that made the wasm port free: the shell implements what core
deliberately lacks.

## The one question

The wallet needs exactly one answer to be safe:

> Of these outpoints, which are carrying anything?

So the wallet-facing contract is a single logical operation:

```
request:  { "outpoints": ["txid:vout", ...] }
response: { "protected": ["txid:vout", ...] }
```

The response may optionally include a per-outpoint `kind` for display
("inscription", "rune: UNCOMMON•GOODS"); sats treats it as an opaque
string — shown to you, never used in logic.

Pinned semantics:

- **Fail closed.** If an indexer is configured but unreachable, planning
  errors — it does not silently proceed. An indexer outage must not
  become a burned inscription. `--no-indexer` is the explicit,
  per-invocation escape.
- **Union, not replace.** Indexer results combine with the dust
  heuristic. An ord indexer doesn't know about alkanes; conservatism
  stacks.
- **Restrictive only.** A protected outpoint is excluded from selection.
  Protection never enables new behavior.

This is deliberately *not* a normalization of existing indexer APIs.
ord servers speak REST (`/output/<outpoint>`), sandshrew/subfrost speak
namespaced JSON-RPC (`ord_*`, `alkanes_*`, `protorunes_*`), and they
will not converge. Normalizing them inside sats would smuggle protocol
knowledge back into the wallet. Instead sats defines its own tiny
contract; thin transport dialects (`kind = "sandshrew" | "ord"`) may
ship as shims that map the one question onto those servers' existing
APIs — pure request/response mapping, still no interpretation. Whether
dialects live in the binary at all, or stay external adapters, is an
open question below.

**Rejected: a generic RPC passthrough** (`sats rpc <method> ...`
proxied to the indexer). It adds no safety, turns sats into a curl
replacement, and implies a support surface for every namespaced method.
If you want raw sandshrew calls, curl exists.

## Configuration

Mirrors `[esplora]`: per-network, no defaults, absence means off. sats
will never ship a default indexer URL — that would make a third party a
silent dependency of every send.

```toml
network = "signet"

[esplora]
signet = "https://mempool.space/signet/api"

# Optional. Absent = no asset awareness; dust heuristic only.
[indexers.signet]
url  = "http://localhost:8080"
kind = "ord"        # "sats" (native contract, default) | "sandshrew" | "ord"
```

CLI surface, sketched:

```sh
sats send tb1p... 25k --indexer <url>   # one-shot override
sats send tb1p... 25k --no-indexer      # skip the asset check, loudly
sats send tb1p... 25k --allow-dust      # override the postage heuristic
```

What you'd see:

```
$ sats send tb1p... 25k
⚠ 2 utxos excluded (indexer: carrying assets)
Send   25,000 sat
Fee       412 sat
...
```

And when the indexer is down:

```
$ sats send tb1p... 25k
error: indexer unreachable (http://localhost:8080) — refusing to plan
       without asset check. retry, or pass --no-indexer to plan anyway
```

Bypass flags are per-invocation only. Config should not be able to
silently disable a safety check.

## Where it lives

| Concern | Where |
|---|---|
| Excluding outpoints from selection | `sats-core::engine::build_plan` gains `unspendable: &[OutPoint]`, passed through to BDK's `unspendable()` |
| Postage heuristic | pure `dust_suspects()` in `sats-core` — testable, and the wasm PWA gets the same safety floor for free |
| `[indexers]` config, HTTP fetch, fail-closed policy, dialects, flags | `crates/sats`, beside the existing Esplora plumbing |
| Grants / authz | unchanged — see below |

Protection runs *before* planning, on the shared path. Agent sends over
MCP hit the same step, so an agent under a grant cannot burn a protected
UTXO either — agents inherit the safety property for free, with no
change to the authorization engine.

## Phases

1. **Phase 0 — this document.** No code. Gating condition for
   everything below: the basic-bitcoin wallet is stable.
2. **Phase 1 — the safety floor.** `unspendable` on `build_plan`, pure
   `dust_suspects()`, heuristic on by default with `--allow-dust`. No
   config, no network. The smallest change that stops "wallet burns your
   inscription" by default.
3. **Phase 2 — the URL.** `[indexers]` config, `--indexer`/
   `--no-indexer`, the native contract, fail-closed enforcement.
   Possibly `sats config get/set` (which also fixes hand-editing
   Esplora URLs) and the first dialects.
4. **Phase 3 — deferred, a sketch only.** Asset-aware operations:
   seeing what a UTXO carries, deliberately transferring an asset,
   asset budgets on grants. Protocol-correct transaction construction is
   the first real breach of principle 1; the likely answer is
   indexer-built PSBTs riding the existing "PSBT in, PSBT out" flow,
   with sats reduced to inspect / sign / broadcast. None of this is
   designed here — the only commitment is that phases 1–2 must not
   preclude it.

## Not in scope

**Ever:** metaprotocol parsing or indexing inside sats; shipping or
defaulting any third-party indexer; the indexer as a spend authority.

**For now:** everything in phase 3; multi-indexer union; RPC
passthrough; marketplace / PSBT-trade flows. Plus the standing V1
exclusions (Lightning, coin control, multi-wallet, RBF). Note the
exclusion mechanics here are *not* coin control — there is no UTXO
picking, only refusal.

## Open questions

1. Do dialects (sandshrew/ord shims) belong in the sats binary, or
   should phase 2 be native-contract-only with adapters external?
2. Multiple indexers per network: union semantics and failure policy.
3. Postage thresholds: fixed constants (546, 330) or configurable?
   Leaning fixed until evidence otherwise.
4. Wasm parity: the dust heuristic ports free; the indexer fetch needs a
   `fetch()` twin in the JS shell. Where does that sit in
   [phone.md](phone.md)'s queue?
5. Should `sats balance` report protected value separately
   ("130,000 sat, of which 1,092 protected")? Cheap, high clarity;
   touches the `--json` schema.
