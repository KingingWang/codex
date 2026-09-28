//! Convert a codex `Prompt` into an `AnthropicRequest` for the
//! Messages API, with cache-control markers placed for maximum prompt-caching
//! hit-rate on subsequent turns.
//!
//! Cache strategy (matches the Claude Code reference client):
//! - The `system` field is tiered into stable blocks: block 0 holds the base
//!   instructions and blocks 1..n hold the lifted AGENTS.md fragments (one
//!   block per fragment, so block boundaries survive instead of being
//!   lossily concatenated). The **first and last system blocks** carry
//!   `cache_control: ephemeral`, so even when a lifted fragment changes
//!   mid-session the base-instructions cache entry still hits.
//! - **Tool definitions carry NO `cache_control` markers.** The system-block
//!   marker alone is sufficient for Anthropic to auto-discover the tools
//!   prefix. Adding a tool-level marker shifts the hashed bytes on every
//!   turn, fighting the gateway's auto-discovery and reducing hit rate.
//! - **The last stable user-origin block carries the message-level
//!   `cache_control` marker**, matching the Anthropic prompt-caching
//!   reference's multi-turn example. The gateway auto-discovers the longest
//!   cached prefix on subsequent turns without needing us to re-assert
//!   markers at older offsets.
//!
//!   `developer`/`system`-role messages have no Anthropic equivalent. Rather
//!   than dropping them, their content is re-routed into the user stream and
//!   tagged `ReroutedSystem`. Those blocks never receive the marker and are
//!   skipped when placing it: `dynamic_context_script` output is re-appended
//!   to (and can drift on) every sampling request, so anchoring the marker
//!   before any re-routed tail keeps the cached prefix byte-stable.
//!
//!   Note: Anthropic's wire format buckets `tool_result` deliveries as
//!   user-role messages, so a trailing tool-result naturally falls into
//!   this slot too.
//! - We use at most 3 of Anthropic's 4 allowed breakpoints (system base +
//!   system tail + last stable user block).
//! - **Adaptive thinking** is enabled for models that support reasoning,
//!   matching Claude Code's `thinking: {type: "adaptive"}`.
//!
//! Cache-hit invariants we preserve:
//! - Tool order is canonicalized (sorted by name) so adjacent turns produce a
//!   byte-identical tool prefix.
//! - System blocks are emitted in a stable order (base first, lifted
//!   fragments in history order), so the cached prefix never shifts.
//! - Messages are converted append-only — we never re-order earlier turns.
//! - Re-routed developer/system content is deterministic per history item,
//!   and the message-level marker never covers drifting per-request bytes.
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
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::create_tools_json_for_chat_completions;
use codex_tools::tool_arguments_to_json_object;
use serde_json::Value;

use crate::client_common::Prompt;

/// Default `max_tokens` ceiling when the caller has not configured one.
/// Anthropic requires this field; codex does not currently surface it through
/// the Prompt, so we pick a generous default that any modern Claude model can
/// honor.
const DEFAULT_MAX_TOKENS: u32 = 64000;

/// Convert a `Prompt` into an `AnthropicRequest`. Honors the same cache-hit
/// invariants documented at the module level.
#[cfg(test)]
pub(crate) fn build_anthropic_request(
    prompt: &Prompt,
    model_info: &ModelInfo,
) -> CodexResult<AnthropicRequest> {
    build_anthropic_request_with_agent_path(prompt, model_info, "/root")
}

pub(crate) fn build_anthropic_request_with_agent_path(
    prompt: &Prompt,
    model_info: &ModelInfo,
    own_agent_path: &str,
) -> CodexResult<AnthropicRequest> {
    let formatted_input = prompt.get_formatted_input_for_request(model_info);
    let (lifted_system_blocks, remaining_input) = lift_agents_md_into_system(&formatted_input);

    let system = build_system(&prompt.base_instructions.text, &lifted_system_blocks);

    let messages = build_messages(&remaining_input, own_agent_path)?;

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
    // Canonical markers emitted by `context::user_instructions`. The ` for
    // <dir>` suffix is optional (host-provided instructions carry no
    // directory), so match on the bare header.
    const START_MARKER: &str = "# AGENTS.md instructions";
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
                ContentItem::InputAudio { .. } => None,
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
                internal_chat_message_metadata_passthrough: None,
            });
        }
    }

    (lifted, remaining)
}

/// Assemble the `system` field as ordered text blocks:
/// - block 0: base instructions (session-stable) — carries `cache_control`,
/// - blocks 1..n: one block per lifted AGENTS.md fragment, boundaries
///   preserved (no lossy `"\n\n"` concatenation) — the tail block carries
///   `cache_control`.
///
/// Two system breakpoints (Claude Code's tiered pattern) mean a mid-session
/// change to the lifted fragments still hits the base-instructions cache
/// entry. With no lifted fragments the output is byte-identical to the
/// single-block form, so sessions without AGENTS.md keep their exact cached
/// prefix. Worst case stays within Anthropic's 4-breakpoint budget (2 system
/// + 1 message-level marker).
fn build_system(instructions: &str, lifted_blocks: &[String]) -> Option<Vec<AnthropicSystemBlock>> {
    let mut texts: Vec<&str> = Vec::with_capacity(lifted_blocks.len() + 1);
    if !instructions.is_empty() {
        texts.push(instructions);
    }
    texts.extend(lifted_blocks.iter().map(String::as_str));
    if texts.is_empty() {
        return None;
    }
    let last = texts.len() - 1;
    Some(
        texts
            .into_iter()
            .enumerate()
            .map(|(idx, text)| {
                let block = AnthropicSystemBlock::text(text);
                if idx == 0 || idx == last {
                    block.with_cache(AnthropicCacheControl::ephemeral())
                } else {
                    block
                }
            })
            .collect(),
    )
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
                let ResponsesApiNamespaceTool::Function(func) = entry else {
                    continue;
                };
                map.insert(func.name.clone(), ns.name.clone());
            }
        }
    }
    map
}

/// Where a user-role content block came from.
///
/// Anthropic has no `developer`/`system` message role, so turn-level system
/// content is re-routed into the user stream. Those blocks can drift between
/// requests (`dynamic_context_script` output is re-appended to every sampling
/// request and regenerated each time), so they must never carry — nor sit
/// before — the message-level cache marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UserBlockOrigin {
    /// Genuine user content, tool results, or peer-agent messages: stable
    /// once recorded, safe to anchor the cache marker on.
    User,
    /// Content re-routed from a `developer`/`system`-role message. May be
    /// re-generated per request; excluded from cache-marker placement.
    ReroutedSystem,
}

/// Convert codex `ResponseItem`s into Anthropic-shape messages while merging
/// consecutive items that belong to the same logical turn. Anthropic strictly
/// alternates user/assistant; we coalesce as needed.
fn build_messages(
    items: &[ResponseItem],
    own_agent_path: &str,
) -> CodexResult<Vec<AnthropicMessage>> {
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    // Per-block provenance for every flushed user-role message, parallel to
    // the user messages in `messages` (in flush order). Drives cache-marker
    // placement in `apply_history_cache_marker`.
    let mut user_origins: Vec<Vec<UserBlockOrigin>> = Vec::new();
    let mut pending_assistant_blocks: Vec<AnthropicContentBlock> = Vec::new();
    let mut pending_user_blocks: Vec<(AnthropicContentBlock, UserBlockOrigin)> = Vec::new();
    // Reasoning is always attached to the next assistant message in order.
    let mut pending_thinking: Vec<AnthropicContentBlock> = Vec::new();
    // Degradation counters: content this converter had to re-route or drop.
    // Surfaced as a single debug log so silent fidelity loss is diagnosable.
    let mut rerouted_system_messages = 0usize;
    let mut dropped_unsigned_reasoning = 0usize;

    for item in items {
        match item {
            ResponseItem::Reasoning {
                summary,
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
                    dropped_unsigned_reasoning += 1;
                    continue;
                };
                // Read the thinking text from `summary` first: the fork's
                // anthropic provider carries it on the summary channel (like
                // the chat-completions provider and the Responses API summary
                // path) so clients that render completed reasoning items can
                // see it. Fall back to `content` for reasoning items recorded
                // by earlier builds.
                let mut texts: Vec<String> = Vec::new();
                for entry in summary {
                    let ReasoningItemReasoningSummary::SummaryText { text } = entry;
                    texts.push(text.clone());
                }
                if texts.iter().all(|text| text.trim().is_empty()) {
                    texts.clear();
                    for entry in content.iter().flatten() {
                        match entry {
                            ReasoningItemContent::ReasoningText { text }
                            | ReasoningItemContent::Text { text } => texts.push(text.clone()),
                        }
                    }
                }
                for text in texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    pending_thinking.push(AnthropicContentBlock::Thinking {
                        thinking: text,
                        signature: Some(signature.clone()),
                    });
                }
            }
            ResponseItem::Message { role, content, .. } => {
                let mapped_role = match role.as_str() {
                    "developer" | "system" => "system",
                    "assistant" => "assistant",
                    _ => "user",
                };
                if mapped_role == "assistant" {
                    flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);
                    if !pending_thinking.is_empty() {
                        pending_assistant_blocks.append(&mut pending_thinking);
                    }
                    for block in content_items_to_blocks(content) {
                        pending_assistant_blocks.push(block);
                    }
                } else {
                    // Anthropic has no developer/system message role. Instead
                    // of dropping turn-level system content (subagent spawn
                    // instructions, `dynamic_context_script` output, guardian
                    // annotations), re-route it into the user stream tagged as
                    // `ReroutedSystem` so the message-level cache marker never
                    // anchors on bytes that can drift between requests.
                    let origin = if mapped_role == "system" {
                        UserBlockOrigin::ReroutedSystem
                    } else {
                        UserBlockOrigin::User
                    };
                    flush_assistant(&mut messages, &mut pending_assistant_blocks);
                    if origin == UserBlockOrigin::User {
                        // Genuine user input closes the previous assistant
                        // turn: any thinking still queued will never be
                        // attached, so drop it. Re-routed system content does
                        // not close the assistant turn (e.g. guardian warnings
                        // recorded mid tool-loop), so pending thinking is kept
                        // for the next assistant message.
                        pending_thinking.clear();
                    }
                    let blocks = content_items_to_blocks(content);
                    if origin == UserBlockOrigin::ReroutedSystem && !blocks.is_empty() {
                        rerouted_system_messages += 1;
                    }
                    for block in blocks {
                        pending_user_blocks.push((block, origin));
                    }
                }
            }
            ResponseItem::AgentMessage {
                author, content, ..
            } => {
                let text = content
                    .iter()
                    .map(|part| match part {
                        AgentMessageInputContent::InputText { text } => text.as_str(),
                        AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                            encrypted_content.as_str()
                        }
                    })
                    .collect::<String>();
                if text.trim().is_empty() {
                    continue;
                }
                if author == own_agent_path {
                    flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);
                    if !pending_thinking.is_empty() {
                        pending_assistant_blocks.append(&mut pending_thinking);
                    }
                    pending_assistant_blocks.push(AnthropicContentBlock::Text {
                        text,
                        cache_control: None,
                    });
                } else {
                    flush_assistant(&mut messages, &mut pending_assistant_blocks);
                    pending_thinking.clear();
                    pending_user_blocks.push((
                        AnthropicContentBlock::Text {
                            text,
                            cache_control: None,
                        },
                        UserBlockOrigin::User,
                    ));
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);
                if !pending_thinking.is_empty() {
                    pending_assistant_blocks.append(&mut pending_thinking);
                }
                // Anthropic requires `tool_use.input` to be a JSON object and
                // rejects the whole request otherwise, so repair anything that
                // is not already an object.
                let input = tool_arguments_to_json_object(name, arguments);
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
                flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);
                if !pending_thinking.is_empty() {
                    pending_assistant_blocks.append(&mut pending_thinking);
                }
                // Freeform tools (e.g. `apply_patch`) record raw text rather
                // than JSON, which Anthropic would reject as a non-dictionary
                // `tool_use.input`.
                let parsed = tool_arguments_to_json_object(name, input);
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
                let blocks = function_output_to_tool_result_blocks(
                    call_id.as_deref().unwrap_or_default(),
                    &output.body,
                );
                pending_user_blocks.extend(
                    blocks
                        .into_iter()
                        .map(|block| (block, UserBlockOrigin::User)),
                );
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                flush_assistant(&mut messages, &mut pending_assistant_blocks);
                pending_thinking.clear();
                let blocks = function_output_to_tool_result_blocks(call_id, &output.body);
                pending_user_blocks.extend(
                    blocks
                        .into_iter()
                        .map(|block| (block, UserBlockOrigin::User)),
                );
            }
            _ => {
                // Unknown items: flush pending state but otherwise ignore so
                // we never leak a half-formed message into the stream.
                flush_assistant(&mut messages, &mut pending_assistant_blocks);
                flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);
                pending_thinking.clear();
            }
        }
    }

    // Final flush so trailing pending blocks are not lost.
    flush_assistant(&mut messages, &mut pending_assistant_blocks);
    flush_user(&mut messages, &mut user_origins, &mut pending_user_blocks);

    apply_history_cache_marker(&mut messages, &user_origins);

    if rerouted_system_messages > 0 || dropped_unsigned_reasoning > 0 {
        tracing::debug!(
            rerouted_system_messages,
            dropped_unsigned_reasoning,
            "anthropic request conversion re-routed/dropped non-representable items"
        );
    }

    Ok(messages)
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

fn flush_user(
    messages: &mut Vec<AnthropicMessage>,
    user_origins: &mut Vec<Vec<UserBlockOrigin>>,
    blocks: &mut Vec<(AnthropicContentBlock, UserBlockOrigin)>,
) {
    if blocks.is_empty() {
        return;
    }
    let drained = std::mem::take(blocks);
    user_origins.push(drained.iter().map(|(_, origin)| *origin).collect());
    messages.push(AnthropicMessage {
        role: "user".to_string(),
        content: AnthropicMessageContent::Blocks(
            drained.into_iter().map(|(block, _)| block).collect(),
        ),
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
            ContentItem::InputImage { image, .. } => match image {
                codex_protocol::models::ImageReference::Inline { image_url } => {
                    Some(image_to_block(image_url))
                }
                codex_protocol::models::ImageReference::File { .. } => None,
            },
            // TypeWise: this fork does not surface audio inputs in Anthropic
            // requests; drop audio items rather than erroring.
            ContentItem::InputAudio { .. } => None,
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
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        Some(AnthropicContentBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        })
                    }
                    FunctionCallOutputContentItem::InputImage { image, .. } => match image {
                        codex_protocol::models::ImageReference::Inline { image_url } => {
                            Some(image_to_block(image_url))
                        }
                        codex_protocol::models::ImageReference::File { .. } => None,
                    },
                    FunctionCallOutputContentItem::InputAudio { .. } => {
                        // Audio tool outputs are not supported in Anthropic tool
                        // result blocks; drop audio items rather than erroring.
                        None
                    }
                    FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                        Some(AnthropicContentBlock::Text {
                            text: encrypted_content.clone(),
                            cache_control: None,
                        })
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

/// Place the single message-level cache marker on the **last stable
/// user-origin block** of the request. This matches the Anthropic
/// prompt-caching reference
/// (`docs.anthropic.com/en/docs/build-with-claude/prompt-caching`) multi-turn
/// example, which uses one `cache_control` on the trailing user turn and lets
/// the gateway auto-discover the longest cached prefix.
///
/// Anthropic's wire format buckets `tool_result` deliveries as user-role
/// messages, so a trailing tool-result (mid-tool-loop) lands here too.
///
/// Re-routed `developer`/`system` blocks are skipped: a trailing
/// dynamic-context payload rides *after* the marker, so the cached prefix
/// (everything up to the marker) stays byte-identical across requests even
/// while the re-routed tail drifts. A user message composed entirely of
/// re-routed content is skipped too, walking the marker back to the previous
/// stable user block.
///
/// Combined with the two system-block markers placed in `build_system`, the
/// request consumes at most 3 of Anthropic's 4 allowed breakpoints.
fn apply_history_cache_marker(
    messages: &mut [AnthropicMessage],
    user_origins: &[Vec<UserBlockOrigin>],
) {
    // `user_origins` entries are parallel to the user-role messages in
    // `messages`, in flush order; walk both from the newest end.
    let mut origins_idx = user_origins.len();
    for message in messages.iter_mut().rev() {
        if message.role != "user" {
            continue;
        }
        if origins_idx == 0 {
            // Unreachable: `flush_user` records origins for every user
            // message it pushes.
            break;
        }
        origins_idx -= 1;
        let AnthropicMessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        debug_assert_eq!(blocks.len(), user_origins[origins_idx].len());
        let Some(idx) = user_origins[origins_idx]
            .iter()
            .rposition(|origin| *origin == UserBlockOrigin::User)
        else {
            // Entirely re-routed content — anchor on an older, stable block.
            continue;
        };
        set_block_cache(&mut blocks[idx]);
        return;
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
