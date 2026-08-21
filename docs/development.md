# Development

Read [AGENTS.md](../AGENTS.md) before changing implementation code. It defines
module ownership, security invariants, and the checks required for each kind
of feature.

## Requirements

- Rust 1.85 or newer;
- `rustfmt` and Clippy;
- the `wasm32-unknown-unknown` target;
- a POSIX shell for installer checks.

```sh
rustup component add rustfmt clippy
rustup target add wasm32-unknown-unknown
```

## Build and run

```sh
cargo build --locked
cargo run -- --help
```

Use an isolated state directory for manual development runs:

```sh
SATS_DIR="$(mktemp -d)" cargo run -- init
```

Signet is the default. Never point a development smoke test at a real wallet
directory or put a real mnemonic in a command, fixture, log, or screenshot.

## Verification gate

The full local gate matching CI is:

```sh
sh -n setup.sh scripts/test-setup.sh
sh scripts/test-setup.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
cargo check -p sats-core --target wasm32-unknown-unknown
```

Run the narrowest relevant unit or integration test during development, then
run the full gate before reporting the work ready.

## Test layers

| Layer | Location | Covers |
|---|---|---|
| Core unit tests | `crates/sats-core/src/` | Planning, authorization, sealing, seed derivation, signing, serialization |
| Native unit tests | `crates/sats/src/` | Amounts, config, providers, storage, and helpers |
| CLI integration | `crates/sats/tests/cli.rs` | Isolated wallet flows, failures, providers, guards, and grant lifecycle |
| MCP integration | `crates/sats/tests/mcp.rs` | Tool schemas, granted sends, startup refusal, and live revocation |
| Installer | `scripts/test-setup.sh` | Targets, checksums, version pinning, PATH edits, and atomic replacement |
| WASM portability | CI `wasm-check` | `sats-core` remains buildable for `wasm32-unknown-unknown` |

Network-dependent behavior should be covered with deterministic providers and
temporary state. Unit and integration tests must not require public services,
credentials, or real funds.

## Manual verification

Tests do not replace exercising a user-facing change. Run the freshly built
binary with an isolated `SATS_DIR` and use a path that demonstrates the
changed behavior. For MCP changes, launch the local binary as the stdio server
and exercise the affected tool contract.

Report exactly which commands ran. If the environment cannot exercise a
required path, state that limitation rather than declaring unverified behavior
complete.

## Documentation

Repository prose documents shipped behavior only:

- `README.md` is the product entry point and short quickstart;
- `docs/cli.md` and `docs/mcp.md` describe user-facing contracts;
- `docs/architecture.md` and `docs/security.md` describe current boundaries;
- `docs/providers.md` describes the shipped provider model;
- `AGENTS.md` describes how to change the system safely.

Do not add roadmap queues, target dates, or speculative designs. Work that is
not implemented belongs in the maintainer's issue or private planning system.

When commands or schemas change, update their focused reference rather than
growing the README. CLI help, MCP schemas, tests, and typed Rust contracts
remain authoritative.

## Release builds

Release builds optimize for size, use fat LTO, abort on panic, and strip
symbols. Stable tags publish four native archives. See
[Releasing](releasing.md) for the tag and asset process.
