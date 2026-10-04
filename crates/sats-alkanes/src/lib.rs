//! sats-alkanes — pure Alkanes protocol composition and interpretation.
//!
//! Like sats-core, everything here is environment-agnostic: no
//! filesystem, no network, no clock. Chain access — fetching bytecode,
//! running simulations, funding and signing the transactions this crate
//! composes — belongs to callers.
//!
//! The byte encodings (LEB128 varints, cellpacks, protostone fields, and
//! the runestone OP_RETURN envelope) are derived from the canonical
//! reference implementation, `kungfuflex/alkanes-rs` at commit
//! `62511e9371a3f9e448841140c51cfe428cfcb955`:
//! `crates/ordinals` (runestone magic, push chunking, varints),
//! `crates/protorune-support` and `crates/protorune` (protostone field
//! tags, 15-byte u128 packing, the tag-16383 embedding), and
//! `crates/alkanes-support` (cellpacks). Frozen byte-vector unit tests
//! pin every layer; changing any constant must fail them.

pub mod build;
pub mod call;
pub mod delta;
pub mod id;
pub mod inspect;
pub mod protostone;
pub mod varint;
pub mod view;
