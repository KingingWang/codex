//! Reasoning/thinking item lifecycle for the Chat Completions SSE decoder.
//!
//! Chat Completions providers with a thinking mode (DeepSeek, Qwen, Kimi, ...)
//! stream `delta.reasoning` chunks before the visible answer. The canonical
//! Responses-shaped event stream carries one output item per segment and closes
//! it as soon as the segment ends, so this module owns that lifecycle for the
//! reasoning item: open it on the first delta, forward the deltas, and close it
//! before any other output item is opened.
//!
//! Closing early is what keeps downstream consumers correct. `codex-core`'s turn
//! processor records and announces items in `OutputItemDone` order, so a
//! reasoning item closed only at `finish_reason`/`[DONE]` lands *after* the
//! assistant message: clients that render on item completion (mindfs, and any
//! app-server v2 consumer reading `ThreadItem::Reasoning`) show the answer
//! before the thinking, and `build_chat_completions_request` — which attaches a
//! reasoning item to the *next* assistant output — can no longer find one.

use crate::common::ResponseEvent;
use crate::error::ApiError;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use tokio::sync::mpsc;

type EventSender = mpsc::Sender<Result<ResponseEvent, ApiError>>;

/// Lifecycle state of the reasoning item currently being streamed.
#[derive(Debug, Default)]
pub(super) struct ReasoningStream {
    /// Text accumulated for the open reasoning item; it becomes the completed
    /// item's summary.
    accumulated: String,
    /// Stable id shared by `OutputItemAdded`, the live `ReasoningContentDelta`
    /// events (via the turn processor's active item), and `OutputItemDone`.
    ///
    /// A fixed id is required because the turn processor only inherits the
    /// active item's id when the done event arrives while that item is still
    /// active. Clients such as codex-acp dedup reasoning by item id
    /// (`seenReasoningDeltaItemIds`), and a mismatched completed-item id would
    /// render the same thinking a second time.
    item_id: Option<ResponseItemId>,
    /// Whether `OutputItemAdded` has been emitted for the open item.
    added: bool,
    /// Whether `OutputItemDone` has been emitted for the open item. Keeps the
    /// transition, `finish_reason`, and `[DONE]` flush points from each
    /// emitting a duplicate done event for the same item.
    done: bool,
    /// Number of reasoning items already closed. Providers normally emit a
    /// single thinking block, but one that resumes after the answer started is
    /// appended as a new item, which needs a distinct id.
    closed_segments: usize,
}

impl ReasoningStream {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Append a reasoning delta, opening the reasoning item on first use.
    ///
    /// Returns `false` once the event receiver is gone so callers can stop.
    pub(super) async fn push_delta(
        &mut self,
        tx_event: &EventSender,
        choice_index: i64,
        text: &str,
    ) -> bool {
        if self.done {
            // Thinking resumed after the item was closed. Start a fresh item
            // instead of appending to a completed one.
            self.added = false;
            self.done = false;
            self.accumulated.clear();
            self.closed_segments += 1;
        }
        if !self.added {
            let item_id = reasoning_item_id(choice_index, self.closed_segments);
            self.item_id = Some(item_id.clone());
            let added = ResponseItem::Reasoning {
                id: Some(item_id),
                summary: Vec::new(),
                content: Some(vec![ReasoningItemContent::ReasoningText {
                    text: String::new(),
                }]),
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            };
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(added)))
                .await
                .is_err()
            {
                return false;
            }
            self.added = true;
        }
        self.accumulated.push_str(text);
        // Reasoning is not a deliverable: callers track `output_emitted` from
        // assistant text and tool calls only, so a reasoning-only response is
        // still retried by the turn layer.
        tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: text.to_string(),
                content_index: choice_index,
            }))
            .await
            .is_ok()
    }

    /// Close the open reasoning item. No-op when nothing is open.
    ///
    /// Call this before opening or closing any other output item so the
    /// thinking stays ahead of the answer and the tool calls.
    ///
    /// Returns `false` once the event receiver is gone so callers can stop.
    pub(super) async fn close(&mut self, tx_event: &EventSender) -> bool {
        if !self.added || self.done {
            return true;
        }
        // The full text goes into `summary` (the Responses API summary path that
        // codex-acp's `seen_reasoning_deltas` dedup covers) and `content` stays
        // empty so no second complete-reasoning event is emitted.
        let done = ResponseItem::Reasoning {
            id: self.item_id.clone(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: std::mem::take(&mut self.accumulated),
            }],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        };
        self.done = true;
        tx_event
            .send(Ok(ResponseEvent::OutputItemDone(done)))
            .await
            .is_ok()
    }
}

fn reasoning_item_id(choice_index: i64, segment: usize) -> ResponseItemId {
    ResponseItemId::from_server(if segment == 0 {
        format!("reasoning_{choice_index}")
    } else {
        format!("reasoning_{choice_index}_{segment}")
    })
}
