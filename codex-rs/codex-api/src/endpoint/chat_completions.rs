//! Client for the OpenAI Chat Completions API (`/v1/chat/completions`).
//! Supports both non-streaming and SSE streaming modes, configurable via
//! the `chat_stream` field in `ModelProviderInfo`.
use std::time::Duration;

use crate::auth::SharedAuthProvider;
use crate::common::ChatCompletionsRequest;
use crate::common::ChatCompletionsResponse;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::sse::chat_completions::spawn_chat_completions_stream;
use codex_client::HttpTransport;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use http::HeaderMap;
use http::Method;
use tokio::sync::mpsc;
use tracing::instrument;

/// Client for the Chat Completions API.
///
/// When `chat_stream` is `false` (default), sends a non-streaming request and
/// converts the full response into synthetic events. When `chat_stream` is
/// `true`, sends a streaming request and processes SSE events in real time.
pub struct ChatCompletionsClient<T: HttpTransport> {
    session: EndpointSession<T>,
    chat_stream: bool,
}

impl<T: HttpTransport> ChatCompletionsClient<T> {
    /// Creates a new ChatCompletionsClient.
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

    /// The API path for chat completions.
    fn path() -> &'static str {
        "chat/completions"
    }

    /// Sends a chat completions request and returns a ResponseStream.
    ///
    /// If `chat_stream` is `true`, the request is sent with `stream: true`
    /// and SSE events are processed in real time. Otherwise, a non-streaming
    /// request is sent and the full response is converted into synthetic events.
    #[instrument(
        name = "chat_completions.request",
        level = "info",
        skip_all,
        fields(
            transport = "chat_completions_http",
            http.method = "POST",
            api.path = "chat/completions",
            chat_stream = self.chat_stream
        )
    )]
    pub async fn request(
        &self,
        req: ChatCompletionsRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        if self.chat_stream {
            self.request_streaming(req, extra_headers).await
        } else {
            self.request_non_streaming(req, extra_headers).await
        }
    }

    /// Sends a streaming chat completions request and processes SSE events.
    async fn request_streaming(
        &self,
        mut req: ChatCompletionsRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        req.stream = true;

        let body = serde_json::to_value(&req).map_err(|e| {
            ApiError::Stream(format!("failed to encode chat completions request: {e}"))
        })?;

        let stream_response = self
            .session
            .stream_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.timeout = Some(Duration::from_secs(600));
                },
            )
            .await?;

        let namespace_map = req.tool_namespace_map;
        let idle_timeout = self.session.provider().stream_idle_timeout;
        Ok(spawn_chat_completions_stream(
            stream_response,
            idle_timeout,
            None,
            namespace_map,
        ))
    }

    /// Sends a non-streaming chat completions request and converts the full
    /// response into synthetic events.
    async fn request_non_streaming(
        &self,
        mut req: ChatCompletionsRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        req.stream = false;

        let body = serde_json::to_value(&req).map_err(|e| {
            ApiError::Stream(format!("failed to encode chat completions request: {e}"))
        })?;

        let response = self
            .session
            .execute_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.timeout = Some(Duration::from_secs(600));
                },
            )
            .await?;

        let raw_body = String::from_utf8_lossy(&response.body).to_string();
        tracing::info!(
            status = %response.status,
            response_body = %raw_body,
            "Non-streaming chat completions response received"
        );

        let mut value: serde_json::Value = serde_json::from_str(&raw_body).map_err(|e| {
            ApiError::Stream(format!(
                "failed to parse chat completions response body as JSON: {e}, body: {raw_body}"
            ))
        })?;

        // Some API providers omit the `model` field in responses. Inject it
        // from the request so deserialization into ChatCompletionsResponse
        // does not fail with "missing field `model`".
        if value.get("model").is_none_or(serde_json::Value::is_null) {
            value["model"] = serde_json::Value::String(req.model.clone());
        }

        let parsed: ChatCompletionsResponse = serde_json::from_value(value).map_err(|e| {
            ApiError::Stream(format!(
                "failed to parse chat completions response: {e}, body: {raw_body}"
            ))
        })?;

        if parsed.choices.is_empty() {
            return Err(ApiError::Retryable {
                message: "chat completions response has no choices".to_string(),
                delay: None,
            });
        }

        let namespace_map = req.tool_namespace_map;
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

/// Converts a non-streaming ChatCompletionsResponse into ResponseEvents sent
/// through the channel, mimicking the streaming protocol.
async fn convert_response_to_events(
    response: ChatCompletionsResponse,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    namespace_map: &std::collections::HashMap<String, String>,
) {
    let response_id = response.id.clone();
    // Emit Created event first, consistent with the Responses API SSE stream
    // which sends Created before any output items.
    if tx.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    // Track whether any output was emitted during conversion.
    // When the API returns HTTP 200 with empty content, the stream
    // completes with no real output, which should trigger a retry.
    let mut output_emitted = false;
    let mut last_finish_reason: Option<String> = None;

    let token_usage = response.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        cached_input_tokens: 0,
        output_tokens: u.completion_tokens,
        reasoning_output_tokens: 0,
        total_tokens: u.total_tokens,
    });

    for choice in &response.choices {
        let message = &choice.message;

        // Handle reasoning content if present
        if let Some(reasoning) = &message.reasoning {
            let reasoning_text = extract_reasoning_text(reasoning);
            if !reasoning_text.is_empty() {
                // Emit OutputItemAdded with empty content to establish the active
                // item, mirroring the streaming path. The turn processor needs an
                // active_item before it can handle delta events.
                let reasoning_added = ResponseItem::Reasoning {
                    id: format!("reasoning_{}", choice.index),
                    summary: Vec::new(),
                    content: Some(vec![
                        codex_protocol::models::ReasoningItemContent::ReasoningText {
                            text: String::new(),
                        },
                    ]),
                    encrypted_content: None,
                };
                output_emitted = true;
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(reasoning_added)))
                    .await
                    .is_err()
                {
                    return;
                }
                // Emit reasoning content delta with the full text
                if tx
                    .send(Ok(ResponseEvent::ReasoningContentDelta {
                        delta: reasoning_text.clone(),
                        content_index: choice.index,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                // Emit OutputItemDone with the full content
                let reasoning_done = ResponseItem::Reasoning {
                    id: format!("reasoning_{}", choice.index),
                    summary: Vec::new(),
                    content: Some(vec![
                        codex_protocol::models::ReasoningItemContent::ReasoningText {
                            text: reasoning_text,
                        },
                    ]),
                    encrypted_content: None,
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(reasoning_done)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        // Handle tool calls if present
        if let Some(tool_calls) = &message.tool_calls {
            for (i, tc) in tool_calls.iter().enumerate() {
                // Emit OutputItemAdded with empty arguments to establish the
                // active item. The turn processor needs an active_item before
                // it can handle delta events.
                let function_call_added = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(&tc.function.name).cloned(),
                    name: tc.function.name.clone(),
                    arguments: String::new(),
                    call_id: tc.id.clone(),
                };
                output_emitted = true;
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(function_call_added)))
                    .await
                    .is_err()
                {
                    return;
                }

                // Emit tool call input delta with the full arguments
                if tx
                    .send(Ok(ResponseEvent::ToolCallInputDelta {
                        item_id: format!("call_{i}"),
                        call_id: Some(tc.id.clone()),
                        delta: tc.function.arguments.clone(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }

                // Emit OutputItemDone with the full arguments
                let function_call_done_item = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(&tc.function.name).cloned(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                    call_id: tc.id.clone(),
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(function_call_done_item)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        // Handle text content if present
        if let Some(content) = &message.content
            && !content.is_empty()
        {
            // Emit OutputItemAdded with empty text to establish the active
            // item, mirroring the streaming path. The turn processor requires
            // an OutputItemAdded before it can handle OutputTextDelta events.
            let assistant_added = ResponseItem::Message {
                id: None,
                role: message.role.clone(),
                content: vec![ContentItem::OutputText {
                    text: String::new(),
                }],
                phase: None,
            };
            output_emitted = true;
            if tx
                .send(Ok(ResponseEvent::OutputItemAdded(assistant_added)))
                .await
                .is_err()
            {
                return;
            }

            // Emit text delta with the full content
            if tx
                .send(Ok(ResponseEvent::OutputTextDelta(content.clone())))
                .await
                .is_err()
            {
                return;
            }

            // Emit OutputItemDone with the full text
            let assistant_done = ResponseItem::Message {
                id: None,
                role: message.role.clone(),
                content: vec![ContentItem::OutputText {
                    text: content.clone(),
                }],
                phase: None,
            };
            if tx
                .send(Ok(ResponseEvent::OutputItemDone(assistant_done)))
                .await
                .is_err()
            {
                return;
            }
        }

        if let Some(reason) = &choice.finish_reason {
            last_finish_reason = Some(reason.clone());
        }
    }

    // If no output was emitted (empty response from the API),
    // treat as a transient error so the turn layer retries.
    if !output_emitted {
        let _ = tx
            .send(Err(ApiError::Retryable {
                message: "chat completions response with no output content".to_string(),
                delay: None,
            }))
            .await;
        return;
    }

    // Emit completion event
    let end_turn = match last_finish_reason.as_deref() {
        Some("stop") | Some("length") => Some(true),
        Some("tool_calls") => Some(false),
        _ => None,
    };
    let _ = tx
        .send(Ok(ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        }))
        .await;
}

/// Extracts reasoning text from a JSON value.
/// The reasoning field can be either a string or an object with a "text" field.
fn extract_reasoning_text(reasoning: &serde_json::Value) -> String {
    match reasoning {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ChatCompletionResponseChoice;
    use crate::common::ChatCompletionResponseMessage;
    use crate::common::ChatCompletionUsage;
    use crate::common::ChatCompletionsResponse;

    async fn collect_events(
        response: ChatCompletionsResponse,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        let namespace_map = std::collections::HashMap::new();
        let handle = tokio::spawn(async move {
            convert_response_to_events(response, tx, &namespace_map).await;
        });
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        handle.await.unwrap();
        events
    }

    fn empty_choices_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-1".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: Some("test-model".to_string()),
            choices: vec![],
            usage: None,
        }
    }

    fn null_content_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-2".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        }
    }

    fn empty_string_content_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-3".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        }
    }

    fn normal_content_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-4".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello from test!".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(ChatCompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        }
    }

    #[tokio::test]
    async fn empty_choices_yields_retryable() {
        let events = collect_events(empty_choices_response()).await;
        assert!(
            events.len() >= 2,
            "expected at least 2 events, got {events:?}"
        );
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Err(ApiError::Retryable { .. })));
    }

    #[tokio::test]
    async fn null_content_yields_retryable() {
        let events = collect_events(null_content_response()).await;
        assert!(
            events.len() >= 2,
            "expected at least 2 events, got {events:?}"
        );
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Err(ApiError::Retryable { .. })));
    }

    #[tokio::test]
    async fn empty_string_content_yields_retryable() {
        let events = collect_events(empty_string_content_response()).await;
        assert!(
            events.len() >= 2,
            "expected at least 2 events, got {events:?}"
        );
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Err(ApiError::Retryable { .. })));
    }

    #[tokio::test]
    async fn normal_content_succeeds() {
        let events = collect_events(normal_content_response()).await;
        assert!(
            events.len() >= 5,
            "expected at least 5 events, got {events:?}"
        );
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(&events[1], Ok(ResponseEvent::OutputItemAdded(_))));
        assert!(matches!(&events[2], Ok(ResponseEvent::OutputTextDelta(_))));
        assert!(matches!(&events[3], Ok(ResponseEvent::OutputItemDone(_))));
        let last = events.last().unwrap();
        assert!(
            matches!(last, Ok(ResponseEvent::Completed { .. })),
            "last event should be Completed, got {last:?}"
        );
    }

    #[tokio::test]
    async fn normal_content_has_no_error() {
        let events = collect_events(normal_content_response()).await;
        assert!(
            events.iter().all(std::result::Result::is_ok),
            "expected all Ok events, got {events:?}"
        );
    }
}
