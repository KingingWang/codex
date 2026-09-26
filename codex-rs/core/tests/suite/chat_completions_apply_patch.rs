//! Chat Completions providers receive `apply_patch` as a function tool when the
//! model metadata opts in via `apply_patch_tool_type = "function"`, and
//! function-style calls execute end to end.

use anyhow::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::protocol::SandboxPolicy;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::run_test_with_large_stack;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-apply-patch",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "gpt-5.5",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    })
}

fn sse_body(chunks: Vec<Value>) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn chat_completions_provider(server: &MockServer) -> ModelProviderInfo {
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.wire_api = WireApi::Chat;
    provider.chat_stream = true;
    provider.supports_websockets = false;
    provider
}

#[test]
fn chat_completions_apply_patch_function_tool_round_trip() -> Result<()> {
    run_test_with_large_stack("chat-apply-patch-round-trip", || async {
        skip_if_no_network!(Ok(()));

        let server = MockServer::start().await;
        let patch = "*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch";
        let arguments = serde_json::to_string(&json!({ "input": patch }))?;

        // wiremock matches mounted mocks in mount order, and a mock stops
        // matching once it has served `up_to_n_times` requests. The first
        // chat-completions POST receives the apply_patch tool call; the
        // follow-up POST (carrying the tool result) receives the final
        // assistant message.
        let tool_call = ResponseTemplate::new(200).set_body_raw(
            sse_body(vec![
                chunk(json!({"role": "assistant"}), None),
                chunk(
                    json!({
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-apply-patch",
                            "type": "function",
                            "function": {"name": "apply_patch", "arguments": arguments},
                        }]
                    }),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ]),
            "text/event-stream",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        let final_answer = ResponseTemplate::new(200).set_body_raw(
            sse_body(vec![
                chunk(json!({"role": "assistant"}), None),
                chunk(json!({"content": "patch applied"}), None),
                chunk(json!({}), Some("stop")),
            ]),
            "text/event-stream",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_answer)
            .expect(1)
            .mount(&server)
            .await;

        let provider = chat_completions_provider(&server);
        let test = test_codex()
            .with_model_info_override("gpt-5.5", |model| {
                model.apply_patch_tool_type = Some(ApplyPatchToolType::Function);
            })
            .with_config(move |config| {
                config.model_provider = provider;
            })
            .build_with_auto_env(&server)
            .await?;

        test.submit_turn_with_policy("create hello.txt", SandboxPolicy::DangerFullAccess)
            .await?;

        // The patch executed against the local workspace.
        assert_eq!(
            std::fs::read_to_string(test.cwd.path().join("hello.txt"))?,
            "hello\n"
        );

        let requests = server
            .received_requests()
            .await
            .expect("mock server should record requests");
        let bodies = requests
            .iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .map(|request| serde_json::from_slice::<Value>(&request.body))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(bodies.len(), 2);

        // First request advertises apply_patch as a Chat Completions function tool.
        let tools = bodies[0]["tools"].as_array().expect("tools array");
        let apply_patch_tool = tools
            .iter()
            .find(|tool| tool["function"]["name"] == "apply_patch")
            .expect("apply_patch function tool should be advertised");
        assert_eq!(apply_patch_tool["type"], Value::from("function"));
        assert!(
            apply_patch_tool["function"]["parameters"]["properties"]["input"].is_object(),
            "apply_patch function tool should take an `input` string parameter"
        );

        // Second request returns the tool result to the model.
        let tool_message = bodies[1]["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("tool result message should be sent back");
        assert_eq!(
            tool_message["tool_call_id"],
            Value::from("call-apply-patch")
        );
        Ok(())
    })
}

async fn assert_apply_patch_not_advertised(
    apply_patch_tool_type: Option<ApplyPatchToolType>,
) -> Result<()> {
    let server = MockServer::start().await;
    let response = ResponseTemplate::new(200).set_body_raw(
        sse_body(vec![
            chunk(json!({"role": "assistant"}), None),
            chunk(json!({"content": "ok"}), None),
            chunk(json!({}), Some("stop")),
        ]),
        "text/event-stream",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;

    let provider = chat_completions_provider(&server);
    let test = test_codex()
        .with_model_info_override("gpt-5.5", move |model| {
            model.apply_patch_tool_type = apply_patch_tool_type;
        })
        .with_config(move |config| {
            config.model_provider = provider;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("hello").await?;

    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let body: Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .expect("chat completions request")
            .body,
    )?;
    let tool_names = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(
        !tool_names.contains(&"apply_patch"),
        "apply_patch should not be advertised, got {tool_names:?}"
    );
    Ok(())
}

/// Without model metadata opting in, `apply_patch` is not registered at all.
#[test]
fn chat_completions_without_apply_patch_metadata_omits_the_tool() -> Result<()> {
    run_test_with_large_stack("chat-apply-patch-omitted", || async {
        skip_if_no_network!(Ok(()));
        assert_apply_patch_not_advertised(/*apply_patch_tool_type*/ None).await
    })
}

/// Freeform `apply_patch` is a Responses-only custom tool: even when the model
/// metadata marks it as available, the Chat Completions wire format cannot
/// represent it and the request must omit it.
#[test]
fn chat_completions_freeform_apply_patch_is_not_advertised() -> Result<()> {
    run_test_with_large_stack("chat-apply-patch-freeform-dropped", || async {
        skip_if_no_network!(Ok(()));
        assert_apply_patch_not_advertised(Some(ApplyPatchToolType::Freeform)).await
    })
}
