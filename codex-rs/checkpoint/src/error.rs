//! Checkpoint error codes: the one place they are defined (the app-server and the clients only refer to them).

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// The thread has a turn running: no rewind while it changes files.
    #[error("a turn is in progress")]
    TurnInProgress,
    /// Another restore of this thread has not finished.
    #[error("a restore is in progress")]
    RestoreInProgress,
    /// No checkpoint for this turn (never taken, or dropped past the snapshot limit).
    #[error("no checkpoint for this turn")]
    CheckpointNotFound,
    /// Nothing to undo: no rewind of this thread was applied.
    #[error("no rewind to undo")]
    NothingToUndo,
    #[error("a backup is missing")]
    BackupMissing,
    #[error("the path is outside the thread's folders")]
    PathOutsideRoots,
    /// Checkpoints are switched off (`[checkpoint] enabled = false`).
    #[error("checkpoints are disabled")]
    Disabled,
    #[error("timed out")]
    TimedOut,
    /// No snapshot yet (a file was edited before the turn's snapshot was taken).
    #[error("no snapshot to record the edit in")]
    NoSnapshot,
    /// Reading or writing the checkpoint store failed.
    #[error("checkpoint store: {0}")]
    Io(String),
    /// The log has a line this version cannot read.
    #[error("checkpoint log: {0}")]
    Corrupt(String),
}

impl CheckpointError {
    /// The stable code clients branch on.
    pub fn code(&self) -> &'static str {
        match self {
            CheckpointError::TurnInProgress => "turn_in_progress",
            CheckpointError::RestoreInProgress => "restore_in_progress",
            CheckpointError::CheckpointNotFound => "checkpoint_not_found",
            CheckpointError::NothingToUndo => "nothing_to_undo",
            CheckpointError::BackupMissing => "backup_missing",
            CheckpointError::PathOutsideRoots => "path_outside_roots",
            CheckpointError::Disabled => "disabled",
            CheckpointError::TimedOut => "timed_out",
            CheckpointError::NoSnapshot => "no_snapshot",
            CheckpointError::Io(_) => "checkpoint_io",
            CheckpointError::Corrupt(_) => "checkpoint_corrupt",
        }
    }
}

impl From<std::io::Error> for CheckpointError {
    fn from(err: std::io::Error) -> Self {
        CheckpointError::Io(err.kind().to_string())
    }
}

/// Why one file was left as it is by a rewind (the result then counts as a partial restore).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The file, or a folder on its way, is a symbolic link (or junction).
    Symlink,
    /// No longer inside the thread's folders.
    OutsideRoots,
    /// Its backup is gone (never taken: too large or over the store's limit; or deleted).
    BackupMissing,
    /// Reading or writing it failed.
    Io,
}

impl SkipReason {
    pub fn code(self) -> &'static str {
        match self {
            SkipReason::Symlink => "symlink",
            SkipReason::OutsideRoots => "outside_roots",
            SkipReason::BackupMissing => "backup_missing",
            SkipReason::Io => "io",
        }
    }
}
