//! Gemini JSON and SSE share one event converter, including usage and signed
//! part replay. A stream is successful only after a terminal finish reason.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use codex_client::StreamResponse;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::gemini_types::GeminiPart;
use crate::gemini_types::GeminiReplay;
use crate::gemini_types::GeminiResponse;
use crate::gemini_types::GeminiUsage;

fn retryable(message: impl Into<String>) -> ApiError {
    ApiError::Retryable {
        message: message.into(),
        retry_after: None,
    }
}

fn new_item_id(prefix: &str) -> ResponseItemId {
    // Process-local counters restart on resume and may collide with retained
    // history. Gemini often omits supplier call IDs, so use durable UUID IDs.
    ResponseItemId::with_suffix(prefix, uuid::Uuid::new_v4())
}

pub(crate) fn parse_response(bytes: &[u8]) -> Result<GeminiResponse, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|err| retryable(format!("failed to parse Gemini response: {err}")))
}

struct TextBlock {
    id: ResponseItemId,
    thought: bool,
    parts: Vec<GeminiPart>,
}

impl TextBlock {
    fn item(&self, complete: bool) -> ResponseItem {
        let text: String = self
            .parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect();
        let encrypted_content = complete
            .then(|| GeminiReplay::encode(self.parts.clone()))
            .flatten();
        if self.thought {
            ResponseItem::Reasoning {
                id: Some(self.id.clone()),
                summary: if complete {
                    vec![ReasoningItemReasoningSummary::SummaryText { text }]
                } else {
                    Vec::new()
                },
                content: None,
                encrypted_content,
                internal_chat_message_metadata_passthrough: None,
            }
        } else {
            ResponseItem::Message {
                id: Some(self.id.clone()),
                role: "assistant".into(),
                content: vec![ContentItem::OutputText {
                    text: if complete { text } else { String::new() },
                    annotations: None,
                    logprobs: None,
                }],
                phase: None,
                status: None,
                encrypted_content,
                internal_chat_message_metadata_passthrough: None,
            }
        }
    }

    fn append(&mut self, part: GeminiPart) {
        // SSE splits a text part into fragments. Never merge past a signature:
        // Google's validation binds it to this exact part.
        if let Some(last) = self.parts.last_mut()
            && last.thought_signature.is_none()
        {
            last.text
                .get_or_insert_with(String::new)
                .push_str(part.text.as_deref().unwrap_or_default());
            last.thought_signature = part.thought_signature;
        } else {
            self.parts.push(part);
        }
    }
}

pub(crate) struct GeminiEvents {
    response_id: String,
    names: HashMap<String, (String, Option<String>)>,
    active: Option<TextBlock>,
    usage: Option<GeminiUsage>,
    finish_reason: Option<String>,
    visible_output: bool,
    tool_calls: bool,
    seen_call_ids: HashSet<String>,
    continuation_token: Option<String>,
    initial_usage: Option<GeminiUsage>,
    continued_output_tokens: i64,
    continued_reasoning_tokens: i64,
    continued: bool,
}

impl GeminiEvents {
    pub(crate) fn new(names: HashMap<String, (String, Option<String>)>) -> Self {
        Self {
            response_id: uuid::Uuid::new_v4().to_string(),
            names,
            active: None,
            usage: None,
            finish_reason: None,
            visible_output: false,
            tool_calls: false,
            seen_call_ids: HashSet::new(),
            continuation_token: None,
            initial_usage: None,
            continued_output_tokens: 0,
            continued_reasoning_tokens: 0,
            continued: false,
        }
    }

    fn flush(&mut self, events: &mut Vec<ResponseEvent>) {
        if let Some(block) = self.active.take() {
            events.push(ResponseEvent::OutputItemDone(block.item(true)));
        }
    }

    pub(crate) fn push(
        &mut self,
        response: GeminiResponse,
    ) -> Result<Vec<ResponseEvent>, ApiError> {
        if let Some(error) = response.error {
            return Err(retryable(format!("Gemini upstream error: {error}")));
        }
        if let Some(feedback) = response.prompt_feedback
            && feedback
                .get("blockReason")
                .and_then(serde_json::Value::as_str)
                .is_some()
        {
            return Err(retryable(format!("Gemini prompt blocked: {feedback}")));
        }
        if let Some(id) = response.response_id {
            self.response_id = id;
        }
        if let Some(usage) = response.usage_metadata {
            // Usage-only trailing frames may omit fields reported earlier.
            let previous = self.usage.get_or_insert_with(GeminiUsage::default);
            macro_rules! update {
                ($($field:ident),+ $(,)?) => {
                    $(if usage.$field.is_some() { previous.$field = usage.$field; })+
                };
            }
            update!(
                prompt_token_count,
                cached_content_token_count,
                candidates_token_count,
                thoughts_token_count,
                total_token_count
            );
        }
        let mut events = Vec::new();
        for candidate in response
            .candidates
            .into_iter()
            .filter(|candidate| candidate.index.unwrap_or_default() == 0)
            .take(1)
        {
            if let Some(token) = candidate.continuation_token {
                self.continuation_token = Some(token);
            } else if candidate.finish_reason.is_some() {
                self.continuation_token = None;
            }
            if let Some(reason) = candidate.finish_reason {
                if !matches!(reason.as_str(), "STOP" | "MAX_TOKENS" | "CONTINUATION") {
                    return Err(retryable(format!("Gemini generation failed: {reason}")));
                }
                self.finish_reason = Some(reason);
            }
            for part in candidate
                .content
                .into_iter()
                .flat_map(|content| content.parts)
            {
                if let Some(call) = part.function_call.as_ref() {
                    if call.name.is_empty() || (!call.args.is_null() && !call.args.is_object()) {
                        return Err(retryable("Gemini returned an invalid function call"));
                    }
                    self.flush(&mut events);
                    let (name, namespace) = self
                        .names
                        .get(&call.name)
                        .cloned()
                        .unwrap_or_else(|| (call.name.clone(), None));
                    let id = new_item_id("gemini_tool");
                    let call_id = call
                        .id
                        .clone()
                        .filter(|id| !id.is_empty() && self.seen_call_ids.insert(id.clone()))
                        .unwrap_or_else(|| id.to_string());
                    let arguments = if call.args.is_null() {
                        "{}".into()
                    } else {
                        call.args.to_string()
                    };
                    let item = ResponseItem::FunctionCall {
                        id: Some(id.clone()),
                        call_id: call_id.clone(),
                        name,
                        namespace,
                        arguments: arguments.clone(),
                        encrypted_content: GeminiReplay::encode(vec![part]),
                        encrypted_function_args: None,
                        status: None,
                        internal_chat_message_metadata_passthrough: None,
                    };
                    let mut added = item.clone();
                    if let ResponseItem::FunctionCall { arguments, .. } = &mut added {
                        arguments.clear();
                    }
                    events.push(ResponseEvent::OutputItemAdded(added));
                    events.push(ResponseEvent::ToolCallInputDelta {
                        item_id: id.to_string(),
                        call_id: Some(call_id),
                        delta: arguments,
                    });
                    events.push(ResponseEvent::OutputItemDone(item));
                    self.visible_output = true;
                    self.tool_calls = true;
                    continue;
                }
                let thought = part.thought == Some(true);
                let text = part.text.as_deref().unwrap_or_default().to_owned();
                if text.is_empty() && part.thought_signature.is_none() {
                    continue;
                }
                if text.is_empty()
                    && part.thought_signature.is_some()
                    && let Some(block) = self.active.as_mut()
                {
                    block.append(part);
                    continue;
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|block| block.thought != thought)
                {
                    self.flush(&mut events);
                }
                if self.active.is_none() {
                    let block = TextBlock {
                        id: new_item_id(if thought {
                            "gemini_reasoning"
                        } else {
                            "gemini_text"
                        }),
                        thought,
                        parts: Vec::new(),
                    };
                    events.push(ResponseEvent::OutputItemAdded(block.item(false)));
                    self.active = Some(block);
                }
                if let Some(block) = self.active.as_mut() {
                    block.append(part);
                }
                if !text.is_empty() {
                    if thought {
                        events.push(ResponseEvent::ReasoningContentDelta {
                            delta: text,
                            content_index: 0,
                        });
                    } else {
                        self.visible_output = true;
                        events.push(ResponseEvent::OutputTextDelta(text));
                    }
                }
            }
        }
        Ok(events)
    }

    pub(crate) fn next_request(&mut self) -> Option<String> {
        if !matches!(self.finish_reason.as_deref(), None | Some("CONTINUATION")) {
            return None;
        }
        let token = self
            .continuation_token
            .take()
            .filter(|token| !token.is_empty())?;
        if let Some(usage) = self.usage.take() {
            self.continued_output_tokens += usage.candidates_token_count.unwrap_or_default();
            self.continued_reasoning_tokens += usage.thoughts_token_count.unwrap_or_default();
            if !self.continued {
                self.initial_usage = Some(usage);
            }
        }
        self.continued = true;
        self.finish_reason = None;
        Some(token)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ApiError> {
        if !matches!(self.finish_reason.as_deref(), Some("STOP" | "MAX_TOKENS")) {
            return Err(retryable("Gemini response closed before finishReason"));
        }
        if !self.visible_output {
            return Err(retryable(
                "Gemini response completed with no output content",
            ));
        }
        let mut events = Vec::new();
        self.flush(&mut events);
        let usage = self
            .usage
            .take()
            .or_else(|| self.initial_usage.as_ref().map(|_| GeminiUsage::default()));
        let token_usage = usage.map(|usage| {
            // Continued requests re-bill prior output as input. Report final
            // context size using initial input and all output, as OpenCode does.
            let initial = self.initial_usage.as_ref().unwrap_or(&usage);
            let input_tokens = initial.prompt_token_count.unwrap_or_default();
            let reasoning_output_tokens =
                usage.thoughts_token_count.unwrap_or_default() + self.continued_reasoning_tokens;
            // Google excludes thoughts from candidatesTokenCount; Codex counts
            // reasoning as part of output, with its own breakdown as well.
            let output_tokens = usage.candidates_token_count.unwrap_or_default()
                + self.continued_output_tokens
                + reasoning_output_tokens;
            TokenUsage {
                input_tokens,
                cached_input_tokens: initial.cached_content_token_count.unwrap_or_default(),
                cache_write_input_tokens: 0,
                output_tokens,
                reasoning_output_tokens,
                total_tokens: if self.continued {
                    input_tokens + output_tokens
                } else {
                    usage
                        .total_token_count
                        .unwrap_or(input_tokens + output_tokens)
                },
                codex_rollout_budget_units: None,
            }
        });
        events.push(ResponseEvent::Completed {
            response_id: self.response_id.clone(),
            token_usage,
            usage_metadata: None,
            end_turn: Some(self.finish_reason.as_deref() == Some("STOP") && !self.tool_calls),
        });
        Ok(events)
    }
}

pub(crate) type GeminiContinuation =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<StreamResponse, ApiError>> + Send + Sync>;

async fn send_events(
    tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    result: Result<Vec<ResponseEvent>, ApiError>,
) -> bool {
    match result {
        Ok(events) => {
            for event in events {
                if tx.send(Ok(event)).await.is_err() {
                    return false;
                }
            }
            true
        }
        Err(error) => {
            let _ = tx.send(Err(error)).await;
            false
        }
    }
}

pub(crate) fn spawn_gemini_stream(
    response: StreamResponse,
    idle_timeout: Duration,
    names: HashMap<String, (String, Option<String>)>,
    continuation: Option<GeminiContinuation>,
) -> ResponseStream {
    let upstream_request_id = response
        .headers
        .get("x-request-id")
        .or_else(|| response.headers.get("x-goog-request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (tx, rx_event) = mpsc::channel(1600);
    tokio::spawn(async move {
        let mut state = GeminiEvents::new(names);
        if tx
            .send(Ok(ResponseEvent::Created { response_id: None }))
            .await
            .is_err()
        {
            return;
        }
        let mut stream = response.bytes.eventsource();
        loop {
            let polled = tokio::select! {
                _ = tx.closed() => return,
                polled = timeout(idle_timeout, stream.next()) => polled,
            };
            let error = match polled {
                Ok(Some(Ok(event))) => {
                    if event.data.trim().is_empty() {
                        continue;
                    }
                    if event.data.trim() == "[DONE]" {
                        // Some Gemini-compatible gateways add this terminator.
                        // Still require finishReason (or a continuation token).
                        None
                    } else {
                        let result = parse_response(event.data.as_bytes())
                            .and_then(|response| state.push(response));
                        if !send_events(&tx, result).await {
                            return;
                        }
                        continue;
                    }
                }
                Ok(None) => None,
                Ok(Some(Err(error))) => {
                    if !matches!(&error, eventsource_stream::EventStreamError::Transport(transport)
                        if !matches!(transport, codex_client::TransportError::Timeout))
                    {
                        let _ = tx
                            .send(Err(retryable(format!("Gemini SSE error: {error}"))))
                            .await;
                        return;
                    }
                    Some(retryable(format!("Gemini SSE error: {error}")))
                }
                Err(_) => {
                    let _ = tx.send(Err(retryable("Gemini stream idle timeout"))).await;
                    return;
                }
            };
            if let Some(resume) = continuation.as_ref()
                && let Some(token) = state.next_request()
            {
                let resumed = tokio::select! {
                    _ = tx.closed() => return,
                    resumed = resume(token) => resumed,
                };
                match resumed {
                    Ok(response) => stream = response.bytes.eventsource(),
                    Err(error) => {
                        let _ = tx
                            .send(Err(retryable(format!(
                                "Gemini continuation failed: {error}"
                            ))))
                            .await;
                        return;
                    }
                }
            } else {
                send_events(
                    &tx,
                    match error {
                        Some(error) => Err(error),
                        None => state.finish(),
                    },
                )
                .await;
                return;
            }
        }
    });
    ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt: None,
    }
}

pub(crate) fn response_stream(
    events: Vec<ResponseEvent>,
    upstream_request_id: Option<String>,
) -> ResponseStream {
    let (tx, rx_event) = mpsc::channel(1600);
    tokio::spawn(async move {
        send_events(&tx, Ok(events)).await;
    });
    ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(value: serde_json::Value) -> GeminiResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn reasoning_and_signed_tools_are_separate_and_tool_turn_is_not_complete() {
        let mut state = GeminiEvents::new(HashMap::from([(
            "wire_tool".into(),
            ("run".into(), Some("mcp".into())),
        )]));
        let events = state.push(response(json!({
            "candidates": [{"finishReason": "STOP", "content": {"parts": [
                {"text": "thinking", "thought": true, "thoughtSignature": "reasoning-signature"},
                {"functionCall": {"name": "wire_tool", "args": {"x": 1}}, "thoughtSignature": "call-signature"}
            ]}}]
        }))).unwrap();
        assert!(events.iter().any(|event| matches!(event,
            ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "thinking")));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ResponseEvent::OutputTextDelta(_)))
        );
        let call = events
            .iter()
            .find_map(|event| match event {
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    name,
                    namespace,
                    encrypted_content,
                    ..
                }) => Some((name, namespace, encrypted_content)),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.0, "run");
        assert_eq!(call.1.as_deref(), Some("mcp"));
        let parts = GeminiReplay::decode(call.2.as_deref().unwrap()).unwrap();
        assert_eq!(
            parts[0].thought_signature.as_deref(),
            Some("call-signature")
        );
        assert!(matches!(
            state.finish().unwrap().last(),
            Some(ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            })
        ));
    }

    #[test]
    fn signed_text_fragments_replay_with_boundaries_and_trailing_usage_is_merged() {
        let mut state = GeminiEvents::new(HashMap::new());
        state
            .push(response(json!({
                "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 80},
                "candidates": [{"content": {"parts": [{"text": "hel"}]}}]
            })))
            .unwrap();
        state.push(response(json!({
            "candidates": [{"content": {"parts": [{"text": "lo"}, {"thoughtSignature": "sig"}]}, "finishReason": "STOP"}]
        }))).unwrap();
        state
            .push(response(json!({
                "usageMetadata": {"candidatesTokenCount": 3, "thoughtsTokenCount": 2}
            })))
            .unwrap();
        let events = state.finish().unwrap();
        let encoded = events
            .iter()
            .find_map(|event| match event {
                ResponseEvent::OutputItemDone(ResponseItem::Message {
                    encrypted_content, ..
                }) => encrypted_content.as_deref(),
                _ => None,
            })
            .unwrap();
        let parts = GeminiReplay::decode(encoded).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_deref(), Some("hello"));
        assert_eq!(parts[0].thought_signature.as_deref(), Some("sig"));
        assert!(matches!(
            events.last(),
            Some(ResponseEvent::Completed {
                token_usage: Some(TokenUsage {
                    cached_input_tokens: 80,
                    output_tokens: 5,
                    total_tokens: 105,
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn errors_empty_reasoning_only_and_truncated_responses_are_retryable() {
        for value in [
            json!({"error": {"code": 503, "message": "overloaded"}}),
            json!({"promptFeedback": {"blockReason": "SAFETY"}}),
            json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]}),
        ] {
            assert!(matches!(
                GeminiEvents::new(HashMap::new()).push(response(value)),
                Err(ApiError::Retryable { .. })
            ));
        }
        for value in [
            json!({}),
            json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]}),
            json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "thinking", "thought": true}]}}]}),
            json!({"candidates": [{"finishReason": "STOP"}]}),
            json!({"candidates": [{"finishReason": "CONTINUATION", "content": {"parts": [{"text": "partial"}]}}]}),
        ] {
            let mut state = GeminiEvents::new(HashMap::new());
            state.push(response(value)).unwrap();
            assert!(matches!(state.finish(), Err(ApiError::Retryable { .. })));
        }
        assert!(matches!(
            parse_response(b"not json"),
            Err(ApiError::Retryable { .. })
        ));
    }

    #[test]
    fn gemini_nullable_fields_and_duplicate_tool_ids_are_handled() {
        let mut state = GeminiEvents::new(HashMap::new());
        for value in [
            json!({"candidates": null, "usageMetadata": null}),
            json!({"candidates": [{"content": null, "finishReason": null}]}),
            json!({"candidates": [{"content": {"parts": null}}]}),
        ] {
            assert!(state.push(response(value)).unwrap().is_empty());
        }
        let events = state.push(response(json!({
            "candidates": [{"index": null, "finishReason": "STOP", "content": {"parts": [
                {"functionCall": {"id": "duplicate", "name": "run", "args": {}}},
                {"functionCall": {"id": "duplicate", "name": "run", "args": {}}}
            ]}}],
            "usageMetadata": {"promptTokenCount": null, "cachedContentTokenCount": null, "candidatesTokenCount": 5}
        }))).unwrap();
        let ids: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. }) => {
                    Some(call_id)
                }
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(state.finish().is_ok());
    }

    #[test]
    fn gemini_generated_call_ids_are_unique_across_response_states() {
        let call_id = || {
            let events = GeminiEvents::new(HashMap::new()).push(response(json!({
                "candidates": [{"content": {"parts": [{"functionCall": {"name": "run", "args": {}}}]},
                    "finishReason": "STOP"}]
            }))).unwrap();
            events
                .into_iter()
                .find_map(|event| match event {
                    ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                        call_id, ..
                    }) => Some(call_id),
                    _ => None,
                })
                .unwrap()
        };
        assert_ne!(call_id(), call_id());
    }

    #[tokio::test]
    async fn sse_error_and_premature_eof_reach_the_retry_layer() {
        use bytes::Bytes;
        use futures::stream;
        for data in [
            "data: {\"error\":{\"code\":503,\"message\":\"overloaded\"}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            "data: invalid-json\n\n",
        ] {
            let response = StreamResponse {
                status: http::StatusCode::OK,
                headers: http::HeaderMap::new(),
                bytes: Box::pin(stream::iter(vec![Ok(Bytes::from(data))])),
            };
            let mut events =
                spawn_gemini_stream(response, Duration::from_secs(1), HashMap::new(), None);
            let mut failed = false;
            while let Some(event) = events.next().await {
                assert!(!matches!(event, Ok(ResponseEvent::Completed { .. })));
                failed |= matches!(event, Err(ApiError::Retryable { .. }));
            }
            assert!(failed);
        }
    }

    #[tokio::test]
    async fn gemini_gateway_done_marker_requires_a_completed_response() {
        use bytes::Bytes;
        use futures::stream;
        for finish in [Some("STOP"), None] {
            let chunk = json!({"candidates": [{
                "content": {"parts": [{"text": "ok"}]}, "finishReason": finish
            }]});
            let response = StreamResponse {
                status: http::StatusCode::OK,
                headers: http::HeaderMap::new(),
                bytes: Box::pin(stream::iter(vec![Ok(Bytes::from(format!(
                    "data: {chunk}\n\ndata: [DONE]\n\n"
                )))])),
            };
            let mut events =
                spawn_gemini_stream(response, Duration::from_secs(1), HashMap::new(), None);
            let mut completed = false;
            let mut failed = false;
            while let Some(event) = events.next().await {
                completed |= matches!(event, Ok(ResponseEvent::Completed { .. }));
                failed |= event.is_err();
            }
            assert_eq!(completed, finish.is_some());
            assert_eq!(failed, finish.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gemini_idle_timeout_is_retryable() {
        let response = StreamResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            bytes: Box::pin(futures::stream::pending()),
        };
        let mut events =
            spawn_gemini_stream(response, Duration::from_secs(1), HashMap::new(), None);
        assert!(matches!(
            events.next().await,
            Some(Ok(ResponseEvent::Created { .. }))
        ));
        assert!(matches!(
            events.next().await,
            Some(Err(ApiError::Retryable { .. }))
        ));
        assert!(events.next().await.is_none());
    }
}
