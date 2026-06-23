//! SSE processing for the Anthropic Messages API streaming endpoint.
//!
//! Anthropic emits this event sequence per message:
//!
//! ```text
//!   message_start
//!     (content_block_start  → content_block_delta*  → content_block_stop)*
//!   message_delta            ← carries final stop_reason and usage
//!   message_stop
//! ```
//!
//! We translate this into the canonical `ResponseEvent` flow used elsewhere in
//! `codex-api`. The mapping is:
//!
//! - `message_start`              → `ResponseEvent::Created`, capture initial usage
//! - `content_block_start: text`  → `OutputItemAdded(Message{role: assistant, content: empty})`
//! - `content_block_delta: text`  → `OutputTextDelta`
//! - `content_block_start: tool_use` → `OutputItemAdded(FunctionCall{empty args})`
//! - `content_block_delta: input_json` → `ToolCallInputDelta`
//! - `content_block_start: thinking` → `OutputItemAdded(Reasoning)`
//! - `content_block_delta: thinking` → `ReasoningContentDelta`
//! - `content_block_stop`         → `OutputItemDone(...)` for the matching block
//! - `message_delta`              → cache stop_reason and usage
//! - `message_stop`               → `Completed { token_usage, end_turn }`

use crate::anthropic_types::AnthropicContentBlock;
use crate::anthropic_types::AnthropicStreamDelta;
use crate::anthropic_types::AnthropicStreamEvent;
use crate::anthropic_types::AnthropicUsage;
use crate::anthropic_types::stop_reason_to_end_turn;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

/// State for a streaming `tool_use` block. We accumulate the name and arguments
/// across deltas so we can emit a single `OutputItemDone` when the block stops.
#[derive(Debug, Default, Clone)]
struct ToolBlockState {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone)]
enum BlockKind {
    Text {
        accumulated: String,
        block_id: String,
    },
    Tool(ToolBlockState),
    Thinking {
        accumulated: String,
        /// Captured from Anthropic's `signature_delta` events. Required by
        /// Vertex AI (and the direct Anthropic API in some modes) when the
        /// thinking block is replayed in a follow-up turn. We persist it via
        /// `ResponseItem::Reasoning::encrypted_content` so build_messages can
        /// reattach it on the next request.
        signature: Option<String>,
    },
    /// Unknown block type — we still emit lifecycle events but do not generate
    /// deltas for it.
    Other,
}

/// Spawns a background task that processes Anthropic SSE events and forwards
/// them as canonical `ResponseEvent`s on a channel.
pub fn spawn_anthropic_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    namespace_map: HashMap<String, String>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        process_anthropic_sse(
            stream_response.bytes,
            tx_event,
            idle_timeout,
            telemetry,
            namespace_map,
        )
        .await;
    });
    ResponseStream {
        rx_event,
        upstream_request_id: None,
    }
}

/// Processes the byte stream of Anthropic SSE events. Errors are forwarded on
/// the channel as `ApiError` instances.
pub async fn process_anthropic_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    namespace_map: HashMap<String, String>,
) {
    let mut stream = stream.eventsource();

    // Active block by index.
    let mut blocks: HashMap<i64, BlockKind> = HashMap::new();
    // Capture the assistant message id for the final Completed event.
    let mut response_id = String::new();
    // Final usage rolled up across message_start + message_delta.
    let mut input_tokens: i64 = 0;
    let mut cached_input_tokens: i64 = 0;
    let mut output_tokens: i64 = 0;
    let mut stop_reason: Option<String> = None;
    let mut output_emitted = false;

    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }

        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("Anthropic SSE error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "anthropic stream closed before message_stop".into(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("anthropic SSE idle timeout".into())))
                    .await;
                return;
            }
        };

        let data = sse.data.trim();
        if data.is_empty() {
            continue;
        }

        trace!("Anthropic SSE event ({}): {}", sse.event, data);

        let event: AnthropicStreamEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(e) => {
                debug!(
                    "failed to parse Anthropic SSE event: {e}, event={}, data={}",
                    sse.event, data
                );
                continue;
            }
        };

        match event {
            AnthropicStreamEvent::Ping | AnthropicStreamEvent::Other => {
                continue;
            }
            AnthropicStreamEvent::MessageStart { message } => {
                if let Some(id) = message.id {
                    response_id = id;
                }
                if let Some(usage) = message.usage {
                    accumulate_usage(
                        &usage,
                        &mut input_tokens,
                        &mut cached_input_tokens,
                        &mut output_tokens,
                    );
                }
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let kind = match &content_block {
                    AnthropicContentBlock::Text { .. } => {
                        let block_id = format!("msg_{index}");
                        let item = ResponseItem::Message {
                            id: Some(block_id.clone()),
                            role: "assistant".to_string(),
                            content: vec![ContentItem::OutputText {
                                text: String::new(),
                            }],
                            phase: None,
                            internal_chat_message_metadata_passthrough: None,
                        };
                        if tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(item)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        output_emitted = true;
                        BlockKind::Text {
                            accumulated: String::new(),
                            block_id,
                        }
                    }
                    AnthropicContentBlock::ToolUse { id, name, .. } => {
                        let item = ResponseItem::FunctionCall {
                            id: None,
                            namespace: namespace_map.get(name).cloned(),
                            name: name.clone(),
                            arguments: String::new(),
                            call_id: id.clone(),
                            internal_chat_message_metadata_passthrough: None,
                        };
                        if tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(item)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        output_emitted = true;
                        BlockKind::Tool(ToolBlockState {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        })
                    }
                    AnthropicContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    } => {
                        let item = ResponseItem::Reasoning {
                            id: Some(format!("reasoning_{index}")),
                            summary: Vec::new(),
                            content: Some(vec![ReasoningItemContent::ReasoningText {
                                text: String::new(),
                            }]),
                            encrypted_content: None,
                            internal_chat_message_metadata_passthrough: None,
                        };
                        if tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(item)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        output_emitted = true;
                        BlockKind::Thinking {
                            accumulated: thinking.clone(),
                            signature: signature.clone(),
                        }
                    }
                    _ => BlockKind::Other,
                };
                blocks.insert(index, kind);
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                let Some(kind) = blocks.get_mut(&index) else {
                    debug!("delta for unknown block index {index}");
                    continue;
                };
                match (kind, delta) {
                    (
                        BlockKind::Text { accumulated, .. },
                        AnthropicStreamDelta::TextDelta { text },
                    ) => {
                        accumulated.push_str(&text);
                        if tx_event
                            .send(Ok(ResponseEvent::OutputTextDelta(text)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    (
                        BlockKind::Tool(state),
                        AnthropicStreamDelta::InputJsonDelta { partial_json },
                    ) => {
                        state.arguments.push_str(&partial_json);
                        if tx_event
                            .send(Ok(ResponseEvent::ToolCallInputDelta {
                                item_id: format!("call_{index}"),
                                call_id: Some(state.id.clone()),
                                delta: partial_json,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    (
                        BlockKind::Thinking { accumulated, .. },
                        AnthropicStreamDelta::ThinkingDelta { thinking },
                    ) => {
                        accumulated.push_str(&thinking);
                        if tx_event
                            .send(Ok(ResponseEvent::ReasoningContentDelta {
                                delta: thinking,
                                content_index: index,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    (
                        BlockKind::Thinking { signature, .. },
                        AnthropicStreamDelta::SignatureDelta { signature: Some(s) },
                    ) => {
                        // Anthropic delivers the thinking-block signature as a
                        // delta. Vertex AI (and Anthropic in extended-thinking
                        // mode) require this value to be echoed back when the
                        // thinking block is replayed; persist it so we can
                        // round-trip it via encrypted_content.
                        *signature = Some(s);
                    }
                    _ => {
                        // Mismatched delta variant for the active block — ignore.
                    }
                }
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                let Some(kind) = blocks.remove(&index) else {
                    continue;
                };
                let item = match kind {
                    BlockKind::Text {
                        accumulated,
                        block_id,
                    } => Some(ResponseItem::Message {
                        id: Some(block_id),
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText { text: accumulated }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }),
                    BlockKind::Tool(state) => Some(ResponseItem::FunctionCall {
                        id: None,
                        namespace: namespace_map.get(&state.name).cloned(),
                        name: state.name,
                        arguments: state.arguments,
                        call_id: state.id,
                        internal_chat_message_metadata_passthrough: None,
                    }),
                    BlockKind::Thinking {
                        accumulated,
                        signature,
                    } => Some(ResponseItem::Reasoning {
                        id: Some(format!("reasoning_{index}")),
                        summary: Vec::new(),
                        content: Some(vec![ReasoningItemContent::ReasoningText {
                            text: accumulated,
                        }]),
                        encrypted_content: signature,
                        internal_chat_message_metadata_passthrough: None,
                    }),
                    BlockKind::Other => None,
                };
                if let Some(item) = item
                    && tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                if let Some(reason) = delta.stop_reason {
                    stop_reason = Some(reason);
                }
                if let Some(usage) = usage {
                    accumulate_usage(
                        &usage,
                        &mut input_tokens,
                        &mut cached_input_tokens,
                        &mut output_tokens,
                    );
                }
            }
            AnthropicStreamEvent::MessageStop => {
                // Drain any blocks that never received an explicit stop.
                let leftover_indices: Vec<i64> = blocks.keys().copied().collect();
                for index in leftover_indices {
                    if let Some(kind) = blocks.remove(&index) {
                        let item = match kind {
                            BlockKind::Text {
                                accumulated,
                                block_id,
                            } => Some(ResponseItem::Message {
                                id: Some(block_id),
                                role: "assistant".to_string(),
                                content: vec![ContentItem::OutputText { text: accumulated }],
                                phase: None,
                                internal_chat_message_metadata_passthrough: None,
                            }),
                            BlockKind::Tool(state) => Some(ResponseItem::FunctionCall {
                                id: None,
                                namespace: namespace_map.get(&state.name).cloned(),
                                name: state.name,
                                arguments: state.arguments,
                                call_id: state.id,
                                internal_chat_message_metadata_passthrough: None,
                            }),
                            BlockKind::Thinking {
                                accumulated,
                                signature,
                            } => Some(ResponseItem::Reasoning {
                                id: Some(format!("reasoning_{index}")),
                                summary: Vec::new(),
                                content: Some(vec![ReasoningItemContent::ReasoningText {
                                    text: accumulated,
                                }]),
                                encrypted_content: signature,
                                internal_chat_message_metadata_passthrough: None,
                            }),
                            BlockKind::Other => None,
                        };
                        if let Some(item) = item {
                            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                        }
                    }
                }

                if !output_emitted {
                    let _ = tx_event
                        .send(Err(ApiError::Retryable {
                            message: "anthropic stream completed with no output content".into(),
                            delay: None,
                        }))
                        .await;
                    return;
                }

                let total = input_tokens + output_tokens;
                let token_usage = Some(TokenUsage {
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    reasoning_output_tokens: 0,
                    total_tokens: total,
                });
                let end_turn = stop_reason_to_end_turn(stop_reason.as_deref());
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                        end_turn,
                    }))
                    .await;
                return;
            }
            AnthropicStreamEvent::Error { error } => {
                let kind = error.r#type.as_deref().unwrap_or("error");
                let message = error
                    .message
                    .unwrap_or_else(|| "anthropic stream error".into());
                let formatted = format!("{kind}: {message}");
                let api_err = match kind {
                    // Server-side transient errors that are safe to retry.
                    "overloaded_error" | "rate_limit_error" | "api_error" => ApiError::Retryable {
                        message: formatted,
                        delay: None,
                    },
                    _ => ApiError::Stream(formatted),
                };
                let _ = tx_event.send(Err(api_err)).await;
                return;
            }
        }
    }
}

fn accumulate_usage(
    usage: &AnthropicUsage,
    input_tokens: &mut i64,
    cached_input_tokens: &mut i64,
    output_tokens: &mut i64,
) {
    // Anthropic reports `input_tokens` as the count it actually billed for the
    // turn, with `cache_read_input_tokens` reported separately as part of the
    // cached prefix that did not need re-encoding. The canonical TokenUsage
    // total includes the cached portion in `input_tokens`, mirroring how the
    // Responses API reports `input_tokens` as the full prompt size.
    let input = usage.input_tokens.max(0);
    let cache_read = usage.cache_read().max(0);
    let cache_creation = usage.cache_creation().max(0);
    *input_tokens += input + cache_read + cache_creation;
    *cached_input_tokens += cache_read;
    *output_tokens += usage.output_tokens.max(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_client::TransportError;
    use futures::TryStreamExt;
    use tokio_test::io::Builder as IoBuilder;
    use tokio_util::io::ReaderStream;

    fn idle_timeout() -> Duration {
        Duration::from_millis(2000)
    }

    async fn collect_events(chunks: &[&[u8]]) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut builder = IoBuilder::new();
        for chunk in chunks {
            builder.read(chunk);
        }
        let reader = builder.build();
        let stream = ReaderStream::new(reader)
            .map_err(|err: std::io::Error| TransportError::Network(err.to_string()));

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(64);
        tokio::spawn(process_anthropic_sse(
            Box::pin(stream),
            tx,
            idle_timeout(),
            None,
            HashMap::new(),
        ));

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn parses_text_response() {
        let chunks: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-3-7-sonnet\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":0,\"output_tokens\":7}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let events = collect_events(chunks).await;

        // Expected: Created, OutputItemAdded(Message), OutputTextDelta("Hi"),
        // OutputTextDelta(" there"), OutputItemDone(Message), Completed
        assert_eq!(events.len(), 6, "events: {events:#?}");
        assert!(matches!(&events[0], Ok(ResponseEvent::Created)));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. }))
                if role == "assistant"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == "Hi"
        ));
        assert!(matches!(
            &events[3],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == " there"
        ));
        assert!(matches!(
            &events[4],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { .. }))
        ));
        let last = events.last().unwrap();
        match last {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                end_turn,
            }) => {
                assert_eq!(response_id, "msg_1");
                let usage = token_usage.as_ref().expect("usage");
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(*end_turn, Some(true));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_tool_use_response() {
        let chunks: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"role\":\"assistant\"}}\n\n",
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\",\"input\":{}}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\":\\\"ls\\\"}\"}}\n\n",
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let events = collect_events(chunks).await;
        assert!(events.len() >= 5, "events: {events:#?}");
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }))
                if name == "shell"
        ));
        let mut delta_concat = String::new();
        for ev in &events {
            if let Ok(ResponseEvent::ToolCallInputDelta { delta, .. }) = ev {
                delta_concat.push_str(delta);
            }
        }
        assert_eq!(delta_concat, "{\"cmd\":\"ls\"}");

        let last = events.last().unwrap();
        match last {
            Ok(ResponseEvent::Completed { end_turn, .. }) => {
                assert_eq!(*end_turn, Some(false));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_stream_yields_retryable() {
        let chunks: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_3\"}}\n\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let events = collect_events(chunks).await;
        assert!(matches!(events.first(), Some(Ok(ResponseEvent::Created))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Err(ApiError::Retryable { .. })))
        );
    }

    #[tokio::test]
    async fn surface_error_event_as_retryable_when_overloaded() {
        let chunks: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_4\"}}\n\n",
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"please slow down\"}}\n\n",
        ];
        let events = collect_events(chunks).await;
        let last = events.last().unwrap();
        assert!(
            matches!(last, Err(ApiError::Retryable { .. })),
            "got {last:?}"
        );
    }

    #[tokio::test]
    async fn cache_read_tokens_roll_into_cached_input() {
        let chunks: &[&[u8]] = &[
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_5\",\"usage\":{\"input_tokens\":3,\"cache_read_input_tokens\":97,\"output_tokens\":0}}}\n\n",
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        let events = collect_events(chunks).await;
        let last = events.last().unwrap();
        match last {
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                let usage = token_usage.as_ref().unwrap();
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.cached_input_tokens, 97);
                assert_eq!(usage.output_tokens, 2);
                assert_eq!(usage.total_tokens, 102);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
