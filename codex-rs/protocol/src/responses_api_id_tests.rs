use pretty_assertions::assert_eq;

use crate::ResponseItemId;
use crate::models::ContentItem;
use crate::models::ReasoningItemContent;
use crate::models::ResponseItem;

fn reasoning_with_id(id: Option<&str>) -> ResponseItem {
    ResponseItem::Reasoning {
        id: id.map(|id| ResponseItemId::from_server(id.to_string())),
        summary: Vec::new(),
        content: Some(vec![ReasoningItemContent::ReasoningText {
            text: "thinking".to_string(),
        }]),
        encrypted_content: Some("signature".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn accepts_only_ids_issued_by_the_responses_api() {
    for (id, expected) in [
        (Some("rs_abc"), true),
        (None, true),
        // Synthesized by the Anthropic and chat-completions adapters.
        (Some("reasoning_0"), false),
        // Prefix of another variant, so still not a valid reasoning ID.
        (Some("msg_abc"), false),
        (Some("rs-abc"), false),
        (Some("rs_"), false),
        (Some("rs"), false),
    ] {
        assert_eq!(
            reasoning_with_id(id).has_responses_api_id(),
            expected,
            "{id:?}"
        );
    }
}

#[test]
fn message_ids_use_their_own_prefix() {
    let message = |id: &str| ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "answer".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(message("msg_abc").has_responses_api_id(), true);
    assert_eq!(message("rs_abc").has_responses_api_id(), false);
}

#[test]
fn clearing_a_foreign_id_omits_the_field_and_keeps_the_signature() {
    let mut item = reasoning_with_id(Some("reasoning_0"));
    item.set_id(None);

    assert_eq!(
        serde_json::to_value(&item).expect("serialize reasoning item"),
        serde_json::json!({
            "type": "reasoning",
            "summary": [],
            "content": [{"type": "reasoning_text", "text": "thinking"}],
            "encrypted_content": "signature",
        })
    );
}
