use super::*;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

use bytes::Bytes;
use codex_client::Response;
use codex_client::RetryConfig;
use codex_client::StreamResponse;
use codex_client::TransportError;
use futures::StreamExt;
use futures::stream;
use http::StatusCode;
use serde_json::json;

#[derive(Clone, Default)]
struct CaptureTransport {
    requests: Arc<Mutex<Vec<Request>>>,
    first_error: Arc<Mutex<Option<u16>>>,
    responses: Arc<Mutex<VecDeque<serde_json::Value>>>,
}

fn success() -> serde_json::Value {
    json!({
        "responseId": "response-1",
        "candidates": [{"content": {"parts": [{"text": "hello"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 80,
            "candidatesTokenCount": 5, "thoughtsTokenCount": 2, "totalTokenCount": 107}
    })
}

impl CaptureTransport {
    fn reply(&self) -> serde_json::Value {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(success)
    }

    fn capture(&self, request: Request) -> Result<(), TransportError> {
        self.requests.lock().unwrap().push(request);
        if let Some(status) = self.first_error.lock().unwrap().take() {
            return Err(TransportError::Http {
                status: StatusCode::from_u16(status).unwrap(),
                url: None,
                headers: None,
                body: Some("try again".into()),
                retry_after: None,
            });
        }
        Ok(())
    }
}

impl HttpTransport for CaptureTransport {
    async fn execute(&self, request: Request) -> Result<Response, TransportError> {
        self.capture(request)?;
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("upstream-1"));
        Ok(Response {
            status: StatusCode::OK,
            headers,
            body: Bytes::from(self.reply().to_string()),
        })
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        self.capture(request)?;
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-request-id", HeaderValue::from_static("upstream-1"));
        let data = format!("data: {}\n\n", self.reply());
        // Split inside JSON to exercise real SSE byte framing.
        let chunks = vec![
            Ok(Bytes::copy_from_slice(&data.as_bytes()[..17])),
            Ok(Bytes::copy_from_slice(&data.as_bytes()[17..])),
        ];
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers,
            bytes: Box::pin(stream::iter(chunks)),
        })
    }
}

struct BearerAuth;
impl AuthProvider for BearerAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer resolved-key"),
        );
        Ok(())
    }
}

fn provider() -> Provider {
    Provider {
        name: "gemini".into(),
        base_url: "https://example.test/v1beta".into(),
        query_params: Some(HashMap::from([("gateway".into(), "enabled".into())])),
        headers: HeaderMap::new(),
        stream_idle_timeout: Duration::from_secs(1),
        retry: RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
        },
    }
}

fn request() -> GeminiRequest {
    GeminiRequest {
        model: "models/gemini-test".into(),
        contents: vec![crate::GeminiContent {
            role: Some("user".into()),
            parts: vec![crate::GeminiPart::text("hello".into())],
        }],
        system_instruction: None,
        tools: Vec::new(),
        generation_config: None,
        tool_name_map: HashMap::new(),
    }
}

#[tokio::test]
async fn gemini_http_contract_auth_headers_queries_usage_and_retries() {
    for streaming in [false, true] {
        for status in [429, 500, 503] {
            let transport = CaptureTransport::default();
            *transport.first_error.lock().unwrap() = Some(status);
            let client = GeminiClient::new(
                transport.clone(),
                provider(),
                Arc::new(BearerAuth),
                streaming,
            );
            let headers = HeaderMap::from_iter([
                (
                    http::HeaderName::from_static("x-session-id"),
                    HeaderValue::from_static("session-1"),
                ),
                (
                    http::HeaderName::from_static("x-codex-window-id"),
                    HeaderValue::from_static("session-1:0"),
                ),
            ]);
            let mut response = client.request(request(), headers).await.unwrap();
            assert_eq!(response.upstream_request_id.as_deref(), Some("upstream-1"));
            let mut completed = false;
            while let Some(event) = response.next().await {
                if let crate::ResponseEvent::Completed {
                    token_usage: Some(usage),
                    end_turn,
                    ..
                } = event.unwrap()
                {
                    assert_eq!(usage.input_tokens, 100);
                    assert_eq!(usage.cached_input_tokens, 80);
                    assert_eq!(usage.output_tokens, 7);
                    assert_eq!(usage.reasoning_output_tokens, 2);
                    assert_eq!(usage.total_tokens, 107);
                    assert_eq!(end_turn, Some(true));
                    completed = true;
                }
            }
            assert!(completed);
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_eq!(request.headers["x-goog-api-key"], "resolved-key");
                assert!(request.headers["x-goog-api-key"].is_sensitive());
                assert!(!request.headers.contains_key(AUTHORIZATION));
                assert_eq!(request.headers["x-session-id"], "session-1");
                assert_eq!(request.headers["x-codex-window-id"], "session-1:0");
                let url = url::Url::parse(&request.url).unwrap();
                assert_eq!(
                    url.path(),
                    if streaming {
                        "/v1beta/models/gemini-test:streamGenerateContent"
                    } else {
                        "/v1beta/models/gemini-test:generateContent"
                    }
                );
                let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(query["gateway"], "enabled");
                assert_eq!(
                    query.get("alt").map(String::as_str),
                    streaming.then_some("sse")
                );
            }
        }
    }
}

#[tokio::test]
async fn explicit_google_api_key_and_oauth_headers_are_preserved() {
    for bearer in [false, true] {
        let transport = CaptureTransport::default();
        let mut provider = provider();
        if bearer {
            provider
                .headers
                .insert(AUTHORIZATION, HeaderValue::from_static("Bearer configured"));
        } else {
            provider
                .headers
                .insert("x-goog-api-key", HeaderValue::from_static("configured-key"));
        }
        let client = GeminiClient::new(transport.clone(), provider, Arc::new(BearerAuth), false);
        client.request(request(), HeaderMap::new()).await.unwrap();
        let requests = transport.requests.lock().unwrap();
        if bearer {
            assert_eq!(requests[0].headers[AUTHORIZATION], "Bearer resolved-key");
            assert!(!requests[0].headers.contains_key("x-goog-api-key"));
        } else {
            assert_eq!(requests[0].headers["x-goog-api-key"], "configured-key");
            assert!(!requests[0].headers.contains_key(AUTHORIZATION));
        }
    }
}

#[test]
fn paths_reject_unsafe_model_identifiers() {
    for model in ["", "../model", "a?key=secret", "a#fragment", ".."] {
        assert!(GeminiClient::<CaptureTransport>::path(model, false).is_err());
    }
    assert_eq!(
        GeminiClient::<CaptureTransport>::path("models/gemini-test", true).unwrap(),
        "models/gemini-test:streamGenerateContent"
    );
}

#[test]
fn gemini_keeps_non_bearer_gateway_authorization() {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Basic gateway-credential"),
    );
    rewrite_headers(&mut headers, false);
    assert_eq!(headers[AUTHORIZATION], "Basic gateway-credential");
    assert!(!headers.contains_key("x-goog-api-key"));
}

#[test]
fn gemini_explicit_bearer_mode_preserves_dual_gateway_auth_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer gateway-token"),
    );
    headers.insert("x-goog-api-key", HeaderValue::from_static("google-key"));
    rewrite_headers(&mut headers, true);
    assert_eq!(headers[AUTHORIZATION], "Bearer gateway-token");
    assert_eq!(headers["x-goog-api-key"], "google-key");
    assert!(headers["x-goog-api-key"].is_sensitive());
}

#[tokio::test]
async fn gemini_continues_from_checkpoints_without_replaying_output_or_double_counting_input() {
    for streaming in [false, true] {
        for reason in [Some("CONTINUATION"), None] {
            let transport = CaptureTransport::default();
            transport.responses.lock().unwrap().push_back(json!({
                "candidates": [{"content": {"parts": [{"text": "first "} ]},
                    "finishReason": reason, "continuationToken": "checkpoint-1"}],
                "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 80,
                    "candidatesTokenCount": 3, "thoughtsTokenCount": 2}
            }));
            let client = GeminiClient::new(
                transport.clone(),
                provider(),
                Arc::new(BearerAuth),
                streaming,
            );
            let mut events = client.request(request(), HeaderMap::new()).await.unwrap();
            let mut text = String::new();
            let mut completed = 0;
            while let Some(event) = events.next().await {
                match event.unwrap() {
                    crate::ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
                    crate::ResponseEvent::Completed {
                        token_usage: Some(usage),
                        ..
                    } => {
                        assert_eq!(usage.input_tokens, 100);
                        assert_eq!(usage.cached_input_tokens, 80);
                        assert_eq!(usage.output_tokens, 12);
                        assert_eq!(usage.total_tokens, 112);
                        completed += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(text, "first hello");
            assert_eq!(completed, 1);
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let first: serde_json::Value =
                serde_json::from_slice(&requests[0].prepare_body_for_send().unwrap().body_bytes())
                    .unwrap();
            let next: serde_json::Value =
                serde_json::from_slice(&requests[1].prepare_body_for_send().unwrap().body_bytes())
                    .unwrap();
            assert_eq!(next["continuationToken"], "checkpoint-1");
            assert_eq!(first["contents"], next["contents"]);
            assert_eq!(requests[0].headers, requests[1].headers);
        }
    }
}
