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

The canonical rule: **agents create requests, humans authorize requests,
sats executes requests.** The MCP server reads the wallet and files
requests under the grant its bearer token names; it holds no key
material and never prepares, signs, or broadcasts. A request is the
first-class workflow object (`sats-core::request`): `pending_approval`,
`denied`, `dismissed`, `signing`, `sent`, `broadcast_pending`,
`unresolved`, or `failed`, bound to the grant instance that created it. The human-authorized execution path (`crates/sats/src/request/`)
prepares, re-verifies, reserves budget, signs, persists, and broadcasts;
in v0.0.1 that path is `sats agent approve`, which unseals the seed with
the human's password for exactly one execution. The agent observes the
result and takes no action after filing.

Prepared spends are PSBTs. Normal sends keep them in memory; an explicit
export writes the unsigned PSBT to a user-named file artifact. Once signed,
durable state contains private raw transaction hex rather than a signed
PSBT. The persisted BDK wallet is watch-only. Human sends and approved
agent requests share validation, sync, protection, fee estimation, and
preparation. There is no autonomous agent spend mode and no resident
unlocked signer. See `docs/direction.md`.

Signet is the default. Mainnet must remain an explicit choice.

## Pre-release compatibility policy

sats has not shipped a release. There are no users and no compatibility
contract to preserve. Until the first release:

- Do not keep obsolete behavior for compatibility.
- Prefer deletion over aliases, migrations, and deprecated code paths.
- Development state may be invalidated: unsupported pre-release state
  fails with a clear error naming the fix — recreate it — never with a
  silent migration.
- Migration code exists only for a released format or real user data.
- Security simplification always wins over compatibility with unreleased
  behavior.

One recorded exception: a pre-daemon grant file carrying `wrapped_seed`
is detected and refused with wallet-rotation guidance, because that file
is a possible seed disclosure and "recreate the grant" would be dangerous
advice for exactly that file.

## Repository ownership

| Path | Owns |
|---|---|
| `crates/sats-core/src/engine.rs` | Pure transaction planning and conservative UTXO exclusion |
| `crates/sats-core/src/authz.rs` | Pure grant decisions (the verdict ladder), reservation, and refund rules |
| `crates/sats-core/src/request.rs` | The request record and its state machine |
| `crates/sats-core/src/intent.rs` | The canonical send intent and its digest |
| `crates/sats-core/src/event.rs` | The causal event kinds appended per request transition |
| `crates/sats-core/src/token.rs` | Agent capability tokens: mint, hash, constant-time verify |
| `crates/sats-core/src/verify.rs` | Recomputing a PSBT's payments and fee from the wallet's own descriptors |
| `crates/sats-core/src/plan.rs` | Ephemeral prepared spends and finalized transaction records |
| `crates/sats-core/src/seed.rs` | BIP-39 seed handling and BIP-86 descriptors |
| `crates/sats-core/src/seal.rs` | Versioned authenticated secret sealing |
| `crates/sats-core/src/signer.rs` | Signer trait and local in-memory signer |
| `crates/sats/src/main.rs` | Composition and command dispatch only |
| `crates/sats/src/cli.rs` | Clap command and flag definitions |
| `crates/sats/src/commands/` | Human CLI workflows and rendering |
| `crates/sats/src/provider/` | Native chain providers, capability resolution, and guards |
| `crates/sats/src/store.rs` | Paths, atomic files, permissions, finalized transactions, grants, requests, and the event log |
| `crates/sats/src/walletd.rs` | SQLite-backed watch-only BDK wallet |
| `crates/sats/src/request/` | The native request workflow: create, dismiss, reconcile, and the human-authorized executor |
| `crates/sats/src/spend.rs` | The shared signing and broadcast tail: persist before broadcast |
| `crates/sats/src/mcp/` | MCP transport and schemas — reads the wallet and files requests |
| `crates/sats/tests/` | Native CLI and MCP integration tests |
| `crates/sats-alkanes/src/` | Pure Alkanes protocol composition: ids, cellpacks, protostones, inspection, simulation views |
| `crates/sats-web/src/lib.rs` | Browser playground: wasm bindings over sats-core and the simulated chain |
| `website/` | Project website, including the interactive playground terminal |

`main.rs` is a composition root. Do not put feature logic there.

## Non-negotiable invariants

These come first; everything else serves them:

- Agents never have signing authority. Nothing agent-facing may hold,
  receive, or reconstruct key material, and no long-lived process holds
  an unsealed seed.
- No agent-originated request may reach the signer without explicit
  human authorization bound to that exact request.
- Only a human-authorized execution path may invoke the signer for an
  agent-originated request. In v0.0.1, `sats agent approve` is that path;
  the invariant is about the path's authorization, not the command.
- A grant defines what an agent is allowed to propose. Human approval
  authorizes one valid proposal inside the grant — it cannot exceed the
  grant.
- MCP and agent-facing code must never expose a generic signing
  primitive.
- Pre-release, prefer removing obsolete security models over supporting
  them (see the pre-release compatibility policy above).

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
- The irreversible boundary is the invocation of `Signer::sign`.
  `signing` is persisted before the signer runs. A refund is permitted
  only before that invocation (audit append, signer construction), when
  the executing process knows no signature exists; from the invocation
  on, nothing the signer reports is trusted to mean "no signature" — an
  error, an unfinalized result, a finalization or persistence failure,
  or a crash is `unresolved`: never refund, never sign again
  automatically. `failed` is re-approvable only because it is provable
  that `Signer::sign` was never invoked. `sent` and `broadcast_pending`
  mean a persisted signature — only rebroadcast. A `signing` record is
  reconciled from the transaction attributed to it (agent, request id,
  intent digest), never guessed.
- Budget is drawn on a per-request ledger on the grant, updated in the
  same atomic write as the totals: one draw per request id, a repeat
  with the same spend draws nothing, a different spend fails closed, and
  a refund happens exactly once. A draw is returned only when the
  durable record proves the signer was never invoked (`pending_approval`,
  `failed`, `denied`); every other draw stays. Never solve a crash
  window by accepting a leaked or double-counted budget.
- Request ids are global (`r-` + 32 hex). A filing derives its id from
  the grant id, the agent, and the agent's idempotency key; the key is
  never the id, and `check_request` deals in ids only. Request creation
  reconciles the reservation ledger under the grant lock before its
  verdict, so a crashed execution's orphaned draw never denies another
  request. A request records the `grant_id` (128 random bits) of
  the grant that created it and executes only under that instance;
  creation holds the grant lock so a revoke or re-issue cannot
  interleave.
- A grant carries no key material. Never reintroduce a field from which a
  seed can be recovered; only a token hash belongs beside a policy.
- A bearer token is emitted once, at creation, and never persisted.
- Write finalized transaction records and agent requests with restrictive
  permissions; both expose wallet and payment metadata.

### Agent authorization

- `evaluate_send` is the single deterministic policy decision, run at
  request creation (fee unknown) and again at execution with the real fee.
- Never execute against an amount or fee a caller reported. Derive both
  from the PSBT with `sats_core::verify` and refuse on disagreement with
  the recorded request.
- Nothing in the MCP process may hold key material or reconstruct a
  mnemonic. The MCP process files requests; it never prepares, signs, or
  broadcasts.
- Operational conditions carry a typed `error_code`; policy refusals are
  a `denied` request with a `reason`. Never conflate them.
- The grant alone never allows a spend: every grant boundary is hard —
  expiry, mode, overflow, recipient allowlist, amount cap, fee cap,
  budget — and a proposal inside all of them is `Decision::Ask`, the
  request state `pending_approval`. A pending request is normal workflow,
  never presented as a denied send. Never reintroduce an autonomous path
  to the signer, and never make a boundary approvable.
- Preserve check order: the specific boundary answers before the
  terminal ask, so a refusal names what was crossed.
- Budget includes amount plus fee.
- Reserve and persist budget, then persist the request as `signing`,
  before the signer is invoked; construct the signer only after the
  reservation is durable (`request::execute::commit` takes it as a
  factory for exactly this reason).
- Refund only before the signer is invoked, or when it reports that no
  signature was produced.
- The human review (recipient, amount, real fee, total) precedes the
  password in every mode; `--json` moves it to stderr, never drops it.
- Never refund a signed transaction after a broadcast failure.
- Re-read the grant at creation and again under the grant lock at
  execution, so revocation takes effect immediately.
- Denials are typed, successful MCP results. Do not turn expected denials
  into transport errors or retries.
- The agent never retries to make a payment happen: after filing it only
  observes. Do not add an agent-facing tool that approves, unlocks,
  signs, executes, or broadcasts.
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
2. Keep the tool description explicit: the agent files and observes; it
   never approves, unlocks, signs, executes, or broadcasts.
3. Run blocking wallet work through the existing blocking boundary.
4. Reuse `crate::request` for anything that touches a request.
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

The pre-release compatibility policy applies: serialized shapes may
change freely, and unsupported development state fails with a clear
error naming the recreate step. Grant records carry
`GRANT_FORMAT_VERSION` (currently 1, the first released schema);
records claiming a newer version are refused because they may carry
restrictions this build cannot see. Request and event records carry
their own versions and are read tolerantly for the same forward-safety
reason. The one backward-reading exception is the pre-daemon
`wrapped_seed` grant, refused with wallet-rotation guidance. Preserve
atomic writes and restrictive permissions for sensitive files. After
the first release, define backward-reading behavior before changing a
released shape.

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

Agent-path work is covered by `crates/sats/src/request/execute.rs` (unit
tests with a signer probe and crash reconciliation) and
`crates/sats/tests/mcp.rs` (the served process, the CLI approval, and the
observed result, each against its own `SATS_DIR`).

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
