//! Ported HTTP routes.
//!
//! One module per Python router. A route lands here only once it has been
//! diffed against the Python it replaces; until then it stays in the
//! proxy fallback in `app.rs`.

pub mod audit;
pub mod cameras;
pub mod mcp_activity;
pub mod motion;
pub mod nodes;
pub mod settings;
pub mod stream_logs;
