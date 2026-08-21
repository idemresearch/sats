//! sats-core — portable Bitcoin wallet engine.
//!
//! Everything in this crate is environment-agnostic: no filesystem, no
//! network, no clocks, no async runtime. Functions that need time take
//! `now_unix: u64`. Persistence and chain sync are the caller's job; the
//! engine operates on a [`bdk_wallet::Wallet`] the caller owns.

pub mod amount;
pub mod authz;
pub mod engine;
pub mod error;
pub mod fmt;
pub mod plan;
pub mod seal;
pub mod seed;
pub mod signer;

pub use bdk_wallet;
pub use bdk_wallet::bitcoin;
