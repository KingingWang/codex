//! Outbound Gemini contract and retries through the real model/turn layers.

use anyhow::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::protocol::SandboxPolicy;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::run_test_with_large_stack;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const MODEL: &str = "gemini-test";

fn provider(server: &MockServer, stream: bool) -> ModelProviderInfo {
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1beta", server.uri())));
    provider.name = "gemini".into();
    provider.wire_api = WireApi::Gemini;
    provider.chat_stream = stream;
    provider.supports_websockets = false;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(2);
    provider
}

async fn session(server: &MockServer, stream: bool) -> Result<TestCodex> {
    let provider = provider(server, stream);
    test_codex()
        .with_model(MODEL)
        .with_config(move |config| {
            config.model_provider = provider;
        })
        .build_with_auto_env(server)
        .await
}

#[test]
fn gemini_apply_patch_and_signed_history_round_trip() -> Result<()> {
    run_test_with_large_stack(
        "gemini_apply_patch_and_signed_history_round_trip",
        || async {
            skip_if_no_network!(Ok(()));
            for streaming in [false, true] {
                let server = MockServer::start().await;
                let endpoint = if streaming {
                    "/v1beta/models/gemini-test:streamGenerateContent"
                } else {
                    "/v1beta/models/gemini-test:generateContent"
                };
                let patch = "*** Begin Patch\n*** Add File: gemini.txt\n+gemini\n*** End Patch";
                let value = json!({
                    "candidates": [{"content": {"parts": [
                        {"text": "thinking", "thought": true, "thoughtSignature": "thinking-signature"},
                        {"functionCall": {"id": "patch-call", "name": "apply_patch", "args": {"input": patch}},
                            "thoughtSignature": "tool-signature"}
                    ]}, "finishReason": "STOP"}]
                });
                let response = if streaming {
                    ResponseTemplate::new(200)
                        .set_body_raw(format!("data: {value}\n\n"), "text/event-stream")
                } else {
                    ResponseTemplate::new(200).set_body_json(value)
                };
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(response)
                    .expect(1)
                    .up_to_n_times(1)
                    .with_priority(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(reply(streaming, "patched", false))
                    .expect(1)
                    .with_priority(2)
                    .mount(&server)
                    .await;
                let provider = provider(&server, streaming);
                let test = test_codex()
                    .with_model_info_override(MODEL, |model| {
                        model.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
                    })
                    .with_config(move |config| config.model_provider = provider)
                    .build_with_auto_env(&server)
                    .await?;
                test.submit_turn_with_policy("create gemini.txt", SandboxPolicy::DangerFullAccess)
                    .await?;
                assert_eq!(
                    std::fs::read_to_string(test.cwd.path().join("gemini.txt"))?,
                    "gemini\n"
                );
                let bodies: Vec<Value> = server
                    .received_requests()
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|request| request.url.path() == endpoint)
                    .map(|request| serde_json::from_slice(&request.body))
                    .collect::<std::result::Result<_, _>>()?;
                assert_eq!(bodies.len(), 2);
                let definitions = bodies[0]["tools"][0]["functionDeclarations"]
                    .as_array()
                    .unwrap();
                let declaration = definitions
                    .iter()
                    .find(|tool| tool["name"] == "apply_patch")
                    .unwrap();
                assert!(declaration["parametersJsonSchema"]["properties"]["input"].is_object());
                let parts: Vec<_> = bodies[1]["contents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|content| content["parts"].as_array().unwrap())
                    .collect();
                let call = parts
                    .iter()
                    .find(|part| part["functionCall"]["name"] == "apply_patch")
                    .unwrap();
                assert_eq!(call["thoughtSignature"], "tool-signature");
                assert!(
                    parts
                        .iter()
                        .any(|part| part["thoughtSignature"] == "thinking-signature")
                );
                let result = parts
                    .iter()
                    .find(|part| part["functionResponse"]["name"] == "apply_patch")
                    .unwrap();
                assert_eq!(result["functionResponse"]["id"], "patch-call");
                server.verify().await;
            }
            Ok(())
        },
    )
}

fn reply(stream: bool, text: &str, thought: bool) -> ResponseTemplate {
    let value = json!({
        "responseId": "gemini-response",
        "candidates": [{"content": {"parts": [{"text": text, "thought": thought}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 80, "candidatesTokenCount": 5}
    });
    if stream {
        ResponseTemplate::new(200).set_body_raw(format!("data: {value}\n\n"), "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(value)
    }
}

#[test]
fn gemini_session_headers_and_cache_prefix_are_stable() -> Result<()> {
    run_test_with_large_stack(
        "gemini_session_headers_and_cache_prefix_are_stable",
        || async {
            skip_if_no_network!(Ok(()));
            for streaming in [false, true] {
                let server = MockServer::start().await;
                let endpoint = if streaming {
                    "/v1beta/models/gemini-test:streamGenerateContent"
                } else {
                    "/v1beta/models/gemini-test:generateContent"
                };
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(reply(streaming, "ok", false))
                    .expect(3)
                    .mount(&server)
                    .await;
                let first = session(&server, streaming).await?;
                first.submit_text_turn("first").await?;
                first.submit_text_turn("second").await?;
                let second = session(&server, streaming).await?;
                second.submit_text_turn("other session").await?;
                let requests: Vec<_> = server
                    .received_requests()
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|request| request.url.path() == endpoint)
                    .collect();
                assert_eq!(requests.len(), 3);
                let first_id = first.session_configured.session_id.to_string();
                let second_id = second.session_configured.session_id.to_string();
                assert_ne!(first_id, second_id);
                for (index, request) in requests.iter().enumerate() {
                    let expected = if index == 2 { &second_id } else { &first_id };
                    assert_eq!(request.headers["x-session-id"].to_str()?, expected);
                    assert_eq!(request.headers["x-session-affinity"].to_str()?, expected);
                    assert_eq!(
                        request.headers["x-codex-window-id"].to_str()?,
                        format!("{expected}:0")
                    );
                    assert!(!request.headers.contains_key("x-codex-session-id"));
                    assert!(request.headers.contains_key("x-goog-api-key"));
                    assert!(!request.headers.contains_key("authorization"));
                    if streaming {
                        assert!(
                            request
                                .url
                                .query_pairs()
                                .any(|(key, value)| key == "alt" && value == "sse")
                        );
                    }
                }
                let initial: Value = serde_json::from_slice(&requests[0].body)?;
                let followup: Value = serde_json::from_slice(&requests[1].body)?;
                assert_eq!(initial["systemInstruction"], followup["systemInstruction"]);
                assert_eq!(initial["tools"], followup["tools"]);
                assert_eq!(initial["contents"][0], followup["contents"][0]);
                assert!(followup.get("promptCacheKey").is_none());
                server.verify().await;
            }
            Ok(())
        },
    )
}

#[test]
fn gemini_reasoning_only_responses_are_retried_until_success() -> Result<()> {
    run_test_with_large_stack(
        "gemini_reasoning_only_responses_are_retried_until_success",
        || async {
            skip_if_no_network!(Ok(()));
            for streaming in [false, true] {
                let server = MockServer::start().await;
                let endpoint = if streaming {
                    "/v1beta/models/gemini-test:streamGenerateContent"
                } else {
                    "/v1beta/models/gemini-test:generateContent"
                };
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(reply(streaming, "thinking", true))
                    .up_to_n_times(1)
                    .with_priority(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(reply(streaming, "success", false))
                    .expect(1)
                    .with_priority(2)
                    .mount(&server)
                    .await;
                session(&server, streaming)
                    .await?
                    .submit_text_turn("please reply")
                    .await?;
                let requests = server.received_requests().await.unwrap();
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.url.path() == endpoint)
                        .count(),
                    2
                );
                server.verify().await;
            }
            Ok(())
        },
    )
}

#[test]
fn gemini_http_400_401_429_and_500_are_retried_by_native_client() -> Result<()> {
    run_test_with_large_stack(
        "gemini_http_400_401_429_and_500_are_retried_by_native_client",
        || async {
            skip_if_no_network!(Ok(()));
            for status in [400, 401, 429, 500] {
                let server = MockServer::start().await;
                let endpoint = "/v1beta/models/gemini-test:generateContent";
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                        "error": {"code": status, "message": "transient test error"}
                    })))
                    .up_to_n_times(1)
                    .with_priority(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(endpoint))
                    .respond_with(reply(false, "success", false))
                    .expect(1)
                    .with_priority(2)
                    .mount(&server)
                    .await;
                session(&server, false)
                    .await?
                    .submit_text_turn("please reply")
                    .await?;
                let requests: Vec<_> = server
                    .received_requests()
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|request| request.url.path() == endpoint)
                    .collect();
                assert_eq!(requests.len(), 2);
                assert_eq!(
                    requests[0].headers["x-session-id"],
                    requests[1].headers["x-session-id"]
                );
                assert_eq!(requests[0].body, requests[1].body);
                server.verify().await;
            }
            Ok(())
        },
    )
}
