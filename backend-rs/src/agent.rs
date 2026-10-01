//! The Sentinel AI agent.
//!
//! Ported from `backend/app/sentinel_agent/`, the last Python in the
//! repository. It runs as its own process — the `agent` Fly process
//! group, or a self-hoster's box — built from this crate as the
//! `sentinel-agent` binary, and it talks to Command Center the way any
//! other client does: the run queue over HTTP, the tools over MCP.
//!
//! Split the way the Python was, so each piece can be compared with the
//! file it replaced:
//!
//! | | |
//! | --- | --- |
//! | `config` | env, defaults, and the rules that refuse to boot |
//! | `prompts` | the system prompts, extracted not retyped |
//! | `llm` | the one model call (rig, replacing LiteLLM) |
//! | `mcp_client` | Command Center's tools (rmcp's client) |
//! | `queue` | the run queue: pending / start / complete |
//! | `run` | the loop for one run |
//! | `processor` | draining the queue |
//! | `server` | `/health`, `/wakeup`, the drain lock, poll mode |
pub mod config;
pub mod llm;
pub mod mcp_client;
pub mod processor;
pub mod prompts;
pub mod queue;
pub mod run;
pub mod server;
