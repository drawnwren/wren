#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! Versioned Prost control-plane protocol.
//!
//! The semantic model lives in `wren-types`; this crate owns frozen wire DTOs
//! and fallible conversions so ABI compatibility never leaks into the editor
//! core.

mod wire;

pub use wire::*;
