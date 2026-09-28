//! The MCP server.
//!
//! Ported from `backend/app/mcp/`. Split by concern rather than kept as
//! one 2,300-line module: the policy here decides what a key may reach
//! and how often, and it is all pure — which is what lets it be tested
//! without a protocol, a database or a network.
pub mod activity;
pub mod scope;
