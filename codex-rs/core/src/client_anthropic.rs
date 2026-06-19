//! Convert a codex `Prompt` into an `AnthropicRequest` for the
//! Messages API, with cache-control markers placed for maximum prompt-caching
//! hit-rate on subsequent turns.
//!
//! Cache strategy (matches the Claude Code reference client exactly):
//! - The **last system block** carries `cache_control: ephemeral`.
//! - **Tool definitions carry NO `cache_control` markers.** The system-block
//!   marker alone is sufficient for Anthropic to auto-discover the tools
//!   prefix. Adding a tool-level marker shifts the hashed bytes on every
//!   turn, fighting the gateway's auto-discovery and reducing hit rate.
//! - **The last `user`-role message carries `cache_control` on its
//!   final block.** Single message-level marker, matching the Anthropic
//!   prompt-caching reference's multi-turn example. The gateway
//!   auto-discovers the longest cached prefix on subsequent turns
//!   without needing us to re-assert markers at older offsets.
//!
//!   Note: Anthropic's wire format buckets `tool_result` deliveries as
//!   user-role messages, so a trailing tool-result naturally falls into
//!   this slot too.
//! - We use 2 of Anthropic's 4 allowed breakpoints (system tail +
//!   last user message).
//! - **Adaptive thinking** is enabled for models that support reasoning,
//!   matching Claude Code's `thinking: {type: "adaptive"}`.
//!
//! Cache-hit invariants we preserve:
//! - Tool order is canonicalized (sorted by name) so adjacent turns produce a
//!   byte-identical tool prefix.
//! - System content is rendered as a single `text` block with stable text
//!   ordering, so the cached prefix never shifts.
//! - Messages are converted append-only — we never re-order earlier turns.
//!
//! Cross-format mapping notes:
//! - Anthropic carries tool results inside a `user` message as `tool_result`
//!   content blocks, immediately after the assistant message that contained
//!   the matching `tool_use`. We coalesce consecutive
//!   `FunctionCallOutput` / `CustomToolCallOutput` items into one user message.
//! - Reasoning items become `thinking` blocks attached to the assistant
//!   message they belong to.
//! - `ContentItem::InputImage` with a `data:` URL is split into the
//!   Anthropic `source: base64` shape; non-data URLs use `source: url`.

use std::collections::HashMap;

use codex_api::AnthropicCacheControl;
use codex_api::AnthropicContentBlock;
use codex_api::AnthropicImageSource;
use codex_api::AnthropicMessage;
use codex_api::AnthropicMessageContent;
use codex_api::AnthropicRequest;
use codex_api::AnthropicSystemBlock;
use codex_api::AnthropicThinking;
use codex_api::AnthropicTool;
use codex_api::AnthropicToolResultContent;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::create_tools_json_for_chat_completions;
use serde_json::Value;

use crate::client_common::Prompt;

/// Default `max_tokens` ceiling when the caller has not configured one.
/// Anthropic requires this field; codex does not currently surface it through
/// the Prompt, so we pick a generous default that any modern Claude model can
/// honor.
const DEFAULT_MAX_TOKENS: u32 = 64000;

/// Convert a `Prompt` into an `AnthropicRequest`. Honors the same cache-hit
/// invariants documented at the module level.
pub(crate) fn build_anthropic_request(
    prompt: &Prompt,
    model_info: &ModelInfo,
) -> CodexResult<AnthropicRequest> {
    let formatted_input = prompt.get_formatted_input_for_request(false);
    let (lifted_system_blocks, remaining_input) = lift_agents_md_into_system(&formatted_input);

    let system = build_system(&prompt.base_instructions.text, &lifted_system_blocks);

    let messages = build_messages(&remaining_input)?;

    let (tools, tool_namespace_map) = build_tools(&prompt.tools)?;

    // Match Claude Code: enable adaptive thinking for models that support
    // reasoning. This produces thinking blocks in the response that get
    // replayed in subsequent turns, matching the Claude Code cache pattern.
    let thinking = if model_info.supported_reasoning_levels.is_empty() {
        None
    } else {
        Some(AnthropicThinking::Adaptive)
    };

    Ok(AnthropicRequest {
        model: model_info.slug.clone(),
        messages,
        max_tokens: DEFAULT_MAX_TOKENS,
        system,
        temperature: None,
        top_p: None,
        stop_sequences: None,
        stream: false,
        tools,
        tool_choice: None,
        thinking,
        metadata: None,
        tool_namespace_map,
    })
}

/// Lift the `# AGENTS.md instructions for ...</INSTRUCTIONS>` fragment out of
/// the input messages and into the system block. The fragment is large
/// (often several thousand tokens) and stable across the whole session, so
/// keeping it as the first user-message block forces every turn to ship
/// those bytes inside the message stream where only m_0-level cache
/// markers cover them. Moving it into the system block lets the
/// system-tail cache marker (always placed by `build_system`) cover the
/// AGENTS.md bytes too, dramatically growing the stable cacheable prefix
/// and letting subsequent turns hit a much larger system+tools cache when
/// deeper message-level entries expire.
///
/// Only blocks that match the AGENTS.md START/END markers are lifted; any
/// other content in the same `ResponseItem::Message` (e.g.
/// `<environment_context>` or the actual user prompt) is preserved in the
/// returned input. Items with no remaining content are dropped.
fn lift_agents_md_into_system(items: &[ResponseItem]) -> (Vec<String>, Vec<ResponseItem>) {
    const START_MARKER: &str = "# AGENTS.md instructions for ";
    const END_MARKER: &str = "</INSTRUCTIONS>";

    let mut lifted: Vec<String> = Vec::new();
    let mut remaining: Vec<ResponseItem> = Vec::with_capacity(items.len());

    for item in items {
        let ResponseItem::Message {
            id,
            role,
            content,
            phase,
            ..
        } = item
        else {
            remaining.push(item.clone());
            continue;
        };
        if !matches!(role.as_str(), "user" | "developer" | "system") {
            remaining.push(item.clone());
            continue;
        }
        let mut kept_content: Vec<ContentItem> = Vec::with_capacity(content.len());
        for c in content {
            let text_ref = match c {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text),
                ContentItem::InputImage { .. } => None,
            };
            if let Some(text) = text_ref {
                let trimmed_start = text.trim_start();
                let trimmed_end = text.trim_end();
                if trimmed_start.starts_with(START_MARKER) && trimmed_end.ends_with(END_MARKER) {
                    lifted.push(text.clone());
                    continue;
                }
            }
            kept_content.push(c.clone());
        }
        if !kept_content.is_empty() {
            remaining.push(ResponseItem::Message {
                id: id.clone(),
                role: role.clone(),
                content: kept_content,
                phase: phase.clone(),
                metadata: None,
            });
        }
    }

    (lifted, remaining)
}

fn build_system(instructions: &str, lifted_blocks: &[String]) -> Option<Vec<AnthropicSystemBlock>> {
    let mut combined = String::new();
    if !instructions.is_empty() {
        combined.push_str(instructions);
    }
    for block in lifted_blocks {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(block);
    }
    if combined.is_empty() {
        return None;
    }
    Some(vec![
        AnthropicSystemBlock::text(combined).with_cache(AnthropicCacheControl::ephemeral()),
    ])
}

fn build_tools(
    tools: &[codex_tools::ToolSpec],
) -> CodexResult<(Vec<AnthropicTool>, HashMap<String, String>)> {
    let chat_tools_json = create_tools_json_for_chat_completions(tools)
        .map_err(|e| CodexErr::InvalidRequest(format!("failed to build tool definitions: {e}")))?;

    let mut anthropic_tools = Vec::with_capacity(chat_tools_json.len());
    for raw in chat_tools_json {
        let function = raw.get("function").cloned().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "tool entry missing `function` object while building anthropic request".to_string(),
            )
        })?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                CodexErr::InvalidRequest("tool entry missing `name` field".to_string())
            })?;
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        anthropic_tools.push(AnthropicTool {
            name,
            description,
            input_schema: parameters,
            cache_control: None,
        });
    }

    // Stable sort by name so adjacent requests produce a byte-identical tool
    // prefix. Without this, HashMap-derived iteration order can change between
    // turns and bust the cache.
    anthropic_tools.sort_by(|a, b| a.name.cmp(&b.name));

    // NOTE: Do NOT place cache_control on tools. Claude Code's reference client
    // leaves tools without cache_control markers. The system-block marker alone
    // is sufficient for Anthropic Direct to auto-discover the tools prefix, and
    // adding a tool-level marker shifts the hashed bytes on every turn, which
    // fights the gateway's auto-discovery and reduces cache hit rate.

    let namespace_map = build_tool_namespace_map(tools);

    Ok((anthropic_tools, namespace_map))
}

fn build_tool_namespace_map(tools: &[codex_tools::ToolSpec]) -> HashMap<String, String> {
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ToolSpec;

    let mut map = HashMap::new();
    for tool in tools {
        if let ToolSpec::Namespace(ns) = tool {
            for entry in &ns.tools {
                let ResponsesApiNamespaceTool::Function(func) = entry;
                map.insert(func.name.clone(), ns.name.clone());
            }
        }
    }
    map
}

/// Convert codex `ResponseItem`s into Anthropic-shape messages while merging
/// consecutive items that belong to the same logical turn. Anthropic strictly
/// alternates user/assistant; we coalesce as needed.
fn build_messages(items: &[ResponseItem]) -> CodexResult<Vec<AnthropicMessage>> {
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    let mut pending_assistant_blocks: Vec<AnthropicContentBlock> = Vec::new();
    let mut pending_user_blocks: Vec<AnthropicContentBlock> = Vec::new();
    // Reasoning is always attached to the next assistant message in order.
    let mut pending_thinking: Vec<AnthropicContentBlock> = Vec::new();

    for item in items {
        match item {
            ResponseItem::Reasoning {
                content,
                encrypted_content,
                ..
            } => {
                // Vertex AI (and Anthropic in extended-thinking mode) requires
                // a non-empty `signature` whenever a `thinking` block is sent
                // back to the API. The signature lives on `encrypted_content`
                // because the canonical ResponseItem has no dedicated field.
                // If we don't have one (older history, non-thinking response,
                // or a provider that doesn't return signatures), drop the
                // block entirely instead of sending an invalid one — replaying
                // an unsigned thinking block fails validation upstream.
                let Some(signature) = encrypted_content.clone() else {
                    continue;
                };
                if let Some(content) = content {
                    for entry in content {
                        let text = match entry {
                            ReasoningItemContent::ReasoningText { text }
                            | ReasoningItemContent::Text { text } => text,
                        };
                        if text.trim().is_empty() {
                            continue;
                        }
                        pending_thinking.push(AnthropicContentBlock::Thinking {
                            thinking: text.clone(),
                            signature: Some(signature.clone()),
                        });
                    }
                }
            }
            ResponseItem::Message { role, content, .. } => {
                let mapped_role = match role.as_str() {
                    "developer" | "system" => "system",
                    "assistant" => "assistant",
                    _ => "user",
                };
                if mapped_role == "system" {
                    // Anthropic carries system content out-of-band; turn-level
                    // system messages from prior turns are skipped.
                    continue;
                }

                if mapped_role == "assistant" {
                    flush_user(&mut messages, &mut pending_user_blocks);
                    if !pending_thinking.is_empty() {
                        pending_assistant_blocks.append(&mut pending_thinking);
                    }
                    for block in content_items_to_blocks(content) {
                        pending_assistant_blocks.push(block);
                    }
                } else {
                    flush_assistant(&mut messages, &mut pending_assistant_blocks);
                    pending_thinking.clear();
                    for block in content_items_to_blocks(content) {
                        pending_user_blocks.push(block);
                    }
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                flush_user(&mut messages, &mut pending_user_blocks);
                if !pending_thinking.is_empty() {
                    pending_assistant_blocks.append(&mut pending_thinking);
                }
                let input = parse_tool_arguments(arguments);
                pending_assistant_blocks.push(AnthropicContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input,
                    cache_control: None,
                });
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                flush_user(&mut messages, &mut pending_user_blocks);
                if !pending_thinking.is_empty() {
                    pending_assistant_blocks.append(&mut pending_thinking);
                }
                let parsed = parse_tool_arguments(input);
                pending_assistant_blocks.push(AnthropicContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: parsed,
                    cache_control: None,
                });
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                flush_assistant(&mut messages, &mut pending_assistant_blocks);
                pending_thinking.clear();
                let blocks = function_output_to_tool_result_blocks(call_id, &output.body);
                pending_user_blocks.extend(blocks);
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                flush_assistant(&mut messages, &mut pending_assistant_blocks);
                pending_thinking.clear();
                let blocks = function_output_to_tool_result_blocks(call_id, &output.body);
                pending_user_blocks.extend(blocks);
            }
            _ => {
                // Unknown items: flush pending state but otherwise ignore so
                // we never leak a half-formed message into the stream.
                flush_assistant(&mut messages, &mut pending_assistant_blocks);
                flush_user(&mut messages, &mut pending_user_blocks);
                pending_thinking.clear();
            }
        }
    }

    // Final flush so trailing pending blocks are not lost.
    flush_assistant(&mut messages, &mut pending_assistant_blocks);
    flush_user(&mut messages, &mut pending_user_blocks);

    apply_history_cache_marker(&mut messages);

    Ok(messages)
}

fn parse_tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn flush_assistant(messages: &mut Vec<AnthropicMessage>, blocks: &mut Vec<AnthropicContentBlock>) {
    if blocks.is_empty() {
        return;
    }
    let drained = std::mem::take(blocks);
    messages.push(AnthropicMessage {
        role: "assistant".to_string(),
        content: AnthropicMessageContent::Blocks(drained),
    });
}

fn flush_user(messages: &mut Vec<AnthropicMessage>, blocks: &mut Vec<AnthropicContentBlock>) {
    if blocks.is_empty() {
        return;
    }
    let drained = std::mem::take(blocks);
    messages.push(AnthropicMessage {
        role: "user".to_string(),
        content: AnthropicMessageContent::Blocks(drained),
    });
}

fn content_items_to_blocks(items: &[ContentItem]) -> Vec<AnthropicContentBlock> {
    items
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if text.is_empty() {
                    None
                } else {
                    Some(AnthropicContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    })
                }
            }
            ContentItem::InputImage { image_url, .. } => Some(image_to_block(image_url)),
        })
        .collect()
}

fn image_to_block(image_url: &str) -> AnthropicContentBlock {
    if let Some(rest) = image_url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        let media_type = meta
            .split(';')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("image/png")
            .to_string();
        return AnthropicContentBlock::Image {
            source: AnthropicImageSource::Base64 {
                media_type,
                data: data.to_string(),
            },
            cache_control: None,
        };
    }

    AnthropicContentBlock::Image {
        source: AnthropicImageSource::Url {
            url: image_url.to_string(),
        },
        cache_control: None,
    }
}

fn function_output_to_tool_result_blocks(
    call_id: &str,
    output: &codex_protocol::models::FunctionCallOutputBody,
) -> Vec<AnthropicContentBlock> {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;

    let content = match output {
        FunctionCallOutputBody::Text(text) => AnthropicToolResultContent::Text(text.clone()),
        FunctionCallOutputBody::ContentItems(items) => {
            let blocks = items
                .iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        AnthropicContentBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        }
                    }
                    FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                        image_to_block(image_url)
                    }
                    FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                        AnthropicContentBlock::Text {
                            text: encrypted_content.clone(),
                            cache_control: None,
                        }
                    }
                })
                .collect::<Vec<_>>();
            AnthropicToolResultContent::Blocks(blocks)
        }
    };

    vec![AnthropicContentBlock::ToolResult {
        tool_use_id: call_id.to_string(),
        content,
        is_error: None,
        cache_control: None,
    }]
}

/// Place a single message-level cache marker on the **last `user`-role
/// message** of the request. This matches the Anthropic prompt-caching
/// reference (`docs.anthropic.com/en/docs/build-with-claude/prompt-caching`)
/// multi-turn example, which uses one `cache_control` on the trailing user
/// turn and lets the gateway auto-discover the longest cached prefix.
///
/// Anthropic's wire format buckets `tool_result` deliveries as user-role
/// messages, so a trailing tool-result (mid-tool-loop) lands here too.
///
/// Combined with the system-block marker placed in `build_system`, the
/// request consumes 2 of Anthropic's 4 allowed breakpoints — matching
/// the Claude Code reference client exactly.
fn apply_history_cache_marker(messages: &mut [AnthropicMessage]) {
    let Some(idx) = messages.iter().rposition(|m| m.role == "user") else {
        return;
    };
    let AnthropicMessageContent::Blocks(blocks) = &mut messages[idx].content else {
        return;
    };
    if let Some(last_block) = blocks.last_mut() {
        set_block_cache(last_block);
    }
}

fn set_block_cache(block: &mut AnthropicContentBlock) {
    let marker = AnthropicCacheControl::ephemeral();
    match block {
        AnthropicContentBlock::Text { cache_control, .. }
        | AnthropicContentBlock::Image { cache_control, .. }
        | AnthropicContentBlock::ToolUse { cache_control, .. }
        | AnthropicContentBlock::ToolResult { cache_control, .. } => {
            *cache_control = Some(marker);
        }
        AnthropicContentBlock::Thinking { .. } | AnthropicContentBlock::Other => {}
    }
}

#[cfg(test)]
#[path = "client_anthropic_tests.rs"]
mod tests;
