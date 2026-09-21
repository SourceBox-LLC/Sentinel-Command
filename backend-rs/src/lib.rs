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

pub mod api;
pub mod app;
pub mod audit;
pub mod auth;
pub mod config;
pub mod cors;
pub mod crypto;
pub mod email_templates;
pub mod email_unsubscribe;
pub mod error;
pub mod headers;
pub mod hls;
pub mod license;
pub mod models;
pub mod plans;
pub mod proxy;
pub mod pydatetime;
pub mod pycodec;
pub mod pyint;
pub mod pyjson;
pub mod pyrepr;
pub mod query;
pub mod ratelimit;
pub mod recipients;
pub mod settings;
pub mod tz_names;
pub mod versions;
pub mod zoneinfo;

pub use app::{build_router, AppState, VERSION};
pub use auth::AuthUser;
