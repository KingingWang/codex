use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test_case::test_case("insufficient_quota"; "quota")]
#[test_case::test_case("credit_balance_exhausted"; "credit_balance")]
#[test_case::test_case("organization_spend_limit_exceeded"; "organization_spend_limit")]
#[test_case::test_case("project_spend_limit_exceeded"; "project_spend_limit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_exceeded_emits_single_error_event(code: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Fork: quota-class failures are stream-retryable, so the first failure is retried
    // once within the stream budget before the terminal error event is emitted.
    let failure = || {
        sse_response(sse(vec![
            ev_response_created("resp-1"),
            json!({
                "type": "response.failed",
                "response": {
                    "id": "resp-1",
                    "error": {
                        "code": code,
                        "message": "You exceeded your current quota, please check your plan and billing details."
                    }
                }
            }),
        ]))
    };
    let response_mock = mount_response_sequence(&server, vec![failure(), failure()]).await;

    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "quota?".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut error_events = 0;
    let mut stream_error_events = 0;

    loop {
        let event = wait_for_event(&test.codex, |_| true).await;

        match event {
            EventMsg::Error(err) => {
                error_events += 1;
                assert_eq!(
                    err.message,
                    "Quota exceeded. Check your plan and billing details."
                );
                assert_eq!(
                    err.codex_error_info,
                    Some(CodexErrorInfo::UsageLimitExceeded)
                );
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(error_events, 1, "expected exactly one Codex:Error event");
    assert_eq!(
        stream_error_events, 1,
        "expected one stream-error retry notice"
    );
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}
