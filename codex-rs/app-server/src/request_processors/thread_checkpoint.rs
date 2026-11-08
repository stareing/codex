//! `thread/checkpoint/*`: list a thread's file checkpoints, rewind its files to the start of a turn, and take the
//! last rewind back (`codex-checkpoint`; the core takes the checkpoints in `core/src/checkpoints.rs`). The
//! conversation is not touched: clients that also want it back call `thread/revert` after a rewind succeeded.

use std::path::PathBuf;
use std::time::Duration;

use codex_app_server_protocol::ThreadCheckpoint;
use codex_app_server_protocol::ThreadCheckpointChange;
use codex_app_server_protocol::ThreadCheckpointFileChange;
use codex_app_server_protocol::ThreadCheckpointListParams;
use codex_app_server_protocol::ThreadCheckpointListResponse;
use codex_app_server_protocol::ThreadCheckpointRestoredNotification;
use codex_app_server_protocol::ThreadCheckpointRewindFilesParams;
use codex_app_server_protocol::ThreadCheckpointRewindResponse;
use codex_app_server_protocol::ThreadCheckpointSkipped;
use codex_app_server_protocol::ThreadCheckpointUndoRewindParams;
use codex_checkpoint::CheckpointError;
use codex_checkpoint::FileHistory;
use codex_checkpoint::RewindReport;

use super::*;

/// A preview reads every tracked file; past this it gives up.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(30);
/// A restore writes them; the lock it holds expires after the same time.
const RESTORE_TIMEOUT: Duration = codex_checkpoint::RESTORE_LOCK;

/// The JSON-RPC error for a checkpoint failure: the stable code in `data.code`.
fn checkpoint_error(err: &CheckpointError) -> JSONRPCErrorError {
    let mut error = invalid_request(format!("checkpoint: {err}"));
    error.data = Some(serde_json::json!({ "code": err.code() }));
    error
}

fn refused(err: &CheckpointError) -> ThreadCheckpointRewindResponse {
    ThreadCheckpointRewindResponse {
        can_rewind: false,
        error: Some(err.code().to_string()),
        files_changed: Vec::new(),
        skipped: Vec::new(),
    }
}

fn file_changes(report: &RewindReport) -> Vec<ThreadCheckpointFileChange> {
    report
        .files_changed
        .iter()
        .map(|change| ThreadCheckpointFileChange {
            path: change.path.to_string_lossy().into_owned(),
            change: match change.change {
                codex_checkpoint::Change::Restore => ThreadCheckpointChange::Restore,
                codex_checkpoint::Change::Delete => ThreadCheckpointChange::Delete,
                codex_checkpoint::Change::Recreate => ThreadCheckpointChange::Recreate,
            },
            insertions: change.insertions,
            deletions: change.deletions,
        })
        .collect()
}

fn rewind_response(report: &RewindReport) -> ThreadCheckpointRewindResponse {
    ThreadCheckpointRewindResponse {
        can_rewind: true,
        error: None,
        files_changed: file_changes(report),
        skipped: report
            .skipped
            .iter()
            .map(|skipped| ThreadCheckpointSkipped {
                path: skipped.path.to_string_lossy().into_owned(),
                reason: skipped.reason.code().to_string(),
            })
            .collect(),
    }
}

/// The history folder of a thread (`<codex_home>/checkpoints/<thread id>`).
pub(crate) fn checkpoint_dir(
    config: &Config,
    thread_id: ThreadId,
) -> Result<PathBuf, CheckpointError> {
    codex_checkpoint::thread_dir(config.codex_home.as_path(), &thread_id.to_string())
}

/// `turn/start` and `thread/revert` wait while a rewind of the thread runs.
pub(crate) async fn ensure_no_restore(
    thread: &CodexThread,
    thread_id: ThreadId,
) -> Result<(), JSONRPCErrorError> {
    let config = thread.config().await;
    match checkpoint_dir(&config, thread_id) {
        Ok(dir) if codex_checkpoint::restore_running(&dir) => {
            Err(checkpoint_error(&CheckpointError::RestoreInProgress))
        }
        _ => Ok(()),
    }
}

impl ThreadRequestProcessor {
    /// The thread's history, shared with its turn loop.
    async fn checkpoint_history(
        &self,
        thread_id: &str,
    ) -> Result<
        (
            ThreadId,
            Arc<CodexThread>,
            Result<Arc<FileHistory>, CheckpointError>,
        ),
        JSONRPCErrorError,
    > {
        let (thread_id, thread) = self.load_thread(thread_id).await?;
        let config = thread.config().await;
        if !config.checkpoint.enabled {
            return Ok((thread_id, thread, Err(CheckpointError::Disabled)));
        }
        let roots: Vec<PathBuf> = std::iter::once(&config.cwd)
            .chain(&config.workspace_roots)
            .map(|path| path.to_path_buf())
            .collect();
        let opened = checkpoint_dir(&config, thread_id).and_then(|dir| {
            let history = codex_checkpoint::shared(&dir, config.checkpoint.clone())?;
            history.set_roots_if_unset(roots);
            Ok(history)
        });
        Ok((thread_id, thread, opened))
    }

    pub(crate) async fn thread_checkpoint_list(
        &self,
        params: ThreadCheckpointListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, _, history) = self.checkpoint_history(&params.thread_id).await?;
        let listing = history
            .and_then(|history| history.list())
            .map_err(|err| checkpoint_error(&err))?;
        let response = ThreadCheckpointListResponse {
            checkpoints: listing
                .checkpoints
                .into_iter()
                .map(|checkpoint| ThreadCheckpoint {
                    turn_id: checkpoint.turn_id,
                    timestamp: i64::try_from(checkpoint.timestamp_ms).unwrap_or(i64::MAX),
                    file_count: u32::try_from(checkpoint.file_count).unwrap_or(u32::MAX),
                })
                .collect(),
            can_undo: listing.can_undo,
        };
        Ok(Some(response.into()))
    }

    pub(crate) async fn thread_checkpoint_rewind_files(
        &self,
        params: ThreadCheckpointRewindFilesParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let ThreadCheckpointRewindFilesParams {
            thread_id,
            turn_id,
            dry_run,
        } = params;
        let notified_turn = turn_id.clone();
        let restored = self
            .checkpoint_restore(
                &thread_id,
                dry_run,
                Some(notified_turn),
                move |history, timeout| history.rewind(&turn_id, dry_run, timeout),
            )
            .await?;
        Ok(Some(ClientResponsePayload::ThreadCheckpointRewindFiles(
            restored,
        )))
    }

    pub(crate) async fn thread_checkpoint_undo_rewind(
        &self,
        params: ThreadCheckpointUndoRewindParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let dry_run = params.dry_run;
        let restored = self
            .checkpoint_restore(&params.thread_id, dry_run, None, move |history, timeout| {
                history.undo_rewind(dry_run, timeout)
            })
            .await?;
        Ok(Some(ClientResponsePayload::ThreadCheckpointUndoRewind(
            restored,
        )))
    }

    /// Runs a rewind or undo off the async threads; refuses while the thread runs a turn.
    async fn checkpoint_restore(
        &self,
        thread_id: &str,
        dry_run: bool,
        turn_id: Option<String>,
        restore: impl FnOnce(&FileHistory, Duration) -> Result<RewindReport, CheckpointError>
        + Send
        + 'static,
    ) -> Result<ThreadCheckpointRewindResponse, JSONRPCErrorError> {
        let (thread_id, thread, history) = self.checkpoint_history(thread_id).await?;
        let history = match history {
            Ok(history) => history,
            Err(err) => return Ok(refused(&err)),
        };
        if matches!(thread.agent_status().await, AgentStatus::Running) {
            return Ok(refused(&CheckpointError::TurnInProgress));
        }
        let timeout = if dry_run {
            PREVIEW_TIMEOUT
        } else {
            RESTORE_TIMEOUT
        };
        let restored = tokio::task::spawn_blocking(move || restore(&history, timeout))
            .await
            .map_err(|err| internal_error(format!("checkpoint restore task failed: {err}")))?;
        let report = match restored {
            Ok(report) => report,
            Err(err) => return Ok(refused(&err)),
        };
        let response = rewind_response(&report);
        if !dry_run && !report.files_changed.is_empty() {
            self.outgoing
                .send_server_notification(ServerNotification::ThreadCheckpointRestored(
                    ThreadCheckpointRestoredNotification {
                        thread_id: thread_id.to_string(),
                        turn_id,
                        files_changed: response.files_changed.clone(),
                    },
                ))
                .await;
        }
        Ok(response)
    }
}

#[cfg(test)]
#[path = "thread_checkpoint_tests.rs"]
mod tests;
