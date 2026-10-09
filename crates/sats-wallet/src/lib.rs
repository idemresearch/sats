//! The sats wallet as a working system: on-disk state, the watch-only BDK
//! wallet, chain providers, transaction preparation, and the request
//! workflow, including the human-authorized executor.
//!
//! Every front end (the `sats` CLI and its MCP server today) builds on
//! this crate, so a safety rule lives here once. `sats-core` makes the
//! decisions; this crate performs them against files and the network.
//!
//! The crate never talks to the human. It doesn't print, prompt, or read
//! a password: warnings go through the `log` facade, sync progress
//! through [`provider::Progress`], and the signer arrives as a factory
//! from a surface that has already obtained the human's authorization.

pub mod config;
pub mod prepare;
pub mod provider;
pub mod request;
pub mod spend;
pub mod store;
pub mod walletd;
