# Local minreq patch

Base: crates.io `minreq` 2.14.1, licensed under ISC (`COPYING.md`). Upstream
source, manifest, examples and tests are retained. Registry bookkeeping and
the upstream standalone lockfile are omitted; the workspace `Cargo.lock`
pins dependencies. This patch is selected by the root `[patch.crates-io]`.
The upstream crate archive SHA-256 is
`05015102dad0f7d61691ca347e9d9d9006685a64aefb3d79eecf62665de2153d`.

## TCP address fallback

The original `Connection::connect` gives the first resolved IP the entire
remaining request timeout. If that IP silently drops connection attempts,
other addresses are never tried within the deadline. This occurred with
Mempool's Signet service even while other IPs answered successfully.

`src/connect.rs` limits each nonfinal TCP candidate to the lesser of two
seconds and the remaining request deadline. The final candidate (and a
single-address host) retains the remaining deadline. Unbounded callers keep
their prior behavior. `src/connection.rs` delegates only the existing TCP
loop to this helper; `src/lib.rs` registers the module. DNS, TLS validation,
hostname/SNI, HTTP framing, response handling and redirect policy are unchanged.

Fallback occurs before TLS or HTTP request bytes are sent. It is not an HTTP
retry and cannot duplicate a transaction broadcast. It never chooses a
different provider or pins an IP. Both native chain drivers use this patch.

The native Esplora unit-test module includes this exact helper so deterministic
stalled-address, shared-deadline, last-error and single-address tests run in
`cargo test --workspace --locked`. Existing native mock HTTP tests cover
30-second request deadlines, limited GET retries and durable signed spends
after a broadcast timeout. Upstream public-network tests are not part of the
workspace gate.

Only `src/lib.rs`, `src/connection.rs`, this note and the new `src/connect.rs`
differ from the upstream package (apart from the omitted bookkeeping files).
