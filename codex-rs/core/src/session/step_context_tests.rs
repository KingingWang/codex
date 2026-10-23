//! Tests for the incremental-tools feature gate across provider wire APIs.
//!
//! Incremental tools record the catalog as `additional_tools` history items, a
//! Responses Lite convention the Chat Completions and Anthropic converters do
//! not understand. The gate must therefore keep the full catalog in
//! `Prompt.tools` unless the provider speaks the Responses wire API.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_features::Feature;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::WireApi;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ToolMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;

use super::step_context::StepContext;
use super::tests::make_session_and_context;
use super::tests::update_turn_settings_for_test;
use super::turn::build_prompt;
use super::turn_context::TurnContext;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;

fn set_wire_api(turn: &mut TurnContext, wire_api: WireApi) {
    let mut provider_info = turn.config.model_provider.clone();
    provider_info.wire_api = wire_api;
    let mut config = (*turn.config).clone();
    config.model_provider = provider_info.clone();
    turn.config = Arc::new(config);
    turn.provider = create_model_provider(provider_info, turn.auth_manager.clone());
}

fn set_responses_lite(turn: &mut TurnContext, use_responses_lite: bool) {
    update_turn_settings_for_test(turn, |settings| {
        Arc::make_mut(&mut settings.model_info).use_responses_lite = use_responses_lite;
    });
}

fn enable_incremental_tools(turn: &mut TurnContext) {
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::IncrementalTools)
        .expect("incremental tools feature should be enableable");
    turn.config = Arc::new(config);
}

/// `incremental_tools` became a stable, default-enabled feature upstream, so the
/// "feature off" case has to turn it off explicitly instead of relying on the
/// built-in default.
fn disable_incremental_tools(turn: &mut TurnContext) {
    let mut config = (*turn.config).clone();
    config
        .features
        .disable(Feature::IncrementalTools)
        .expect("incremental tools feature should be disableable");
    turn.config = Arc::new(config);
}

fn shell_tool_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "shell".to_string(),
        description: "Execute a shell command".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    })
}

fn step_context_with_tools(turn: TurnContext) -> Arc<StepContext> {
    let tool_router = ToolRouter::from_parts(
        ToolRegistry::empty_for_test(),
        vec![shell_tool_spec()],
        ToolMode::Direct,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    );
    StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(Arc::new(tool_router))
}

fn base_instructions() -> BaseInstructions {
    BaseInstructions {
        text: "base instructions".to_string(),
        provenance: None,
    }
}

#[tokio::test]
async fn incremental_tools_gate_requires_responses_wire_api() {
    let cases = [
        // (wire_api, use_responses_lite, feature_enabled, expected)
        (WireApi::Responses, false, true, false),
        (WireApi::Responses, true, true, true),
        (WireApi::Responses, true, false, false),
        (WireApi::Chat, true, true, false),
        (WireApi::Anthropic, true, true, false),
    ];
    for (wire_api, use_responses_lite, feature_enabled, expected) in cases {
        let (session, mut turn) = make_session_and_context().await;
        set_wire_api(&mut turn, wire_api);
        set_responses_lite(&mut turn, use_responses_lite);
        if feature_enabled {
            enable_incremental_tools(&mut turn);
        } else {
            disable_incremental_tools(&mut turn);
        }
        let step_context = StepContext::for_test(Arc::new(turn));
        let context = format!(
            "wire_api={wire_api} use_responses_lite={use_responses_lite} \
             feature_enabled={feature_enabled}"
        );
        assert_eq!(
            step_context.incremental_tools_enabled(),
            expected,
            "step gate: {context}"
        );
        assert_eq!(
            session
                .current_window_uses_incremental_tools(&step_context)
                .await,
            expected,
            "window gate: {context}"
        );
    }
}

#[tokio::test]
async fn window_with_tool_declarations_stays_incremental_only_on_responses() {
    let cases = [
        (WireApi::Responses, true),
        (WireApi::Chat, false),
        (WireApi::Anthropic, false),
    ];
    for (wire_api, expected) in cases {
        let (session, mut turn) = make_session_and_context().await;
        set_wire_api(&mut turn, wire_api);
        set_responses_lite(&mut turn, true);
        enable_incremental_tools(&mut turn);
        // Simulate a window that already recorded its catalog, as happens when
        // a Responses Lite session is resumed onto another provider.
        session
            .record_conversation_items(
                &turn,
                turn.model_info(),
                &[ResponseItem::AdditionalTools {
                    id: None,
                    role: "developer".to_string(),
                    tools: vec![serde_json::json!({
                        "type": "function",
                        "name": "shell",
                    })]
                    .into(),
                }],
            )
            .await;
        let step_context = StepContext::for_test(Arc::new(turn));
        assert_eq!(
            session
                .current_window_uses_incremental_tools(&step_context)
                .await,
            expected,
            "wire_api={wire_api}"
        );
    }
}

#[tokio::test]
async fn build_prompt_keeps_full_catalog_and_instructions_off_responses() {
    for wire_api in [WireApi::Chat, WireApi::Anthropic] {
        let (session, mut turn) = make_session_and_context().await;
        set_wire_api(&mut turn, wire_api);
        set_responses_lite(&mut turn, true);
        enable_incremental_tools(&mut turn);
        let step_context = step_context_with_tools(turn);
        let incremental_tools = session
            .current_window_uses_incremental_tools(&step_context)
            .await;
        let prompt = build_prompt(
            Vec::new(),
            &step_context,
            base_instructions(),
            incremental_tools,
        );
        assert!(!incremental_tools, "wire_api={wire_api}");
        assert_eq!(prompt.tools.len(), 1, "wire_api={wire_api}");
        assert_eq!(prompt.tools[0].name(), "shell", "wire_api={wire_api}");
        assert_eq!(
            prompt.base_instructions.text, "base instructions",
            "wire_api={wire_api}"
        );
    }
}

#[tokio::test]
async fn build_prompt_moves_catalog_into_history_on_responses_lite() {
    let (session, mut turn) = make_session_and_context().await;
    set_wire_api(&mut turn, WireApi::Responses);
    set_responses_lite(&mut turn, true);
    enable_incremental_tools(&mut turn);
    let step_context = step_context_with_tools(turn);
    let incremental_tools = session
        .current_window_uses_incremental_tools(&step_context)
        .await;
    let prompt = build_prompt(
        Vec::new(),
        &step_context,
        base_instructions(),
        incremental_tools,
    );
    assert!(incremental_tools);
    assert!(prompt.tools.is_empty());
    assert!(prompt.base_instructions.text.is_empty());
}
