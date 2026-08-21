# sats on your phone, with no app

The goal: use your wallet from a phone without installing anything from an
app store. The architecture already supports two complementary routes, and
they compose.

## Route 1 — the wallet is a web page (wasm PWA)

Compile `sats-core` to WebAssembly and serve it as a static page. Open it
in the phone's browser; optionally "Add to Home Screen" for an app-like
feel. Everything sensitive happens client-side:

- **Network** — a chain provider over `fetch()` (Esplora first). mempool.space serves CORS-friendly
  APIs, so chain sync works straight from the browser.
- **Storage** — the sealed seed and the watch-only wallet DB live in
  IndexedDB/OPFS. The seal format (argon2id + XChaCha20-Poly1305) carries
  over unchanged: the browser holds only ciphertext at rest.
- **Randomness** — the browser's crypto RNG, via getrandom's wasm backend.

Honest cost: you trust the web origin serving the page. Self-hosting and
subresource integrity mitigate but don't eliminate that. Use this route for
spending money you'd carry in a pocket, not savings.

## Route 2 — the phone is a channel, not a wallet (grants)

Keep `sats` running on a machine at home and put `sats mcp --agent phone`
behind something you can message (an agent bridged to a chat app, or a
small bot). From the phone you say "send 10k to tb1p…"; every send is
checked against the grant's budget, per-tx cap, fee cap, and expiry. The
phone holds nothing — no keys, no ciphertext, not even a page. A stolen
phone is capped at the grant's remaining budget, and `sats revoke phone`
kills it from anywhere, mid-session.

This needs no new wallet code: it's the existing grant + MCP surface with a
transport in front of it.

## Route 3 — air-gapped hybrid (optional)

The wasm page runs watch-only (balance, receive, plan), signing stays home:
the page exports the unsigned plan as a QR, the home machine `sats sign`s
it, and hands back a signed-PSBT QR to broadcast. This is the existing
"PSBT in, PSBT out" step-by-step flow with QR as the transport.

## Priority

Route 1 is the build; Route 2 already works modulo a transport. So the work
queue is the wasm path, smallest risk first:

1. **Prove the core compiles to wasm.** ✅ Done — `sats-core` builds for
   `wasm32-unknown-unknown` with no code changes; the only wiring needed
   was getrandom's browser backends (see `crates/sats-core/Cargo.toml`
   target section and `.cargo/config.toml`). Guarded in CI by
   `cargo build -p sats-core --target wasm32-unknown-unknown`.
2. **`sats-web` crate: a wasm-bindgen boundary.** A thin crate exporting
   the core flows to JS — create/restore (seal/unseal), receive address,
   build plan from a UTXO snapshot, sign, serialize PSBT. Core stays free
   of wasm-bindgen; only `sats-web` depends on it.
3. **JS shell: network + persistence.** Chain-provider sync over `fetch()`
   feeding the core's update types; sealed-seed and changeset persistence
   in IndexedDB/OPFS. This is where core's "no filesystem, no network"
   discipline pays off — the shell implements what core deliberately lacks.
4. **PWA wrapper.** Static page, manifest, service worker for offline
   balance viewing. No backend.
5. **Route 2 transport.** A minimal bridge from a messaging surface to the
   MCP server, riding the existing grant enforcement.

**Not in scope** (same as V1 generally): Lightning, coin control,
multi-wallet, RBF.
