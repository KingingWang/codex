//! Integration test: a chat-completions turn that streams thinking before the
//! answer must announce the reasoning item as completed BEFORE the assistant
//! message.
//!
//! Clients that render on item completion — mindfs and any other app-server v2
//! consumer reading `ThreadItem::Reasoning` — append items in `item.completed`
//! order. While codex-api deferred the reasoning `OutputItemDone` to the
//! `finish_reason`/`[DONE]` flush, the answer was announced first, so those
//! frontends rendered answer -> thinking -> tool call. The same ordering is
//! what `build_chat_completions_request` relies on to attach `reasoning_content`
//! to the assistant message that follows the reasoning item.

use codex_core::TurnInputRequest;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_completes_reasoning_before_agent_message() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let chunk = |delta: &str| {
        format!(
            "data: {{\"id\":\"chatcmpl-order\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-5.4\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":null}}]}}\n\n"
        )
    };
    let body = format!(
        "{}{}{}data: {{\"id\":\"chatcmpl-order\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"gpt-5.4\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
        chunk("{\"role\":\"assistant\"}"),
        chunk("{\"reasoning\":\"thinking first\"}"),
        chunk("{\"content\":\"answer second\"}"),
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_raw(body, "text/event-stream"))
        .expect(/*requests*/ 1)
        .mount(&server)
        .await;

    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.wire_api = WireApi::Chat;
    provider.chat_stream = true;
    provider.supports_websockets = false;
    let test = test_codex()
        .with_config(move |config| config.model_provider = provider)
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "say something".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut item_lifecycle: Vec<&str> = Vec::new();
    let mut reasoning_summary: Vec<String> = Vec::new();
    let mut agent_message_text = String::new();
    loop {
        match test.codex.next_event().await?.msg {
            EventMsg::ItemStarted(ItemStartedEvent { item, .. }) => match item {
                TurnItem::Reasoning(_) => item_lifecycle.push("reasoning.started"),
                TurnItem::AgentMessage(_) => item_lifecycle.push("agentMessage.started"),
                _ => {}
            },
            EventMsg::ItemCompleted(ItemCompletedEvent { item, .. }) => match item {
                TurnItem::Reasoning(reasoning) => {
                    item_lifecycle.push("reasoning.completed");
                    reasoning_summary = reasoning.summary_text;
                }
                TurnItem::AgentMessage(message) => {
                    item_lifecycle.push("agentMessage.completed");
                    agent_message_text = message
                        .content
                        .iter()
                        .map(|entry| match entry {
                            AgentMessageContent::Text { text } => text.as_str(),
                        })
                        .collect();
                }
                _ => {}
            },
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(err) => panic!("turn failed: {}", err.message),
            _ => {}
        }
    }

    // The reasoning item must be fully completed before the assistant message
    // is even announced, and each item is announced exactly once: finalizing
    // reasoning while the message is the turn processor's active item steals
    // that active item, which makes the message completion re-emit
    // `item.started` (a duplicate render for clients that honour it).
    assert_eq!(
        item_lifecycle,
        vec![
            "reasoning.started",
            "reasoning.completed",
            "agentMessage.started",
            "agentMessage.completed",
        ]
    );
    assert_eq!(reasoning_summary, vec!["thinking first".to_string()]);
    assert_eq!(agent_message_text, "answer second");
    Ok(())
}
