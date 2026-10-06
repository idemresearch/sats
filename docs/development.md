# Development

Build, test, and verify sats locally. Read [AGENTS.md](../AGENTS.md) first:
it defines module ownership, the security invariants, and per-feature
recipes.

## Requirements

- Rust 1.89 or newer (the workspace MSRV), with `rustfmt`, Clippy, and
  `rust-analyzer` from `rust-toolchain.toml`;
- the `wasm32-unknown-unknown` target;
- a POSIX shell, for installer tests;
- Python 3 (standard library only), for terminal approval tests.

```sh
rustup component add rustfmt clippy rust-analyzer
rustup target add wasm32-unknown-unknown
```

## Build and run

```sh
cargo build --locked
SATS_DIR="$(mktemp -d)" cargo run -- init
```

Always use an isolated `SATS_DIR` for manual runs. Never point a development
build at a real wallet, or put a real mnemonic in a command, fixture, log, or
screenshot.

| Build | Command |
|---|---|
| Without MCP | `cargo install --locked --path crates/sats --no-default-features` |
| With Alkanes views (experimental) | `cargo install --locked --path crates/sats --features experimental-alkanes` |
| With Alkanes execution (development only, never released) | `cargo test -p sats --locked --features experimental-alkanes-execute --test alkanes` |

## Verification gate

Run the narrowest relevant test while you work. Before calling a change
ready, run the full gate, which matches CI:

```sh
sh -n setup.sh scripts/test-setup.sh
sh scripts/test-setup.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
cargo clippy -p sats --all-targets --features experimental-alkanes-execute -- -D warnings
cargo test -p sats --locked --features experimental-alkanes-execute --bins --test alkanes
cargo check -p sats-core --target wasm32-unknown-unknown
cargo check -p sats-web --target wasm32-unknown-unknown
```

Terminal tests open a controlling TTY for hidden password entry, and provider
fixtures bind localhost sockets, so sandboxes must allow both. WASM
dependency builds need a C compiler with WASM support. On macOS you may need
LLVM Clang instead of Apple Clang, set through `CC_wasm32_unknown_unknown`
and `AR_wasm32_unknown_unknown`.

Then exercise the freshly built binary on the path you changed, with an
isolated `SATS_DIR`. For MCP changes, run the local binary as the stdio
server and call the affected tool. Report exactly what you ran. If something
couldn't be run, say so rather than calling the change complete.

## Test layers

| Layer | Location | Covers |
|---|---|---|
| Core unit | `crates/sats-core/src/` | Preparation, authorization, sealing, seeds, signing, amounts |
| Native unit | `crates/sats/src/` | Config, providers, storage, the executor with a signer probe |
| Playground unit | `crates/sats-web/src/` | Simulated wallet loop, grant lifecycle, denials |
| CLI integration | `crates/sats/tests/` | Wallet flows, failures, providers, grants, request review, Alkanes |
| MCP integration | `crates/sats/tests/mcp.rs` | Tool schemas, filing, idempotency, the approval loop, startup refusal, revocation |
| Installer | `scripts/test-setup.sh` | Targets, checksums, version pinning, PATH edits, atomic replacement |
| WASM | CI `wasm-check` | `sats-core` and `sats-web` build for `wasm32-unknown-unknown` |

Tests must not need public services, credentials, or real funds. Cover network
behavior with deterministic providers and temporary state.

### Vendored `minreq`

The root `[patch.crates-io]` selects `vendor/minreq`, based on upstream
2.14.1. It caps each non-final TCP connection attempt at 2 seconds, so one
unreachable DNS address can't consume the whole request deadline. No HTTP
bytes are sent during fallback, and TLS verification is unchanged. The
Esplora test module includes the patched connector directly, so its deadline
and fallback tests run in the normal gate. Keep those tests and the
broadcast-timeout tests when updating the dependency. Provenance is in
[SATS-PATCH.md](../vendor/minreq/SATS-PATCH.md).

## Website playground

The site's terminal runs `sats-web` against a simulated chain. The generated
module in `website/public/playground/` is committed, so the site deploys
without Rust. After changing `sats-core` or `sats-web`, regenerate it:

```sh
cargo install wasm-bindgen-cli --version <pinned in Cargo.toml>
sh scripts/build-playground.sh
```

The social preview image, `website/public/og.png`, is rendered from
`website/og/og.html` with headless Chrome and also committed. After changing
the template, regenerate it:

```sh
sh scripts/build-og.sh
```

## Writing docs

`docs/cli.md`, `mcp.md`, `providers.md`, `security.md`, `architecture.md`,
and `direction.md` are also published on the website. Keep them lean:

- **One fact, one home.** Link instead of restating. Commands and flags go in
  `cli.md`, the agent contract in `mcp.md`, configuration in `providers.md`,
  failure and signing semantics in `security.md`, code structure in
  `architecture.md`, and rationale in `direction.md`. The README is a
  pitch and quickstart only.
- **Open with one sentence under 160 characters.** The site uses a page's
  first paragraph as its description.
- **Write for the reader of that page.** User pages say what you see and
  what to do. Locks, ledgers, and crash windows belong in `security.md`.
- **Use tables for anything enumerable,** such as flags, states, errors, and
  paths.
- **Document shipped behavior only.** No roadmaps, dates, or speculative
  designs.
- **Use plain blockquotes for callouts in `docs/`.** The site doesn't render
  GitHub's `> [!NOTE]` syntax.

CLI help, MCP schemas, tests, and typed contracts are authoritative. When
prose disagrees with them, fix it in the same change.

## Release builds

Release builds optimize for size, use fat LTO, abort on panic, and strip
symbols. See [Releasing](releasing.md).
