//! File checkpoints per turn (`codex-checkpoint`): a snapshot of the tracked files when a turn starts, and a first
//! backup of each file before a patch changes it, so the app-server can rewind a thread's files to the start of
//! one of its turns. Only the local environment's files are covered (a remote executor's files are not on this
//! machine), and only changes made through patches (Edit/Write/apply_patch): shell commands are not tracked.
//!
//! A checkpoint that cannot be taken never stops the turn or the edit: it is logged, and a later rewind reports the
//! files it could not restore.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use codex_checkpoint::FileHistory;
use codex_utils_path_uri::PathUri;

use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnEnvironment;

/// The thread's history, opened on first use (`None`: disabled, or it could not be opened).
#[derive(Default)]
struct ThreadCheckpoints(OnceLock<Option<Arc<FileHistory>>>);

fn thread_history(session: &Session, config: &Config) -> Option<Arc<FileHistory>> {
    if !config.checkpoint.enabled {
        return None;
    }
    let slot = session
        .services
        .thread_extension_data
        .get_or_init(ThreadCheckpoints::default);
    slot.0
        .get_or_init(|| {
            let thread_id = session.thread_id().to_string();
            codex_checkpoint::thread_dir(config.codex_home.as_path(), &thread_id)
                .and_then(|dir| codex_checkpoint::shared(&dir, config.checkpoint.clone()))
                .inspect_err(|err| {
                    tracing::warn!(%thread_id, code = err.code(), "checkpoints unavailable: {err}");
                })
                .ok()
        })
        .clone()
}

/// The folders a rewind may write in: the environment's work folder and workspace roots.
fn roots(environment: &TurnEnvironment) -> Vec<PathBuf> {
    std::iter::once(environment.cwd())
        .chain(environment.workspace_roots())
        .filter_map(|uri| uri.to_abs_path().ok())
        .map(|path| path.to_path_buf())
        .collect()
}

/// Hook A: records the tracked files as the start of the turn `turn_id`, before the turn changes anything.
pub(crate) async fn snapshot_turn(
    session: &Arc<Session>,
    config: &Arc<Config>,
    turn_id: &str,
    local: Option<&TurnEnvironment>,
) {
    let Some(environment) = local else {
        return;
    };
    let roots = roots(environment);
    let session = Arc::clone(session);
    let config = Arc::clone(config);
    let turn_id = turn_id.to_string();
    let taken = tokio::task::spawn_blocking(move || {
        let Some(history) = thread_history(&session, &config) else {
            return Ok(());
        };
        history.set_roots(roots);
        history.make_snapshot(&turn_id)
    })
    .await;
    match taken {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!(code = err.code(), "turn checkpoint not taken: {err}"),
        Err(err) => tracing::warn!("turn checkpoint task failed: {err}"),
    }
}

/// Hook B: backs up the files a patch is about to change that are not tracked yet (their state before the first
/// change), and waits for it so that the patch goes ahead only afterwards.
pub(crate) async fn track_patch(
    session: &Arc<Session>,
    config: &Arc<Config>,
    environment: &TurnEnvironment,
    file_paths: &[PathUri],
) {
    if environment.environment().is_remote() || file_paths.is_empty() {
        return;
    }
    let paths: Vec<PathBuf> = file_paths
        .iter()
        .filter_map(|uri| uri.to_abs_path().ok())
        .map(|path| path.to_path_buf())
        .collect();
    let session = Arc::clone(session);
    let config = Arc::clone(config);
    let tracked = tokio::task::spawn_blocking(move || {
        let Some(history) = thread_history(&session, &config) else {
            return;
        };
        for path in paths {
            if let Err(err) = history.track_edit(&path) {
                tracing::warn!(code = err.code(), "file not tracked for checkpoints: {err}");
            }
        }
    })
    .await;
    if let Err(err) = tracked {
        tracing::warn!("checkpoint tracking task failed: {err}");
    }
}
