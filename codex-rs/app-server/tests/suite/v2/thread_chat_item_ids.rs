//! Chat Completions must preserve every model response within one stored turn.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Copy)]
enum ChatMode {
    NonStreaming,
    Streaming,
}

fn response_template(response: Value, mode: ChatMode) -> ResponseTemplate {
    match mode {
        ChatMode::NonStreaming => {
            ResponseTemplate::new(/*status*/ 200).set_body_json(response)
        }
        ChatMode::Streaming => {
            let choice = &response["choices"][0];
            let message = &choice["message"];
            let deltas = [
                (json!({"reasoning": message["reasoning"]}), Value::Null),
                (json!({"content": message["content"]}), Value::Null),
                (
                    json!({"tool_calls": message["tool_calls"]}),
                    choice["finish_reason"].clone(),
                ),
            ];
            let mut body = String::new();
            for (delta, finish_reason) in deltas {
                let chunk = json!({
                    "id": response["id"],
                    "choices": [{
                        "index": 0,
                        "delta": delta,
                        "finish_reason": finish_reason,
                    }],
                });
                body.push_str(&format!("data: {chunk}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            ResponseTemplate::new(/*status*/ 200).set_body_raw(body, "text/event-stream")
        }
    }
}

#[test_case::test_case(ChatMode::NonStreaming; "non_streaming")]
#[test_case::test_case(ChatMode::Streaming; "streaming")]
#[tokio::test]
async fn chat_round_trips_keep_all_items_in_stored_turns_list(mode: ChatMode) -> Result<()> {
    let server = MockServer::start().await;
    let first_response = json!({
        "id": "chatcmpl-first",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning": "thinking first",
                "content": "first answer",
                "tool_calls": [{
                    "index": 0,
                    "id": "call-first",
                    "type": "function",
                    "function": {"name": "exec_command", "arguments": r#"{"cmd":"echo hello"}"#}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response_template(first_response, mode))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let second_response = json!({
        "id": "chatcmpl-second",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning": "thinking second",
                "content": "second answer"
            },
            "finish_reason": "stop"
        }]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response_template(second_response, mode))
        .expect(1)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let chat_stream = matches!(mode, ChatMode::Streaming);
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.5")
        .with_sandbox_mode("danger-full-access")
        .with_extra_config(&format!(
            "[model_providers.chat_test]\nname = \"Chat test\"\nbase_url = \"{}/v1\"\nwire_api = \"chat\"\nchat_stream = {chat_stream}\n",
            server.uri()
        ))
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model_provider: Some("chat_test".to_string()),
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } = mcp.read_response(start_id).await?;
    let completed = timeout(
        std::time::Duration::from_secs(30),
        mcp.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Answer after using the tool".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    let read_id = mcp
        .send_thread_turns_list_request(ThreadTurnsListParams {
            thread_id: thread.id,
            cursor: None,
            limit: None,
            sort_direction: Some(SortDirection::Asc),
            items_view: Some(TurnItemsView::Full),
        })
        .await?;
    let ThreadTurnsListResponse { data, .. } = mcp.read_response(read_id).await?;
    assert_eq!(data.len(), 1);
    let items = &data[0].items;
    let reasoning = items
        .iter()
        .filter_map(|item| match item {
            ThreadItem::Reasoning { id, summary, .. } => Some((id.as_str(), summary.as_slice())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let messages = items
        .iter()
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { id, text, .. } => Some((id.as_str(), text.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning.len(),
        2,
        "each response must retain its reasoning"
    );
    assert_eq!(messages.len(), 2, "each response must retain its message");
    assert_eq!(reasoning[0].1, &["thinking first".to_string()]);
    assert_eq!(reasoning[1].1, &["thinking second".to_string()]);
    assert_eq!(
        messages.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
        vec!["first answer", "second answer"]
    );
    assert!(messages.iter().all(|(id, _)| id.starts_with("msg_")));
    assert_eq!(
        reasoning
            .iter()
            .map(|(id, _)| *id)
            .chain(messages.iter().map(|(id, _)| *id))
            .collect::<HashSet<_>>()
            .len(),
        reasoning.len() + messages.len(),
        "all model-produced message and reasoning IDs must be distinct"
    );
    Ok(())
}
