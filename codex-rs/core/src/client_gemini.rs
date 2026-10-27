//! Prompt conversion for Google's native protocol. Keep the system, canonical
//! tool catalog, and historical contents stable for Gemini's implicit cache.
//! Dynamic developer context stays in history, not in the system prefix.

use std::collections::HashMap;

use codex_api::GeminiContent;
use codex_api::GeminiFunctionCall;
use codex_api::GeminiPart;
use codex_api::GeminiReplay;
use codex_api::GeminiRequest;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_tools::tool_arguments_to_json_object;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::client_common::Prompt;

#[path = "client_gemini_schema.rs"]
mod schema;

fn wire_name(name: &str, namespace: Option<&str>) -> String {
    let full = namespace.map_or_else(|| name.to_owned(), |ns| format!("{ns}__{name}"));
    if full.len() <= 64
        && !full.is_empty()
        && full.bytes().enumerate().all(|(i, byte)| {
            byte.is_ascii_alphabetic()
                || byte == b'_'
                || (i > 0 && (byte.is_ascii_digit() || byte == b'-'))
        })
    {
        return full;
    }
    let mut prefix: String = full
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !prefix.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        prefix.insert(0, '_');
    }
    prefix.truncate(47);
    let digest = Uuid::new_v5(&Uuid::NAMESPACE_URL, full.as_bytes())
        .simple()
        .to_string();
    format!("{prefix}_{}", &digest[..16])
}

fn push_content(contents: &mut Vec<GeminiContent>, role: &str, parts: Vec<GeminiPart>) {
    let parts: Vec<_> = parts
        .into_iter()
        .filter(|part| {
            part.text.as_ref().is_some_and(|text| !text.is_empty())
                || part.function_call.is_some()
                || part.function_response.is_some()
                || part.inline_data.is_some()
        })
        .collect();
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last.role.as_deref() == Some(role)
        && (role != "user"
            || last
                .parts
                .iter()
                .any(|part| part.function_response.is_some())
                == parts.iter().any(|part| part.function_response.is_some()))
    {
        last.parts.extend(parts);
    } else {
        contents.push(GeminiContent {
            role: Some(role.into()),
            parts,
        });
    }
}

fn image_part(image: &ImageReference) -> Result<GeminiPart> {
    let ImageReference::Inline { image_url } = image else {
        return Err(CodexErr::InvalidRequest(
            "Gemini requires inline image data".into(),
        ));
    };
    let Some((metadata, data)) = image_url
        .strip_prefix("data:")
        .and_then(|url| url.split_once(','))
    else {
        return Err(CodexErr::InvalidRequest(
            "Gemini image input must be a base64 data URL".into(),
        ));
    };
    let Some(mime_type) = metadata
        .strip_suffix(";base64")
        .filter(|mime| !mime.is_empty())
    else {
        return Err(CodexErr::InvalidRequest(
            "Gemini image input must include a MIME type and base64 data".into(),
        ));
    };
    Ok(GeminiPart {
        inline_data: Some(json!({"mimeType": mime_type, "data": data})),
        ..Default::default()
    })
}

fn content_parts(content: &[ContentItem]) -> Result<Vec<GeminiPart>> {
    content
        .iter()
        .filter_map(|part| match part {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                (!text.is_empty()).then(|| Ok(GeminiPart::text(text.clone())))
            }
            ContentItem::InputImage { image, .. } => Some(image_part(image)),
            ContentItem::InputAudio { .. } => Some(Err(CodexErr::InvalidRequest(
                "Gemini audio input is not supported by this adapter".into(),
            ))),
        })
        .collect()
}

fn output_parts(body: &FunctionCallOutputBody) -> Result<(String, Vec<GeminiPart>)> {
    match body {
        FunctionCallOutputBody::Text(text) => Ok((text.clone(), Vec::new())),
        FunctionCallOutputBody::ContentItems(items) => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for item in items {
                match item {
                    FunctionCallOutputContentItem::InputText { text } => texts.push(text.clone()),
                    FunctionCallOutputContentItem::InputImage { image, .. } => {
                        images.push(image_part(image)?)
                    }
                    FunctionCallOutputContentItem::InputAudio { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
                }
            }
            Ok((texts.join("\n"), images))
        }
    }
}

fn add_tool(
    tool: &ResponsesApiTool,
    namespace: Option<&str>,
    declarations: &mut Vec<Value>,
    names: &mut HashMap<String, (String, Option<String>)>,
) -> Result<()> {
    let name = wire_name(&tool.name, namespace);
    if names
        .insert(
            name.clone(),
            (tool.name.clone(), namespace.map(str::to_owned)),
        )
        .is_some()
    {
        return Err(CodexErr::InvalidRequest(format!(
            "duplicate Gemini tool name: {name}"
        )));
    }
    // Native Gemini accepts JSON Schema directly. Do not pass the OpenAI schema
    // as Google's narrower OpenAPI `parameters` type or strip MCP constraints.
    let parameters = serde_json::to_value(&tool.parameters).map_err(|error| {
        CodexErr::InvalidRequest(format!("failed to encode Gemini tool schema: {error}"))
    })?;
    declarations.push(json!({
        "name": name, "description": tool.description, "parametersJsonSchema": schema::normalize(parameters)
    }));
    Ok(())
}

pub(crate) fn build_gemini_request(
    prompt: &Prompt,
    model: &ModelInfo,
    effort: Option<ReasoningEffort>,
    own_agent_path: &str,
) -> Result<GeminiRequest> {
    let mut declarations = Vec::new();
    let mut names = HashMap::new();
    for tool in prompt.tools.iter() {
        match tool {
            ToolSpec::Function(tool) => add_tool(tool, None, &mut declarations, &mut names)?,
            ToolSpec::Namespace(ns) => {
                for tool in &ns.tools {
                    if let ResponsesApiNamespaceTool::Function(tool) = tool {
                        add_tool(tool, Some(&ns.name), &mut declarations, &mut names)?;
                    }
                }
            }
            // Tool planning lowers freeform tools for all non-Responses protocols.
            _ => {}
        }
    }
    declarations.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut contents = Vec::new();
    let mut calls: HashMap<String, String> = HashMap::new();
    let mut pending_tool_media = Vec::new();
    let legacy = model
        .slug
        .strip_prefix("models/")
        .unwrap_or(&model.slug)
        .strip_prefix("gemini-")
        .and_then(|slug| slug.split(['-', '.']).next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major < 3);
    for item in prompt.get_formatted_input_for_request(model) {
        if !matches!(
            &item,
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
        ) {
            flush_tool_media(&mut contents, &mut pending_tool_media);
        }
        match item {
            ResponseItem::Message {
                role,
                content,
                encrypted_content,
                ..
            } => {
                let is_model = role == "assistant";
                let parts = if let Some(parts) = is_model
                    .then(|| encrypted_content.as_deref().and_then(GeminiReplay::decode))
                    .flatten()
                {
                    parts
                } else {
                    content_parts(&content)?
                };
                push_content(
                    &mut contents,
                    if is_model { "model" } else { "user" },
                    parts,
                );
            }
            ResponseItem::AgentMessage {
                author, content, ..
            } => {
                let text: String = content
                    .iter()
                    .filter_map(|part| match part {
                        AgentMessageInputContent::InputText { text } => Some(text.as_str()),
                        AgentMessageInputContent::EncryptedContent { .. } => None,
                    })
                    .collect();
                if !text.is_empty() {
                    push_content(
                        &mut contents,
                        if author == own_agent_path {
                            "model"
                        } else {
                            "user"
                        },
                        vec![GeminiPart::text(text)],
                    );
                }
            }
            ResponseItem::Reasoning {
                encrypted_content: Some(encoded),
                ..
            } => {
                if let Some(parts) = GeminiReplay::decode(&encoded) {
                    // Unsigned thoughts are not valid signed model-history parts.
                    let parts = parts
                        .into_iter()
                        .filter(|part| part.thought_signature.is_some())
                        .collect();
                    push_content(&mut contents, "model", parts);
                }
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                encrypted_content,
                ..
            } => {
                let args = tool_arguments_to_json_object(&name, &arguments);
                let name = wire_name(&name, namespace.as_deref());
                calls.insert(call_id.clone(), name.clone());
                let signature = encrypted_content
                    .as_deref()
                    .and_then(GeminiReplay::decode)
                    .and_then(|parts| parts.into_iter().find(|part| part.function_call.is_some()))
                    .and_then(|part| part.thought_signature);
                push_content(
                    &mut contents,
                    "model",
                    vec![GeminiPart {
                        function_call: Some(GeminiFunctionCall {
                            id: Some(call_id),
                            name,
                            args,
                        }),
                        thought_signature: signature,
                        ..Default::default()
                    }],
                );
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                let wire = wire_name(&name, None);
                calls.insert(call_id.clone(), wire.clone());
                push_content(
                    &mut contents,
                    "model",
                    vec![GeminiPart {
                        function_call: Some(GeminiFunctionCall {
                            id: Some(call_id),
                            name: wire,
                            args: tool_arguments_to_json_object(&name, &input),
                        }),
                        ..Default::default()
                    }],
                );
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                append_output(
                    &mut contents,
                    &calls,
                    call_id.as_deref().unwrap_or_default(),
                    &output.body,
                    legacy,
                    &mut pending_tool_media,
                )?;
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                append_output(
                    &mut contents,
                    &calls,
                    &call_id,
                    &output.body,
                    legacy,
                    &mut pending_tool_media,
                )?;
            }
            _ => {}
        }
    }
    flush_tool_media(&mut contents, &mut pending_tool_media);
    repair_tool_batches(&mut contents);
    lower_history_metadata(&mut contents, &model.slug, legacy);
    if contents
        .last()
        .is_none_or(|content| content.role.as_deref() == Some("model"))
    {
        push_content(
            &mut contents,
            "user",
            vec![GeminiPart::text("(continue)".into())],
        );
    }
    let mut generation = json!({});
    if !model.supported_reasoning_levels.is_empty() {
        let mut thinking = json!({"includeThoughts": true});
        if let Some(effort) = effort {
            let slug = model.slug.strip_prefix("models/").unwrap_or(&model.slug);
            if slug.starts_with("gemini-3") {
                // Pro 3.0/3.1 exposes low/high, Flash exposes the full ladder.
                let level = match effort {
                    ReasoningEffort::None | ReasoningEffort::Minimal if slug.contains("flash") => {
                        "minimal"
                    }
                    ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => {
                        "low"
                    }
                    ReasoningEffort::Medium if slug.contains("flash") => "medium",
                    _ => "high",
                };
                thinking["thinkingLevel"] = json!(level);
            } else if slug.starts_with("gemini-2.5") {
                let budget = match effort {
                    ReasoningEffort::None if !slug.contains("pro") => 0,
                    ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => 1024,
                    ReasoningEffort::Medium => 8192,
                    _ => 24576,
                };
                thinking["thinkingBudget"] = json!(budget);
            }
        }
        generation["thinkingConfig"] = thinking;
    }
    if let Some(schema) = &prompt.output_schema {
        generation["responseMimeType"] = json!("application/json");
        generation["responseJsonSchema"] = schema.clone();
    }
    Ok(GeminiRequest {
        model: model.slug.clone(),
        contents,
        system_instruction: (!prompt.base_instructions.text.is_empty()).then(|| GeminiContent {
            role: None,
            parts: vec![GeminiPart::text(prompt.base_instructions.text.clone())],
        }),
        tools: if declarations.is_empty() {
            Vec::new()
        } else {
            vec![json!({"functionDeclarations": declarations})]
        },
        generation_config: generation
            .as_object()
            .is_some_and(|object| !object.is_empty())
            .then_some(generation),
        tool_name_map: names,
    })
}

fn append_output(
    contents: &mut Vec<GeminiContent>,
    calls: &HashMap<String, String>,
    call_id: &str,
    body: &FunctionCallOutputBody,
    legacy: bool,
    pending_media: &mut Vec<GeminiPart>,
) -> Result<()> {
    let (text, mut images) = output_parts(body)?;
    let part = if let Some(name) = calls.get(call_id) {
        let mut response = json!({
            "id": call_id, "name": name, "response": {"output": text}
        });
        if legacy {
            pending_media.append(&mut images);
        } else if !images.is_empty() {
            response["parts"] = json!(std::mem::take(&mut images));
        }
        GeminiPart {
            function_response: Some(response),
            ..Default::default()
        }
    } else {
        // Never invent a function name for a result with no retained call.
        GeminiPart::text(format!("Tool result ({call_id}): {text}"))
    };
    let mut parts = vec![part];
    parts.append(&mut images);
    push_content(contents, "user", parts);
    Ok(())
}

fn flush_tool_media(contents: &mut Vec<GeminiContent>, pending: &mut Vec<GeminiPart>) {
    if !pending.is_empty() {
        let mut parts = vec![GeminiPart::text("Attached media from tool result:".into())];
        parts.append(pending);
        push_content(contents, "user", parts);
    }
}

fn lower_history_metadata(contents: &mut [GeminiContent], model: &str, omit_ids: bool) {
    let model = model.strip_prefix("models/").unwrap_or(model);
    let fallback = model.starts_with("gemini-")
        && !omit_ids
        && !matches!(model, "gemini-pro" | "gemini-pro-vision")
        && !model.starts_with("gemini-robotics-er-1.5");
    for content in contents {
        let mut signed_call = false;
        for part in &mut content.parts {
            if let Some(call) = &mut part.function_call {
                if part.thought_signature.as_deref().is_some_and(|signature| {
                    !signature.is_empty() && signature != "skip_thought_signature_validator"
                }) {
                    signed_call = true;
                } else if fallback && !signed_call {
                    // Google's documented compatibility sentinel for unsigned
                    // old/imported history, not a fabricated thought signature.
                    part.thought_signature = Some("skip_thought_signature_validator".into());
                }
                if omit_ids {
                    call.id = None;
                }
            }
            if omit_ids
                && let Some(response) = part
                    .function_response
                    .as_mut()
                    .and_then(Value::as_object_mut)
            {
                response.remove("id");
            }
        }
    }
}

fn repair_tool_batches(contents: &mut Vec<GeminiContent>) {
    let mut index = 0;
    while index < contents.len() {
        let calls: Vec<_> = contents[index]
            .parts
            .iter()
            .filter_map(|part| part.function_call.clone())
            .collect();
        if !calls.is_empty() {
            if contents.get(index + 1).is_none_or(|content| {
                content.role.as_deref() != Some("user")
                    || !content
                        .parts
                        .iter()
                        .any(|part| part.function_response.is_some())
            }) {
                contents.insert(
                    index + 1,
                    GeminiContent {
                        role: Some("user".into()),
                        parts: Vec::new(),
                    },
                );
            }
            let required: std::collections::HashSet<_> =
                calls.iter().filter_map(|call| call.id.as_deref()).collect();
            let mut results = HashMap::new();
            let mut extra = Vec::new();
            for part in std::mem::take(&mut contents[index + 1].parts) {
                let id = part
                    .function_response
                    .as_ref()
                    .and_then(|response| response["id"].as_str());
                if let Some(id) = id
                    && required.contains(id)
                    && !results.contains_key(id)
                {
                    results.insert(id.to_owned(), part);
                } else {
                    extra.extend(orphan_result_parts(part));
                }
            }
            let parts = &mut contents[index + 1].parts;
            for call in calls {
                if let Some(result) = call.id.as_deref().and_then(|id| results.remove(id)) {
                    parts.push(result);
                } else {
                    parts.push(GeminiPart { function_response: Some(json!({
                        "id": call.id, "name": call.name,
                        "response": {"error": "Tool execution was interrupted; no result is available."}
                    })), ..Default::default() });
                }
            }
            if !extra.is_empty() {
                contents.insert(
                    index + 2,
                    GeminiContent {
                        role: Some("user".into()),
                        parts: extra,
                    },
                );
            }
            index += 2;
            continue;
        } else if contents[index]
            .parts
            .iter()
            .any(|part| part.function_response.is_some())
        {
            // A delayed, duplicate, or orphan result cannot start a second
            // function-response turn. Preserve it as ordinary user content.
            contents[index].parts = std::mem::take(&mut contents[index].parts)
                .into_iter()
                .flat_map(orphan_result_parts)
                .collect();
        }
        index += 1;
    }
}

fn orphan_result_parts(part: GeminiPart) -> Vec<GeminiPart> {
    let Some(response) = part.function_response else {
        return vec![part];
    };
    let id = response["id"].as_str().unwrap_or("unknown");
    let mut parts = vec![GeminiPart::text(format!(
        "Tool result ({id}): {}",
        response["response"]
    ))];
    if let Some(images) = response.get("parts")
        && let Ok(mut images) = serde_json::from_value::<Vec<GeminiPart>>(images.clone())
    {
        parts.append(&mut images);
    }
    parts
}
#[cfg(test)]
#[path = "client_gemini_tests.rs"]
mod tests;
