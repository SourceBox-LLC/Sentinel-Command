//! Ported HTTP routes.
//!
//! One module per Python router. A route lands here only once it has been
//! diffed against the Python it replaces; until then it stays in the
//! proxy fallback in `app.rs`.

pub mod audit;
pub mod cameras;
pub mod clerk_webhook;
pub mod docs;
pub mod gdpr;
pub mod groups;
pub mod health;
pub mod hls;
pub mod incidents;
pub mod install;
pub mod integration;
pub mod keys;
pub mod local_auth;
pub mod mcp_activity;
pub mod motion;
pub mod notifications;
pub mod node_register;
pub mod node_writes;
pub mod nodes;
pub mod recording;
pub mod sentinel;
pub mod sentinel_config;
pub mod settings;
pub mod timezone;
pub mod webhooks;
pub mod ws;
pub mod well_known;
pub mod stream_logs;
