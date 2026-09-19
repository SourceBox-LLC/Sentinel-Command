//! Ported HTTP routes.
//!
//! One module per Python router. A route lands here only once it has been
//! diffed against the Python it replaces; until then it stays in the
//! proxy fallback in `app.rs`.

pub mod audit;
pub mod cameras;
pub mod groups;
pub mod incidents;
pub mod install;
pub mod integration;
pub mod keys;
pub mod local_auth;
pub mod mcp_activity;
pub mod motion;
pub mod notifications;
pub mod node_writes;
pub mod nodes;
pub mod recording;
pub mod sentinel;
pub mod sentinel_config;
pub mod settings;
pub mod well_known;
pub mod stream_logs;
