//! `satsd` — the local signing daemon.
//!
//! The wallet's spending policy is only as strong as the boundary that
//! enforces it. Before satsd, an agent's grant file carried the master
//! seed re-sealed beside its own key, so every budget, cap, and expiry
//! held because the `sats` binary chose to honor it — and any process
//! that could read the file could sign without asking.
//!
//! satsd moves the seed behind a process boundary. It holds the unsealed
//! mnemonic in memory, decides every agent spend itself, and signs. The
//! grant file becomes a capability token: a name for a policy, not a key.
//!
//! What that does and does not buy, stated plainly:
//!
//! - Reading a grant file no longer yields anything that can spend.
//! - Revocation and expiry become real rather than voluntary, because the
//!   key never left this process.
//! - A stolen token still spends its own budget. That is what a budget
//!   is; the point is that it cannot spend anything else.
//! - satsd runs as the wallet's user, so this is a process-memory
//!   boundary, not a privilege boundary. It defeats a file read, which is
//!   what a shell-capable agent actually does. It does not defeat root, a
//!   debugger attaching to this process, or a core dump.

pub mod client;
pub mod protocol;
pub mod send;
pub mod server;
pub mod session;

pub use client::Client;
pub use server::run;
