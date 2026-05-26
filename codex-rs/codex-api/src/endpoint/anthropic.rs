//! Client for the Anthropic Messages API (`/v1/messages`).
//!
//! Mirrors `chat_completions.rs` — supports both non-streaming and SSE
//! streaming paths, gated by the same `chat_stream` provider flag (we treat it
//! as a generic "wire-level streaming preference").
//!
//! Auth differences vs OpenAI:
//! - Anthropic expects `x-api-key: <key>` instead of `Authorization: Bearer`.
//! - Anthropic requires the `anthropic-version: 2023-06-01` header.
//!
//! The codex `AuthProvider` chain hardcodes `Authorization: Bearer`, so this
//! client reshapes the headers in-place: it strips the `Authorization` header
//! and rewrites it to `x-api-key`, then injects `anthropic-version` if the
//! caller has not already supplied one. This keeps the existing auth.json /
//! command-auth flow working unchanged.

use std::time::Duration;

use crate::anthropic_types::AnthropicContentBlock;
use crate::anthropic_types::AnthropicRequest;
use crate::anthropic_types::AnthropicResponse;
use crate::anthropic_types::stop_reason_to_end_turn;
use crate::auth::SharedAuthProvider;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::sse::anthropic::spawn_anthropic_stream;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use http::header::AUTHORIZATION;
use tokio::sync::mpsc;
use tracing::instrument;

const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
const ANTHROPIC_VERSION_VALUE: &str = "2023-06-01";
const ANTHROPIC_API_KEY_HEADER: &str = "x-api-key";

/// Apply Anthropic-specific header rewrites to the outgoing request.
///
/// - Move `Authorization: Bearer <token>` into `x-api-key: <token>` (only
///   when the caller has not already supplied an `x-api-key`).
/// - Inject `anthropic-version: 2023-06-01` if missing.
///
/// Note: we do NOT auto-inject any `anthropic-beta` header. Prompt caching is
/// now GA on the direct Anthropic API and on Vertex AI, and Vertex actively
/// rejects the legacy `prompt-caching-2024-07-31` value. Gateways that still
/// require a beta flag (e.g. Bedrock) should set it explicitly via the
/// provider's `headers` config so other backends are not broken.
fn rewrite_anthropic_headers(req: &mut Request) {
    let api_key_header = HeaderName::from_static(ANTHROPIC_API_KEY_HEADER);
    let version_header = HeaderName::from_static(ANTHROPIC_VERSION_HEADER);

    if !req.headers.contains_key(&api_key_header)
        && let Some(auth_value) = req.headers.remove(AUTHORIZATION)
    {
        let raw = auth_value.to_str().unwrap_or("").trim();
        let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
        if !token.is_empty()
            && let Ok(value) = HeaderValue::from_str(token)
        {
            req.headers.insert(api_key_header, value);
        }
    }

    if !req.headers.contains_key(&version_header) {
        req.headers.insert(
            version_header,
            HeaderValue::from_static(ANTHROPIC_VERSION_VALUE),
        );
    }
}

/// Diagnostic: dump the outgoing JSON body to disk when
/// `CODEX_DEBUG_ANTHROPIC_DUMP_DIR` is set. Each call writes a new file
/// `anthropic-<kind>-<unix_nanos>.json` so consecutive turns can be diffed
/// to find prefix drift that breaks prompt-cache hits.
fn debug_dump_request(body: &serde_json::Value, kind: &str) {
    let Ok(dir) = std::env::var("CODEX_DEBUG_ANTHROPIC_DUMP_DIR") else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::path::Path::new(&dir).join(format!("anthropic-{kind}-{now}.json"));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(?err, ?dir, "failed to create anthropic dump dir");
        return;
    }
    match serde_json::to_vec_pretty(body) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(&path, &bytes) {
                tracing::warn!(?err, ?path, "failed to write anthropic dump");
            }
        }
        Err(err) => tracing::warn!(?err, "failed to serialize anthropic dump"),
    }
}

/// Client for the Anthropic Messages API.
pub struct AnthropicClient<T: HttpTransport> {
    session: EndpointSession<T>,
    chat_stream: bool,
}

impl<T: HttpTransport> AnthropicClient<T> {
    pub fn new(
        transport: T,
        provider: crate::provider::Provider,
        auth: SharedAuthProvider,
        chat_stream: bool,
    ) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            chat_stream,
        }
    }

    fn path() -> &'static str {
        "v1/messages"
    }

    #[instrument(
        name = "anthropic.request",
        level = "info",
        skip_all,
        fields(
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "v1/messages",
            chat_stream = self.chat_stream
        )
    )]
    pub async fn request(
        &self,
        req: AnthropicRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        if self.chat_stream {
            self.request_streaming(req, extra_headers).await
        } else {
            self.request_non_streaming(req, extra_headers).await
        }
    }

    async fn request_streaming(
        &self,
        mut req: AnthropicRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        req.stream = true;

        // Extract before serialization — `tool_namespace_map` is `#[serde(skip)]`
        // so it does not affect the wire body, but moving it out keeps the
        // ownership story explicit (and resilient if `serde_json::to_value` is
        // ever swapped for a by-value form).
        let namespace_map = std::mem::take(&mut req.tool_namespace_map);

        let body = serde_json::to_value(&req)
            .map_err(|e| ApiError::Stream(format!("failed to encode anthropic request: {e}")))?;

        debug_dump_request(&body, "stream");

        let stream_response = self
            .session
            .stream_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    rewrite_anthropic_headers(req);
                    req.timeout = Some(Duration::from_secs(600));
                },
            )
            .await?;

        let idle_timeout = self.session.provider().stream_idle_timeout;
        Ok(spawn_anthropic_stream(
            stream_response,
            idle_timeout,
            None,
            namespace_map,
        ))
    }

    async fn request_non_streaming(
        &self,
        mut req: AnthropicRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        req.stream = false;

        let namespace_map = std::mem::take(&mut req.tool_namespace_map);

        let body = serde_json::to_value(&req)
            .map_err(|e| ApiError::Stream(format!("failed to encode anthropic request: {e}")))?;

        debug_dump_request(&body, "nonstream");

        let response = self
            .session
            .execute_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    rewrite_anthropic_headers(req);
                    req.timeout = Some(Duration::from_secs(600));
                },
            )
            .await?;

        let raw_body = String::from_utf8_lossy(&response.body).to_string();
        tracing::debug!(
            status = %response.status,
            response_bytes = raw_body.len(),
            "Non-streaming anthropic response received"
        );

        let parsed: AnthropicResponse = serde_json::from_str(&raw_body).map_err(|e| {
            ApiError::Stream(format!(
                "failed to parse anthropic response ({} bytes): {e}",
                raw_body.len()
            ))
        })?;

        let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(async move {
            convert_response_to_events(parsed, tx_event, &namespace_map).await;
        });

        Ok(ResponseStream {
            rx_event,
            upstream_request_id: None,
        })
    }
}

/// Convert a non-streaming Anthropic response into the canonical synthetic
/// event stream consumed by codex-core. Mirrors the chat-completions converter
/// in shape so downstream code can stay format-agnostic.
async fn convert_response_to_events(
    response: AnthropicResponse,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    namespace_map: &std::collections::HashMap<String, String>,
) {
    let response_id = response.id.clone().unwrap_or_default();
    if tx.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    let mut output_emitted = false;

    let token_usage = response.usage.as_ref().map(|u| {
        let cache_read = u.cache_read();
        let cache_creation = u.cache_creation();
        // Anthropic reports cached + uncached input separately. Total input is
        // the sum of all three counters.
        let input_tokens = u.input_tokens + cache_read + cache_creation;
        let output_tokens = u.output_tokens;
        TokenUsage {
            input_tokens,
            cached_input_tokens: cache_read,
            output_tokens,
            reasoning_output_tokens: 0,
            total_tokens: input_tokens + output_tokens,
        }
    });

    for (idx, block) in response.content.iter().enumerate() {
        match block {
            AnthropicContentBlock::Thinking {
                thinking, signature, ..
            } => {
                if thinking.is_empty() {
                    continue;
                }
                let added = ResponseItem::Reasoning {
                    id: format!("reasoning_{idx}"),
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: String::new(),
                    }]),
                    encrypted_content: None,
                };
                output_emitted = true;
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(added)))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::ReasoningContentDelta {
                        delta: thinking.clone(),
                        content_index: idx as i64,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                let done = ResponseItem::Reasoning {
                    id: format!("reasoning_{idx}"),
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: thinking.clone(),
                    }]),
                    // Persist Anthropic's signature so build_messages can echo
                    // it on the next turn. Vertex AI rejects unsigned thinking
                    // blocks when replayed, so dropping the signature here
                    // would force build_messages to elide thinking content
                    // from history.
                    encrypted_content: signature.clone(),
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(done)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            AnthropicContentBlock::Text { text, .. } => {
                if text.is_empty() {
                    continue;
                }
                let added = ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: String::new(),
                    }],
                    phase: None,
                };
                output_emitted = true;
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(added)))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::OutputTextDelta(text.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
                let done = ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText { text: text.clone() }],
                    phase: None,
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(done)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            AnthropicContentBlock::ToolUse {
                id, name, input, ..
            } => {
                let arguments = match input {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let added = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(name).cloned(),
                    name: name.clone(),
                    arguments: String::new(),
                    call_id: id.clone(),
                };
                output_emitted = true;
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(added)))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::ToolCallInputDelta {
                        item_id: format!("call_{idx}"),
                        call_id: Some(id.clone()),
                        delta: arguments.clone(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                let done = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(name).cloned(),
                    name: name.clone(),
                    arguments,
                    call_id: id.clone(),
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(done)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Tool results, image blocks, and unknown blocks are not expected
            // in an assistant response — skip them.
            _ => {}
        }
    }

    if !output_emitted {
        let _ = tx
            .send(Err(ApiError::Retryable {
                message: "anthropic response with no output content".to_string(),
                delay: None,
            }))
            .await;
        return;
    }

    let end_turn = stop_reason_to_end_turn(response.stop_reason.as_deref());
    let _ = tx
        .send(Ok(ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic_types::AnthropicUsage;
    use crate::provider::Provider;
    use crate::provider::RetryConfig;

    fn test_provider() -> Provider {
        Provider {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            query_params: None,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(0),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(60),
        }
    }

    async fn drain(
        mut rx: mpsc::Receiver<Result<ResponseEvent, ApiError>>,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn empty_content_yields_retryable() {
        let response = AnthropicResponse {
            id: Some("msg-1".to_string()),
            r#type: Some("message".to_string()),
            role: Some("assistant".to_string()),
            content: Vec::new(),
            model: Some("claude-3-7".to_string()),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: None,
        };
        let (tx, rx) = mpsc::channel(16);
        let namespace = std::collections::HashMap::new();
        let drain_handle = tokio::spawn(drain(rx));
        convert_response_to_events(response, tx, &namespace).await;
        let events = drain_handle.await.unwrap();
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Err(ApiError::Retryable { .. })));
    }

    #[tokio::test]
    async fn text_content_emits_message_events_and_rolls_up_cache_tokens() {
        let response = AnthropicResponse {
            id: Some("msg-2".to_string()),
            r#type: Some("message".to_string()),
            role: Some("assistant".to_string()),
            content: vec![AnthropicContentBlock::Text {
                text: "hello".to_string(),
                cache_control: None,
            }],
            model: Some("claude-3-7".to_string()),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Some(AnthropicUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_input_tokens: Some(100),
                cache_creation_input_tokens: None,
            }),
        };
        let (tx, rx) = mpsc::channel(16);
        let namespace = std::collections::HashMap::new();
        let drain_handle = tokio::spawn(drain(rx));
        convert_response_to_events(response, tx, &namespace).await;
        let events = drain_handle.await.unwrap();

        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Ok(ResponseEvent::OutputItemAdded(_))));
        assert!(matches!(&events[2], Ok(ResponseEvent::OutputTextDelta(_))));
        assert!(matches!(&events[3], Ok(ResponseEvent::OutputItemDone(_))));
        match events.last().unwrap() {
            Ok(ResponseEvent::Completed {
                token_usage: Some(usage),
                end_turn,
                ..
            }) => {
                assert_eq!(usage.input_tokens, 105);
                assert_eq!(usage.cached_input_tokens, 100);
                assert_eq!(usage.output_tokens, 3);
                assert_eq!(usage.total_tokens, 108);
                assert_eq!(*end_turn, Some(true));
            }
            other => panic!("unexpected last event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_use_emits_function_call_events() {
        let response = AnthropicResponse {
            id: Some("msg-3".to_string()),
            r#type: Some("message".to_string()),
            role: Some("assistant".to_string()),
            content: vec![AnthropicContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"cmd": "ls"}),
                cache_control: None,
            }],
            model: Some("claude-3-7".to_string()),
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: None,
        };
        let (tx, rx) = mpsc::channel(16);
        let namespace = std::collections::HashMap::new();
        let drain_handle = tokio::spawn(drain(rx));
        convert_response_to_events(response, tx, &namespace).await;
        let events = drain_handle.await.unwrap();

        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        match &events[1] {
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                name,
                call_id,
                ..
            })) => {
                assert_eq!(name, "shell");
                assert_eq!(call_id, "toolu_1");
            }
            other => panic!("expected FunctionCall added, got {other:?}"),
        }
        match &events[2] {
            Ok(ResponseEvent::ToolCallInputDelta {
                delta,
                call_id: Some(call_id),
                ..
            }) => {
                assert!(delta.contains("\"cmd\""));
                assert_eq!(call_id, "toolu_1");
            }
            other => panic!("expected ToolCallInputDelta, got {other:?}"),
        }
        match events.last().unwrap() {
            Ok(ResponseEvent::Completed { end_turn, .. }) => {
                assert_eq!(*end_turn, Some(false));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_headers_moves_bearer_to_x_api_key() {
        let provider = test_provider();
        let mut req = provider.build_request(Method::POST, "v1/messages");
        req.headers
            .insert(AUTHORIZATION, HeaderValue::from_static("Bearer test-key"));

        rewrite_anthropic_headers(&mut req);

        assert!(!req.headers.contains_key(AUTHORIZATION));
        assert_eq!(
            req.headers
                .get(HeaderName::from_static(ANTHROPIC_API_KEY_HEADER))
                .unwrap()
                .to_str()
                .unwrap(),
            "test-key"
        );
        assert_eq!(
            req.headers
                .get(HeaderName::from_static(ANTHROPIC_VERSION_HEADER))
                .unwrap()
                .to_str()
                .unwrap(),
            ANTHROPIC_VERSION_VALUE
        );
    }

    #[test]
    fn rewrite_headers_preserves_existing_x_api_key() {
        let provider = test_provider();
        let mut req = provider.build_request(Method::POST, "v1/messages");
        req.headers.insert(
            HeaderName::from_static(ANTHROPIC_API_KEY_HEADER),
            HeaderValue::from_static("preset-key"),
        );
        req.headers
            .insert(AUTHORIZATION, HeaderValue::from_static("Bearer ignored"));

        rewrite_anthropic_headers(&mut req);

        assert_eq!(
            req.headers
                .get(HeaderName::from_static(ANTHROPIC_API_KEY_HEADER))
                .unwrap()
                .to_str()
                .unwrap(),
            "preset-key"
        );
    }

    #[test]
    fn rewrite_headers_preserves_user_supplied_version() {
        let provider = test_provider();
        let mut req = provider.build_request(Method::POST, "v1/messages");
        req.headers.insert(
            HeaderName::from_static(ANTHROPIC_VERSION_HEADER),
            HeaderValue::from_static("2024-10-22"),
        );

        rewrite_anthropic_headers(&mut req);

        assert_eq!(
            req.headers
                .get(HeaderName::from_static(ANTHROPIC_VERSION_HEADER))
                .unwrap()
                .to_str()
                .unwrap(),
            "2024-10-22"
        );
    }

    #[test]
    fn rewrite_headers_does_not_inject_beta() {
        let provider = test_provider();
        let mut req = provider.build_request(Method::POST, "v1/messages");

        rewrite_anthropic_headers(&mut req);

        // We must NOT inject any anthropic-beta header automatically — Vertex
        // rejects the legacy prompt-caching value, and prompt caching is GA
        // anyway. Gateways that need a beta flag should set it via provider
        // headers.
        assert!(
            !req.headers
                .contains_key(HeaderName::from_static("anthropic-beta"))
        );
    }

    #[test]
    fn rewrite_headers_preserves_user_supplied_beta() {
        let provider = test_provider();
        let mut req = provider.build_request(Method::POST, "v1/messages");
        req.headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("computer-use-2024-10-22"),
        );

        rewrite_anthropic_headers(&mut req);

        assert_eq!(
            req.headers
                .get(HeaderName::from_static("anthropic-beta"))
                .unwrap()
                .to_str()
                .unwrap(),
            "computer-use-2024-10-22"
        );
    }
}
