//! IDs synthesized for non-Responses providers, which do not supply item IDs.

use codex_protocol::ResponseItemId;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static NEXT_ITEM_ID: AtomicU64 = AtomicU64::new(/*v*/ 0);

/// Assigns one ID per item, including across model requests in the same turn.
pub(crate) fn unique_item_id(prefix: &str) -> ResponseItemId {
    ResponseItemId::with_suffix(prefix, NEXT_ITEM_ID.fetch_add(/*val*/ 1, Ordering::Relaxed))
}
