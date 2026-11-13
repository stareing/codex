//! `thread/title/generate`: the TUI's native thread-title worker, run inside the app-server.
//!
//! The title comes from one structured turn on a hidden thread started in-process through the
//! `ThreadManager`, with the semantics of the TUI's temporary structured request: ephemeral,
//! `ThreadSource::Feature("thread_title")`, read-only permissions, approval `never`, no workspace
//! roots, environments, dynamic tools, MCP servers, or side features, and 30 s per step. The hidden
//! thread never gets a listener, so connections receive none of its notifications, and it is
//! marked so `thread/loaded/list` skips it. Interaction requests other than a blocking question
//! end the attempt; a blocking question is held until the turn times out, as on desktop.
//!
//! Success saves the title like `thread/name/set`. Any failure saves the first-message summary
//! instead, unless the thread got a name meanwhile: a manual name always wins.

use super::thread_processor::ThreadReadViewError;
use super::thread_processor::thread_read_view_error;
use super::*;
use codex_app_server_protocol::ThreadTitleGenerateParams;
use codex_app_server_protocol::ThreadTitleGenerateResponse;
use codex_core::McpServersDisabled;
use codex_core::NewThread;
use codex_otel::auth_storage::AuthStorageOriginator;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput as CoreUserInput;
use codex_thread_title::STRUCTURED_RESPONSE_MAX_BYTES;
use codex_thread_title::STRUCTURED_TURN_TIMEOUT;
use codex_thread_title::THREAD_TITLE_THREAD_SOURCE;
use codex_thread_title::fallback_thread_title;
use codex_thread_title::hidden_thread_config_overrides;
use codex_thread_title::parse_thread_title;
use codex_thread_title::thread_title_output_schema;
use codex_thread_title::thread_title_prompt;
use codex_thread_title::thread_title_source;
use tokio::sync::watch;

type TitleResult = Result<ThreadTitleGenerateResponse, JSONRPCErrorError>;

/// How long to wait for a running first turn to record its user message.
const FIRST_MESSAGE_WAIT: Duration = Duration::from_secs(10);
const FIRST_MESSAGE_POLL: Duration = Duration::from_millis(50);

/// Marks hidden title threads from startup through cleanup.
pub(crate) struct TitleThread;

/// One title worker per thread; later calls for the same thread wait for its result.
#[derive(Default)]
pub(crate) struct ThreadTitleWorkers {
    pending: std::sync::Mutex<HashMap<ThreadId, PendingTitle>>,
}

struct PendingTitle {
    cancellation: CancellationToken,
    result: watch::Receiver<Option<TitleResult>>,
}

/// What a caller does after registering interest in a thread's title.
enum TitleWorkerSlot {
    Join(watch::Receiver<Option<TitleResult>>),
    Start {
        result: watch::Receiver<Option<TitleResult>>,
        sender: watch::Sender<Option<TitleResult>>,
        cancellation: CancellationToken,
    },
}

impl ThreadTitleWorkers {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<ThreadId, PendingTitle>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn join_or_start(&self, thread_id: ThreadId) -> TitleWorkerSlot {
        let mut pending = self.pending();
        if let Some(existing) = pending.get(&thread_id) {
            return TitleWorkerSlot::Join(existing.result.clone());
        }
        let (sender, result) = watch::channel(None);
        let cancellation = CancellationToken::new();
        pending.insert(
            thread_id,
            PendingTitle {
                cancellation: cancellation.clone(),
                result: result.clone(),
            },
        );
        TitleWorkerSlot::Start {
            result,
            sender,
            cancellation,
        }
    }

    /// A saved manual name supersedes the pending title of that thread.
    pub(crate) fn cancel(&self, thread_id: ThreadId) {
        if let Some(pending) = self.pending().get(&thread_id) {
            pending.cancellation.cancel();
        }
    }

    fn finish(&self, thread_id: ThreadId) {
        self.pending().remove(&thread_id);
    }
}

impl ThreadRequestProcessor {
    pub(crate) async fn thread_title_generate(
        &self,
        params: ThreadTitleGenerateParams,
    ) -> TitleResult {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let mut result = match self.thread_titles.join_or_start(thread_id) {
            TitleWorkerSlot::Join(result) => result,
            TitleWorkerSlot::Start {
                result,
                sender,
                cancellation,
            } => {
                // The worker outlives this request so a disconnecting client still gets a name.
                let processor = self.clone();
                let originator = AuthStorageOriginator::current();
                self.background_tasks.spawn(originator.scope(async move {
                    let outcome = processor
                        .generate_thread_title(thread_id, &cancellation)
                        .await;
                    sender.send_replace(Some(outcome));
                    processor.thread_titles.finish(thread_id);
                }));
                result
            }
        };
        let outcome = result
            .wait_for(Option::is_some)
            .await
            .map_err(|_| internal_error("thread title worker stopped"))?
            .clone();
        outcome.unwrap_or_else(|| Err(internal_error("thread title worker stopped")))
    }

    async fn generate_thread_title(
        &self,
        thread_id: ThreadId,
        cancellation: &CancellationToken,
    ) -> TitleResult {
        let thread = self
            .read_thread_view(thread_id, /*include_turns*/ false)
            .await
            .map_err(|err| title_thread_read_error(thread_id, err))?;
        if thread.name.is_some() {
            return Ok(unchanged_title(thread.name));
        }
        let Some(source) = self.first_user_message(thread_id).await? else {
            return Ok(unchanged_title(/*name*/ None));
        };

        let generated = match self
            .run_title_turn(&thread, thread_title_prompt(&source), cancellation)
            .await
        {
            Ok(response) => parse_thread_title(&response),
            Err(error) => {
                tracing::debug!(%error, %thread_id, "thread title generation failed");
                None
            }
        };
        let is_generated = generated.is_some();
        let Some(title) = generated.or_else(|| fallback_thread_title(&source)) else {
            return Ok(unchanged_title(/*name*/ None));
        };
        self.save_generated_title(thread_id, title, is_generated, cancellation)
            .await
    }

    /// The title source of the thread's first user message.
    ///
    /// Clients call this right after `turn/start` returns, when the first message may not be in
    /// the thread's history yet. While the thread has a running turn, keep reading until the
    /// message appears or [`FIRST_MESSAGE_WAIT`] passes; the running turn is only read.
    async fn first_user_message(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<String>, JSONRPCErrorError> {
        let deadline = tokio::time::Instant::now() + FIRST_MESSAGE_WAIT;
        loop {
            let (loaded, in_flight) = match self.thread_manager.get_thread(thread_id).await {
                Ok(thread) => (true, thread.has_active_turn().await),
                Err(_) => (false, false),
            };
            match self
                .read_thread_view(thread_id, /*include_turns*/ true)
                .await
            {
                Ok(thread) => {
                    if let Some(source) = first_user_message_title_source(&thread.turns) {
                        return Ok(source);
                    }
                }
                // A loaded thread has no readable history until a turn records one, so it
                // has no first message to title from yet.
                Err(err) if loaded => {
                    let error = thread_read_view_error(err).message;
                    tracing::debug!(%thread_id, %error, "title source history not readable yet");
                }
                Err(err) => return Err(title_thread_read_error(thread_id, err)),
            }
            if !in_flight || tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(FIRST_MESSAGE_POLL).await;
        }
    }

    /// Saves an automatic title unless the thread got a name or a manual rename began meanwhile.
    async fn save_generated_title(
        &self,
        thread_id: ThreadId,
        title: String,
        generated: bool,
        cancellation: &CancellationToken,
    ) -> TitleResult {
        let permit = self.acquire_thread_list_state_permit().await?;
        let current = self
            .read_thread_view(thread_id, /*include_turns*/ false)
            .await
            .map_err(|err| title_thread_read_error(thread_id, err))?
            .name;
        let name = codex_core::util::normalize_thread_name(&title);
        let (true, None, Some(name)) = (!cancellation.is_cancelled(), &current, name) else {
            return Ok(unchanged_title(current));
        };
        let notification = self.write_thread_name(thread_id, name.clone()).await?;
        drop(permit);
        self.outgoing
            .send_server_notification(ServerNotification::ThreadNameUpdated(notification))
            .await;
        Ok(ThreadTitleGenerateResponse {
            name: Some(name),
            generated,
        })
    }

    /// Runs the structured title turn on a hidden thread and always disposes of that thread.
    async fn run_title_turn(
        &self,
        source: &Thread,
        prompt: String,
        cancellation: &CancellationToken,
    ) -> Result<String, String> {
        let hidden = self.start_title_thread(source).await?;
        let result = run_structured_turn(&hidden, prompt, cancellation).await;
        dispose_title_thread(&self.thread_manager, hidden).await;
        result
    }

    /// Starts the hidden thread as the TUI's `thread/start` would, then checks it is read-only.
    async fn start_title_thread(&self, source: &Thread) -> Result<NewThread, String> {
        let mut overrides = self.build_thread_config_overrides(
            source.model.clone(),
            Some(source.model_provider.clone()),
            /*service_tier*/ None,
            Some(source.cwd.to_string_lossy().into_owned()),
            Some(Vec::new()),
            Some(codex_app_server_protocol::AskForApproval::Never),
            /*approvals_reviewer*/ None,
            /*sandbox*/ None,
            Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
            /*base_instructions*/ None,
            /*developer_instructions*/ None,
            /*personality*/ None,
        );
        overrides.ephemeral = Some(true);
        let config_manager = self.config_manager.clone();
        let thread_manager = Arc::clone(&self.thread_manager);
        // Startup runs to completion in its own task: dropping it could strand a half-started thread.
        let mut startup = tokio::spawn(async move {
            let config = config_manager
                .load_with_overrides(Some(hidden_thread_config_overrides()), overrides)
                .await
                .map_err(|err| format!("failed to load title thread config: {err}"))?;
            let mut options = StartThreadOptions::new(config);
            options.thread_source = Some(ThreadSource::Feature(
                THREAD_TITLE_THREAD_SOURCE.to_string(),
            ));
            options.environments = Some(Vec::new());
            options.thread_extension_init.insert(McpServersDisabled);
            options.thread_extension_init.insert(TitleThread);
            thread_manager
                .start_thread(options)
                .await
                .map_err(|err| format!("failed to start title thread: {err}"))
        });
        let hidden = match tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, &mut startup).await {
            Ok(Ok(started)) => started?,
            Ok(Err(err)) => return Err(format!("title thread startup stopped: {err}")),
            Err(_) => {
                let thread_manager = Arc::clone(&self.thread_manager);
                tokio::spawn(async move {
                    if let Ok(Ok(hidden)) = startup.await {
                        dispose_title_thread(&thread_manager, hidden).await;
                    }
                });
                return Err("title thread start timed out".to_string());
            }
        };

        let profile = PermissionProfile::from_legacy_sandbox_policy_template(
            &hidden.thread.config_snapshot().await.sandbox_policy(),
        );
        if !matches!(profile, PermissionProfile::Managed { .. })
            || profile
                .file_system_sandbox_policy()
                .entries
                .iter()
                .any(|entry| entry.access.can_write())
        {
            dispose_title_thread(&self.thread_manager, hidden).await;
            return Err("title thread did not start with read-only permissions".to_string());
        }
        Ok(hidden)
    }
}

/// Runs one bounded structured turn and interrupts it on timeout or cancellation.
async fn run_structured_turn(
    hidden: &NewThread,
    prompt: String,
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let thread = &hidden.thread;
    let mut started = false;
    let result = tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, async {
        if cancellation.is_cancelled() {
            return Err("title turn cancelled".to_string());
        }
        let input = TurnInputRequest::user_input(vec![CoreUserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }])
        .on_start(TurnStartOptions {
            final_output_json_schema: Some(thread_title_output_schema()),
            ..Default::default()
        });
        match thread
            .start_turn_if_idle(input)
            .await
            .map_err(|err| format!("failed to start title turn: {err}"))?
        {
            StartIfIdleSubmission::Started { .. } => started = true,
            StartIfIdleSubmission::NotSubmitted { reason } => {
                return Err(format!("title turn was not admitted: {reason:?}"));
            }
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("title turn cancelled".to_string()),
            result = collect_structured_response(thread) => result,
        }
    })
    .await;
    let timed_out = result.is_err();
    let result = result.unwrap_or_else(|_| Err("title turn timed out".to_string()));

    // Interruption gets its own deadline so a late cancellation still stops the hidden turn.
    if (timed_out || cancellation.is_cancelled()) && started {
        match tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, thread.submit(Op::Interrupt)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::debug!(%error, "failed to interrupt title turn"),
            Err(_) => tracing::debug!("title turn interrupt timed out"),
        }
    }
    result
}

/// Returns the final assistant message once the hidden turn completes successfully.
async fn collect_structured_response(thread: &CodexThread) -> Result<String, String> {
    let mut turn_error = None;
    loop {
        let event = thread
            .next_event()
            .await
            .map_err(|err| format!("title thread event stream failed: {err}"))?;
        match event.msg {
            EventMsg::Error(error) if error.affects_turn_status() => {
                turn_error = Some(error.message);
            }
            EventMsg::TurnComplete(turn) => {
                if let Some(error) = turn.error.map(|error| error.message).or(turn_error) {
                    return Err(format!("title turn failed: {error}"));
                }
                let response = turn
                    .last_agent_message
                    .ok_or_else(|| "title turn completed without a response".to_string())?;
                if response.len() > STRUCTURED_RESPONSE_MAX_BYTES {
                    return Err(format!(
                        "title response exceeds {STRUCTURED_RESPONSE_MAX_BYTES} bytes"
                    ));
                }
                return Ok(response);
            }
            EventMsg::TurnAborted(_) | EventMsg::ShutdownComplete => {
                return Err("title turn stopped".to_string());
            }
            // Managed policy can let a title turn ask a blocking question. Leave it unanswered
            // until the turn times out, as desktop and the TUI do.
            EventMsg::RequestUserInput(request) if request.is_blocking => {}
            EventMsg::ExecApprovalRequest(_)
            | EventMsg::ApplyPatchApprovalRequest(_)
            | EventMsg::RequestPermissions(_)
            | EventMsg::RequestUserInput(_)
            | EventMsg::DynamicToolCallRequest(_)
            | EventMsg::ElicitationRequest(_) => {
                return Err(
                    "hidden title threads cannot request tools or user interaction".to_string(),
                );
            }
            _ => {}
        }
    }
}

/// Shuts the hidden thread down within the step deadline and drops it from the registry.
async fn dispose_title_thread(thread_manager: &ThreadManager, hidden: NewThread) {
    match tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, hidden.thread.shutdown_and_wait()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "failed to shut down title thread"),
        Err(_) => tracing::debug!("title thread shutdown timed out"),
    }
    thread_manager
        .remove_thread_if_matches(&hidden.thread_id, &hidden.thread)
        .await;
}

/// The first user message's request text, which the TUI titles a new thread from.
///
/// `None` while there is no user message; `Some(None)` when it holds no text to title from.
fn first_user_message_title_source(turns: &[Turn]) -> Option<Option<String>> {
    let content = turns
        .iter()
        .flat_map(Turn::item_payloads)
        .find_map(|item| match item {
            ThreadItemPayload::UserMessage { content, .. } => Some(content),
            _ => None,
        })?;
    Some(thread_title_source(content.iter().filter_map(
        |input| match input {
            V2UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        },
    )))
}

fn unchanged_title(name: Option<String>) -> ThreadTitleGenerateResponse {
    ThreadTitleGenerateResponse {
        name,
        generated: false,
    }
}

/// Reports an unknown thread as `thread/name/set` does.
fn title_thread_read_error(thread_id: ThreadId, err: ThreadReadViewError) -> JSONRPCErrorError {
    match err {
        ThreadReadViewError::InvalidRequest(message)
            if message == format!("thread not loaded: {thread_id}") =>
        {
            invalid_request(format!("thread not found: {thread_id}"))
        }
        err => thread_read_view_error(err),
    }
}

#[cfg(test)]
#[path = "thread_title_tests.rs"]
mod tests;
