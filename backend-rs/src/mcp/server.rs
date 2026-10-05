//! The MCP protocol surface: `POST /mcp`.
//!
//! Ported from `backend/app/mcp/server.py`, which builds it with
//! `fastmcp`. Here it is `rmcp`'s `ServerHandler` behind its
//! streamable-HTTP tower service, mounted into the same axum router as
//! everything else.
//!
//! **Two passes over the same token, deliberately.** `list_tools`
//! filters by the scope lookup and `call_tool` gates on it, but the
//! tool bodies resolve the org themselves — so an unrecognised key
//! sees an unfiltered catalog and is refused only when it calls
//! something. That is Python's shape and the reason the middleware
//! never raises: raising there would replace the tool's own, better
//! error with a worse one.
//!
//! **What matches, and what deliberately does not.** The call results
//! match byte for byte: the content block, `structuredContent`, and
//! `isError` with the message as the sole text block. Two things
//! cannot and should not — `_meta.fastmcp` on every tool, and a
//! `serverInfo.version` reporting fastmcp's own release. Matching
//! those would mean impersonating the framework this port exists to
//! delete. Both are normalised in the differential and written up in
//! expected_divergences.md.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{json, Map, Value};

use crate::app::AppState;
use crate::mcp::activity::McpEvent;
use crate::mcp::tools::{Frame, ToolOutput};

/// The instructions a client sees on connect.
const INSTRUCTIONS: &str = "You are connected to a Sentinel Command Center organization. \
You can SEE what cameras see via view_camera (returns a live JPEG snapshot), list cameras, \
check node status, get stream URLs, manage recording settings, and view audit logs. \
All operations are scoped to the authenticated organization.";

#[derive(Clone)]
pub struct SentinelMcp {
    pub state: AppState,
}

impl ServerHandler for SentinelMcp {
    fn get_info(&self) -> ServerConfig {
        // The same four capabilities FastMCP advertises. Tools are
        // the only one this server actually serves; the rest are
        // declared because the framework declares them and a client
        // may branch on their presence.
        // `logging` is deprecated in the spec and in rmcp, and kept
        // anyway: FastMCP advertises it today, a client may branch on
        // its presence, and dropping it would be a difference in what
        // the two stacks say about themselves rather than a fix. It
        // goes when the Python does.
        #[allow(deprecated)]
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .enable_logging()
            .build();
        // The flags FastMCP sets, which the builder leaves empty. A
        // client reads them to decide whether to subscribe to a
        // list-changed notification, so an empty object is a different
        // answer rather than a terser one.
        if let Some(tools) = capabilities.tools.as_mut() {
            tools.list_changed = Some(true);
        }
        if let Some(prompts) = capabilities.prompts.as_mut() {
            prompts.list_changed = Some(true);
        }
        if let Some(resources) = capabilities.resources.as_mut() {
            resources.list_changed = Some(true);
            resources.subscribe = Some(false);
        }
        let mut info = ServerConfig::new(capabilities);
        let mut server_info = Implementation::from_build_env();
        server_info.name = "Sentinel by SourceBox".to_string();
        // The port's own version, not fastmcp's. See the module note.
        server_info.version = crate::app::VERSION.to_string();
        info.server_info = server_info;
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let allowed = match headers_of(&context) {
            Some(headers) => crate::mcp::auth::lookup_allowed(&self.state, &headers).await,
            None => None,
        };
        let tools = crate::mcp::scope::TOOL_DESCRIPTIONS
            .iter()
            // `None` means the middleware did not recognise the key and
            // leaves the decision to the tool — so the catalog is
            // unfiltered rather than empty.
            .filter(|(name, _)| allowed.as_ref().is_none_or(|set| set.contains(name)))
            .map(|(name, description)| build_tool(name, description))
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let args: Map<String, Value> = request.arguments.clone().unwrap_or_default();
        let headers = headers_of(&context).unwrap_or_default();

        // The scope gate, before anything else touches the database.
        if let Some(allowed) = crate::mcp::auth::lookup_allowed(&self.state, &headers).await {
            if !allowed.contains(name.as_str()) {
                return Ok(tool_error(format!(
                    "Tool '{name}' is not enabled for this API key. \
                     Update the key's scope in the Sentinel dashboard or use a \
                     different key."
                ))
                .into());
            }
        }

        let started = std::time::Instant::now();
        let summary = crate::mcp::scope::summarize_args(&args);
        let args_summary = if summary.is_empty() {
            None
        } else {
            Some(summary)
        };

        let principal = match crate::mcp::auth::resolve(&self.state, &headers).await {
            Ok(principal) => principal,
            Err(err) => {
                // A failed auth is still logged, with whatever org the
                // resolver had established — that is what makes a
                // refused call visible to forensics instead of
                // vanishing.
                self.log(&name, "", "", started, args_summary, Some(err.0.clone()));
                return Ok(tool_error(err.0).into());
            }
        };

        // A NUL anywhere in the arguments is refused before any tool
        // runs. PostgreSQL text cannot hold one, so a tool that stored an
        // argument failed on the INSERT and answered with its generic
        // database message — "Authentication error" — which describes
        // nothing the caller did. The REST decoder refuses NUL the same
        // way.
        let outcome = if args.values().any(carries_nul) {
            Err("Arguments must not contain a NUL (\\u0000) character.".to_string())
        } else {
            self.dispatch(&name, &principal, &args).await
        };
        match outcome {
            Ok(output) => {
                self.log(
                    &name,
                    &principal.org_id,
                    &principal.key_name,
                    started,
                    args_summary,
                    None,
                );
                Ok(success(&name, output).into())
            }
            Err(message) => {
                self.log(
                    &name,
                    &principal.org_id,
                    &principal.key_name,
                    started,
                    args_summary,
                    // `str(e)[:200]` — the stored error is truncated,
                    // the one the caller sees is not.
                    Some(message.chars().take(200).collect()),
                );
                Ok(tool_error(message).into())
            }
        }
    }
}

impl SentinelMcp {
    async fn dispatch(
        &self,
        name: &str,
        principal: &crate::mcp::auth::Principal,
        args: &Map<String, Value>,
    ) -> Result<ToolOutput, String> {
        use crate::mcp::tools as t;
        let state = &self.state;
        let org = principal.org_id.as_str();
        let key = principal.key_name.as_str();
        let json = |result: t::ToolResult| result.map(ToolOutput::Json);

        match name {
            "list_cameras" => json(t::list_cameras(state, org).await),
            "get_camera" => json(t::get_camera(state, org, args).await),
            "get_stream_url" => json(t::get_stream_url(state, org, args).await),
            "list_camera_groups" => json(t::list_camera_groups(state, org).await),
            "list_nodes" => json(t::list_nodes(state, org).await),
            "get_node" => json(t::get_node(state, org, args).await),
            "get_camera_recording_policy" => {
                json(t::get_camera_recording_policy(state, org, args).await)
            }
            "set_camera_recording_policy" => {
                json(t::set_camera_recording_policy(state, org, args).await)
            }
            "get_stream_logs" => json(t::get_stream_logs(state, org, args).await),
            "get_stream_stats" => json(t::get_stream_stats(state, org, args).await),
            "get_system_status" => json(t::get_system_status(state, org).await),
            "list_incidents" => json(t::list_incidents(state, org, args).await),
            "get_incident" => json(t::get_incident(state, org, args).await),
            "create_incident" => json(t::create_incident(state, org, key, args).await),
            "add_observation" => json(t::add_observation(state, org, args).await),
            "update_incident" => json(t::update_incident(state, org, key, args).await),
            "finalize_incident" => json(t::finalize_incident(state, org, args).await),
            "attach_snapshot" => json(t::attach_snapshot(state, org, args).await),
            "attach_clip" => json(t::attach_clip(state, org, args).await),
            "get_incident_clip" => json(t::get_incident_clip(state, org, args).await),
            "view_camera" => t::view_camera(state, org, args).await,
            "watch_camera" => t::watch_camera(state, org, args).await,
            "get_incident_snapshot" => t::get_incident_snapshot(state, org, args).await,
            // Unreachable through a client, which can only call what
            // `tools/list` offered.
            _ => Err(format!("Unknown tool: {name}")),
        }
    }

    fn log(
        &self,
        tool_name: &str,
        org_id: &str,
        key_name: &str,
        started: std::time::Instant,
        args_summary: Option<String>,
        error: Option<String>,
    ) {
        let event = McpEvent {
            // `str(uuid4())[:8]`.
            id: crate::crypto::token_hex(4),
            // The START, not the end — Python stamps the event with
            // when the call began.
            timestamp: unix_seconds() - started.elapsed().as_secs_f64(),
            tool_name: tool_name.to_string(),
            org_id: org_id.to_string(),
            key_name: key_name.to_string(),
            status: if error.is_some() {
                "error"
            } else {
                "completed"
            }
            .to_string(),
            // `round((time.time() - start) * 1000)` — an integer.
            duration_ms: Some(crate::pyrepr::round_half_even(
                started.elapsed().as_secs_f64() * 1000.0,
            ) as i64),
            error,
            args_summary,
        };
        crate::mcp::activity::TRACKER.log_event(&self.state.pool, event);
    }
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// The HTTP request's headers, which the transport puts in the request
/// extensions. That is how the bearer token reaches a handler at all.
fn headers_of(context: &RequestContext<RoleServer>) -> Option<axum::http::HeaderMap> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .map(|parts| parts.headers.clone())
}

/// A successful result, in FastMCP's shape.
///
/// A JSON result is serialised into the text block AND repeated as
/// `structuredContent`; media returns only blocks, with no structured
/// half, which is what an `Image` return produces.
///
/// The framing of the structured half depends on the tool, not on the
/// value — see [`crate::mcp::scope::WRAP_RESULT_TOOLS`].
fn success(name: &str, output: ToolOutput) -> CallToolResult {
    match output {
        ToolOutput::Json(value) => {
            // An empty array gets NO content block. That is not a
            // special case in FastMCP so much as a consequence of one:
            // it checks whether every item is already a content block
            // before serialising, and `all()` of nothing is true, so
            // the empty list passes straight through as a block list.
            // A non-empty one is serialised whole into a single block,
            // as is every object.
            let blocks = if value.as_array().is_some_and(|items| items.is_empty()) {
                vec![]
            } else {
                // Compact separators: FastMCP writes the text block
                // with `separators=(",", ":")`.
                vec![ContentBlock::text(crate::audit::python_json_compact(
                    &value,
                ))]
            };
            let mut result = CallToolResult::success(blocks);
            result.structured_content =
                Some(if crate::mcp::scope::WRAP_RESULT_TOOLS.contains(&name) {
                    json!({ "result": value })
                } else {
                    value
                });
            result.is_error = Some(false);
            result
        }
        ToolOutput::Image(bytes, mime) => {
            let mut result = CallToolResult::success(vec![ContentBlock::image(
                crate::crypto::base64_standard(&bytes),
                mime,
            )]);
            result.is_error = Some(false);
            result
        }
        ToolOutput::Frames(frames) => {
            let blocks = frames
                .into_iter()
                .map(|frame| match frame {
                    Frame::Image(bytes, mime) => {
                        ContentBlock::image(crate::crypto::base64_standard(&bytes), mime)
                    }
                    Frame::Text(text) => ContentBlock::text(text),
                })
                .collect();
            let mut result = CallToolResult::success(blocks);
            result.is_error = Some(false);
            result
        }
    }
}

/// A `ToolError`: the message as the only content block, and the flag.
///
/// Note there is no `structuredContent` and no `isError: false` beside
/// it — FastMCP omits the structured half entirely on an error.
fn tool_error(message: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(message)]);
    result.structured_content = None;
    result.is_error = Some(true);
    result
}

/// One tool's entry in the catalog.
///
/// The schema is written here rather than derived, because the port has
/// no signature to derive it from — and a schema is what tells a model
/// which arguments exist at all.
///
/// Two fields are easy to get subtly wrong:
///
///   * `title` is DERIVED, not declared. FastMCP title-cases the tool
///     name because some clients drop a tool that has no title instead
///     of falling back to the name, as the spec says they should.
///   * `annotations` is present only on the read tools, which are the
///     only ones Python decorates with `readOnlyHint`. Sending
///     `readOnlyHint: false` on a write tool says the same thing to a
///     reader and a different thing on the wire.
fn build_tool(name: &'static str, description: &'static str) -> Tool {
    let mut tool = Tool::new(name, description, Arc::new(input_schema(name)));
    tool.title = Some(crate::pyrepr::title_case(&name.replace(['_', '-'], " ")));
    if crate::mcp::scope::MCP_READ_TOOLS.contains(&name) {
        let mut annotations = ToolAnnotations::default();
        annotations.read_only_hint = Some(true);
        tool.annotations = Some(annotations);
    }
    tool
}

fn object_schema(properties: Value, required: &[&str]) -> Map<String, Value> {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn int_prop(description: &str, minimum: i64, maximum: Option<i64>) -> Value {
    let mut prop = json!({
        "type": "integer",
        "description": description,
        "minimum": minimum,
    });
    if let Some(maximum) = maximum {
        prop["maximum"] = json!(maximum);
    }
    prop
}

fn input_schema(name: &str) -> Map<String, Value> {
    match name {
        "list_cameras" | "list_camera_groups" | "list_nodes" | "get_system_status" => {
            object_schema(json!({}), &[])
        }
        "get_camera" | "get_stream_url" | "get_camera_recording_policy" => object_schema(
            json!({ "camera_id": string_prop("The camera_id string (e.g. 'node1-video0')") }),
            &["camera_id"],
        ),
        "view_camera" => object_schema(
            json!({ "camera_id": string_prop("The camera_id to view (e.g. 'node1-video0')") }),
            &["camera_id"],
        ),
        "watch_camera" => object_schema(
            json!({
                "camera_id": string_prop("The camera_id to watch"),
                "count": int_prop("Number of snapshots to take", 2, Some(10)),
                "interval_seconds": int_prop("Seconds between snapshots", 1, Some(30)),
            }),
            &["camera_id"],
        ),
        "get_node" => object_schema(
            json!({ "node_id": string_prop("The node_id to look up") }),
            &["node_id"],
        ),
        "set_camera_recording_policy" => object_schema(
            json!({
                "camera_id": string_prop("The camera_id whose policy to change"),
                "continuous_24_7": { "type": "boolean" },
                "scheduled_recording": { "type": "boolean" },
                "scheduled_start": string_prop("HH:MM in the org's timezone"),
                "scheduled_end": string_prop("HH:MM in the org's timezone"),
            }),
            &["camera_id"],
        ),
        "get_stream_logs" => object_schema(
            json!({
                "camera_id": string_prop("Filter to one camera"),
                "limit": int_prop("Max rows to return", 1, Some(200)),
            }),
            &[],
        ),
        "get_stream_stats" => object_schema(
            json!({ "days": int_prop("Number of days to look back", 1, Some(30)) }),
            &[],
        ),
        "list_incidents" => object_schema(
            json!({
                "status": string_prop("Filter by status: open, acknowledged, resolved, or dismissed"),
                "severity": string_prop("Filter by severity: low, medium, high, or critical"),
                "camera_id": string_prop("Filter to incidents attached to a specific camera"),
                "limit": int_prop("Max number of incidents to return", 1, Some(100)),
                "offset": int_prop("How many rows to skip (for pagination)", 0, None),
            }),
            &[],
        ),
        "get_incident" => object_schema(
            json!({ "incident_id": int_prop("The incident id to fetch", 0, None) }),
            &["incident_id"],
        ),
        "create_incident" => object_schema(
            json!({
                "title": string_prop("Short title for the incident (max 200 chars)"),
                "summary": string_prop("One or two sentence summary of what was observed"),
                "severity": string_prop("Severity level: low, medium, high, or critical"),
                "camera_id": string_prop("Optional: the primary camera_id this incident relates to"),
            }),
            &["title", "summary"],
        ),
        "add_observation" => object_schema(
            json!({
                "incident_id": int_prop("The incident id returned by create_incident", 0, None),
                "text": string_prop("Free-form observation text"),
                "camera_id": string_prop("Optional: camera this observation pertains to"),
            }),
            &["incident_id", "text"],
        ),
        "update_incident" => object_schema(
            json!({
                "incident_id": int_prop("The incident id to update", 0, None),
                "status": string_prop("New status: open, acknowledged, resolved, or dismissed"),
                "severity": string_prop("New severity: low, medium, high, or critical"),
                "summary": string_prop("New short summary text"),
                "report": string_prop(
                    "Full replacement for the long-form markdown report body. Provide the \
                     COMPLETE revised text — this overwrites the existing body."
                ),
            }),
            &["incident_id"],
        ),
        "finalize_incident" => object_schema(
            json!({
                "incident_id": int_prop("The incident id to finalize", 0, None),
                "report": string_prop("Full incident report in markdown"),
            }),
            &["incident_id", "report"],
        ),
        "attach_snapshot" => object_schema(
            json!({
                "incident_id": int_prop("The incident id to attach the snapshot to", 0, None),
                "camera_id": string_prop("The camera_id to capture from"),
                "note": string_prop("Optional caption for this snapshot"),
            }),
            &["incident_id", "camera_id"],
        ),
        "attach_clip" => object_schema(
            json!({
                "incident_id": int_prop("The incident id to attach the clip to", 0, None),
                "camera_id": string_prop("The camera_id to capture from"),
                "duration_seconds": int_prop(
                    "How many seconds of recent video to capture", 2, Some(60)
                ),
                "note": string_prop("Optional caption for this clip"),
            }),
            &["incident_id", "camera_id"],
        ),
        "get_incident_snapshot" | "get_incident_clip" => object_schema(
            json!({
                "incident_id": int_prop("The incident id the evidence belongs to", 0, None),
                "evidence_id": int_prop("The evidence id from get_incident", 0, None),
            }),
            &["incident_id", "evidence_id"],
        ),
        _ => object_schema(json!({}), &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool in the catalog gets a schema, and every required
    /// argument it names is a property that exists.
    #[test]
    fn every_tool_has_a_coherent_schema() {
        for (name, _) in crate::mcp::scope::TOOL_DESCRIPTIONS {
            let schema = input_schema(name);
            assert_eq!(schema["type"], "object", "{name}");
            let properties = schema["properties"].as_object().expect("properties");
            for required in schema["required"].as_array().expect("required") {
                let field = required.as_str().expect("a string");
                assert!(
                    properties.contains_key(field),
                    "{name} requires {field}, which it does not declare"
                );
            }
        }
    }

    /// A tool with no arguments still declares an object schema —
    /// clients reject a missing one.
    #[test]
    fn an_argument_free_tool_still_has_a_schema() {
        let schema = input_schema("list_cameras");
        assert_eq!(schema["properties"], json!({}));
        assert_eq!(schema["required"], json!([]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    /// The read hint is what a client uses to decide whether a tool is
    /// safe to call speculatively, so it has to track the real split —
    /// and a write tool carries NO annotations object at all, because
    /// Python decorates only the reads.
    #[test]
    fn the_read_only_hint_matches_the_catalog() {
        let mut annotated = 0;
        for (name, _) in crate::mcp::scope::TOOL_DESCRIPTIONS {
            let tool = build_tool(name, "x");
            let hint = tool.annotations.and_then(|a| a.read_only_hint);
            if crate::mcp::scope::MCP_READ_TOOLS.contains(&name) {
                assert_eq!(hint, Some(true), "{name}");
                annotated += 1;
            } else {
                assert_eq!(hint, None, "{name} should carry no annotations");
            }
        }
        assert_eq!(annotated, 16);
    }

    /// The display title FastMCP derives from the name. `Url` rather
    /// than `URL` is Python's `title()`, not a typo.
    #[test]
    fn every_tool_carries_a_derived_title() {
        assert_eq!(
            build_tool("get_stream_url", "x").title.as_deref(),
            Some("Get Stream Url")
        );
        assert_eq!(
            build_tool("list_camera_groups", "x").title.as_deref(),
            Some("List Camera Groups")
        );
        for (name, _) in crate::mcp::scope::TOOL_DESCRIPTIONS {
            let title = build_tool(name, "x").title.expect("a title");
            assert!(!title.contains('_'), "{name}: {title}");
        }
    }

    /// An error carries the message and the flag, and NOT a structured
    /// half — FastMCP omits it entirely.
    #[test]
    fn an_error_result_has_no_structured_content() {
        let result = tool_error("Camera 'nope' not found".to_string());
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
        assert_eq!(result.content.len(), 1);
    }

    /// A JSON result repeats itself: once serialised as text, once
    /// structured.
    #[test]
    fn a_json_result_is_both_text_and_structured() {
        let value = json!({ "camera_id": "cam-1", "ok": true });
        let result = success("get_camera", ToolOutput::Json(value.clone()));
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(value));
        assert_eq!(result.content.len(), 1);
    }

    /// A list-returning tool's structured half is wrapped under
    /// `result`, because MCP output schemas must be objects. The text
    /// block is NOT wrapped — it is the serialised return value.
    #[test]
    fn a_list_result_is_wrapped_structured_but_not_as_text() {
        let value = json!([{ "camera_id": "cam-1" }]);
        let result = success("list_cameras", ToolOutput::Json(value.clone()));
        assert_eq!(result.structured_content, Some(json!({ "result": value })));
        assert_eq!(result.content.len(), 1);
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected a text block");
        };
        assert_eq!(text.text, r#"[{"camera_id":"cam-1"}]"#);
    }

    /// An EMPTY list gets no content block at all — only the wrapped
    /// structured half. A dict-returning tool with an empty object
    /// still gets its block, so this is about the array, not emptiness.
    #[test]
    fn an_empty_list_result_has_no_content_block() {
        let result = success("list_nodes", ToolOutput::Json(json!([])));
        assert!(result.content.is_empty());
        assert_eq!(result.structured_content, Some(json!({ "result": [] })));

        let object = success("get_system_status", ToolOutput::Json(json!({})));
        assert_eq!(object.content.len(), 1);
        assert_eq!(object.structured_content, Some(json!({})));
    }
}

/// Whether a NUL character appears in any string or key in `value`.
fn carries_nul(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(carries_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || carries_nul(v)),
        _ => false,
    }
}

#[cfg(test)]
mod nul_tests {
    use super::*;

    #[test]
    fn a_nul_is_found_wherever_it_is() {
        assert!(carries_nul(&json!("a\0")));
        assert!(carries_nul(&json!([1, {"x": ["\0"]}])));
        assert!(carries_nul(&json!({"k\0": 1})));
        assert!(!carries_nul(
            &json!({"title": "fine", "n": 3, "list": ["ok"]})
        ));
    }
}
