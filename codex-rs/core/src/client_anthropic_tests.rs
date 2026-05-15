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
fn tools_are_sorted_and_last_one_cached() {
    let mut prompt = Prompt::default();
    prompt.tools = vec![shell_tool(), read_tool()];
    let req = build_anthropic_request(&prompt, &test_model_info()).unwrap();
    assert_eq!(req.tools.len(), 2);
    assert_eq!(req.tools[0].name, "read_file");
    assert_eq!(req.tools[1].name, "shell");
    assert!(req.tools[0].cache_control.is_none());
    assert!(req.tools[1].cache_control.is_some());
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

    // First user message (turn 1) is the most recent *historical* user
    // message, so its last block must carry the cache marker.
    let first = &req.messages[0];
    assert_eq!(first.role, "user");
    let blocks = match &first.content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(cache_control.is_some(), "history user must be cached");
        }
        other => panic!("unexpected block: {other:?}"),
    }

    // Final user message (turn 2) should NOT have the marker.
    let last = req.messages.last().unwrap();
    assert_eq!(last.role, "user");
    let last_blocks = match &last.content {
        AnthropicMessageContent::Blocks(b) => b,
        _ => panic!("expected blocks"),
    };
    match last_blocks.last().unwrap() {
        AnthropicContentBlock::Text { cache_control, .. } => {
            assert!(
                cache_control.is_none(),
                "trailing user must not carry cache marker"
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
    //   user("run ls")            ← cached (history boundary)
    //   assistant(tool_use)
    //   user(tool_result)
    //   assistant(text "done")
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
