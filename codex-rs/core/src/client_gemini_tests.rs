use super::*;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::JsonSchema;
use pretty_assertions::assert_eq;

fn model() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gemini-3-flash", "display_name": "Gemini", "description": null,
        "supported_reasoning_levels": [], "shell_type": "shell_command",
        "visibility": "list", "supported_in_api": true, "priority": 1,
        "availability_nux": null, "upgrade": null, "base_instructions": "base",
        "model_messages": null, "support_verbosity": false, "default_verbosity": null,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": true, "supports_image_detail_original": false,
        "context_window": null, "auto_compact_token_limit": null,
        "effective_context_window_percent": 95, "experimental_supported_tools": [],
        "input_modalities": ["text", "image"], "supports_search_tool": false
    }))
    .expect("model")
}

fn message(role: &str, text: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "message", "role": role, "content": [{"type": "input_text", "text": text}],
        "encrypted_content": null
    }))
    .expect("message")
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.into(),
        description: "tool".into(),
        strict: false,
        defer_loading: None,
        output_schema: None,
        parameters: JsonSchema::object(std::collections::BTreeMap::new(), None, None),
    })
}

fn tool_output(call_id: &str, output: FunctionCallOutputPayload) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: None,
        namespace: None,
        output,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn stable_prefix_and_canonical_tools_across_turns() {
    let mut prompt = Prompt {
        base_instructions: BaseInstructions {
            text: "stable system".into(),
            provenance: None,
        },
        input: vec![message("user", "first")],
        tools: vec![tool("z_tool"), tool("a_tool")].into(),
        ..Default::default()
    };
    let first = build_gemini_request(&prompt, &model(), None, "/root").unwrap();
    prompt.input.extend([
        message("assistant", "answer"),
        message("developer", "dynamic"),
        message("user", "second"),
    ]);
    prompt.tools = vec![tool("a_tool"), tool("z_tool")].into();
    let second = build_gemini_request(&prompt, &model(), None, "/root").unwrap();
    let first = serde_json::to_value(first).unwrap();
    let second = serde_json::to_value(second).unwrap();
    assert_eq!(first["systemInstruction"], second["systemInstruction"]);
    assert_eq!(first["tools"], second["tools"]);
    assert_eq!(first["contents"][0], second["contents"][0]);
    assert_eq!(second["contents"][2]["parts"][0]["text"], "dynamic");
    assert!(second.get("model").is_none());
    assert!(second.get("promptCacheKey").is_none());
    assert!(second.get("cache_control").is_none());
}

#[test]
fn signed_parallel_tool_calls_replay_and_results_remain_adjacent() {
    let call = |id: &str, signature: &str| -> ResponseItem {
        let part = GeminiPart {
            function_call: Some(GeminiFunctionCall {
                id: Some(id.into()),
                name: "run".into(),
                args: json!({"x": id}),
            }),
            thought_signature: Some(signature.into()),
            ..Default::default()
        };
        serde_json::from_value(json!({
            "type": "function_call", "name": "run", "call_id": id, "arguments": format!("{{\"x\":\"{id}\"}}"),
            "encrypted_content": GeminiReplay::encode(vec![part])
        })).unwrap()
    };
    let prompt = Prompt {
        input: vec![
            message("user", "go"),
            call("a", "signature-a"),
            call("b", "signature-b"),
            tool_output("a", FunctionCallOutputPayload::from_text("ok".into())),
            tool_output("b", FunctionCallOutputPayload::from_text("done".into())),
        ],
        ..Default::default()
    };
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert_eq!(
        request["contents"][1]["parts"][0]["thoughtSignature"],
        "signature-a"
    );
    assert_eq!(
        request["contents"][1]["parts"][1]["thoughtSignature"],
        "signature-b"
    );
    assert_eq!(request["contents"][2]["role"], "user");
    assert_eq!(
        request["contents"][2]["parts"][0]["functionResponse"]["id"],
        "a"
    );
    assert_eq!(
        request["contents"][2]["parts"][1]["functionResponse"]["id"],
        "b"
    );
}

#[test]
fn tool_name_encoding_is_deterministic_and_preserves_namespace() {
    let a = wire_name("tools.run", Some("mcp"));
    let b = wire_name("tools-run", Some("mcp"));
    assert_eq!(a, wire_name("tools.run", Some("mcp")));
    assert_ne!(a, b);
    assert!(a.len() <= 64);
    assert!(wire_name(&"a".repeat(120), None).len() <= 64);
    assert_ne!(wire_name("run", Some("one")), wire_name("run", Some("two")));
}

#[test]
fn interrupted_call_has_explicit_error_not_fabricated_success() {
    let prompt = Prompt {
        input: vec![
            serde_json::from_value(json!({
                "type": "function_call", "call_id": "lost", "name": "run", "arguments": "{}"
            }))
            .unwrap(),
        ],
        ..Default::default()
    };
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert!(
        request["contents"][1]["parts"][0]["functionResponse"]["response"]["error"].is_string()
    );
}

#[test]
fn structured_output_and_base64_images_are_native_gemini_fields() {
    let prompt = Prompt {
        input: vec![
            serde_json::from_value(json!({
                "type": "message", "role": "user", "content": [{
                    "type": "input_image", "image_url": "data:image/png;base64,YQ=="
                }]
            }))
            .unwrap(),
        ],
        output_schema: Some(json!({"type": "object", "properties": {}})),
        ..Default::default()
    };
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert_eq!(
        request["contents"][0]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        request["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        request["generationConfig"]["responseJsonSchema"],
        prompt.output_schema.unwrap()
    );
}

#[test]
fn gemini_2_5_omits_call_ids_and_gemini_3_repairs_unsigned_history() {
    let prompt = Prompt {
        input: vec![
            serde_json::from_value(json!({"type": "function_call", "name": "run", "arguments": "{}", "call_id": "call"})).unwrap(),
            tool_output("call", FunctionCallOutputPayload::from_text("ok".into())),
        ], ..Default::default()
    };
    let mut legacy = model();
    legacy.slug = "gemini-2.5-pro".into();
    let legacy =
        serde_json::to_value(build_gemini_request(&prompt, &legacy, None, "/root").unwrap())
            .unwrap();
    assert!(
        legacy["contents"][0]["parts"][0]["functionCall"]
            .get("id")
            .is_none()
    );
    assert!(
        legacy["contents"][1]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
    assert!(
        legacy["contents"][0]["parts"][0]
            .get("thoughtSignature")
            .is_none()
    );
    let modern =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert_eq!(
        modern["contents"][0]["parts"][0]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
}

#[test]
fn gemini_function_responses_are_not_merged_with_dynamic_context() {
    let prompt = Prompt {
        input: vec![
            serde_json::from_value(json!({"type": "function_call", "name": "run", "arguments": "{}", "call_id": "call"})).unwrap(),
            tool_output("call", FunctionCallOutputPayload::from_text("ok".into())),
            message("developer", "dynamic update"),
        ], ..Default::default()
    };
    let request = build_gemini_request(&prompt, &model(), None, "/root").unwrap();
    assert_eq!(request.contents.len(), 3);
    assert!(
        request.contents[1]
            .parts
            .iter()
            .all(|part| part.function_response.is_some())
    );
    assert_eq!(
        request.contents[2].parts[0].text.as_deref(),
        Some("dynamic update")
    );
}

#[test]
fn gemini_filters_empty_signed_model_history_and_nests_modern_tool_images() {
    let mut empty = message("assistant", "");
    if let ResponseItem::Message {
        encrypted_content, ..
    } = &mut empty
    {
        *encrypted_content = GeminiReplay::encode(vec![GeminiPart {
            text: Some(String::new()),
            thought_signature: Some("empty-signature".into()),
            ..Default::default()
        }]);
    }
    let prompt = Prompt {
        input: vec![
            empty,
            serde_json::from_value(json!({"type": "function_call", "name": "run", "arguments": "{}", "call_id": "call"})).unwrap(),
            tool_output("call", FunctionCallOutputPayload::from_content_items(vec![FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url: "data:image/png;base64,YQ==".into() }, detail: None,
                }])),
        ], ..Default::default()
    };
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert_eq!(request["contents"][0]["parts"].as_array().unwrap().len(), 1);
    assert_eq!(
        request["contents"][1]["parts"][0]["functionResponse"]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    let mut legacy = model();
    legacy.slug = "gemini-2.5-flash".into();
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &legacy, None, "/root").unwrap())
            .unwrap();
    assert_eq!(request["contents"][1]["parts"].as_array().unwrap().len(), 1);
    assert_eq!(
        request["contents"][2]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
}

#[test]
fn gemini_reasoning_uses_model_specific_native_configuration() {
    let mut model = model();
    model.supported_reasoning_levels = vec![
        serde_json::from_value(json!({
            "effort": "high", "description": "High"
        }))
        .unwrap(),
    ];
    let request = build_gemini_request(
        &Prompt::default(),
        &model,
        Some(ReasoningEffort::Medium),
        "/root",
    )
    .unwrap();
    assert_eq!(
        request.generation_config.unwrap()["thinkingConfig"]["thinkingLevel"],
        "medium"
    );
    model.slug = "gemini-3.1-pro-preview".into();
    let request = build_gemini_request(
        &Prompt::default(),
        &model,
        Some(ReasoningEffort::Medium),
        "/root",
    )
    .unwrap();
    assert_eq!(
        request.generation_config.unwrap()["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    model.slug = "gemini-2.5-pro".into();
    let request = build_gemini_request(
        &Prompt::default(),
        &model,
        Some(ReasoningEffort::High),
        "/root",
    )
    .unwrap();
    let config = request.generation_config.unwrap();
    assert_eq!(config["thinkingConfig"]["thinkingBudget"], 24576);
    assert!(config["thinkingConfig"].get("thinkingLevel").is_none());
    assert_eq!(config["thinkingConfig"]["includeThoughts"], true);
}

#[test]
fn gemini_parallel_results_follow_call_order_and_duplicate_results_become_text() {
    let call = |id: &str| {
        serde_json::from_value(json!({
            "type": "function_call", "name": "run", "arguments": "{}", "call_id": id
        }))
        .unwrap()
    };
    let output = |id: &str| {
        tool_output(
            id,
            FunctionCallOutputPayload::from_text(format!("result {id}")),
        )
    };
    let prompt = Prompt {
        input: vec![call("a"), call("b"), output("b"), output("a"), output("a")],
        ..Default::default()
    };
    let request =
        serde_json::to_value(build_gemini_request(&prompt, &model(), None, "/root").unwrap())
            .unwrap();
    assert_eq!(
        request["contents"][1]["parts"][0]["functionResponse"]["id"],
        "a"
    );
    assert_eq!(
        request["contents"][1]["parts"][1]["functionResponse"]["id"],
        "b"
    );
    assert!(
        request["contents"][2]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("result a")
    );
}
