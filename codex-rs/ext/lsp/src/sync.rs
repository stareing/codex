//! Applied patches → document sync, so servers see the agent's edits and publish diagnostics for them.
//!
//! The diagnostics wait is armed here, synchronously, before the sync runs in the background: the next model step
//! must not start its wait before the edit is known. Paths from patches are those of the agent's own machine (the
//! desktop runs local agents only); a relative path is taken from the conversation's working directory.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_lsp::FileEvent;
use codex_lsp::uri::path_to_uri;
use codex_protocol::items::FileChangeItem;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;

use crate::LspThread;

enum Planned {
    /// Read the file after the patch and send its text.
    Written(PathBuf),
    /// Send the text the patch carried (new files).
    Created(PathBuf, String),
    Deleted(PathBuf),
}

impl Planned {
    fn path(&self) -> &Path {
        match self {
            Self::Written(path) | Self::Created(path, _) | Self::Deleted(path) => path,
        }
    }
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// What a completed patch means for the servers (nothing for declined or failed patches).
fn plan(change: &FileChangeItem, cwd: &Path) -> Vec<Planned> {
    if change.status != Some(PatchApplyStatus::Completed) {
        return Vec::new();
    }
    let mut planned = Vec::new();
    for (path, file_change) in &change.changes {
        let path = absolute(path, cwd);
        match file_change {
            FileChange::Add { content } => planned.push(Planned::Created(path, content.clone())),
            FileChange::Delete { .. } => planned.push(Planned::Deleted(path)),
            FileChange::Update {
                move_path: Some(target),
                ..
            } => {
                planned.push(Planned::Deleted(path));
                planned.push(Planned::Written(absolute(target, cwd)));
            }
            FileChange::Update {
                move_path: None, ..
            } => planned.push(Planned::Written(path)),
        }
    }
    planned
}

pub(crate) fn sync_file_change(thread: &Arc<LspThread>, change: &FileChangeItem) {
    let planned: Vec<Planned> = plan(change, &thread.ctx.cwd)
        .into_iter()
        .filter(|item| thread.ctx.table.server_for(item.path()).is_some())
        .collect();
    if planned.is_empty() {
        return;
    }
    for item in &planned {
        if !matches!(item, Planned::Deleted(_))
            && let Some(uri) = path_to_uri(item.path())
        {
            thread
                .ctx
                .sink
                .file_edited(&uri, thread.ctx.limits.diagnostics_wait);
        }
    }
    let thread = thread.clone();
    tokio::spawn(async move {
        for item in planned {
            let event = match &item {
                Planned::Created(_, text) => FileEvent::Written(text.clone()),
                Planned::Deleted(_) => FileEvent::Deleted,
                Planned::Written(path) => {
                    match read_bounded(path, thread.ctx.limits.max_file_bytes).await {
                        Some(text) => FileEvent::Written(text),
                        None => {
                            if let Some(uri) = path_to_uri(path) {
                                thread.ctx.sink.stop_waiting(&uri);
                            }
                            continue;
                        }
                    }
                }
            };
            thread
                .manager
                .file_changed(&thread.ctx, item.path(), event)
                .await;
        }
    });
}

async fn read_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn item(status: PatchApplyStatus, changes: Vec<(&str, FileChange)>) -> FileChangeItem {
        FileChangeItem {
            id: "call".into(),
            changes: changes
                .into_iter()
                .map(|(path, change)| (PathBuf::from(path), change))
                .collect::<HashMap<_, _>>(),
            status: Some(status),
            auto_approved: None,
            stdout: None,
            stderr: None,
        }
    }

    fn describe(planned: &[Planned]) -> Vec<String> {
        let mut out: Vec<String> = planned
            .iter()
            .map(|p| match p {
                Planned::Written(path) => format!("write {}", path.display()),
                Planned::Created(path, text) => format!("create {} {text}", path.display()),
                Planned::Deleted(path) => format!("delete {}", path.display()),
            })
            .collect();
        out.sort();
        out
    }

    #[cfg(unix)]
    #[test]
    fn completed_patches_plan_writes_moves_and_deletes() {
        let change = item(
            PatchApplyStatus::Completed,
            vec![
                (
                    "/w/new.rs",
                    FileChange::Add {
                        content: "fn x() {}".into(),
                    },
                ),
                (
                    "old.rs",
                    FileChange::Delete {
                        content: String::new(),
                    },
                ),
                (
                    "/w/a.rs",
                    FileChange::Update {
                        unified_diff: String::new(),
                        move_path: None,
                    },
                ),
                (
                    "/w/b.rs",
                    FileChange::Update {
                        unified_diff: String::new(),
                        move_path: Some("c.rs".into()),
                    },
                ),
            ],
        );
        assert_eq!(
            describe(&plan(&change, Path::new("/w"))),
            [
                "create /w/new.rs fn x() {}",
                "delete /w/b.rs",
                "delete /w/old.rs",
                "write /w/a.rs",
                "write /w/c.rs",
            ]
        );
    }

    #[test]
    fn declined_and_failed_patches_change_nothing() {
        for status in [PatchApplyStatus::Declined, PatchApplyStatus::Failed] {
            let change = item(
                status,
                vec![(
                    "/w/a.rs",
                    FileChange::Add {
                        content: String::new(),
                    },
                )],
            );
            assert!(plan(&change, Path::new("/w")).is_empty());
        }
    }
}
