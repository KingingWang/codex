//! Convert a codex `Prompt` into an `AnthropicRequest` for the
//! Messages API, with cache-control markers placed for maximum prompt-caching
//! hit-rate on subsequent turns.
//!
//! Cache strategy (matches the patterns Claude Code uses):
//! - The **last system block** carries `cache_control: ephemeral`.
//! - The **last tool definition** (after stable sort by name) carries
//!   `cache_control: ephemeral`.
//! - The **last block of the most recent historical user message** — that is,
//!   the message before the latest user turn — carries `cache_control:
//!   ephemeral` so the conversation history stays cached as turns advance.
//! - Anthropic accepts at most 4 cache breakpoints per request, and we only
//!   place 3.
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
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Convert a `Prompt` into an `AnthropicRequest`. Honors the same cache-hit
/// invariants documented at the module level.
pub(crate) fn build_anthropic_request(
    prompt: &Prompt,
    model_info: &ModelInfo,
) -> CodexResult<AnthropicRequest> {
    let system = build_system(&prompt.base_instructions.text);

    let messages = build_messages(&prompt.get_formatted_input())?;

    let (tools, tool_namespace_map) = build_tools(&prompt.tools)?;

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
        thinking: None,
        metadata: None,
        tool_namespace_map,
    })
}

fn build_system(instructions: &str) -> Option<Vec<AnthropicSystemBlock>> {
    if instructions.is_empty() {
        return None;
    }
    Some(vec![
        AnthropicSystemBlock::text(instructions).with_cache(AnthropicCacheControl::ephemeral()),
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

    if let Some(last) = anthropic_tools.last_mut() {
        last.cache_control = Some(AnthropicCacheControl::ephemeral());
    }

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
            ResponseItem::Reasoning { content, .. } => {
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
                            signature: None,
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
            ResponseItem::FunctionCallOutput { call_id, output } => {
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

/// Place the message-level cache marker on the last block of the most recent
/// historical user message. "Historical" means: not the very last message in
/// the list (the trailing user turn carries the new prompt and shouldn't be
/// cached on its own — caching the message *before* keeps the turn-N prefix
/// reusable when turn N+1 arrives).
fn apply_history_cache_marker(messages: &mut [AnthropicMessage]) {
    if messages.len() < 2 {
        return;
    }
    let last_user_index = messages
        .iter()
        .enumerate()
        .rev()
        .skip(1) // skip the very last message
        .find(|(_, m)| m.role == "user")
        .map(|(i, _)| i);
    let Some(idx) = last_user_index else {
        return;
    };
    if let AnthropicMessageContent::Blocks(blocks) = &mut messages[idx].content
        && let Some(last_block) = blocks.last_mut()
    {
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
