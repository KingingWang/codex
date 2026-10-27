//! Native Gemini wire types. Opaque part metadata is kept in history so signed
//! model parts survive tool round trips and session resume without a process cache.

use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequest {
    #[serde(skip)]
    pub model: String,
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<Value>,
    /// Wire name -> original flattened name and namespace, never sent upstream.
    #[serde(skip)]
    pub tool_name_map: HashMap<String, (String, Option<String>)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, deserialize_with = "nullable_vec")]
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(
        default,
        alias = "thought_signature",
        skip_serializing_if = "Option::is_none"
    )]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_response: Option<Value>,
    #[serde(
        default,
        alias = "inline_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub inline_data: Option<Value>,
}

impl GeminiPart {
    pub fn text(text: String) -> Self {
        Self {
            text: Some(text),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiResponse {
    #[serde(default, deserialize_with = "nullable_vec")]
    pub candidates: Vec<GeminiCandidate>,
    pub usage_metadata: Option<GeminiUsage>,
    pub response_id: Option<String>,
    pub prompt_feedback: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiCandidate {
    pub content: Option<GeminiContent>,
    pub finish_reason: Option<String>,
    pub continuation_token: Option<String>,
    pub index: Option<usize>,
}

fn nullable_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiUsage {
    pub prompt_token_count: Option<i64>,
    pub cached_content_token_count: Option<i64>,
    pub candidates_token_count: Option<i64>,
    pub thoughts_token_count: Option<i64>,
    pub total_token_count: Option<i64>,
}

/// Tagged metadata avoids mistaking another provider's encrypted content for a
/// Google signature. Keep complete parts, not just a signature matched by name.
#[derive(Debug, Serialize, Deserialize)]
pub struct GeminiReplay {
    pub gemini_parts: Vec<GeminiPart>,
}

impl GeminiReplay {
    pub fn encode(parts: Vec<GeminiPart>) -> Option<String> {
        serde_json::to_string(&Self {
            gemini_parts: parts,
        })
        .ok()
    }

    pub fn decode(encoded: &str) -> Option<Vec<GeminiPart>> {
        serde_json::from_str::<Self>(encoded)
            .ok()
            .map(|replay| replay.gemini_parts)
    }
}
