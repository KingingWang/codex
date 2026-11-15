//! Chat Completions resolves effort independently of summary support in both HTTP modes.

use anyhow::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_matrix;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Copy, Debug)]
enum EffortCase {
    ExplicitUltra,
    DefaultUltra,
    Persistent,
    ExplicitOverridesDefault,
    Custom,
    Omitted,
}

#[test_matrix(
    [false, true],
    [false, true],
    [
        EffortCase::ExplicitUltra,
        EffortCase::DefaultUltra,
        EffortCase::Persistent,
        EffortCase::ExplicitOverridesDefault,
        EffortCase::Custom,
        EffortCase::Omitted,
    ]
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_reasoning_effort_wire_contract(
    chat_stream: bool,
    supports_reasoning_summary: bool,
    case: EffortCase,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (effort, default_effort, expected_effort) = match case {
        EffortCase::ExplicitUltra => (
            Some(ReasoningEffort::Ultra),
            Some(ReasoningEffort::Low),
            Some("high"),
        ),
        EffortCase::DefaultUltra => (None, Some(ReasoningEffort::Ultra), Some("high")),
        EffortCase::Persistent => (
            Some(ReasoningEffort::Persistent),
            Some(ReasoningEffort::Low),
            Some("disabled"),
        ),
        EffortCase::ExplicitOverridesDefault => (
            Some(ReasoningEffort::High),
            Some(ReasoningEffort::Low),
            Some("high"),
        ),
        EffortCase::Custom => (
            Some(ReasoningEffort::Custom("provider-effort".to_string())),
            Some(ReasoningEffort::Low),
            Some("provider-effort"),
        ),
        EffortCase::Omitted => (None, None, None),
    };

    let server = MockServer::start().await;
    let response = if chat_stream {
        let chunk = json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 123,
            "model": "gpt-5.4",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        });
        ResponseTemplate::new(/*status*/ 200).set_body_raw(
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "gpt-5.4",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        }))
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .expect(/*requests*/ 1)
        .mount(&server)
        .await;

    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.wire_api = WireApi::Chat;
    provider.chat_stream = chat_stream;
    provider.supports_websockets = false;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", move |model| {
            model.default_reasoning_level = default_effort;
            model.supports_reasoning_summary_parameter = supports_reasoning_summary;
            model.multi_agent_reasoning_effort = Some(ReasoningEffort::High);
            model.supported_reasoning_levels = vec![ReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "high".to_string(),
            }];
        })
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_reasoning_effort = effort;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("test reasoning effort").await?;

    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let actual = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body)?;
            Ok((
                body["stream"].clone(),
                body.get("reasoning_effort").cloned(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        actual,
        vec![(Value::Bool(chat_stream), expected_effort.map(Value::from))]
    );
    Ok(())
}
