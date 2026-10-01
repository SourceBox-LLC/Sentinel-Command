//! The agent's MCP client: Command Center's tools, over streamable HTTP.
//!
//! Ported from `app/sentinel_agent/mcp_client.py`, with rmcp's client in
//! place of the Python SDK's. Built per run and torn down per run — the
//! connection carries `X-Agent-Org-Override` naming the org the run
//! belongs to, and a client that outlived its run would be one bad
//! refactor away from making the next org's tool calls under the
//! previous org's header.
//!
//! Two properties the loop depends on:
//!
//! * **A tool call never fails the run.** A timeout, a transport error,
//!   an unknown tool name — each comes back as a `{"error": …}` text
//!   result the model can read and react to. The alternative is the run
//!   dying on the first camera that is mid-restart.
//! * **Images stay raw base64.** The provider-specific wrapping is
//!   `llm::image_message`'s job, so changing provider never reaches back
//!   here.

use std::collections::HashMap;
use std::time::Duration;

use rig_core::completion::ToolDefinition;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

/// What a tool returned, as the loop consumes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    /// Raw base64, no `data:` prefix.
    pub images: Vec<String>,
}

impl ToolOutput {
    /// An error the model can read. `json.dumps` spacing, because the
    /// model sees this string and the trace stores it.
    pub fn error(message: impl AsRef<str>) -> Self {
        Self {
            text: crate::audit::python_json_value(&json!({ "error": message.as_ref() })),
            images: Vec::new(),
        }
    }
}

/// The tool surface, as a trait so the loop can run against scripted
/// tools. What a tool returns decides the run's outcome, and none of that
/// logic needs a server.
pub trait Tools: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn call(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;
}

pub struct McpClient {
    service: RunningService<RoleClient, ()>,
    tools: Vec<rmcp::model::Tool>,
    tool_timeout: Duration,
}

impl McpClient {
    /// Connect, initialise and list the tools.
    ///
    /// `org_id` is trimmed before it becomes a header, belt and braces
    /// with the server, which trims it too: a pending row with stray
    /// whitespace must not match a different org's override, and a value
    /// that is not a legal header must fail here rather than be sent
    /// mangled.
    pub async fn connect(
        url: &str,
        bearer: &str,
        org_id: &str,
        tool_timeout_seconds: f64,
    ) -> Result<Self, String> {
        let org = org_id.trim();
        let mut headers = HashMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("x-agent-org-override"),
            axum::http::HeaderValue::from_str(org)
                .map_err(|_| format!("org id {org:?} is not a valid header value"))?,
        );
        // `auth_header` takes the token; rmcp adds the `Bearer ` itself.
        let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
            .auth_header(bearer.to_string())
            .custom_headers(headers);
        let transport = StreamableHttpClientTransport::from_config(config);

        let service =
            ().serve(transport)
                .await
                .map_err(|err| format!("MCP server at {url}: {err}"))?;
        let tools = service
            .list_all_tools()
            .await
            .map_err(|err| format!("MCP server at {url}: listing tools failed: {err}"))?;
        tracing::info!(tools = tools.len(), "connected to MCP server");
        Ok(Self {
            service,
            tools,
            tool_timeout: Duration::from_secs_f64(tool_timeout_seconds),
        })
    }

    /// Close the session. Bounded by the caller: a hung teardown must not
    /// be able to hold a drain open.
    pub async fn disconnect(self) {
        let _ = self.service.cancel().await;
    }
}

impl Tools for McpClient {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.to_string(),
                description: tool.description.as_deref().unwrap_or("").to_string(),
                // `inputSchema or {"type": "object", "properties": {}}`.
                parameters: if tool.input_schema.is_empty() {
                    json!({ "type": "object", "properties": {} })
                } else {
                    Value::Object((*tool.input_schema).clone())
                },
            })
            .collect()
    }

    async fn call(&self, name: &str, arguments: Map<String, Value>) -> ToolOutput {
        if !self.tools.iter().any(|tool| tool.name == name) {
            return ToolOutput::error(format!("Unknown tool: {name}"));
        }
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let call = self.service.call_tool(params);
        // Per call, because a stuck tool — `watch_camera` against a node
        // that is mid-restart — would otherwise hang the whole loop until
        // the 270 s wall clock.
        match tokio::time::timeout(self.tool_timeout, call).await {
            Err(_) => {
                tracing::warn!(tool = name, "tool call timed out");
                ToolOutput::error(format!(
                    "Tool '{name}' timed out after {:.0}s",
                    self.tool_timeout.as_secs_f64()
                ))
            }
            Ok(Err(err)) => {
                tracing::error!(tool = name, error = %err, "tool call failed");
                ToolOutput::error(err.to_string())
            }
            Ok(Ok(result)) => {
                let mut texts = Vec::new();
                let mut images = Vec::new();
                for content in &result.content {
                    match content {
                        ContentBlock::Text(text) => texts.push(text.text.clone()),
                        ContentBlock::Image(image) => images.push(image.data.clone()),
                        // The Python's `hasattr(content, "data")` also
                        // caught audio; nothing on this server returns
                        // any, and a frame list holding audio bytes
                        // labelled as JPEG would be worse than dropping
                        // it.
                        _ => {}
                    }
                }
                ToolOutput {
                    text: if texts.is_empty() {
                        "OK (no output)".to_string()
                    } else {
                        texts.join("\n")
                    },
                    images,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error envelope is what `_parse_id_from_tool_json` is written
    /// to reject, and what the model reads — so its exact shape matters.
    #[test]
    fn a_tool_error_is_python_shaped_json() {
        assert_eq!(
            ToolOutput::error("Unknown tool: nope").text,
            r#"{"error": "Unknown tool: nope"}"#
        );
        assert!(ToolOutput::error("x").images.is_empty());
    }
}
