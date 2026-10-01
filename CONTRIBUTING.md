# Contributing to sats

sats is a small, security-sensitive Bitcoin wallet. Keep changes narrow,
typed, tested, and honest about their behavior.

1. Read [AGENTS.md](AGENTS.md): module ownership, safety invariants, and
   feature recipes.
2. Set up with [Development](docs/development.md), which lists the
   toolchain, the verification gate, and the docs style guide.
3. Find the owning module and its tests. Write the narrowest test that
   expresses the change, then the smallest coherent implementation.
4. Run the built binary on the changed path, with an isolated `SATS_DIR`.
5. Run the full verification gate and update the docs that describe the
   behavior.

## Pull requests

- Keep one primary purpose per pull request.
- Explain the user-visible behavior and its security implications.
- List the exact checks and manual paths you ran.
- Call out changes to JSON output, serialized state, or configuration.
- Leave out roadmap promises, target dates, and speculative designs.
- Never include real mnemonics, provider credentials, wallet databases, or
  grant files in fixtures, logs, screenshots, or commits.

Review starts from the invariants in [Security](docs/security.md), not from
whether the code compiles.
