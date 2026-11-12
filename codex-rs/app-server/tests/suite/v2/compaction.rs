//! End-to-end compaction flow tests.
//!
//! Phases:
//! 1) Arrange: mock Responses endpoints + config.
//! 2) Act: start a thread and submit multiple turns to trigger auto-compaction.
//! 3) Assert: verify item/started + item/completed notifications for context compaction.

use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RawResponseCompletedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ResponseUsageMetadata;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_config::types::AuthCredentialsStoreMode;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::ResponseTemplate;

use super::analytics::mount_analytics_capture;
use super::analytics::wait_for_matching_analytics_event;

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AUTO_COMPACT_LIMIT: i64 = 1_000;
const COMPACT_PROMPT: &str = "Summarize the conversation.";
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[derive(Clone, Copy)]
enum CompactionRoute {
    Local,
    Remote,
}

#[test_case(CompactionRoute::Local, Some(300), 429, "usage_limit_reached"; "local five hour")]
#[test_case(CompactionRoute::Remote, Some(10_080), 429, "usage_limit_reached"; "remote weekly")]
#[test_case(CompactionRoute::Local, None, 429, "usage_limit_reached"; "missing window")]
#[test_case(CompactionRoute::Remote, Some(300), 400, "invalid_request"; "other error")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_error_window_reaches_analytics(
    route: CompactionRoute,
    window: Option<u16>,
    status: u16,
    expected_error: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let seed = responses::sse(vec![
        responses::ev_assistant_message("seed", "FIRST_REPLY"),
        responses::ev_completed_with_tokens("seed", /*total_tokens*/ 120),
    ]);
    let mut error = json!({
        "type": if status == 429 { "usage_limit_reached" } else { "invalid_request_error" },
        "message": "request failed",
    });
    if let Some(window) = window {
        error["limit_window_minutes"] = json!(window);
    }
    let _requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(seed),
            ResponseTemplate::new(status).set_body_json(json!({ "error": error })),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    let mut config = compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000)
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", server.uri()));
    if let CompactionRoute::Remote = route {
        config = config
            .with_provider_name("OpenAI")
            .with_provider_config("requires_openai_auth = true");
    }
    config.write(codex_home.path())?;
    mount_analytics_capture(&server, codex_home.path()).await?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .without_managed_config()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let thread_id = start_thread(&mut mcp).await?;
    send_turn_and_wait(&mut mcp, &thread_id, "seed history").await?;
    let compact_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
            instructions: None,
            from_turn_id: None,
        })
        .await?;
    let _: ThreadCompactStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(compact_id)).await??;

    let event = wait_for_matching_analytics_event(&server, DEFAULT_READ_TIMEOUT, |event| {
        event["event_type"] == "codex_compaction_event"
            && event["event_params"]["thread_id"] == thread_id
    })
    .await?;
    let params = &event["event_params"];
    let implementation = match route {
        CompactionRoute::Local => "responses",
        CompactionRoute::Remote => "responses_compaction_v2",
    };
    let expected_window = if status == 429 { window } else { None };
    assert_eq!(
        json!([
            params["implementation"],
            params["phase"],
            params["status"],
            params["codex_error_kind"],
            params["usage_limit_window_minutes"],
        ]),
        json!([
            implementation,
            "standalone_turn",
            "failed",
            expected_error,
            expected_window
        ])
    );
    Ok(())
}

#[test_case(CompactionRoute::Local; "local")]
#[test_case(CompactionRoute::Remote; "streamed remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compaction_emits_started_and_completed_items(route: CompactionRoute) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let sse1 = responses::sse(vec![
        responses::ev_assistant_message("m1", "FIRST_REPLY"),
        responses::ev_completed_with_tokens("r1", /*total_tokens*/ 70_000),
    ]);
    let sse2 = responses::sse(vec![
        responses::ev_assistant_message("m2", "SECOND_REPLY"),
        responses::ev_completed_with_tokens("r2", /*total_tokens*/ 330_000),
    ]);
    let summary = match route {
        CompactionRoute::Local => responses::ev_assistant_message("m3", "LOCAL_SUMMARY"),
        CompactionRoute::Remote => serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "compaction",
                "encrypted_content": "ENCRYPTED_COMPACTION_SUMMARY",
            },
        }),
    };
    let sse3 = responses::sse(vec![
        summary,
        responses::ev_completed_with_tokens("r3", /*total_tokens*/ 200),
    ]);
    let sse4 = responses::sse(vec![
        responses::ev_assistant_message("m4", "FINAL_REPLY"),
        responses::ev_completed_with_tokens("r4", /*total_tokens*/ 120),
    ]);
    let requests = responses::mount_sse_sequence(&server, vec![sse1, sse2, sse3, sse4]).await;

    let codex_home = TempDir::new()?;
    let mut config = compaction_config(&server.uri(), /*auto_compact_limit*/ 200_000);
    if let CompactionRoute::Remote = route {
        config = config
            .with_provider_name("OpenAI")
            .with_provider_config("requires_openai_auth = true");
        write_chatgpt_auth(
            codex_home.path(),
            ChatGptAuthFixture::new("access-chatgpt").plan_type("pro"),
            AuthCredentialsStoreMode::File,
        )?;
    }
    config.write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let thread_id = start_thread(&mut mcp).await?;
    for message in ["first", "second", "third"] {
        send_turn_and_wait(&mut mcp, &thread_id, message).await?;
    }

    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;

    let ThreadItem::ContextCompaction {
        id: started_id,
        summary: started_summary,
        ..
    } = started.item
    else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction {
        id: completed_id,
        summary: completed_summary,
        ..
    } = completed.item
    else {
        unreachable!("completed item should be context compaction");
    };

    assert_eq!(started.thread_id, thread_id);
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(started_id, completed_id);
    assert_eq!(started_summary, None);
    // A local compaction hands its summary to clients; a remote one is opaque.
    let expected_summary = match route {
        CompactionRoute::Local => Some("LOCAL_SUMMARY".to_string()),
        CompactionRoute::Remote => None,
    };
    assert_eq!(completed_summary, expected_summary);

    let requests = requests.requests();
    assert_eq!(requests.len(), 4);
    let compact_request = &requests[2];
    assert_eq!(compact_request.path(), "/v1/responses");
    match route {
        CompactionRoute::Local => {
            assert!(
                compact_request
                    .inputs_of_type("compaction_trigger")
                    .is_empty()
            );
            assert!(compact_request.body_contains_text(COMPACT_PROMPT));
        }
        CompactionRoute::Remote => {
            assert_eq!(
                compact_request.inputs_of_type("compaction_trigger").len(),
                1
            );
            assert_eq!(
                requests[3].inputs_of_type("compaction")[0]["encrypted_content"],
                "ENCRYPTED_COMPACTION_SUMMARY"
            );
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_triggers_compaction_and_returns_empty_response() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let seed = responses::sse(vec![
        responses::ev_assistant_message("seed", "FIRST_REPLY"),
        responses::ev_completed_with_tokens("seed", /*total_tokens*/ 120),
    ]);
    let mut completed = responses::ev_completed_with_tokens("r1", /*total_tokens*/ 200);
    completed["response"]["usage_metadata"] = serde_json::json!({ "amount": "0.125" });
    completed["response"]["usage"]["extra"] = serde_json::json!({ "label": "example" });
    let expected_metadata = completed["response"]["usage"].clone();
    let sse = responses::sse(vec![
        responses::ev_assistant_message("m1", "MANUAL_COMPACT_SUMMARY"),
        completed,
    ]);
    let followup = responses::sse(vec![
        responses::ev_assistant_message("followup", "FINAL_REPLY"),
        responses::ev_completed_with_tokens("followup", /*total_tokens*/ 120),
    ]);
    let _responses = responses::mount_sse_sequence(&server, vec![seed, sse, followup]).await;

    let codex_home = TempDir::new()?;
    let initial_cwd = TempDir::new()?;
    let updated_cwd = TempDir::new()?;
    let extra_root = TempDir::new()?;
    let updated_roots = vec![updated_cwd.path().abs(), extra_root.path().abs()];
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000).write(codex_home.path())?;

    // Top-level cwd restoration uses host-native paths, not a foreign executor's paths.
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(initial_cwd.path().to_string_lossy().into_owned()),
            history_mode: Some(ThreadHistoryMode::Paginated),
            experimental_raw_events: true,
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(thread_req)).await??;
    let thread_id = thread.id;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.clone(),
            cwd: Some(updated_cwd.path().to_path_buf()),
            runtime_workspace_roots: Some(updated_roots.clone()),
            input: vec![V2UserInput::Text {
                text: "seed history".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    mcp.clear_message_buffer();

    let compact_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
            instructions: None,
            from_turn_id: None,
        })
        .await?;
    let _: ThreadCompactStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(compact_id)).await??;

    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let raw_completed: RawResponseCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("rawResponse/completed"),
    )
    .await??;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;
    wait_for_turn_completed(&mut mcp, &started.turn_id).await?;

    let ThreadItem::ContextCompaction { id: started_id, .. } = started.item else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction {
        id: completed_id, ..
    } = completed.item
    else {
        unreachable!("completed item should be context compaction");
    };

    assert_eq!(started.thread_id, thread_id);
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(started_id, completed_id);
    assert_eq!(
        raw_completed,
        RawResponseCompletedNotification {
            thread_id: thread_id.clone(),
            turn_id: started.turn_id,
            response_id: "r1".to_string(),
            usage_metadata: Some(ResponseUsageMetadata {
                amount: Some("0.125".to_string()),
                metadata: Some(expected_metadata),
            }),
            usage: Some(TokenUsageBreakdown {
                total_tokens: 200,
                input_tokens: 200,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        }
    );

    // A completed turn after compaction permits bounded replay. Neither this turn nor
    // resume resends settings, so restoring the updated cwd and roots depends on the
    // checkpoint; startup metadata does not contain the extra root.
    send_turn_and_wait(&mut mcp, &thread_id, "continue").await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse {
        cwd,
        runtime_workspace_roots,
        ..
    } = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(resume_id)).await??;
    assert_eq!(
        (cwd.as_path(), runtime_workspace_roots),
        (updated_cwd.path(), updated_roots)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_rejects_invalid_thread_id() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), AUTO_COMPACT_LIMIT).write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: "not-a-thread-id".to_string(),
            instructions: None,
            from_turn_id: None,
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(error.error.message.contains("invalid thread id"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_rejects_unknown_thread_id() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), AUTO_COMPACT_LIMIT).write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            instructions: None,
            from_turn_id: None,
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(error.error.message.contains("thread not found"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_from_turn_keeps_earlier_turns_verbatim() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let reply = |id: &str, text: &str| {
        responses::sse(vec![
            responses::ev_assistant_message(id, text),
            responses::ev_completed_with_tokens(&format!("r-{id}"), /*total_tokens*/ 100),
        ])
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            reply("m1", "FIRST_REPLY"),
            reply("m2", "SECOND_REPLY"),
            reply("m3", "THIRD_REPLY"),
            reply(
                "m4",
                "<analysis>draft</analysis>\n<summary>RECENT_SUMMARY</summary>",
            ),
            reply("m5", "FOURTH_REPLY"),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let thread_id = start_thread(&mut mcp).await?;
    send_turn_and_wait(&mut mcp, &thread_id, "first message").await?;
    let second_turn = send_turn_and_wait(&mut mcp, &thread_id, "second message").await?;
    send_turn_and_wait(&mut mcp, &thread_id, "third message").await?;

    let compact_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
            instructions: Some("User context: keep the API notes".to_string()),
            from_turn_id: Some(second_turn.clone()),
        })
        .await?;
    let _: ThreadCompactStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(compact_id)).await??;
    let started = wait_for_context_compaction_started(&mut mcp).await?;
    let completed = wait_for_context_compaction_completed(&mut mcp).await?;
    wait_for_turn_completed(&mut mcp, &started.turn_id).await?;

    let ThreadItem::ContextCompaction {
        from_turn_id: started_from,
        ..
    } = started.item
    else {
        unreachable!("started item should be context compaction");
    };
    let ThreadItem::ContextCompaction {
        summary,
        from_turn_id: completed_from,
        ..
    } = completed.item
    else {
        unreachable!("completed item should be context compaction");
    };
    assert_eq!(started_from.as_deref(), Some(second_turn.as_str()));
    assert_eq!(completed_from.as_deref(), Some(second_turn.as_str()));
    assert_eq!(summary.as_deref(), Some("RECENT_SUMMARY"));

    send_turn_and_wait(&mut mcp, &thread_id, "fourth message").await?;

    let requests = requests.requests();
    assert_eq!(requests.len(), 5);
    // One call over the whole history, the partial prompt with the user's context, and no tools.
    let compact_request = &requests[3];
    for text in [
        "first message",
        "second message",
        "third message",
        "THIRD_REPLY",
    ] {
        assert!(compact_request.body_contains_text(text), "{text}");
    }
    assert!(compact_request.body_contains_text("CONTEXT CHECKPOINT for the RECENT part"));
    assert!(compact_request.body_contains_text("User context: keep the API notes"));
    assert!(!compact_request.body_contains_text(COMPACT_PROMPT));
    let tools = compact_request.body_json()["tools"].clone();
    assert!(
        tools.is_null() || tools.as_array().is_some_and(Vec::is_empty),
        "the summary call offers no tools: {tools}"
    );

    // The next turn sees turn 1 verbatim, then the summary in place of turns 2 and 3.
    let next = &requests[4];
    let user_texts = next.message_input_texts("user");
    let first = user_texts
        .iter()
        .position(|text| text == "first message")
        .expect("turn 1 kept");
    let summary_at = user_texts
        .iter()
        .position(|text| text.ends_with("\nRECENT_SUMMARY"))
        .expect("summary present");
    assert!(first < summary_at, "{user_texts:?}");
    assert!(next.body_contains_text("FIRST_REPLY"));
    for gone in [
        "second message",
        "third message",
        "SECOND_REPLY",
        "THIRD_REPLY",
    ] {
        assert!(
            !next.body_contains_text(gone),
            "{gone} should be summarized away"
        );
    }
    assert!(next.body_contains_text("fourth message"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_from_turn_refusals() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_assistant_message("m1", "FIRST_REPLY"),
            responses::ev_completed_with_tokens("r1", /*total_tokens*/ 100),
        ])],
    )
    .await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let thread_id = start_thread(&mut mcp).await?;
    send_turn_and_wait(&mut mcp, &thread_id, "first message").await?;

    let refusal = |thread_id: String, from_turn_id: &str| ThreadCompactStartParams {
        thread_id,
        instructions: None,
        from_turn_id: Some(from_turn_id.to_string()),
    };
    // Empty: refused before the thread is looked up, so clients can probe for the field.
    for thread_id in [thread_id.clone(), "not-a-thread-id".to_string()] {
        let request_id = mcp
            .send_thread_compact_start_request(refusal(thread_id, ""))
            .await?;
        let error: JSONRPCError = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
        assert_eq!(error.error.message, "fromTurnId must not be empty");
    }
    // Unknown turn: refused, never a whole-thread compaction.
    let request_id = mcp
        .send_thread_compact_start_request(refusal(thread_id.clone(), "no-such-turn"))
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(
        error
            .error
            .message
            .contains("'no-such-turn' is not in the model's context"),
        "{}",
        error.error.message
    );
    assert_eq!(
        requests.requests().len(),
        1,
        "no compaction request was made"
    );

    Ok(())
}

#[test_case(true; "after the turn started")]
#[test_case(false; "right after turn start was accepted")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_from_turn_refuses_while_a_turn_runs(
    wait_for_turn_started: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let reply = |id: &str, text: &str| {
        responses::sse(vec![
            responses::ev_assistant_message(id, text),
            responses::ev_completed_with_tokens(&format!("r-{id}"), /*total_tokens*/ 100),
        ])
    };
    let requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(reply("m1", "FIRST_REPLY")),
            // Keeps the second turn running while the compaction is requested.
            responses::sse_response(reply("m2", "SECOND_REPLY"))
                .set_delay(std::time::Duration::from_secs(/*secs*/ 2)),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let thread_id = start_thread(&mut mcp).await?;
    let first_turn = send_turn_and_wait(&mut mcp, &thread_id, "first message").await?;

    let turn_request = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "second message".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn: running } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_request)).await??;
    if wait_for_turn_started {
        loop {
            let started: TurnStartedNotification =
                timeout(DEFAULT_READ_TIMEOUT, mcp.read_notification("turn/started")).await??;
            if started.turn.id == running.id {
                break;
            }
        }
    }

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id,
            instructions: None,
            from_turn_id: Some(first_turn),
        })
        .await?;
    let refusal = "a turn is in progress; summarize from a message once it has finished";
    // Read until both the answer to the compaction request and the running turn's end arrived.
    let mut answer = None;
    let mut warnings = Vec::new();
    let mut completed = None;
    while answer.is_none() || completed.is_none() {
        match timeout(DEFAULT_READ_TIMEOUT, mcp.read_next_message()).await?? {
            JSONRPCMessage::Error(error) if error.id == RequestId::Integer(request_id) => {
                answer = Some(Err(error));
            }
            JSONRPCMessage::Response(response) if response.id == RequestId::Integer(request_id) => {
                answer = Some(Ok(response));
            }
            JSONRPCMessage::Notification(notification) if notification.method == "warning" => {
                let warning: WarningNotification =
                    serde_json::from_value(notification.params.unwrap_or_default())?;
                warnings.push(warning.message);
            }
            JSONRPCMessage::Notification(notification)
                if notification.method == "turn/completed" =>
            {
                let done: TurnCompletedNotification =
                    serde_json::from_value(notification.params.unwrap_or_default())?;
                if done.turn.id == running.id {
                    completed = Some(done);
                }
            }
            _ => {}
        }
    }
    match answer.expect("answer read") {
        Err(error) => {
            assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
            assert_eq!(error.error.message, refusal);
        }
        // The app-server looked before the turn was running: the session refuses it when the op
        // is processed and says so with a warning instead of replacing the turn.
        Ok(_) => {
            assert!(
                !wait_for_turn_started,
                "a running turn must be refused by the app-server"
            );
            assert!(warnings.iter().any(|w| w == refusal), "{warnings:?}");
        }
    }

    // The running turn was not aborted and no compaction request was made.
    let completed = completed.expect("turn completed");
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(requests.requests().len(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_compact_start_from_turn_needs_local_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000)
        .with_provider_name("OpenAI")
        .with_provider_config("requires_openai_auth = true")
        .write(codex_home.path())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt").plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let thread_id = start_thread(&mut mcp).await?;

    let request_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id,
            instructions: None,
            from_turn_id: Some("turn-1".to_string()),
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(
        error.error.message.contains("needs local compaction"),
        "{}",
        error.error.message
    );

    Ok(())
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let thread_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(thread_id)).await??;
    Ok(thread.id)
}

async fn send_turn_and_wait(
    mcp: &mut TestAppServer,
    thread_id: &str,
    text: &str,
) -> Result<String> {
    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.to_string(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;
    wait_for_turn_completed(mcp, &turn.id).await?;
    Ok(turn.id)
}

async fn wait_for_turn_completed(mcp: &mut TestAppServer, turn_id: &str) -> Result<()> {
    loop {
        let completed: TurnCompletedNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_notification("turn/completed"),
        )
        .await??;
        if completed.turn.id == turn_id {
            return Ok(());
        }
    }
}

async fn wait_for_context_compaction_started(
    mcp: &mut TestAppServer,
) -> Result<ItemStartedNotification> {
    loop {
        let started: ItemStartedNotification =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_notification("item/started")).await??;
        if let ThreadItem::ContextCompaction { .. } = started.item {
            return Ok(started);
        }
    }
}

async fn wait_for_context_compaction_completed(
    mcp: &mut TestAppServer,
) -> Result<ItemCompletedNotification> {
    loop {
        let completed: ItemCompletedNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_notification("item/completed"),
        )
        .await??;
        if let ThreadItem::ContextCompaction { .. } = completed.item {
            return Ok(completed);
        }
    }
}

fn compaction_config(server_uri: &str, auto_compact_limit: i64) -> MockResponsesConfig {
    MockResponsesConfig::new(server_uri)
        .with_root_config(&format!(
            "compact_prompt = \"{COMPACT_PROMPT}\"\nmodel_auto_compact_token_limit = {auto_compact_limit}"
        ))
        .with_provider_config("supports_websockets = false")
}
