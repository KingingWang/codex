//! Integration test: the Chat Completions outbound wire contract for prompt
//! cache routing.
//!
//! Mirroring OpenCode, every `WireApi::Chat` request must carry the raw Codex
//! session ID in both `x-session-affinity` and `X-Session-Id` headers, and a
//! matching camelCase body field `promptCacheKey`. The value must stay stable
//! across turns of one session and differ across sessions. The legacy
//! `x-codex-session-id` header must not be sent, and the body must not contain
//! `prompt_cache_key` or other session-metadata keys.
//!
//! The contract is exercised at the outbound HTTP boundary for both response
//! modes: non-streaming JSON (`chat_stream = false`) and streaming SSE
//! (`chat_stream = true`).

use anyhow::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const X_SESSION_AFFINITY_HEADER: &str = "x-session-affinity";
const X_SESSION_ID_HEADER: &str = "x-session-id";

async fn build_chat_session(server: &MockServer, chat_stream: bool) -> Result<TestCodex> {
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.wire_api = WireApi::Chat;
    provider.chat_stream = chat_stream;
    provider.supports_websockets = false;

    test_codex()
        .with_config(move |config| {
            config.model_provider = provider;
        })
        .build_with_auto_env(server)
        .await
}

/// Non-streaming Chat Completions JSON completion body.
fn chat_completion_json_body() -> Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "gpt-test",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "ok"
            },
            "finish_reason": "stop"
        }]
    })
}

/// Streaming Chat Completions SSE body with a real assistant text answer,
/// modeled on `chat_completions_reasoning_retry.rs`: valid
/// `chat.completion.chunk` `data:` records followed by `data: [DONE]`.
fn chat_completion_sse_body() -> Vec<u8> {
    let mut body = String::new();
    body.push_str("data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n");
    body.push_str("data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n");
    body.push_str("data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

/// Runs the three-request scenario (two turns in one session, one turn in a
/// second session) against a provider with the given `chat_stream` mode and
/// asserts the full prompt-cache wire contract on every recorded request.
async fn assert_prompt_cache_wire_contract(chat_stream: bool) -> Result<()> {
    let server = MockServer::start().await;
    let response = if chat_stream {
        ResponseTemplate::new(/*status*/ 200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(chat_completion_sse_body(), "text/event-stream")
    } else {
        ResponseTemplate::new(/*status*/ 200).set_body_json(chat_completion_json_body())
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .expect(/*requests*/ 3)
        .mount(&server)
        .await;

    let first_session = build_chat_session(&server, chat_stream).await?;
    first_session.submit_text_turn("first request").await?;
    first_session.submit_text_turn("second request").await?;

    let second_session = build_chat_session(&server, chat_stream).await?;
    second_session.submit_text_turn("different session").await?;

    let first_session_id = first_session.session_configured.session_id.to_string();
    let second_session_id = second_session.session_configured.session_id.to_string();
    assert_ne!(first_session_id, second_session_id);

    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);

    let session_ids = requests
        .iter()
        .map(|request| {
            request
                .headers
                .get(X_SESSION_ID_HEADER)
                .expect("chat completions request should include an `X-Session-Id` header")
                .to_str()
                .expect("`X-Session-Id` should be a valid header value")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        session_ids,
        vec![
            first_session_id.as_str(),
            first_session_id.as_str(),
            second_session_id.as_str(),
        ],
        "`X-Session-Id` must be stable within a session and unique across sessions"
    );

    let affinity_session_ids = requests
        .iter()
        .map(|request| {
            request
                .headers
                .get(X_SESSION_AFFINITY_HEADER)
                .expect("chat completions request should include an `x-session-affinity` header")
                .to_str()
                .expect("`x-session-affinity` should be a valid header value")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        affinity_session_ids,
        vec![
            first_session_id.as_str(),
            first_session_id.as_str(),
            second_session_id.as_str(),
        ],
        "`x-session-affinity` must be stable within a session and unique across sessions"
    );

    for (request, (header_session_id, affinity_session_id)) in requests
        .iter()
        .zip(session_ids.iter().zip(affinity_session_ids.iter()))
    {
        assert_eq!(
            header_session_id, affinity_session_id,
            "`X-Session-Id` and `x-session-affinity` must have the same session ID"
        );

        {
            let forbidden_header = "x-codex-session-id";
            assert!(
                !request.headers.contains_key(forbidden_header),
                "chat completions request should not include a `{forbidden_header}` header"
            );
        }

        let body: Value = serde_json::from_slice(&request.body)?;
        let body = body
            .as_object()
            .expect("chat completions request body should be an object");

        let prompt_cache_key = body
            .get("promptCacheKey")
            .and_then(Value::as_str)
            .expect("chat completions request body should include a string `promptCacheKey` field");
        assert_eq!(
            prompt_cache_key, *header_session_id,
            "body `promptCacheKey` must equal the request's `X-Session-Id` header"
        );

        for forbidden_key in [
            "prompt_cache_key",
            "session-id",
            "session_id",
            "client_metadata",
        ] {
            assert!(
                !body.contains_key(forbidden_key),
                "chat completions request body should not contain `{forbidden_key}`"
            );
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_prompt_cache_key_contract_non_streaming() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_prompt_cache_wire_contract(/*chat_stream*/ false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_prompt_cache_key_contract_streaming() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_prompt_cache_wire_contract(/*chat_stream*/ true).await
}
