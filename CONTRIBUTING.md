# Contributing to sats

sats is a small, security-sensitive Bitcoin wallet. Keep changes narrow,
typed, tested, and honest about their behavior.

Before editing code, read [AGENTS.md](AGENTS.md). It is the canonical guide to
module ownership, safety invariants, feature recipes, and verification. The
[documentation index](docs/README.md) links to the current architecture and
surface-specific references.

## Setup

Requirements:

- Rust 1.85 or newer;
- the `wasm32-unknown-unknown` target for the portability check;
- a POSIX shell for installer tests.

```sh
rustup target add wasm32-unknown-unknown
cargo build --locked
```

Signet is the default network. Use an isolated state directory when manually
testing so development data never touches a real wallet:

```sh
SATS_DIR="$(mktemp -d)" cargo run -- init
```

## Development loop

1. Identify the owning module and read its tests.
2. Add or adjust the narrowest test that expresses the desired contract.
3. Implement the smallest coherent change.
4. Exercise the built binary on the changed path when feasible.
5. Run the full verification gate from [Development](docs/development.md).
6. Update the relevant shipped-behavior documentation.

## Pull requests

- Keep one primary purpose per pull request.
- Explain the user-visible behavior and security implications.
- List the exact checks and manual paths you ran.
- Call out JSON, serialized-state, or configuration compatibility changes.
- Do not include roadmap promises, target dates, or speculative designs.
- Never include real mnemonics, provider credentials, wallet databases, plan
  files, or grant files in fixtures, logs, screenshots, or commits.

Review starts from the invariants in [Security](docs/security.md), not merely
from whether the code compiles.
