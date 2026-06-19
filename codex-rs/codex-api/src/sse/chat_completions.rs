//! SSE processing for the OpenAI Chat Completions API.

use crate::common::ChatCompletionChoice;
use crate::common::ChatCompletionsStreamEvent;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
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

// Accumulated state for a single streaming tool call:
// (call_id, function_name, concatenated_arguments)
type ToolCallAccumulator = (Option<String>, Option<String>, Option<String>);

/// Normalizes accumulated tool call arguments to a valid JSON string.
///
/// When no argument deltas were received, defaults to `"{}"`. When the
/// accumulated string is not valid JSON, also falls back to `"{}"`.
/// This prevents the next request from being rejected with a 400 error
/// ("arguments must be in JSON format").
fn normalize_tool_call_arguments(arguments: Option<String>) -> String {
    match arguments {
        Some(args) if !args.is_empty() => {
            if serde_json::from_str::<serde_json::Value>(&args).is_ok() {
                args
            } else {
                "{}".to_string()
            }
        }
        _ => "{}".to_string(),
    }
}

/// Spawns a background task to process SSE events from a chat completions stream.
pub fn spawn_chat_completions_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    namespace_map: HashMap<String, String>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        process_chat_completions_sse(
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

/// Processes SSE events from the chat completions streaming API.
pub async fn process_chat_completions_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    namespace_map: HashMap<String, String>,
) {
    let mut stream = stream.eventsource();
    let mut accumulated_tool_calls: HashMap<i64, ToolCallAccumulator> = HashMap::new();
    let mut final_usage: Option<TokenUsage> = None;
    // Whether an OutputItemAdded for the assistant text message has been emitted.
    // The turn processor requires an OutputItemAdded before it can handle
    // OutputTextDelta events; without it the deltas are silently dropped.
    let mut text_item_added = false;
    // Whether an OutputItemDone for the assistant text message has been emitted.
    let mut text_item_done = false;
    // Accumulated text content from all OutputTextDelta events, used to build
    // the OutputItemDone message.
    let mut accumulated_text = String::new();
    // Accumulated reasoning content for models that support reasoning/thinking mode.
    let mut accumulated_reasoning = String::new();
    // Whether an OutputItemAdded for reasoning has been emitted.
    let mut reasoning_item_added = false;
    // The last finish_reason seen across all choices, used to infer
    // end_turn for the Completed event.
    let mut last_finish_reason: Option<String> = None;
    // Whether any output item was emitted during the stream (text or tool call).
    // This is distinct from the accumulation buffers, which may be drained
    // before [DONE] arrives (e.g. by finish_reason handling).
    let mut output_emitted = false;

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "stream closed before completion".into(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        let data = sse.data.trim();

        // Handle the [DONE] marker
        if data == "[DONE]" {
            // Emit OutputItemDone for the assistant text message if we added one
            // but haven't yet closed it (e.g. the stream ended without a
            // finish_reason chunk). This must happen BEFORE tool call events so
            // the TUI can finalize the stream_controller while it is still
            // active, preventing duplicate rendering of the text content.
            if text_item_added && !text_item_done {
                let done_item = ResponseItem::Message {
                    id: Some("msg_assistant".to_string()),
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: accumulated_text.clone(),
                    }],
                    phase: None,
                    metadata: None,
                };
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(done_item)))
                    .await;
                // No need to update `text_item_done`: the [DONE] handler
                // returns from this function once it finishes flushing.
            }

            // Emit OutputItemDone for reasoning if we started one.
            if reasoning_item_added {
                let reasoning_done = ResponseItem::Reasoning {
                    id: String::new(),
                    summary: Vec::new(),
                    content: Some(vec![
                        codex_protocol::models::ReasoningItemContent::ReasoningText {
                            text: accumulated_reasoning.clone(),
                        },
                    ]),
                    encrypted_content: None,
                    metadata: None,
                };
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(reasoning_done)))
                    .await;
            }

            // Emit any remaining tool calls with the proper event sequence
            // (OutputItemAdded -> ToolCallInputDelta -> OutputItemDone) so the
            // turn processor can establish the active tool before receiving deltas.
            for (index, (id, name, arguments)) in accumulated_tool_calls.drain() {
                if let (Some(id), Some(name)) = (id, name) {
                    let args = normalize_tool_call_arguments(arguments);
                    let function_call_item = ResponseItem::FunctionCall {
                        id: None,
                        namespace: namespace_map.get(&name).cloned(),
                        name: name.clone(),
                        arguments: args.clone(),
                        call_id: id.clone(),
                        metadata: None,
                    };
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(
                            function_call_item.clone(),
                        )))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::ToolCallInputDelta {
                            item_id: format!("call_{index}"),
                            call_id: Some(id),
                            delta: args,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(function_call_item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    output_emitted = true;
                }
            }

            // Check if stream had no meaningful output - treat as retryable error.
            // Must happen AFTER flushing accumulated tool calls and text items,
            // since those flushes may produce output not tracked during streaming.
            if !output_emitted {
                let _ = tx_event
                    .send(Err(ApiError::Retryable {
                        message: "chat completions stream completed with no output content"
                            .to_string(),
                        delay: None,
                    }))
                    .await;
                return;
            }

            // Emit completion event with end_turn inferred from finish_reason.
            // "stop" means the model finished its turn; "tool_calls" means it
            // expects tool output before continuing.
            let end_turn = match last_finish_reason.as_deref() {
                Some("stop") | Some("length") => Some(true),
                Some("tool_calls") => Some(false),
                _ => None,
            };
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: final_usage,
                    end_turn,
                }))
                .await;
            return;
        }

        trace!("Chat completions SSE event: {}", data);

        let event: ChatCompletionsStreamEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(e) => {
                debug!(
                    "Failed to parse chat completions SSE event: {e}, data: {}",
                    data
                );
                continue;
            }
        };

        // Extract usage if present (usually in the final chunk when streaming)
        if let Some(usage) = &event.usage {
            final_usage = Some(TokenUsage {
                input_tokens: usage.prompt_tokens,
                cached_input_tokens: 0,
                output_tokens: usage.completion_tokens,
                reasoning_output_tokens: 0,
                total_tokens: usage.total_tokens,
            });
        }

        // Skip events with no choices (e.g., usage-only events).
        if event.choices.is_empty() {
            continue;
        }

        // Process choices
        for choice in &event.choices {
            if let Some(fr) = &choice.finish_reason
                && !fr.is_empty()
            {
                last_finish_reason = Some(fr.clone());
            }
            if let Err(_e) = process_chat_choice(
                choice,
                &tx_event,
                &mut accumulated_tool_calls,
                &mut accumulated_reasoning,
                &mut reasoning_item_added,
                &mut text_item_added,
                &mut text_item_done,
                &mut accumulated_text,
                &mut output_emitted,
                &namespace_map,
            )
            .await
            {
                // Errors from individual choice processing are logged within
                // process_chat_choice; we continue processing remaining choices.
            }
        }
    }
}

/// Processes a single choice from the chat completions stream.
#[allow(clippy::too_many_arguments)]
async fn process_chat_choice(
    choice: &ChatCompletionChoice,
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    accumulated_tool_calls: &mut HashMap<i64, ToolCallAccumulator>,
    accumulated_reasoning: &mut String,
    reasoning_item_added: &mut bool,
    text_item_added: &mut bool,
    text_item_done: &mut bool,
    accumulated_text: &mut String,
    output_emitted: &mut bool,
    namespace_map: &HashMap<String, String>,
) -> Result<(), ApiError> {
    let delta = &choice.delta;

    // Handle role (usually in first chunk)
    if let Some(_role) = &delta.role {
        // Role is typically "assistant" - we don't need to emit an event for this
    }

    // Handle content delta
    if let Some(content) = &delta.content
        && !content.is_empty()
    {
        // Emit OutputItemAdded before the first text delta so the turn
        // processor has an active_item to attach deltas to.
        if !*text_item_added {
            let added_item = ResponseItem::Message {
                id: Some("msg_assistant".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: String::new(),
                }],
                phase: None,
                metadata: None,
            };
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(added_item)))
                .await;
            *text_item_added = true;
        }
        accumulated_text.push_str(content);
        *output_emitted = true;
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(content.clone())))
            .await;
    }

    // Handle reasoning delta (e.g., DeepSeek thinking mode)
    if let Some(reasoning) = &delta.reasoning {
        let reasoning_text = match reasoning {
            serde_json::Value::String(s) => s.clone(),
            _ => String::new(),
        };
        if !reasoning_text.is_empty() {
            if !*reasoning_item_added {
                let reasoning_added = ResponseItem::Reasoning {
                    id: String::new(),
                    summary: Vec::new(),
                    content: Some(vec![
                        codex_protocol::models::ReasoningItemContent::ReasoningText {
                            text: String::new(),
                        },
                    ]),
                    encrypted_content: None,
                    metadata: None,
                };
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(reasoning_added)))
                    .await;
                *reasoning_item_added = true;
            }
            accumulated_reasoning.push_str(&reasoning_text);
            *output_emitted = true;
            let _ = tx_event
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: reasoning_text,
                    content_index: choice.index,
                }))
                .await;
        }
    }

    // Handle tool calls delta
    if let Some(tool_calls) = &delta.tool_calls {
        for tool_call_delta in tool_calls {
            let index = tool_call_delta.index;

            // Get or create entry for this tool call
            let entry = accumulated_tool_calls
                .entry(index)
                .or_insert((None, None, None));

            // Update ID if present and non-empty.
            // Some providers (e.g. qwen) send {"id": ""} in subsequent chunks,
            // which would overwrite the real call_id with an empty string.
            if let Some(id) = &tool_call_delta.id
                && !id.is_empty()
            {
                entry.0 = Some(id.clone());
            }

            // Update function name if present and non-empty.
            // OpenAI Chat Completions API sends the name only in the first chunk,
            // but subsequent chunks may include {"name": ""} which would
            // overwrite the accumulated name with an empty string.
            if let Some(func) = &tool_call_delta.function {
                if let Some(name) = &func.name
                    && !name.is_empty()
                {
                    entry.1 = Some(name.clone());
                }
                if let Some(args) = &func.arguments {
                    // Accumulate arguments
                    entry.2 = Some(entry.2.clone().unwrap_or_default() + args);
                }
            }
        }
    }

    // Handle finish reason — skip empty strings sent by some providers
    // (e.g. qwen) in intermediate chunks, which are not real finish signals.
    if let Some(finish_reason) = &choice.finish_reason {
        if finish_reason.is_empty() {
            return Ok(());
        }
        // Emit OutputItemDone for the assistant text message if we added one.
        // This must happen BEFORE tool call events so the TUI can finalize the
        // stream_controller while it is still active, preventing duplicate rendering.
        if *text_item_added && !*text_item_done {
            let done_item = ResponseItem::Message {
                id: Some("msg_assistant".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: accumulated_text.clone(),
                }],
                phase: None,
                metadata: None,
            };
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(done_item)))
                .await;
            *text_item_done = true;
        }

        // Emit OutputItemDone for reasoning if we started one.
        if *reasoning_item_added {
            let reasoning_done = ResponseItem::Reasoning {
                id: String::new(),
                summary: Vec::new(),
                content: Some(vec![
                    codex_protocol::models::ReasoningItemContent::ReasoningText {
                        text: accumulated_reasoning.clone(),
                    },
                ]),
                encrypted_content: None,
                metadata: None,
            };
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(reasoning_done)))
                .await;
        }

        // Emit accumulated tool calls with the proper event sequence
        // (OutputItemAdded -> ToolCallInputDelta -> OutputItemDone) so the
        // turn processor can establish the active tool before receiving deltas.
        for (index, (id, name, arguments)) in accumulated_tool_calls.drain() {
            if let (Some(id), Some(name)) = (id, name) {
                let args = normalize_tool_call_arguments(arguments);
                let function_call_item = ResponseItem::FunctionCall {
                    id: None,
                    namespace: namespace_map.get(&name).cloned(),
                    name: name.clone(),
                    arguments: args.clone(),
                    call_id: id.clone(),
                    metadata: None,
                };
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(
                        function_call_item.clone(),
                    )))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                *output_emitted = true;
                if tx_event
                    .send(Ok(ResponseEvent::ToolCallInputDelta {
                        item_id: format!("call_{index}"),
                        call_id: Some(id),
                        delta: args,
                    }))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(function_call_item)))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_client::TransportError;
    use futures::TryStreamExt;
    use tokio_test::io::Builder as IoBuilder;
    use tokio_util::io::ReaderStream;

    async fn collect_chat_events(chunks: &[&[u8]]) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut builder = IoBuilder::new();
        for chunk in chunks {
            builder.read(chunk);
        }

        let reader = builder.build();
        let stream = ReaderStream::new(reader)
            .map_err(|err: std::io::Error| TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_completions_sse(
            Box::pin(stream),
            tx,
            idle_timeout(),
            /*telemetry*/ None,
            HashMap::new(),
        ));

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    }

    fn idle_timeout() -> Duration {
        Duration::from_millis(1000)
    }

    #[tokio::test]
    async fn parses_content_delta() {
        let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
        let chunk2 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let chunk3 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n";
        let chunk4 = b"data: [DONE]\n\n";

        let events = collect_chat_events(&[chunk1, chunk2, chunk3, chunk4]).await;

        // Now includes OutputItemAdded + OutputItemDone events:
        // OutputItemAdded, OutputTextDelta("Hello"), OutputTextDelta(" world"),
        // OutputItemDone, Completed
        assert_eq!(events.len(), 5);
        assert!(matches!(&events[0], Ok(ResponseEvent::OutputItemAdded(_))));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == "Hello"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == " world"
        ));
        assert!(matches!(&events[3], Ok(ResponseEvent::OutputItemDone(_))));
        assert!(matches!(&events[4], Ok(ResponseEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn parses_tool_calls() {
        let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"loc\"}}]},\"finish_reason\":null}]}\n\n";
        let chunk2 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ation\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
        let chunk3 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let chunk4 = b"data: [DONE]\n\n";

        let events = collect_chat_events(&[chunk1, chunk2, chunk3, chunk4]).await;

        // Expected sequence:
        //   OutputItemAdded(FunctionCall)  ← establishes active tool
        //   ToolCallInputDelta             ← carries the arguments
        //   OutputItemDone(FunctionCall)   ← finalizes the tool call
        //   Completed
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::FunctionCall { name, .. }
            )) if name == "get_weather"
        ));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::ToolCallInputDelta { call_id, delta, .. })
            if call_id.as_deref() == Some("call_123")
            && delta == "{\"location\":\"SF\"}"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputItemDone(
                ResponseItem::FunctionCall { name, .. }
            )) if name == "get_weather"
        ));
        assert!(matches!(&events[3], Ok(ResponseEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn tool_calls_empty_name_in_delta_does_not_overwrite() {
        // Test that empty name in subsequent chunks does not overwrite accumulated name.
        // Some API implementations send {"name": ""} in subsequent chunks, which should
        // be ignored to preserve the name from the first chunk.
        let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\"\"}}]},\"finish_reason\":null}]}\n\n";
        // Second chunk has empty name - should NOT overwrite "shell"
        let chunk2 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"\",\"arguments\":\"\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
        let chunk3 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let chunk4 = b"data: [DONE]\n\n";

        let events = collect_chat_events(&[chunk1, chunk2, chunk3, chunk4]).await;

        // The tool name should still be "shell", not empty
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::FunctionCall { name, .. }
            )) if name == "shell"
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputItemDone(
                ResponseItem::FunctionCall { name, .. }
            )) if name == "shell"
        ));
        assert!(matches!(&events[3], Ok(ResponseEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn text_then_tool_calls_emits_item_done_before_tools() {
        // When a response has both text content AND tool calls, the OutputItemDone
        // for the text message must be emitted BEFORE the tool call events.
        // This ensures the TUI can finalize the stream_controller while it is still
        // active, preventing duplicate rendering of the text content.
        let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
        let chunk2 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me check that.\"},\"finish_reason\":null}]}\n\n";
        let chunk3 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
        let chunk4 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let chunk5 = b"data: [DONE]\n\n";

        let events = collect_chat_events(&[chunk1, chunk2, chunk3, chunk4, chunk5]).await;

        // Expected sequence:
        // 1. OutputItemAdded(Message) - for text
        // 2. OutputTextDelta("Let me check that.")
        // 3. OutputItemDone(Message) - text finalized BEFORE tool calls
        // 4. OutputItemAdded(FunctionCall) - tool call begins
        // 5. ToolCallInputDelta - tool arguments
        // 6. OutputItemDone(FunctionCall) - tool call finalized
        // 7. Completed
        assert_eq!(events.len(), 7);

        // 1. OutputItemAdded(Message)
        assert!(matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. }))
            if role == "assistant"
        ));

        // 2. OutputTextDelta
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == "Let me check that."
        ));

        // 3. OutputItemDone(Message) - text MUST be finalized before tool calls
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
            if role == "assistant"
        ));

        // 4. OutputItemAdded(FunctionCall)
        assert!(matches!(
            &events[3],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }))
            if name == "shell"
        ));

        // 5. ToolCallInputDelta
        assert!(matches!(
            &events[4],
            Ok(ResponseEvent::ToolCallInputDelta { delta, .. })
            if delta == "{\"cmd\":\"pwd\"}"
        ));

        // 6. OutputItemDone(FunctionCall)
        assert!(matches!(
            &events[5],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }))
            if name == "shell"
        ));

        // 7. Completed
        assert!(matches!(&events[6], Ok(ResponseEvent::Completed { .. })));
    }
    #[tokio::test]
    async fn text_then_tool_calls_done_only_emits_text_done_before_tools() {
        // Regression test: some providers terminate a streaming response with
        // [DONE] without first emitting a chunk that carries `finish_reason`
        // (or send finish_reason in a chunk that lacks any tool_calls, leaving
        // accumulated tool calls to be flushed by the [DONE] handler).
        // In that case the [DONE] branch must still emit OutputItemDone for
        // the assistant text BEFORE flushing tool calls; otherwise the TUI
        // observes the assistant text being finalized AFTER a tool call has
        // already consumed the stream_controller, which renders the assistant
        // message a second time.
        let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
        let chunk2 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me check that.\"},\"finish_reason\":null}]}\n\n";
        let chunk3 = b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
        // No finish_reason chunk; stream terminates directly with [DONE].
        let chunk4 = b"data: [DONE]\n\n";

        let events = collect_chat_events(&[chunk1, chunk2, chunk3, chunk4]).await;

        // Expected sequence:
        // 1. OutputItemAdded(Message)
        // 2. OutputTextDelta("Let me check that.")
        // 3. OutputItemDone(Message)             <- text finalized BEFORE tool calls
        // 4. OutputItemAdded(FunctionCall)
        // 5. ToolCallInputDelta
        // 6. OutputItemDone(FunctionCall)
        // 7. Completed
        assert_eq!(events.len(), 7);

        assert!(matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. }))
            if role == "assistant"
        ));
        assert!(matches!(
            &events[1],
            Ok(ResponseEvent::OutputTextDelta(s)) if s == "Let me check that."
        ));
        assert!(matches!(
            &events[2],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
            if role == "assistant"
        ));
        assert!(matches!(
            &events[3],
            Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }))
            if name == "shell"
        ));
        assert!(matches!(
            &events[4],
            Ok(ResponseEvent::ToolCallInputDelta { delta, .. })
            if delta == "{\"cmd\":\"pwd\"}"
        ));
        assert!(matches!(
            &events[5],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }))
            if name == "shell"
        ));
        assert!(matches!(&events[6], Ok(ResponseEvent::Completed { .. })));
    }
}
