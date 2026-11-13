//! `thread/title/generate` runs the native title worker on a hidden thread and saves the name.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTitleGenerateParams;
use codex_app_server_protocol::ThreadTitleGenerateResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const WAIT: Duration = Duration::from_secs(60);
const TITLE_PROMPT_MARKER: &str = "Generate a concise, single-line task title";
const FIRST_MESSAGE: &str = "Fix the login flow when the session cookie expires";

#[derive(Clone, Copy)]
enum TitleReply {
    Title,
    Fail,
    Slow,
}

struct Harness {
    _home: TempDir,
    _server: MockServer,
    requests: ResponseMock,
    client: TestAppServer,
}

/// The user's turns answer after `user_delay`; hidden title turns answer per `title`.
async fn harness(title: TitleReply, user_delay: Duration) -> Result<Harness> {
    let server = responses::start_mock_server().await;
    let requests = responses::mount_response_handler(&server, move |request| {
        if request.body_contains_text(TITLE_PROMPT_MARKER) {
            return match title {
                TitleReply::Title => assistant(r#"{"title":"Fix login expiry"}"#),
                TitleReply::Fail => responses::sse_response(responses::sse_failed(
                    "resp-title",
                    "server_error",
                    "title model failed",
                )),
                TitleReply::Slow => {
                    assistant(r#"{"title":"Too late"}"#).set_delay(Duration::from_secs(20))
                }
            };
        }
        assistant("Done").set_delay(user_delay)
    })
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let client = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized_with_timeout(WAIT)
        .await?;
    Ok(Harness {
        _home: home,
        _server: server,
        requests,
        client,
    })
}

fn assistant(text: &str) -> ResponseTemplate {
    responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp"),
        responses::ev_assistant_message("msg", text),
        responses::ev_completed("resp"),
    ]))
}

fn input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

impl Harness {
    fn title_requests(&self) -> usize {
        self.requests
            .requests()
            .iter()
            .filter(|request| request.body_contains_text(TITLE_PROMPT_MARKER))
            .count()
    }

    async fn start_thread(&mut self) -> Result<String> {
        let id = self
            .client
            .send_thread_start_request_with_auto_env(ThreadStartParams::default())
            .await?;
        let ThreadStartResponse { thread, .. } =
            timeout(WAIT, self.client.read_response(id)).await??;
        Ok(thread.id)
    }

    async fn run_first_turn(&mut self, thread_id: &str) -> Result<()> {
        let completed = timeout(
            WAIT,
            self.client
                .start_turn_and_wait_for_completion(TurnStartParams {
                    thread_id: thread_id.to_string(),
                    input: input(FIRST_MESSAGE),
                    ..Default::default()
                }),
        )
        .await??;
        assert_eq!(completed.turn.status, TurnStatus::Completed);
        Ok(())
    }

    async fn send_generate(&mut self, thread_id: &str) -> Result<i64> {
        self.client
            .send_raw_request(
                "thread/title/generate",
                Some(serde_json::to_value(ThreadTitleGenerateParams {
                    thread_id: thread_id.to_string(),
                })?),
            )
            .await
    }

    async fn generate(&mut self, thread_id: &str) -> Result<ThreadTitleGenerateResponse> {
        let id = self.send_generate(thread_id).await?;
        timeout(WAIT, self.client.read_response(id)).await?
    }

    async fn saved_name(&mut self, thread_id: &str) -> Result<Option<String>> {
        let id = self
            .client
            .send_thread_read_request(ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns: false,
            })
            .await?;
        let ThreadReadResponse { thread, .. } =
            timeout(WAIT, self.client.read_response(id)).await??;
        Ok(thread.name)
    }

    async fn set_name(&mut self, thread_id: &str, name: &str) -> Result<()> {
        let id = self
            .client
            .send_thread_set_name_request(ThreadSetNameParams {
                thread_id: thread_id.to_string(),
                name: name.to_string(),
            })
            .await?;
        let _: ThreadSetNameResponse = timeout(WAIT, self.client.read_response(id)).await??;
        Ok(())
    }

    async fn loaded_threads(&mut self) -> Result<Vec<String>> {
        let id = self
            .client
            .send_thread_loaded_list_request(ThreadLoadedListParams::default())
            .await?;
        let ThreadLoadedListResponse { data, .. } =
            timeout(WAIT, self.client.read_response(id)).await??;
        Ok(data)
    }
}

#[tokio::test]
async fn generated_title_is_saved_and_announced() -> Result<()> {
    let mut harness = harness(TitleReply::Title, Duration::ZERO).await?;
    let thread_id = harness.start_thread().await?;
    harness.run_first_turn(&thread_id).await?;

    let response = harness.generate(&thread_id).await?;
    assert_eq!(
        response,
        ThreadTitleGenerateResponse {
            name: Some("Fix login expiry".to_string()),
            generated: true,
        }
    );
    let notification: ThreadNameUpdatedNotification = timeout(
        WAIT,
        harness.client.read_notification("thread/name/updated"),
    )
    .await??;
    assert_eq!(
        notification,
        ThreadNameUpdatedNotification {
            thread_id: thread_id.clone(),
            thread_name: Some("Fix login expiry".to_string()),
        }
    );
    assert_eq!(
        harness.saved_name(&thread_id).await?.as_deref(),
        Some("Fix login expiry")
    );

    let title_request = harness
        .requests
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text(TITLE_PROMPT_MARKER))
        .expect("title request");
    assert_eq!(title_request.body_contains_text(FIRST_MESSAGE), true);
    let body = title_request.body_json();
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(
        body["text"]["format"]["schema"]["required"],
        serde_json::json!(["title"])
    );
    assert_eq!(harness.title_requests(), 1);
    // The hidden thread is gone and was never listed.
    assert_eq!(harness.loaded_threads().await?, vec![thread_id]);
    Ok(())
}

#[tokio::test]
async fn named_thread_keeps_its_name_without_a_model_call() -> Result<()> {
    let mut harness = harness(TitleReply::Title, Duration::ZERO).await?;
    let thread_id = harness.start_thread().await?;
    harness.run_first_turn(&thread_id).await?;
    harness.set_name(&thread_id, "Manual name").await?;

    assert_eq!(
        harness.generate(&thread_id).await?,
        ThreadTitleGenerateResponse {
            name: Some("Manual name".to_string()),
            generated: false,
        }
    );
    assert_eq!(harness.title_requests(), 0);
    Ok(())
}

#[tokio::test]
async fn model_failure_saves_the_first_message_summary() -> Result<()> {
    let mut harness = harness(TitleReply::Fail, Duration::ZERO).await?;
    let thread_id = harness.start_thread().await?;
    harness.run_first_turn(&thread_id).await?;

    assert_eq!(
        harness.generate(&thread_id).await?,
        ThreadTitleGenerateResponse {
            name: Some(FIRST_MESSAGE.to_string()),
            generated: false,
        }
    );
    assert_eq!(harness.title_requests(), 1);
    assert_eq!(
        harness.saved_name(&thread_id).await?.as_deref(),
        Some(FIRST_MESSAGE)
    );
    Ok(())
}

#[tokio::test]
async fn rename_during_generation_is_not_overwritten() -> Result<()> {
    let mut harness = harness(TitleReply::Slow, Duration::ZERO).await?;
    let thread_id = harness.start_thread().await?;
    harness.run_first_turn(&thread_id).await?;

    let generate_id = harness.send_generate(&thread_id).await?;
    // Wait until the hidden title turn is talking to the model, then rename.
    timeout(WAIT, async {
        while harness.title_requests() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    harness.set_name(&thread_id, "Manual name").await?;

    let response: ThreadTitleGenerateResponse =
        timeout(WAIT, harness.client.read_response(generate_id)).await??;
    assert_eq!(
        response,
        ThreadTitleGenerateResponse {
            name: Some("Manual name".to_string()),
            generated: false,
        }
    );
    assert_eq!(
        harness.saved_name(&thread_id).await?.as_deref(),
        Some("Manual name")
    );
    Ok(())
}

#[tokio::test]
async fn generation_right_after_turn_start_titles_the_running_first_turn() -> Result<()> {
    let mut harness = harness(TitleReply::Title, Duration::from_secs(3)).await?;
    let thread_id = harness.start_thread().await?;
    let turn_id = harness
        .client
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: input(FIRST_MESSAGE),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = timeout(WAIT, harness.client.read_response(turn_id)).await??;

    let response = harness.generate(&thread_id).await?;
    assert_eq!(
        response,
        ThreadTitleGenerateResponse {
            name: Some("Fix login expiry".to_string()),
            generated: true,
        }
    );

    // The user's turn, still waiting on its delayed model reply, completes undisturbed.
    let completed: TurnCompletedNotification =
        timeout(WAIT, harness.client.read_notification("turn/completed")).await??;
    assert_eq!(completed.turn.id, turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(completed.turn.error, None);
    assert_eq!(harness.title_requests(), 1);
    Ok(())
}

#[tokio::test]
async fn thread_without_user_message_has_nothing_to_name() -> Result<()> {
    let mut harness = harness(TitleReply::Title, Duration::ZERO).await?;
    let thread_id = harness.start_thread().await?;

    assert_eq!(
        harness.generate(&thread_id).await?,
        ThreadTitleGenerateResponse {
            name: None,
            generated: false,
        }
    );
    assert_eq!(harness.title_requests(), 0);
    Ok(())
}

#[tokio::test]
async fn unknown_thread_is_an_invalid_request() -> Result<()> {
    let mut harness = harness(TitleReply::Title, Duration::ZERO).await?;
    let missing = "00000000-0000-7000-8000-000000000001";
    let id = harness.send_generate(missing).await?;
    let error: JSONRPCError = timeout(
        WAIT,
        harness
            .client
            .read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert_eq!(error.error.message, format!("thread not found: {missing}"));
    Ok(())
}
