//! Wire-format types for the Anthropic Messages API (`POST /v1/messages`).
//!
//! See <https://docs.anthropic.com/en/api/messages> for the canonical reference.
//!
//! These mirror the request and response shape and the SSE event flow so the
//! `codex-api` client can speak the protocol natively. Cache-control markers
//! travel on individual content blocks, system blocks, and tool definitions to
//! support Anthropic prompt caching.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// Anthropic ephemeral cache marker. Attaching one of these to a system block,
/// tool definition, or content block tells the API to cache the prefix up to
/// and including that point.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicCacheControl {
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl AnthropicCacheControl {
    pub fn ephemeral() -> Self {
        Self {
            r#type: "ephemeral".to_string(),
            ttl: None,
        }
    }
}

/// A single block within an Anthropic message. The wire format is a discriminated
/// union over `type`. We keep deserialization permissive: unknown blocks decode
/// into `AnthropicContentBlock::Other` and pass through unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: AnthropicToolResultContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// `tool_result.content` is either a plain string or a list of nested content
/// blocks (text/image only). We model both wire shapes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicMessageContent,
}

/// Anthropic accepts either a plain string or an array of content blocks for
/// `messages[].content`. We always serialize as an array on outbound requests
/// to keep cache-control plumbing predictable, but accept both shapes on input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicSystemBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

impl AnthropicSystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: None,
        }
    }

    pub fn with_cache(mut self, cache: AnthropicCacheControl) -> Self {
        match &mut self {
            Self::Text { cache_control, .. } => *cache_control = Some(cache),
        }
        self
    }
}

/// Tool definition in the Anthropic shape. Note that Anthropic uses
/// `input_schema`, not OpenAI's `parameters`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    None,
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicThinking {
    Enabled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_tokens: Option<u32>,
    },
    Disabled,
}

/// Outbound request body for `POST /v1/messages`.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    /// Map of flat tool name → namespace prefix used when reconstructing
    /// `ResponseItem::FunctionCall` for namespaced MCP tools. Not sent on the
    /// wire — see `tool_namespace_map` on `ChatCompletionsRequest`.
    #[serde(skip)]
    pub tool_namespace_map: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<i64>,
}

impl AnthropicUsage {
    pub fn cache_read(&self) -> i64 {
        self.cache_read_input_tokens.unwrap_or(0)
    }

    pub fn cache_creation(&self) -> i64 {
        self.cache_creation_input_tokens.unwrap_or(0)
    }
}

/// Non-streaming response body for `POST /v1/messages`.
///
/// `id` is `Option<String>` because some Anthropic-compatible providers omit it.
/// Mirrors the streaming `MessageStart` shape so both paths tolerate the same
/// upstream variation.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

// ─── SSE event types ────────────────────────────────────────────────────────

/// SSE event payloads from the Anthropic streaming endpoint.
///
/// Anthropic dispatches by an explicit `event:` line *and* a redundant
/// `type` field inside the JSON body. We rely on `type` for parsing, which
/// makes deserialization tolerant of the line-level event name.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    Ping,
    MessageStart {
        message: AnthropicStreamMessageStart,
    },
    ContentBlockStart {
        index: i64,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: i64,
        delta: AnthropicStreamDelta,
    },
    ContentBlockStop {
        index: i64,
    },
    MessageDelta {
        delta: AnthropicStreamMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    /// `event: error` payload — we propagate `message` upstream as a stream error.
    Error {
        error: AnthropicStreamErrorPayload,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicStreamMessageStart {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicStreamMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicStreamErrorPayload {
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Maps an Anthropic `stop_reason` to whether the model affirmatively ended
/// its turn.
///
/// - `end_turn` / `stop_sequence`: model finished cleanly → `Some(true)`.
/// - `tool_use`: model paused for tool output before continuing → `Some(false)`.
/// - `max_tokens`: response was truncated by the token limit, not by the
///   model's own decision — surfaced as `Some(false)` so callers can
///   distinguish silent truncation from a clean turn boundary.
/// - Unknown / missing: `None`.
pub fn stop_reason_to_end_turn(stop_reason: Option<&str>) -> Option<bool> {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") => Some(true),
        Some("tool_use") | Some("max_tokens") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_text_block() {
        let v: AnthropicContentBlock =
            serde_json::from_str(r#"{"type":"text","text":"hi"}"#).unwrap();
        assert!(matches!(
            v,
            AnthropicContentBlock::Text { ref text, cache_control: None } if text == "hi"
        ));
    }

    #[test]
    fn deserializes_tool_use_block() {
        let v: AnthropicContentBlock = serde_json::from_str(
            r#"{"type":"tool_use","id":"toolu_1","name":"shell","input":{"cmd":"ls"}}"#,
        )
        .unwrap();
        match v {
            AnthropicContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"], "ls");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_unknown_block_as_other() {
        let v: AnthropicContentBlock = serde_json::from_str(
            r#"{"type":"document","source":{"type":"text","media_type":"text/plain","data":"x"}}"#,
        )
        .unwrap();
        assert!(matches!(v, AnthropicContentBlock::Other));
    }

    #[test]
    fn deserializes_message_start_event() {
        let v: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-3-7-sonnet","role":"assistant","usage":{"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":10}}}"#,
        )
        .unwrap();
        match v {
            AnthropicStreamEvent::MessageStart { message } => {
                assert_eq!(message.id.as_deref(), Some("msg_1"));
                let u = message.usage.unwrap();
                assert_eq!(u.input_tokens, 12);
                assert_eq!(u.cache_read_input_tokens, Some(10));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_text_delta_event() {
        let v: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        )
        .unwrap();
        match v {
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert!(matches!(
                    delta,
                    AnthropicStreamDelta::TextDelta { ref text } if text == "hi"
                ));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_input_json_delta_event() {
        let v: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
        )
        .unwrap();
        match v {
            AnthropicStreamEvent::ContentBlockDelta { delta, .. } => match delta {
                AnthropicStreamDelta::InputJsonDelta { partial_json } => {
                    assert_eq!(partial_json, r#"{"a":1}"#);
                }
                other => panic!("unexpected delta: {other:?}"),
            },
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn maps_stop_reason() {
        assert_eq!(stop_reason_to_end_turn(Some("end_turn")), Some(true));
        assert_eq!(stop_reason_to_end_turn(Some("tool_use")), Some(false));
        assert_eq!(stop_reason_to_end_turn(Some("max_tokens")), Some(false));
        assert_eq!(stop_reason_to_end_turn(None), None);
    }

    #[test]
    fn round_trip_request() {
        let req = AnthropicRequest {
            model: "claude-3-7-sonnet".into(),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: AnthropicMessageContent::Blocks(vec![AnthropicContentBlock::Text {
                    text: "hello".into(),
                    cache_control: None,
                }]),
            }],
            max_tokens: 1024,
            system: Some(vec![
                AnthropicSystemBlock::text("you are helpful")
                    .with_cache(AnthropicCacheControl::ephemeral()),
            ]),
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: false,
            tools: vec![],
            tool_choice: None,
            thinking: None,
            metadata: None,
            tool_namespace_map: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"max_tokens\":1024"));
        assert!(json.contains("\"cache_control\":{\"type\":\"ephemeral\"}"));
        assert!(!json.contains("tool_namespace_map"));
    }

    /// Some Anthropic-compatible providers omit `id` from non-streaming
    /// responses; we tolerate that to avoid silent retry storms.
    #[test]
    fn deserializes_response_without_id() {
        let body = r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#;
        let resp: AnthropicResponse = serde_json::from_str(body).unwrap();
        assert!(resp.id.is_none());
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.content.len(), 1);
    }
}
