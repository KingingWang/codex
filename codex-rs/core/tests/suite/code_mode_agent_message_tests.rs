//! Exec-spawned tasks reach the child model as attributed plaintext.

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::matchers::header;

#[test_case(false; "websocket")]
#[test_case(true; "grpc")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_spawn_delivers_plaintext(use_grpc: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.tool_mode = Some(ToolMode::CodeMode);
            model.use_responses_lite = true;
        })
        .with_config(move |config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CodeMode,
                Feature::CodeModeHost,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
            config
                .features
                .set_enabled(Feature::CodeModeHostGrpc, use_grpc)
                .expect("select code-mode transport");
            config.multi_agent_v2.non_code_mode_only = false;
        })
        .build_with_auto_env(&server)
        .await?;
    let root_header = test.session_configured.thread_id.to_string();
    let mut created = test.thread_manager.subscribe_thread_created();

    mount_sse_once_match(
        &server,
        header("thread-id", root_header.as_str()),
        sse(vec![
            ev_response_created("spawn"),
            ev_custom_tool_call(
                "spawn-worker",
                "exec",
                r#"text(await tools.agent_tools__spawn_agent({task_name: "worker", message: "Summarize the three stories.", fork_turns: "all"}));"#,
            ),
            ev_completed("spawn"),
        ]),
    )
    .await;
    let root_requests = mount_sse_once_match(
        &server,
        header("thread-id", root_header.as_str()),
        sse(vec![
            ev_assistant_message("delegated", "The worker started."),
            ev_completed("delegated"),
        ]),
    )
    .await;
    let child_requests = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request
                .headers
                .get("thread-id")
                .is_some_and(|id| id != root_header.as_str())
        },
        sse(vec![ev_response_created("child"), ev_completed("child")]),
    )
    .await;
    test.submit_turn("Delegate the summary task.").await?;
    let child_id = created.try_recv().with_context(|| {
        let outputs = root_requests
            .requests()
            .iter()
            .flat_map(|request| {
                let mut outputs = request.inputs_of_type("function_call_output");
                outputs.extend(request.inputs_of_type("custom_tool_call_output"));
                outputs
            })
            .collect::<Vec<_>>();
        format!("Code Mode did not create a child; exec outputs: {outputs:?}")
    })?;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let child_request = child_requests
        .requests()
        .into_iter()
        .find(|request| request.header("thread-id") == Some(child_id.to_string()))
        .expect("child receives its task");
    assert_eq!(
        strip_response_item_ids_from_json(strip_metadata_from_json(Value::Array(
            child_request.inputs_of_type("agent_message"),
        ))),
        json!([{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/worker",
            "content": [{
                "type": "input_text",
                "text": "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\nSummarize the three stories.",
            }],
        }]),
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);
    Ok(())
}
