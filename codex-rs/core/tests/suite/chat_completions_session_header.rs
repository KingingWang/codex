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

const X_CODEX_SESSION_ID_HEADER: &str = "x-codex-session-id";

async fn build_chat_session(server: &MockServer) -> Result<TestCodex> {
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.wire_api = WireApi::Chat;
    provider.chat_stream = false;
    provider.supports_websockets = false;

    test_codex()
        .with_config(move |config| {
            config.model_provider = provider;
        })
        .build_with_auto_env(server)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_session_header_is_stable_and_unique_without_changing_body() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(/*status*/ 200).set_body_json(serde_json::json!({
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
            })),
        )
        .expect(/*requests*/ 3)
        .mount(&server)
        .await;

    let first_session = build_chat_session(&server).await?;
    first_session.submit_text_turn("first request").await?;
    first_session.submit_text_turn("second request").await?;

    let second_session = build_chat_session(&server).await?;
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
                .get(X_CODEX_SESSION_ID_HEADER)
                .expect("chat completions request should include a Codex session ID")
                .to_str()
                .expect("Codex session ID should be a valid header value")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        session_ids,
        vec![
            first_session_id.as_str(),
            first_session_id.as_str(),
            second_session_id.as_str(),
        ]
    );

    for request in requests {
        let body: Value = serde_json::from_slice(&request.body)?;
        let body = body
            .as_object()
            .expect("chat completions request body should be an object");
        for key in [
            X_CODEX_SESSION_ID_HEADER,
            "session-id",
            "session_id",
            "client_metadata",
        ] {
            assert!(
                !body.contains_key(key),
                "chat completions request body should not contain {key}"
            );
        }
    }

    Ok(())
}
