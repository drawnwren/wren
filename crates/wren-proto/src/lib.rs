#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! Versioned control-plane protocol.
//!
//! The semantic model lives in `wren-types`; the envelope provides protocol
//! versioning and bounded framing without maintaining a second object graph.

mod wire;

pub use wire::*;
