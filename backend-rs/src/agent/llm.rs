//! LLM access for the agent, via rig. Replaces LiteLLM.
//!
//! Ported from `app/sentinel_agent/llm.py`, whose docstring listed the
//! three places Ollama-shaped and OpenAI-shaped APIs genuinely disagree:
//!
//! * tool results — Ollama keys them by `tool_name`, OpenAI by the
//!   `tool_call_id` it issued;
//! * tool arguments — Ollama hands back an object, OpenAI a JSON
//!   *string*;
//! * images — Ollama takes raw base64 in an `images` list, while an
//!   OpenAI-shaped `tool` message cannot carry an image at all, so it has
//!   to be a `user` message with a `data:` URI part.
//!
//! The Python built OpenAI-shaped dicts and had LiteLLM translate. Here
//! the history is rig's provider-neutral [`Message`] and each wire does
//! its own translation, which was checked rather than assumed: one
//! tool-and-image conversation was sent through each provider at a
//! capture server before this was written, and all three of the shapes
//! above came out as that docstring describes. `tests` pins them.
//!
//! One deliberate choice among rig's OpenAI wires: **Chat Completions,
//! not the Responses API**, which is rig's default. LiteLLM used
//! `/chat/completions` for `openai/…`, and it is the only one of the two
//! that an "OpenAI-compatible endpoint" — vLLM, llama.cpp, a future
//! SourceBox model — can be assumed to implement.
//!
//! Two places rig is stricter than LiteLLM was, both on the OpenAI wire,
//! and both found by the differential rather than by reading:
//!
//! * a tool call whose `arguments` string is not JSON fails rig's decode
//!   of the WHOLE response, so one bad call from a small model would end
//!   the run as a provider error. [`RepairingHttp`] rewrites such a
//!   string to `"{}"` before rig sees the body; the tool then fails its
//!   own validation with a message the model can react to.
//! * a turn with neither text nor tool calls is an error to rig and was
//!   an empty answer to the Python. It is mapped back to an empty turn.
//!
//! This module owns ALL provider-shaped knowledge. The loop builds
//! messages through the helpers below, so a format change lands here and
//! nowhere else.

use std::time::Duration;

use bytes::Bytes;
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::http_client::{
    DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use rig_core::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, Message, Text, ToolCall,
    ToolResultContent, UserContent, EMPTY_RESPONSE_ERROR,
};
use rig_core::operation::Completion;
use rig_core::providers::{anthropic, ollama, openai};
use rig_core::DynModel;
use serde_json::{Map, Value};

use crate::agent::config::{AgentConfig, Provider};

/// Left on a message whose frames were pruned, so the pass is idempotent
/// and a re-prune does not stack notices.
pub const PRUNED_NOTE: &str = "[frames pruned — superseded by newer visual output]";

#[derive(Debug, Clone, PartialEq)]
pub enum LlmError {
    /// The whole call exceeded its budget — not just the HTTP request.
    Timeout,
    Failed(String),
}

/// One assistant turn, as the loop needs it.
#[derive(Debug, Clone)]
pub struct AssistantTurn {
    /// Replayed verbatim on the next request. Kept as rig's own content
    /// rather than flattened, because a provider's reasoning items have
    /// to go back exactly as they came.
    pub content: Vec<AssistantContent>,
}

impl AssistantTurn {
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    /// The text parts, concatenated — `response_msg.content`.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The one call the agent makes, as a trait so the loop can be driven by
/// a scripted model. Everything the loop does is a reaction to what the
/// model returns, and none of that needs a network to test.
pub trait Llm: Send + Sync {
    fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl std::future::Future<Output = Result<AssistantTurn, LlmError>> + Send;

    /// For the message a timed-out run reports.
    fn timeout_seconds(&self) -> f64;
}

pub struct LlmProvider {
    model: DynModel<Completion>,
    max_tokens: u64,
    timeout_seconds: f64,
}

impl LlmProvider {
    pub fn new(config: &AgentConfig) -> Result<Self, String> {
        let model_ref = config.model_ref()?;
        let key = config.resolved_llm_api_key();
        let base = config.resolved_llm_api_base();
        let model: DynModel<Completion> = match model_ref.provider {
            Provider::Ollama => {
                let mut wire = ollama::OllamaConfig::new();
                if !base.is_empty() {
                    wire = wire.with_base_url(&base);
                }
                if !key.is_empty() {
                    wire = wire.with_api_key(key);
                }
                wire.client().completion(model_ref.model).erase()
            }
            Provider::Anthropic => {
                let mut wire = anthropic::AnthropicConfig::new(key);
                if !base.is_empty() {
                    wire = wire.with_base_url(&base);
                }
                wire.client().completion(model_ref.model).erase()
            }
            Provider::OpenAi => {
                let mut wire = openai::OpenAIConfig::new(key);
                if !base.is_empty() {
                    wire = wire.with_base_url(base);
                }
                // `.chat`, not `.completion`: see the module note.
                wire.connect(RepairingHttp(rig_reqwest::shared()))
                    .chat(model_ref.model)
                    .erase()
            }
        };
        Ok(Self {
            model,
            max_tokens: config.max_tokens,
            timeout_seconds: config.llm_call_timeout_seconds,
        })
    }
}

impl Llm for LlmProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError> {
        let mut request = CompletionRequest::new("");
        request.chat_history = messages.to_vec();
        request.tools = tools.to_vec();
        request.max_tokens = Some(self.max_tokens);

        // Bounded as a WHOLE, not per HTTP request. A provider that
        // accepts the connection and then stalls mid-response would
        // otherwise hold the machine alive until the 270 s wall clock —
        // burning Fly minutes and stranding the run in `running`.
        let call = self.model.call(request);
        match tokio::time::timeout(Duration::from_secs_f64(self.timeout_seconds), call).await {
            Err(_) => Err(LlmError::Timeout),
            Ok(Err(err)) => {
                let text = err.to_string();
                if text.contains(EMPTY_RESPONSE_ERROR) {
                    // The model said nothing at all. The loop reports
                    // that as "(agent returned no summary)"; it is not a
                    // provider failure.
                    Ok(AssistantTurn {
                        content: Vec::new(),
                    })
                } else {
                    Err(LlmError::Failed(text))
                }
            }
            Ok(Ok(response)) => Ok(AssistantTurn {
                content: response.choice,
            }),
        }
    }

    fn timeout_seconds(&self) -> f64 {
        self.timeout_seconds
    }
}

// ── Transport ─────────────────────────────────────────────────────────

/// rig's shared reqwest client, with one repair applied to unary
/// responses: see [`repair_tool_arguments`]. Streaming and multipart pass
/// through untouched — the agent uses neither.
#[derive(Clone)]
pub struct RepairingHttp(pub DynHttpClient);

impl HttpClientExt for RepairingHttp {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = rig_core::http_client::Result<Response<LazyBody<U>>>>
           + Send
           + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let sent = self.0.send::<Bytes, Bytes>(req.map(Into::into));
        async move {
            let (parts, body) = sent.await?.into_parts();
            let repaired: LazyBody<U> =
                Box::pin(async move { Ok(U::from(repair_tool_arguments(body.await?))) });
            Ok(Response::from_parts(parts, repaired))
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl std::future::Future<Output = rig_core::http_client::Result<Response<LazyBody<U>>>>
           + Send
           + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.0.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = rig_core::http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        self.0.send_streaming(req)
    }
}

/// Replace each Chat Completions tool call's unparseable `arguments`
/// string with `"{}"`.
///
/// The body is returned byte-for-byte unless a repair was made, so a
/// response this does not understand — an error envelope, another wire —
/// reaches rig exactly as the provider sent it.
pub fn repair_tool_arguments(body: Bytes) -> Bytes {
    let Ok(mut document) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let mut repaired = false;
    let choices = document.get_mut("choices").and_then(Value::as_array_mut);
    for choice in choices.into_iter().flatten() {
        let calls = choice
            .pointer_mut("/message/tool_calls")
            .and_then(Value::as_array_mut);
        for call in calls.into_iter().flatten() {
            let Some(arguments) = call.pointer_mut("/function/arguments") else {
                continue;
            };
            let Value::String(raw) = arguments else {
                continue;
            };
            // Empty is fine as it is: rig reads it as an empty object.
            if raw.trim().is_empty() || serde_json::from_str::<Value>(raw).is_ok() {
                continue;
            }
            tracing::warn!("llm: tool call had unparseable arguments");
            *arguments = Value::String("{}".to_string());
            repaired = true;
        }
    }
    if !repaired {
        return body;
    }
    serde_json::to_vec(&document)
        .map(Bytes::from)
        .unwrap_or(body)
}

// ── Message construction (the only place the wire format is known) ────

pub fn system_message(text: impl Into<String>) -> Message {
    Message::System {
        content: text.into(),
    }
}

pub fn user_message(text: impl Into<String>) -> Message {
    Message::User {
        content: vec![UserContent::Text(Text::new(text))],
    }
}

/// The assistant turn, for the running history.
pub fn assistant_message(turn: &AssistantTurn) -> Message {
    Message::Assistant {
        id: None,
        content: turn.content.clone(),
    }
}

/// Arguments for a tool call, always as an object.
///
/// rig parses an OpenAI-shaped JSON string for us, but a model can still
/// emit something that is not an object — a bare string, a list, or JSON
/// that did not parse and arrived as text. None of those may take the run
/// down: an empty object lets the tool fail on its own validation, with a
/// message the model can actually react to.
pub fn tool_call_arguments(call: &ToolCall) -> Map<String, Value> {
    match &call.function.arguments {
        Value::Object(map) => map.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => map,
            Ok(_) => Map::new(),
            Err(_) => {
                tracing::warn!(
                    tool = call.function.name.as_str(),
                    "llm: tool call had unparseable arguments"
                );
                Map::new()
            }
        },
        _ => Map::new(),
    }
}

/// The tool's textual result, keyed back to the call that asked.
pub fn tool_result_message(call: &ToolCall, text: &str) -> Message {
    Message::User {
        content: vec![UserContent::ToolResult(
            call.result(vec![ToolResultContent::Text(Text::new(text))]),
        )],
    }
}

/// Frames from a tool, as a user message with image parts.
///
/// A separate message because an OpenAI-shaped `tool` message cannot
/// carry an image. `images` are raw base64, as the MCP layer returns
/// them; each wire applies its own wrapping — a `data:` URI for OpenAI, a
/// base64 source block for Anthropic, the bare string for Ollama — so
/// switching providers never reaches back into the MCP client.
pub fn image_message(tool_name: &str, images: &[String]) -> Message {
    let mut parts = vec![UserContent::Text(Text::new(format!(
        "[Visual output from {tool_name}]"
    )))];
    for b64 in images {
        parts.push(UserContent::Image(Image {
            data: DocumentSourceKind::Base64(b64.clone()),
            media_type: Some(ImageMediaType::JPEG),
            detail: None,
            additional_params: None,
        }));
    }
    Message::User { content: parts }
}

pub fn has_images(message: &Message) -> bool {
    match message {
        Message::User { content } => content
            .iter()
            .any(|part| matches!(part, UserContent::Image(_))),
        _ => false,
    }
}

/// Strip the image parts, collapsing the message back to its text.
///
/// Idempotent: a message already pruned has no image parts left and is
/// not touched again by the caller, and the note is not stacked if it is.
pub fn prune_images(message: &mut Message) {
    let Message::User { content } = message else {
        return;
    };
    if !content
        .iter()
        .any(|part| matches!(part, UserContent::Image(_)))
    {
        return;
    }
    let joined = content
        .iter()
        .filter_map(|part| match part {
            UserContent::Text(text) if !text.text.is_empty() => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let joined = joined.trim();
    let text = if joined.contains(PRUNED_NOTE) {
        joined.to_string()
    } else {
        format!("{joined} {PRUNED_NOTE}").trim().to_string()
    };
    *content = vec![UserContent::Text(Text::new(text))];
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::ToolName;
    use serde_json::json;

    fn call(arguments: Value) -> ToolCall {
        match AssistantContent::tool_call(
            "call_1",
            ToolName::new("view_camera").unwrap(),
            arguments,
        ) {
            AssistantContent::ToolCall(call) => call,
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_are_always_an_object() {
        assert_eq!(
            tool_call_arguments(&call(json!({"camera_id": "cam-1"}))),
            json!({"camera_id": "cam-1"}).as_object().unwrap().clone()
        );
        // An OpenAI-shaped JSON string that was not parsed upstream.
        assert_eq!(
            tool_call_arguments(&call(json!("{\"camera_id\": \"cam-1\"}")))["camera_id"],
            "cam-1"
        );
        // Malformed, a list, a number, null: all degrade to empty rather
        // than failing the run.
        for bad in [
            json!("{not json"),
            json!("[1,2]"),
            json!([1, 2]),
            json!(7),
            Value::Null,
        ] {
            assert!(tool_call_arguments(&call(bad.clone())).is_empty(), "{bad}");
        }
    }

    #[test]
    fn pruning_collapses_frames_to_text_once() {
        let mut message = image_message("view_camera", &["QUJD".into(), "REVG".into()]);
        assert!(has_images(&message));

        prune_images(&mut message);
        assert!(!has_images(&message));
        let Message::User { content } = &message else {
            panic!()
        };
        assert_eq!(content.len(), 1);
        let UserContent::Text(text) = &content[0] else {
            panic!()
        };
        assert_eq!(
            text.text,
            format!("[Visual output from view_camera] {PRUNED_NOTE}")
        );

        // A second pass changes nothing — no stacked notice.
        let before = format!("{message:?}");
        prune_images(&mut message);
        assert_eq!(format!("{message:?}"), before);
    }

    #[test]
    fn unparseable_arguments_are_repaired_and_nothing_else_is_touched() {
        let bad = br#"{"choices":[{"message":{"tool_calls":[
            {"function":{"name":"a","arguments":"{not json"}},
            {"function":{"name":"b","arguments":"{\"x\": 1}"}},
            {"function":{"name":"c","arguments":""}}]}}]}"#;
        let out: Value =
            serde_json::from_slice(&repair_tool_arguments(Bytes::from_static(bad))).unwrap();
        let calls = out.pointer("/choices/0/message/tool_calls").unwrap();
        assert_eq!(calls[0]["function"]["arguments"], "{}");
        assert_eq!(calls[1]["function"]["arguments"], "{\"x\": 1}");
        assert_eq!(calls[2]["function"]["arguments"], "");

        // Byte-identical when there is nothing to repair — including for
        // a body that is not JSON at all.
        for untouched in [
            &br#"{"choices": [{"message": {"content":  "spaced"}}]}"#[..],
            &b"upstream said <html>"[..],
            &br#"{"error": {"message": "nope"}}"#[..],
        ] {
            assert_eq!(
                repair_tool_arguments(Bytes::copy_from_slice(untouched)),
                untouched
            );
        }
    }

    #[test]
    fn only_image_bearing_user_messages_count() {
        assert!(!has_images(&user_message("plain")));
        assert!(!has_images(&system_message("s")));
        let mut plain = user_message("plain");
        prune_images(&mut plain);
        let Message::User { content } = &plain else {
            panic!()
        };
        let UserContent::Text(text) = &content[0] else {
            panic!()
        };
        assert_eq!(text.text, "plain", "a message with no frames is left alone");
    }
}
