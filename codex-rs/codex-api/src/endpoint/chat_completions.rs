//! Client for the OpenAI Chat Completions API (`/v1/chat/completions`).
//! Supports both non-streaming and SSE streaming modes, configurable via
//! the `chat_stream` field in `ModelProviderInfo`.
use std::time::Duration;

use crate::auth::SharedAuthProvider;
use crate::common::ChatCompletionsRequest;
use crate::common::ChatCompletionsResponse;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::normalize_chat_completion_tool_arguments;
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

        let encoded_body = codex_client::EncodedJsonBody::encode(&body).map_err(|e| {
            ApiError::Stream(format!("failed to encode chat completions request: {e}"))
        })?;

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(encoded_body),
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

    // Track whether any deliverable output was emitted during conversion
    // (assistant text or tool calls). Reasoning/thinking content is NOT
    // a deliverable: a response that only produced reasoning is treated as
    // empty so the turn layer retries the request.
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

        // Handle reasoning content if present.
        // Non-streaming responses already carry the full reasoning text in the
        // completed item. Do not synthesize an OutputItemAdded +
        // ReasoningContentDelta too; clients (e.g. Zed via codex-acp) that
        // render both the delta and the completed item would display the same
        // reasoning twice. The full text goes into `summary` (the Responses
        // API summary path that codex-acp's seen_reasoning_deltas dedup is
        // designed for; with no delta stream here the flag stays false and the
        // single AgentReasoning is rendered once). `content` is left None so
        // no second complete-reasoning event (AgentReasoningRawContent, which
        // some codex-acp versions do not dedup) is emitted.
        // Assistant text below is the opposite case: codex-acp renders it only
        // from deltas, so a delta IS emitted there (see the text branch).
        if let Some(reasoning) = &message.reasoning {
            let reasoning_text = extract_reasoning_text(reasoning);
            if !reasoning_text.is_empty() {
                let reasoning_done = ResponseItem::Reasoning {
                    id: Some(format!("reasoning_{}", choice.index)),
                    summary: vec![
                        codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                            text: reasoning_text,
                        },
                    ],
                    content: None,
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                };
                // Reasoning is not a deliverable; do not set output_emitted here.
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
                let arguments = normalize_chat_completion_tool_arguments(&tc.function.arguments);
                // Emit OutputItemAdded with empty arguments to establish the
                // active item. The turn processor needs an active_item before
                // it can handle delta events.
                let function_call_added = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(&tc.function.name).cloned(),
                    name: tc.function.name.clone(),
                    arguments: String::new(),
                    call_id: tc.id.clone(),
                    internal_chat_message_metadata_passthrough: None,
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
                        delta: arguments.clone(),
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
                    arguments,
                    call_id: tc.id.clone(),
                    internal_chat_message_metadata_passthrough: None,
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
            output_emitted = true;

            // Mirror the streaming path: OutputItemAdded -> OutputTextDelta ->
            // OutputItemDone. The Added event is required so the turn processor
            // has an active item to attach the delta to.
            //
            // Unlike the reasoning branch above, the delta here is also needed
            // for rendering: codex-acp (Zed's ACP adapter) draws agentMessage
            // text ONLY from item/agentMessage/delta and ignores item/started
            // and item/completed, so without a delta Zed shows no answer text.
            let assistant_added = ResponseItem::Message {
                id: Some("msg_assistant".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: String::new(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            };
            if tx
                .send(Ok(ResponseEvent::OutputItemAdded(assistant_added)))
                .await
                .is_err()
            {
                return;
            }

            // Full content delivered as a single delta; the turn processor
            // forwards it as AgentMessageContentDelta.
            if tx
                .send(Ok(ResponseEvent::OutputTextDelta(content.clone())))
                .await
                .is_err()
            {
                return;
            }

            // Emit OutputItemDone with the full text to finalize the item.
            let assistant_done = ResponseItem::Message {
                id: Some("msg_assistant".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: content.clone(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
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
    //
    // Retry semantics (see codex-core/src/responses_retry.rs):
    //   - max retries: `stream_max_retries()` (default 5, hard cap 100).
    //   - backoff is applied BEFORE each retry (sleep, then resend):
    //     delay = 200ms * 2^(n-1) * jitter(0.9..1.1), no upper bound.
    //   - after max retries exhausted (and no transport fallback), the
    //     error is surfaced to the turn layer and the turn ends.
    // A reasoning-only response falls into this branch, so a provider that
    // persistently returns reasoning-only will retry up to the configured
    // limit before failing the turn; worst-case extra requests = max_retries
    // (default 5) per affected turn.
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
    use crate::common::ChatCompletionResponseFunction;
    use crate::common::ChatCompletionResponseMessage;
    use crate::common::ChatCompletionResponseToolCall;
    use crate::common::ChatCompletionUsage;
    use crate::common::ChatCompletionsResponse;
    use pretty_assertions::assert_eq;

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
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![],
            usage: None,
        }
    }

    fn null_content_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-2".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
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
            created: Some(1234567890),
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
            created: Some(1234567890),
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

    fn concatenated_tool_arguments_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-5".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![ChatCompletionResponseToolCall {
                        id: "call_123".to_string(),
                        r#type: "function".to_string(),
                        function: ChatCompletionResponseFunction {
                            name: "exec_command".to_string(),
                            arguments: r#"{}{"cmd":"pwd"}"#.to_string(),
                        },
                    }]),
                    reasoning: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        }
    }

    #[tokio::test]
    async fn concatenated_tool_arguments_are_normalized() {
        let events = collect_events(concatenated_tool_arguments_response()).await;

        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }))
            if name == "exec_command"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::ToolCallInputDelta { delta, .. })
            if delta == r#"{"cmd":"pwd"}"#
        ));
        assert!(matches!(
            &events[3],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { arguments, .. }))
            if arguments == r#"{"cmd":"pwd"}"#
        ));
        assert!(matches!(&events[4], Ok(ResponseEvent::Completed { .. })));
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
        // Non-streaming mirrors the streaming sequence (see the text branch in
        // convert_response_to_events for why the delta is required):
        //   [0] Created
        //   [1] OutputItemAdded(Message)    <- establishes the active item
        //   [2] OutputTextDelta("Hello from test!")  <- full text in one delta
        //   [3] OutputItemDone(Message)     <- finalizes the item
        //   [4] Completed
        assert_eq!(events.len(), 5, "expected exactly 5 events, got {events:?}");
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. }))
            if role == "assistant"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == "Hello from test!"
        ));
        assert!(matches!(
            &events[3],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }))
            if role == "assistant"
                && content.iter().any(
                    |c| matches!(c, ContentItem::OutputText { text } if text == "Hello from test!")
                )
        ));
        assert!(matches!(&events[4], Ok(ResponseEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn normal_content_has_no_error() {
        let events = collect_events(normal_content_response()).await;
        assert!(
            events.iter().all(std::result::Result::is_ok),
            "expected all Ok events, got {events:?}"
        );
    }

    fn reasoning_only_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-reasoning".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: None,
                    reasoning: Some(serde_json::Value::String("thinking about it".to_string())),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        }
    }

    fn reasoning_then_text_response() -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "resp-reasoning-text".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![ChatCompletionResponseChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello from test!".to_string()),
                    tool_calls: None,
                    reasoning: Some(serde_json::Value::String("hmm".to_string())),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        }
    }

    #[tokio::test]
    async fn reasoning_only_yields_retryable() {
        // Regression: a non-streaming response that only produced reasoning
        // content (no assistant text, no tool calls) must be treated as an
        // empty response so the turn layer retries the request, rather than
        // completing the turn with no deliverable output.
        let events = collect_events(reasoning_only_response()).await;

        // Expected ordered sequence:
        //   [0] Created
        //   [1] OutputItemDone(Reasoning)  <- summary carries full text, content None
        //   [2] Err(Retryable { "no output content" })
        //
        // Non-streaming responses already carry the full reasoning text in the
        // completed item, so we emit only a single OutputItemDone (no
        // OutputItemAdded + ReasoningContentDelta). Clients (e.g. Zed via
        // codex-acp) that render both a delta and the completed item would
        // otherwise display the same reasoning twice. The full text lives in
        // `summary` (the Responses API summary path that codex-acp's
        // seen_reasoning_deltas dedup is designed for); `content` is None so
        // no second complete-reasoning event (AgentReasoningRawContent) is
        // emitted by the legacy event expansion.
        assert_eq!(events.len(), 3, "expected exactly 3 events, got {events:?}");
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemDone(
                ResponseItem::Reasoning {
                    summary,
                    content: None,
                    ..
                }
            )) if summary.len() == 1
        ));
        // The single done item carries the full reasoning text exactly once.
        if let Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning { summary, .. })) =
            &events[1]
        {
            match &summary[0] {
                codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text } => {
                    assert_eq!(text, "thinking about it");
                }
            }
        } else {
            panic!("expected OutputItemDone(Reasoning) with summary text: {events:?}");
        }
        assert!(
            matches!(
                &events[2],
                Err(ApiError::Retryable { message, .. })
                    if message.contains("no output content")
            ),
            "last event must be Retryable 'no output content', got {:?}",
            &events[2]
        );

        // No-duplicate assertions: a non-streaming response must NOT emit an
        // OutputItemAdded or a ReasoningContentDelta for reasoning — those
        // would be rendered alongside the completed item and show the same
        // reasoning twice.
        assert!(
            !events.iter().any(|ev| matches!(
                ev,
                Ok(ResponseEvent::OutputItemAdded(
                    ResponseItem::Reasoning { .. }
                )) | Ok(ResponseEvent::ReasoningContentDelta { .. })
            )),
            "non-streaming reasoning must not emit Added/Delta (would duplicate): {events:?}"
        );

        // Negative assertions: no deliverable output and no Completed.
        assert!(
            !events.iter().any(|ev| matches!(
                ev,
                Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { .. }))
                    | Ok(ResponseEvent::Completed { .. })
            )),
            "reasoning-only response must not emit assistant text or Completed: {events:?}"
        );
    }

    #[tokio::test]
    async fn reasoning_then_text_completes_normally() {
        // Sanity: reasoning followed by real assistant text must complete
        // normally without a retryable error.
        let events = collect_events(reasoning_then_text_response()).await;

        let mut saw_completed = false;
        let mut saw_text_done = false;
        for ev in &events {
            match ev {
                Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }))
                    if role == "assistant"
                        && content.iter().any(
                            |c| matches!(c, ContentItem::OutputText { text } if text == "Hello from test!"),
                        ) =>
                {
                    saw_text_done = true;
                }
                Ok(ResponseEvent::Completed { .. }) => saw_completed = true,
                Err(ApiError::Retryable { .. }) => {
                    panic!("must not retry when assistant text is present: {events:?}")
                }
                _ => {}
            }
        }
        assert!(saw_text_done, "assistant text item should be finalized");
        assert!(saw_completed, "response should complete normally");
    }

    fn mixed_choices_one_reasoning_one_text_response() -> ChatCompletionsResponse {
        // Multi-choice response where choice 0 only has reasoning and
        // choice 1 has assistant text. A deliverable exists (choice 1 text),
        // so this must complete normally and not be treated as empty.
        ChatCompletionsResponse {
            id: "resp-mixed".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![
                ChatCompletionResponseChoice {
                    index: 0,
                    message: ChatCompletionResponseMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_calls: None,
                        reasoning: Some(serde_json::Value::String("think0".to_string())),
                    },
                    finish_reason: Some("stop".to_string()),
                },
                ChatCompletionResponseChoice {
                    index: 1,
                    message: ChatCompletionResponseMessage {
                        role: "assistant".to_string(),
                        content: Some("answer1".to_string()),
                        tool_calls: None,
                        reasoning: None,
                    },
                    finish_reason: Some("stop".to_string()),
                },
            ],
            usage: None,
        }
    }

    fn all_choices_reasoning_only_response() -> ChatCompletionsResponse {
        // Multi-choice response where every choice only has reasoning and
        // none has assistant text or tool calls. No deliverable exists across
        // any choice, so this must be treated as empty and retried.
        ChatCompletionsResponse {
            id: "resp-all-reasoning".to_string(),
            object: "chat.completion".to_string(),
            created: Some(1234567890),
            model: Some("test-model".to_string()),
            choices: vec![
                ChatCompletionResponseChoice {
                    index: 0,
                    message: ChatCompletionResponseMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_calls: None,
                        reasoning: Some(serde_json::Value::String("t0".to_string())),
                    },
                    finish_reason: Some("stop".to_string()),
                },
                ChatCompletionResponseChoice {
                    index: 1,
                    message: ChatCompletionResponseMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_calls: None,
                        reasoning: Some(serde_json::Value::String("t1".to_string())),
                    },
                    finish_reason: Some("stop".to_string()),
                },
            ],
            usage: None,
        }
    }

    #[tokio::test]
    async fn mixed_choices_with_text_completes_normally() {
        // Multi-choice: one reasoning-only choice + one text choice. The text
        // choice is a deliverable, so the response must complete normally
        // without a retryable error.
        let events = collect_events(mixed_choices_one_reasoning_one_text_response()).await;

        let mut saw_completed = false;
        let mut saw_text_done = false;
        for ev in &events {
            match ev {
                Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                    role, content, ..
                })) if role == "assistant"
                    && content.iter().any(
                        |c| matches!(c, ContentItem::OutputText { text } if text == "answer1"),
                    ) =>
                {
                    saw_text_done = true;
                }
                Ok(ResponseEvent::Completed { .. }) => saw_completed = true,
                Err(ApiError::Retryable { .. }) => {
                    panic!("must not retry when a choice has assistant text: {events:?}")
                }
                _ => {}
            }
        }
        assert!(saw_text_done, "choice 1 text should be finalized");
        assert!(saw_completed, "response should complete normally");
    }

    #[tokio::test]
    async fn all_choices_reasoning_only_yields_retryable() {
        // Multi-choice: every choice only has reasoning, no deliverable
        // anywhere. Must be treated as empty and retried; no Completed.
        let events = collect_events(all_choices_reasoning_only_response()).await;

        let last = events.last().expect("expected at least one event");
        assert!(
            matches!(
                last,
                Err(ApiError::Retryable { message, .. }) if message.contains("no output content")
            ),
            "last event must be Retryable 'no output content', got {last:?}"
        );
        assert!(
            !events.iter().any(|ev| matches!(
                ev,
                Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { .. }))
                    | Ok(ResponseEvent::Completed { .. })
            )),
            "all-reasoning multi-choice must not emit assistant text or Completed: {events:?}"
        );
    }
}
