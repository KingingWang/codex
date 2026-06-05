#![allow(clippy::field_reassign_with_default, clippy::needless_update)]

use super::*;
use codex_api::AnthropicContentBlock;
use codex_api::AnthropicImageSource;
use codex_api::AnthropicMessageContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "claude-3-7-sonnet",
        "display_name": "Claude 3.7 Sonnet",
        "description": null,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "base",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "default_reasoning_summary": "auto",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {
            "mode": "bytes",
            "limit": 10000
        },
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": false,
        "context_window": null,
        "auto_compact_token_limit": null,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
    }))
    .expect("valid ModelInfo fixture")
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn function_call(name: &str, call_id: &str, args: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        namespace: None,
        name: name.to_string(),
        arguments: args.to_string(),
        call_id: call_id.to_string(),
    }
}

fn developer_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn function_output(call_id: &str, body: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(body.to_string()),
            success: None,
        },
    }
}

fn shell_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "shell".to_string(),
        description: "Execute a shell command".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
        output_schema: None,
    })
}

fn read_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "read_file".to_string(),
        description: "Read a file".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
        output_schema: None,
    })
}

#[test]
fn empty_prompt_yields_empty_request() {
    let mut prompt = Prompt::default();
    prompt.base_instructions = BaseInstructions {
        text: String::new(),
        ..Default::default()
    };
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    assert_eq!(req.messages.len(), 0);
    assert!(req.tools.is_empty());
    assert_eq!(req.system, None);
    assert_eq!(req.model, "claude-3-7-sonnet");
}

#[test]
fn system_block_is_cached() {
    let mut prompt = Prompt::default();
    prompt.base_instructions = BaseInstructions {
        text: "You are helpful.".to_string(),
        ..Default::default()
    };
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    let system = req.system.as_ref().expect("system block present");
    assert_eq!(system.len(), 1);
    match &system[0] {
        AnthropicSystemBlock::Text {
            text,
            cache_control,
        } => {
            assert_eq!(text, "You are helpful.");
            assert!(
                cache_control.is_some(),
                "system block must carry cache marker"
            );
        }
    }
}

#[test]
fn no_tool_carries_cache_marker() {
    let mut prompt = Prompt::default();
    prompt.tools = vec![shell_tool(), read_tool()];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    assert_eq!(req.tools.len(), 2);
    assert_eq!(req.tools[0].name, "read_file");
    assert_eq!(req.tools[1].name, "shell");
    // Matches Claude Code: no tool carries cache_control. The system-block
    // marker alone is sufficient for Anthropic to auto-discover the tools
    // prefix.
    assert!(
        req.tools[0].cache_control.is_none(),
        "tool must not carry cache_control"
    );
    assert!(
        req.tools[1].cache_control.is_none(),
        "tool must not carry cache_control"
    );
}

#[test]
fn user_assistant_pair_with_history_marker() {
    let mut prompt = Prompt::default();
    prompt.input = vec![
        user_message("turn 1 question"),
        assistant_message("turn 1 answer"),
        user_message("turn 2 question"),
    ];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    assert_eq!(req.messages.len(), 3);

    // Single-marker strategy: only the LAST user-role message carries
    // a marker. Earlier user messages and assistant messages are NOT
    // marked — placing more than one marker shifts cache_control bytes
    // between turns and breaks the gateway's prefix lookup.
    let first_user_blocks = match &req.messages[0].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match first_user_blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_none(),
                "first user message must NOT carry a marker — only the trailing user does"
            );
        }
        other => panic!("unexpected block: {other:?}"),
    }

    let assistant_blocks = match &req.messages[1].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match assistant_blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_none(),
                "assistant message must NOT carry a cache marker"
            );
        }
        other => panic!("unexpected block: {other:?}"),
    }

    // Trailing user message (turn 2) carries the marker.
    let last_blocks = match &req.messages[2].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match last_blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_some(),
                "trailing user must carry cache marker"
            );
        }
        other => panic!("unexpected block: {other:?}"),
    }
}

#[test]
fn function_call_and_output_round_trip() {
    let mut prompt = Prompt::default();
    prompt.input = vec![
        user_message("run ls"),
        function_call("shell", "toolu_1", r#"{"cmd":"ls"}"#),
        function_output("toolu_1", "file1\nfile2"),
        assistant_message("done"),
    ];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();

    // Expected sequence:
    //   user("run ls")
    //   assistant(tool_use)
    //   user(tool_result)
    //   assistant(text "done")  ← trailing message (carries cache marker)
    assert_eq!(req.messages.len(), 4);

    let assistant_call = &req.messages[1];
    assert_eq!(assistant_call.role, "assistant");
    let blocks = match &assistant_call.content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match &blocks[0] {
        AnthropicContentBlock::ToolUse { id, name, .. } => {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "shell");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }

    let tool_result_user = &req.messages[2];
    assert_eq!(tool_result_user.role, "user");
    let blocks = match &tool_result_user.content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match &blocks[0] {
        AnthropicContentBlock::ToolResult { tool_use_id, .. } => {
            assert_eq!(tool_use_id, "toolu_1");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn data_url_image_becomes_base64_source() {
    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,iVBORw0K".to_string(),
            detail: None,
        }],
        phase: None,
    }];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    let blocks = match &req.messages[0].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match &blocks[0] {
        AnthropicContentBlock::Image { source, .. } => match source {
            AnthropicImageSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "iVBORw0K");
            }
            other => panic!("expected base64 source, got {other:?}"),
        },
        other => panic!("expected Image block, got {other:?}"),
    }
}

/// Cache hit invariant under content-hash caching (which is how Anthropic's
/// caching actually works): the historical message CONTENT (modulo
/// cache_control field placement) must remain byte-identical across turns.
/// The marker shifts with the trailing pair, but the underlying message
/// bytes — text, tool_use input, tool_result content — must never drift.
#[test]
fn history_content_stable_across_turns_modulo_cache_control() {
    let model_info = test_model_info();

    // Turn N: user → assistant tool_use → user(tool_result_1).
    let mut prompt_n = Prompt::default();
    prompt_n.input = vec![
        user_message("turn 1 question"),
        function_call("shell", "toolu_1", r#"{"cmd":"ls"}"#),
        function_output("toolu_1", "file1\nfile2"),
    ];

    // Turn N+1: same as N, plus an asst tool_use_2 and tool_result_2.
    let mut prompt_next = Prompt::default();
    prompt_next.input = vec![
        user_message("turn 1 question"),
        function_call("shell", "toolu_1", r#"{"cmd":"ls"}"#),
        function_output("toolu_1", "file1\nfile2"),
        function_call("shell", "toolu_2", r#"{"cmd":"pwd"}"#),
        function_output("toolu_2", "/tmp"),
    ];

    let req_n = build_anthropic_request(&prompt_n, &model_info).unwrap();
    let req_next = build_anthropic_request(&prompt_next, &model_info).unwrap();

    // Strip cache_control from the first 3 messages and assert content
    // bytes match. Anthropic's cache hashes the prompt content, so as long
    // as historical content stays stable, the prefix hash matches and the
    // prior turn's cache entry can be discovered.
    fn strip_cache(msg: &AnthropicMessage) -> AnthropicMessage {
        let blocks = match &msg.content {
            AnthropicMessageContent::Blocks(b) => b
                .iter()
                .map(|block| match block.clone() {
                    AnthropicContentBlock::Text { text, .. } => AnthropicContentBlock::Text {
                        text,
                        cache_control: None,
                    },
                    AnthropicContentBlock::Image { source, .. } => AnthropicContentBlock::Image {
                        source,
                        cache_control: None,
                    },
                    AnthropicContentBlock::ToolUse {
                        id, name, input, ..
                    } => AnthropicContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control: None,
                    },
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        cache_control: None,
                    },
                    other => other,
                })
                .collect(),
            other => panic!("expected blocks, got {other:?}"),
        };
        AnthropicMessage {
            role: msg.role.clone(),
            content: AnthropicMessageContent::Blocks(blocks),
        }
    }

    for i in 0..3 {
        let n = strip_cache(&req_n.messages[i]);
        let next = strip_cache(&req_next.messages[i]);
        assert_eq!(
            n, next,
            "message {i} content must be stable across turns (modulo cache_control)"
        );
    }
}

/// Critical regression test: two consecutive turns of the SAME conversation
/// must produce byte-identical prefixes for everything up to (and including)
/// the previous turn's trailing message. This is what makes Anthropic prompt
/// caching hit across turns — a single byte of drift in the early prefix
/// invalidates everything that follows.
#[test]
fn consecutive_turns_share_byte_identical_prefix() {
    let model_info = test_model_info();
    let tools = vec![shell_tool(), read_tool()];
    let base = "You are helpful.".to_string();

    // Turn N: user → assistant tool_use → tool_result. This is the request the
    // server receives *after* the tool ran and we're feeding the result back.
    let mut prompt_n = Prompt::default();
    prompt_n.base_instructions = BaseInstructions {
        text: base.clone(),
        ..Default::default()
    };
    prompt_n.tools = tools.clone();
    prompt_n.input = vec![
        user_message("list files"),
        function_call("shell", "toolu_1", r#"{"cmd":"ls"}"#),
        function_output("toolu_1", "file1\nfile2"),
    ];

    // Turn N+1: same as N, plus the assistant's text reply and the user's
    // follow-up. The first three items must serialize byte-identically to N.
    let mut prompt_next = Prompt::default();
    prompt_next.base_instructions = BaseInstructions {
        text: base,
        ..Default::default()
    };
    prompt_next.tools = tools;
    prompt_next.input = vec![
        user_message("list files"),
        function_call("shell", "toolu_1", r#"{"cmd":"ls"}"#),
        function_output("toolu_1", "file1\nfile2"),
        assistant_message("here are the files: file1, file2"),
        user_message("thanks"),
    ];

    let req_n = build_anthropic_request(&prompt_n, &model_info).unwrap();
    let req_next = build_anthropic_request(&prompt_next, &model_info).unwrap();

    // System block must be identical.
    assert_eq!(req_n.system, req_next.system, "system block must match");
    // Tools must be identical (sort order included).
    assert_eq!(req_n.tools, req_next.tools, "tool list must match");

    // The first three messages of req_next correspond to req_n's three
    // messages, EXCEPT the cache_control marker placement: req_n marks its
    // trailing message (tool_result), req_next does NOT — it marks the new
    // trailing user. We strip cache_control before comparing, since Anthropic
    // hashes the prompt content, not the directive.
    assert_eq!(req_n.messages.len(), 3);
    assert!(req_next.messages.len() >= 3);

    fn strip_cache(msg: &AnthropicMessage) -> AnthropicMessage {
        let blocks = match &msg.content {
            AnthropicMessageContent::Blocks(b) => b
                .iter()
                .map(|block| match block.clone() {
                    AnthropicContentBlock::Text { text, .. } => AnthropicContentBlock::Text {
                        text,
                        cache_control: None,
                    },
                    AnthropicContentBlock::Image { source, .. } => AnthropicContentBlock::Image {
                        source,
                        cache_control: None,
                    },
                    AnthropicContentBlock::ToolUse {
                        id, name, input, ..
                    } => AnthropicContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control: None,
                    },
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        cache_control: None,
                    },
                    other => other,
                })
                .collect(),
            other => panic!("expected blocks, got {other:?}"),
        };
        AnthropicMessage {
            role: msg.role.clone(),
            content: AnthropicMessageContent::Blocks(blocks),
        }
    }

    for i in 0..3 {
        let n = strip_cache(&req_n.messages[i]);
        let next = strip_cache(&req_next.messages[i]);
        assert_eq!(
            n, next,
            "message {i} must serialize identically across turns (modulo cache_control)"
        );
    }
}

#[test]
fn cache_markers_stay_under_breakpoint_limit() {
    // The Anthropic API caps cache breakpoints at 4 per request. Verify we
    // never exceed that even when system, tools, and history are all in play.
    let mut prompt = Prompt::default();
    prompt.base_instructions = BaseInstructions {
        text: "You are helpful.".to_string(),
        ..Default::default()
    };
    prompt.tools = vec![shell_tool(), read_tool()];
    prompt.input = vec![
        user_message("hello"),
        assistant_message("hi"),
        user_message("again"),
    ];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();

    let mut count = 0;
    if req
        .system
        .as_ref()
        .map(|s| {
            s.iter().any(|b| match b {
                AnthropicSystemBlock::Text { cache_control, .. } => cache_control.is_some(),
            })
        })
        .unwrap_or(false)
    {
        count += 1;
    }
    count += req
        .tools
        .iter()
        .filter(|t| t.cache_control.is_some())
        .count();
    for msg in &req.messages {
        if let AnthropicMessageContent::Blocks(blocks) = &msg.content {
            count += blocks
                .iter()
                .filter(|b| match b {
                    AnthropicContentBlock::Text { cache_control, .. }
                    | AnthropicContentBlock::Image { cache_control, .. }
                    | AnthropicContentBlock::ToolUse { cache_control, .. }
                    | AnthropicContentBlock::ToolResult { cache_control, .. } => {
                        cache_control.is_some()
                    }
                    _ => false,
                })
                .count();
        }
    }
    assert!(
        count <= 4,
        "expected at most 4 cache markers, found {count}"
    );
}

/// Single-marker invariant: only the LAST user-role message carries a
/// cache marker. Earlier user messages, assistant messages, tool_use,
/// and tool_result messages all stay marker-free. This matches the
/// Anthropic prompt-caching reference's multi-turn example and lets the
/// gateway auto-discover older cache prefixes via byte-naive walk-back.
#[test]
fn only_last_user_message_carries_history_marker() {
    let mut prompt = Prompt::default();
    prompt.input = vec![
        user_message("session bootstrap"),
        assistant_message("got it"),
        user_message("trailing"),
    ];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();

    let user_marker_count = req
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .filter(|m| {
            let AnthropicMessageContent::Blocks(blocks) = &m.content else {
                return false;
            };
            matches!(
                blocks.last(),
                Some(AnthropicContentBlock::Text {
                    cache_control: Some(_),
                    ..
                })
            )
        })
        .count();
    assert_eq!(
        user_marker_count, 1,
        "exactly one user-role message must carry the cache marker"
    );

    // The marker must be on the trailing user message, not an earlier one.
    let last_user = req
        .messages
        .iter()
        .rposition(|m| m.role == "user")
        .expect("a trailing user message must exist");
    let blocks = match &req.messages[last_user].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_some(),
                "trailing user message must carry the cache marker"
            );
        }
        other => panic!("unexpected block: {other:?}"),
    }
}

/// Single-message prompts get exactly one message-level marker (the trailing
/// one). Adding a second on the same message would waste a breakpoint without
/// improving cache hits.
#[test]
fn single_message_gets_one_marker() {
    let mut prompt = Prompt::default();
    prompt.input = vec![user_message("only message")];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    assert_eq!(req.messages.len(), 1);
    let blocks = match &req.messages[0].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    let marker_count = blocks
        .iter()
        .filter(|b| match b {
            AnthropicContentBlock::Text { cache_control, .. } => cache_control.is_some(),
            _ => false,
        })
        .count();
    assert_eq!(
        marker_count, 1,
        "single message must carry exactly one marker"
    );
}

/// Byte-stability regression: across consecutive turns, every message
/// must serialize byte-identically when `cache_control` is stripped.
/// That is the invariant the gateway's prefix lookup relies on — if
/// turn N+1 ever changes any non-`cache_control` bytes within a message
/// from a previous turn, the cache lookup at that prefix misses.
///
/// `cache_control` itself necessarily moves with the trailing user
/// marker, so we strip it before comparing. What we're guarding here is
/// content drift introduced by the converter (role re-routing, ordering,
/// json field changes, etc.).
#[test]
fn earlier_messages_stay_byte_stable_across_turns() {
    let model = test_model_info();

    let mut turn_n = Prompt::default();
    turn_n.input = vec![
        user_message("turn 1 question"),
        assistant_message("turn 1 answer"),
    ];
    let req_n = build_anthropic_request(&turn_n, &model).unwrap();

    let mut turn_n_plus_1 = Prompt::default();
    turn_n_plus_1.input = vec![
        user_message("turn 1 question"),
        assistant_message("turn 1 answer"),
        user_message("turn 2 question"),
    ];
    let req_n1 = build_anthropic_request(&turn_n_plus_1, &model).unwrap();

    fn strip_cc(m: &AnthropicMessage) -> serde_json::Value {
        let mut v = serde_json::to_value(m).unwrap();
        if let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in content {
                if let Some(obj) = block.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        v
    }

    for i in 0..req_n.messages.len() {
        let prev = strip_cc(&req_n.messages[i]);
        let next = strip_cc(&req_n1.messages[i]);
        assert_eq!(
            prev, next,
            "message at index {i} must be byte-identical across turns once cache_control is stripped"
        );
    }

    // Sanity: the trailing-user marker DID move between turns. The marker
    // on req_n's last message is gone in req_n1 (it's no longer trailing).
    let last_n = req_n.messages.last().unwrap();
    let last_n1 = req_n1.messages.last().unwrap();
    let last_n_text_first = match &last_n.content {
        AnthropicMessageContent::Blocks(b) => b.first(),
        _ => None,
    }
    .and_then(|b| match b {
        AnthropicContentBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });
    let last_n1_text_first = match &last_n1.content {
        AnthropicMessageContent::Blocks(b) => b.first(),
        _ => None,
    }
    .and_then(|b| match b {
        AnthropicContentBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });
    assert_ne!(
        last_n_text_first, last_n1_text_first,
        "trailing message must differ between turns (turn N+1 added new user input)"
    );
}

/// Even when developer-role context drifts every turn (e.g. dynamic_context
/// scripts), the first converted message stays byte-identical because
/// developer/system roles are dropped at conversion time. This is what lets
/// the SYSTEM/TOOLS cache anchors survive across turns even though the
/// developer payload is unstable.
#[test]
fn developer_message_drift_doesnt_shift_first_message() {
    let mut prompt_a = Prompt::default();
    prompt_a.input = vec![
        user_message("real bootstrap"),
        developer_message("dynamic context A"),
        assistant_message("ok"),
        user_message("trailing"),
    ];
    let mut prompt_b = Prompt::default();
    prompt_b.input = vec![
        user_message("real bootstrap"),
        developer_message("entirely different B content"),
        assistant_message("ok"),
        user_message("trailing"),
    ];

    let req_a = build_anthropic_request(&prompt_a, &test_model_info()).unwrap();
    let req_b = build_anthropic_request(&prompt_b, &test_model_info()).unwrap();

    // Same number of messages on both sides — developer items dropped.
    assert_eq!(req_a.messages.len(), req_b.messages.len());
    // First message must serialize byte-identically (no cache_control on
    // either after the cc-haha-aligned single-marker design).
    assert_eq!(req_a.messages[0], req_b.messages[0]);
}

/// AGENTS.md fragments are large (often thousands of tokens) and stable for
/// the whole session. Routing them through the user message stream means the
/// system cache can never grow past "You are helpful." Lifting them into the
/// system block grows the system-tail cache by the AGENTS.md size every
/// turn, which is what lets subsequent turns hit a much larger system+tools
/// cache when deeper message-level entries expire.
#[test]
fn agents_md_fragment_is_lifted_into_system_block() {
    let agents_md_text =
        "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nuse rust\n</INSTRUCTIONS>"
            .to_string();

    let mut prompt = Prompt::default();
    prompt.base_instructions = BaseInstructions {
        text: "You are helpful.".to_string(),
        ..Default::default()
    };
    prompt.input = vec![
        user_message(&agents_md_text),
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"
                    .to_string(),
            }],
            phase: None,
        },
        user_message("review the diff"),
    ];

    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();

    // System block must contain BOTH the original instructions and the
    // lifted AGENTS.md text.
    let system = req.system.as_ref().expect("system block present");
    assert_eq!(system.len(), 1);
    let AnthropicSystemBlock::Text {
        text,
        cache_control,
    } = &system[0];
    assert!(
        text.contains("You are helpful."),
        "system block must keep the base instructions"
    );
    assert!(
        text.contains("# AGENTS.md instructions for /tmp"),
        "system block must contain the lifted AGENTS.md fragment, got: {text:?}"
    );
    assert!(
        text.contains("</INSTRUCTIONS>"),
        "system block must contain the AGENTS.md end marker"
    );
    assert!(
        cache_control.is_some(),
        "system block must carry cache marker"
    );

    // Messages must NO LONGER contain the AGENTS.md text — only the env
    // context block and the actual user prompt remain.
    let all_message_text: String = req
        .messages
        .iter()
        .flat_map(|m| match &m.content {
            AnthropicMessageContent::Blocks(b) => b.clone(),
            _ => Vec::new(),
        })
        .filter_map(|b| match b {
            AnthropicContentBlock::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        !all_message_text.contains("# AGENTS.md instructions for"),
        "AGENTS.md must be lifted out of messages, but found in: {all_message_text:?}"
    );
    assert!(
        all_message_text.contains("<environment_context>"),
        "non-AGENTS user content must be preserved in messages"
    );
    assert!(
        all_message_text.contains("review the diff"),
        "actual user prompt must be preserved in messages"
    );
}

/// When AGENTS.md and other content live in the SAME `ResponseItem::Message`
/// (multiple ContentItems), only the AGENTS.md block is lifted. The other
/// blocks stay in the message — which becomes m_0 with one less block.
#[test]
fn agents_md_lifts_only_matching_block_from_mixed_message() {
    let agents_md_text =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nfollow style guide\n</INSTRUCTIONS>"
            .to_string();

    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: agents_md_text,
            },
            ContentItem::InputText {
                text: "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"
                    .to_string(),
            },
            ContentItem::InputText {
                text: "actual question".to_string(),
            },
        ],
        phase: None,
    }];

    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();

    // Lifted into system.
    let system = req.system.as_ref().expect("system");
    let AnthropicSystemBlock::Text { text, .. } = &system[0];
    assert!(text.contains("# AGENTS.md instructions for /repo"));

    // m_0 still exists with the remaining 2 blocks.
    assert_eq!(req.messages.len(), 1);
    let blocks = match &req.messages[0].content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    assert_eq!(blocks.len(), 2, "AGENTS block lifted, two blocks remain");
    let texts: Vec<&String> = blocks
        .iter()
        .filter_map(|b| match b {
            AnthropicContentBlock::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("<environment_context>")));
    assert!(texts.iter().any(|t| t.as_str() == "actual question"));
}
