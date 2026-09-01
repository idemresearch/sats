# Working in sats

Instructions for coding agents and contributors changing this repository.
Read this file before editing code. Then read only the focused documents
linked for the area you are changing.

## Fast start

1. Read the task and identify the owning module before editing.
2. Inspect `git status` and preserve unrelated work.
3. Run the narrowest relevant test before and after the change.
4. Keep domain behavior in `sats-core` and environment effects in `sats`.
5. Update tests and shipped-behavior documentation in the same change.
6. Run the repository verification commands before declaring the work ready.

Primary references:

- [Direction](docs/direction.md): the invariant and stable design decisions.
- [Architecture](docs/architecture.md): module ownership and request flows.
- [Security](docs/security.md): key, grant, signing, and provider invariants.
- [CLI](docs/cli.md): current user-facing commands and configuration.
- [MCP](docs/mcp.md): agent tool contracts and denial semantics.
- [Providers](docs/providers.md): capability resolution and UTXO guards.
- [Development](docs/development.md): local workflow and verification.

## Product contract

sats is an on-chain Bitcoin wallet with two native surfaces:

- a human-operated CLI;
- an MCP server operating under a named, human-created spending grant.

Agent signing happens in `satsd`, a local per-network daemon holding the
seed in memory. The MCP server is a shim: it prepares and broadcasts, and
carries a bearer token that names a policy rather than opening anything.

Prepared spends are PSBTs. Normal sends keep them in memory; an explicit
export writes the unsigned PSBT to a user-named file artifact, and stored
PSBT sessions are read-only legacy state from older releases. Once signed,
durable state contains private raw transaction hex rather than a signed
PSBT. The persisted BDK wallet is watch-only. Human and agent sends share
validation, sync, protection, fee estimation, and preparation. Agent sends
add deterministic authorization and a one-time human approval before a
signature is produced: no agent-originated spend may reach the signer
without explicit, one-time human authorization bound to that action.
There is no autonomous agent spend mode. See `docs/direction.md`.

Signet is the default. Mainnet must remain an explicit choice.

## Repository ownership

| Path | Owns |
|---|---|
| `crates/sats-core/src/engine.rs` | Pure transaction planning and conservative UTXO exclusion |
| `crates/sats-core/src/authz.rs` | Pure grant decisions, reservation, and refund rules |
| `crates/sats-core/src/token.rs` | Agent capability tokens: mint, hash, constant-time verify |
| `crates/sats-core/src/verify.rs` | Recomputing a PSBT's payments and fee from the wallet's own descriptors |
| `crates/sats-core/src/plan.rs` | Ephemeral prepared spends, explicit PSBT sessions, finalized transaction records, and legacy-plan compatibility |
| `crates/sats-core/src/seed.rs` | BIP-39 seed handling and BIP-86 descriptors |
| `crates/sats-core/src/seal.rs` | Versioned authenticated secret sealing |
| `crates/sats-core/src/signer.rs` | Signer trait and local in-memory signer |
| `crates/sats/src/main.rs` | Composition and command dispatch only |
| `crates/sats/src/cli.rs` | Clap command and flag definitions |
| `crates/sats/src/commands/` | Human CLI workflows and rendering |
| `crates/sats/src/provider/` | Native chain providers, capability resolution, and guards |
| `crates/sats/src/store.rs` | Paths, atomic files, permissions, PSBT sessions, finalized transactions, legacy plans, and grants |
| `crates/sats/src/walletd.rs` | SQLite-backed watch-only BDK wallet |
| `crates/sats/src/daemon/` | satsd: socket protocol, lock state, and the granted agent send path |
| `crates/sats/src/mcp/` | MCP transport and schemas — a shim over the daemon |
| `crates/sats/tests/` | Native CLI and MCP integration tests |
| `crates/sats-alkanes/src/` | Pure Alkanes protocol composition: ids, cellpacks, protostones, inspection, simulation views |
| `crates/sats-web/src/lib.rs` | Browser playground: wasm bindings over sats-core and the simulated chain |
| `website/` | Project website, including the interactive playground terminal |

`main.rs` is a composition root. Do not put feature logic there.

## Non-negotiable invariants

### Portable core

`sats-core` must remain environment-agnostic:

- no filesystem access;
- no networking;
- no system clock reads;
- no async runtime;
- no terminal output;
- no CLI, MCP, or WebAssembly binding types.

Pass facts and time into the core as typed inputs. Persistence, chain access,
and rendering belong to callers.

### Keys and signing

- Persist only public wallet descriptors in SQLite.
- Never persist plaintext mnemonic or private descriptors.
- Keep private key material inside the shortest practical scope and zeroize
  it where supported.
- Preserve the `Signer` boundary; do not make callers depend directly on the
  local mnemonic signer.
- Treat a signed transaction as spendable even when broadcast fails.
- Persist finalized transaction hex before attempting broadcast, in the
  process that produced the signature. Do not persist a signed PSBT for a
  fully finalized single-sig send.
- A grant carries no key material. Never reintroduce a field from which a
  seed can be recovered; only a token hash belongs beside a policy.
- A bearer token is emitted once, at creation, and never persisted.
- Write PSBT sessions and finalized transaction records with restrictive
  permissions; both expose wallet and payment metadata.

### Agent authorization

- `authorize_spend` is the single deterministic policy decision.
- Never authorize against an amount or fee a caller reported. Derive both
  from the PSBT with `sats_core::verify` and refuse on disagreement.
- Only `satsd` signs for an agent. Nothing in the MCP process may hold key
  material or reconstruct a mnemonic.
- Operational conditions carry a typed `error_code`; policy refusals carry a
  denial `reason`. Never conflate them — a locked wallet is not a denial a
  human can approve.
- A send never allows on the grant alone: the ladder terminates every
  in-envelope proposal in the approvable `ask_required`, and the only
  allow a spend can reach rides a digest-bound one-time approval. Never
  reintroduce an autonomous path to the signer.
- Preserve check order: the hard envelope (expiry, suspension, observe,
  intent authority, overflow, hard ceiling) before the ask band
  (recipient rule, amount cap, fee cap, budget, terminal ask). An
  approvable reason must never mask a hard one.
- Legacy `auto` records read as `ask` and are rewritten as `ask` by the
  stores on first use; never honor them as autonomous.
- Budget includes amount plus fee.
- Reserve and persist budget before signing, and construct the signer only
  after the reservation succeeds (`send::authorize` takes it as a factory
  for exactly this reason).
- Refund only when signing failed and no signature exists.
- Never refund a signed transaction after a broadcast failure.
- Re-read the grant on every send so revocation takes effect immediately.
- Denials are typed, successful MCP results. Do not turn expected denials
  into transport errors or retries.
- Agents must not receive `--allow-dust`, `--no-guards`, password, seed, or
  raw signing authority outside the grant contract.

### Providers and UTXO safety

- Planning must not continue on stale chain state after sync failure.
- Guards may only remove spendable candidates; they never authorize spends.
- Union every configured guard result with the local dust heuristic.
- A configured guard fails closed unless a human explicitly uses the
  per-invocation CLI bypass.
- Do not ship or silently enable a default third-party asset guard.
- Validate that a provider serves the selected Bitcoin network.
- Treat authentication material as secret in errors and debug output. Keep
  Subfrost path credentials behind its redacted display URL, never print
  bearer tokens, and do not assume arbitrary endpoint paths are safe.

### Output and transport

- MCP owns stdout while serving. Diagnostics belong on stderr.
- Keep JSON field names and denial codes stable; changing them is an API
  change and requires explicit compatibility consideration.
- Prefer typed request/result values for reusable workflows. Render text and
  JSON at the outer surface rather than printing inside domain logic.
- A new safety rule must affect every relevant surface, not only the CLI.

## Adding or changing a feature

Answer these before implementation:

1. Which module owns the behavior?
2. What typed request, result, or state transition defines it?
3. Which security invariants does it touch?
4. Does it require persistence or a compatibility migration?
5. Which surfaces need it: CLI, MCP, or both?
6. What unit, CLI, and MCP tests prove it?
7. Which shipped-behavior docs change with it?

If ownership or the contract is unclear, define those first.

### New domain rule

Put deterministic behavior in `sats-core` and unit-test edge cases there.
Call it from the shared native workflow so CLI and MCP cannot diverge.

### New CLI command or flag

1. Define parsing and help text in `crates/sats/src/cli.rs`.
2. Implement the workflow in `crates/sats/src/commands/`.
3. Wire dispatch in `crates/sats/src/main.rs` without adding leaf logic.
4. Provide machine-readable output when the command is useful to scripts.
5. Add integration coverage in `crates/sats/tests/cli.rs`.
6. Update `docs/cli.md` and the README only when it changes the primary
   quickstart or product promise.

### New MCP tool

1. Define typed parameters and results in `crates/sats/src/mcp/server.rs`.
2. Keep the tool description explicit about authorization and retry behavior.
3. Run blocking wallet work through the existing blocking boundary.
4. Reuse shared planning, safety, and authorization paths.
5. Add or extend `crates/sats/tests/mcp.rs`.
6. Update `docs/mcp.md`.

### New provider or capability

1. Add the audited capability to `provider::Capability` only if necessary.
2. Implement the driver under `crates/sats/src/provider/`.
3. Keep resolution pure and network I/O inside driver operations.
4. Give the driver a display-safe URL and keep credentials out of every error
   surface.
5. Test network mismatch, unavailable service, ambiguous configuration, and
   the successful path with deterministic mocks.
6. Update `docs/providers.md`.

### State format change

Existing seed, PSBT-session, finalized-transaction, legacy-plan, grant,
config, and SQLite state are compatibility surfaces. Grant records are
versioned (`GRANT_FORMAT_VERSION`); v1 records are read, reported, and
refused rather than honored, because honoring one restores a seed
disclosure. Define backward-reading
behavior before changing a serialized shape. Preserve atomic writes and
restrictive permissions for sensitive files.

## Documentation policy

Public repository documentation describes shipped behavior and stable design
decisions. Do not add private roadmaps, target dates, speculative work queues,
or unaccepted feature designs to the repository. Future work belongs in the
maintainer-provided issue or private project.

Do not document intended behavior as implemented. Source-of-truth order is:

1. tests and typed contracts;
2. implementation;
3. CLI help and MCP schemas;
4. prose documentation.

If these disagree, resolve the disagreement in the same change.

## Verification

Run the focused test first, then the full local gate:

```sh
sh -n setup.sh scripts/test-setup.sh
sh scripts/test-setup.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
cargo check -p sats-core --target wasm32-unknown-unknown
cargo check -p sats-web --target wasm32-unknown-unknown
```

Agent-path work also needs a live daemon: `crates/sats/tests/daemon.rs` and
`crates/sats/tests/mcp.rs` each start one against their own `SATS_DIR`.

After changing `sats-core` or `sats-web`, regenerate the committed playground
module with `sh scripts/build-playground.sh` (see
[Development](docs/development.md)).

For user-facing changes, also run the freshly built binary through at least
one relevant happy path using an isolated `SATS_DIR`. Never claim a change is
complete based only on compilation.

If an environment cannot run a required check, state exactly what was not run
and why. Do not silently substitute a weaker check.

## Definition of done

- The owning module and security boundary remain clear.
- Focused tests cover success and meaningful failure paths.
- CLI and MCP behavior remain aligned where both apply.
- Serialized and JSON compatibility was considered explicitly.
- Shipped-behavior docs are accurate and contain no roadmap promises.
- The full local verification gate passes, or limitations are reported.
- The built binary was exercised on the changed path when feasible.
