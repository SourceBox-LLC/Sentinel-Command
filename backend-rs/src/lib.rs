//! Command Center backend — Rust web tier.
//!
//! Mid-migration from Python. Rust owns the listening port and serves what
//! it has ported; `proxy.rs` forwards the rest to the Python process on
//! localhost. See `.claude/plans` for the slice order — the short version
//! is that the route table in `app.rs` is the progress bar.
//!
//! Library surface so integration tests build the same router the binary
//! serves. A test that re-declares its own routes tests a copy, and the
//! copy drifts.

pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod proxy;

pub use app::{build_router, AppState, VERSION};
