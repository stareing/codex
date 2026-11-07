//! File checkpoints per turn, as Claude Code's file history (`docs/agent-codex/CHECKPOINTING.md` in the site's
//! repository): before a turn starts, [`FileHistory::make_snapshot`] records every tracked file; before a file is
//! changed for the first time, [`FileHistory::track_edit`] backs it up as version 1. [`FileHistory::rewind`] puts
//! the tracked files back as they were when a turn started, and keeps their current state first so that
//! [`FileHistory::undo_rewind`] can take the rewind back.
//!
//! Only edits that went through [`FileHistory::track_edit`] are covered: files changed by shell commands or other
//! programs are not tracked. The store (`<dir>/log.jsonl` plus `<hash>@v<n>` backup files) stays on the machine the
//! files are on.
//!
//! The calls block on file IO: async callers run them on a blocking thread.

mod error;
mod log;
mod store;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;

pub use error::CheckpointError;
pub use error::SkipReason;
use log::LogLine;
pub use store::Backup;
use store::Budget;

/// Restores of one thread hold this long at most; past it a lock left behind counts as gone.
pub const RESTORE_LOCK: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointConfig {
    pub enabled: bool,
    /// Snapshots kept per thread; older ones are dropped with the backups only they used.
    pub max_snapshots: usize,
    /// Files larger than this are tracked without a copy (restoring them reports `backup_missing`).
    pub max_file_bytes: u64,
    /// What one thread's backups may take; past it new backups are not kept.
    pub max_total_bytes: u64,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_snapshots: 100,
            max_file_bytes: 10 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// The tracked files' state when one turn started.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    turn_id: String,
    timestamp_ms: u64,
    backups: BTreeMap<PathBuf, Backup>,
}

/// The files' state just before the last rewind, which [`FileHistory::undo_rewind`] restores.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreRewind {
    seq: u64,
    backups: BTreeMap<PathBuf, Backup>,
}

#[derive(Default)]
struct State {
    snapshots: Vec<Snapshot>,
    tracked: BTreeSet<PathBuf>,
    /// Paths whose first backup is being taken (a second `track_edit` waits instead of taking another).
    pending: BTreeSet<PathBuf>,
    last_rewind: Option<PreRewind>,
    rewinds: u64,
    restoring: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointSummary {
    pub turn_id: String,
    pub timestamp_ms: u64,
    pub file_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    /// Oldest first.
    pub checkpoints: Vec<CheckpointSummary>,
    /// A rewind was applied and nothing changed the files through the history since.
    pub can_undo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// The file gets its earlier content back.
    Restore,
    /// The file did not exist then: it is deleted.
    Delete,
    /// The file is gone now: it is written again.
    Recreate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: PathBuf,
    pub change: Change,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// What a rewind changed (or, as a preview, would change).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindReport {
    pub files_changed: Vec<FileChange>,
    pub skipped: Vec<Skipped>,
}

/// `<codex_home>/checkpoints/<thread_id>`; a thread id with anything but letters, digits, `-` and `_` is refused.
pub fn thread_dir(codex_home: &Path, thread_id: &str) -> Result<PathBuf, CheckpointError> {
    let plain = !thread_id.is_empty()
        && thread_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return Err(CheckpointError::PathOutsideRoots);
    }
    Ok(codex_home.join("checkpoints").join(thread_id))
}

fn registry() -> MutexGuard<'static, HashMap<PathBuf, Weak<FileHistory>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<FileHistory>>>> = OnceLock::new();
    OPEN.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The one open history of `dir` in this process (the turn loop and the app-server's rewind requests must share
/// it: the restore lock and the undoable rewind live in memory). Opened with `config` when none is open.
pub fn shared(dir: &Path, config: CheckpointConfig) -> Result<Arc<FileHistory>, CheckpointError> {
    let mut open = registry();
    open.retain(|_, history| history.strong_count() > 0);
    if let Some(history) = open.get(dir).and_then(Weak::upgrade) {
        return Ok(history);
    }
    let history = Arc::new(FileHistory::open(dir, Vec::new(), config)?);
    open.insert(dir.to_path_buf(), Arc::downgrade(&history));
    Ok(history)
}

/// A rewind or undo of the history in `dir` is running in this process (without opening the history).
pub fn restore_running(dir: &Path) -> bool {
    let history = registry().get(dir).and_then(Weak::upgrade);
    history.is_some_and(|history| history.restore_in_progress())
}

#[cfg(test)]
type PhaseHook = std::sync::Arc<dyn Fn(&'static str, &Path) + Send + Sync>;

/// One thread's file history.
pub struct FileHistory {
    dir: PathBuf,
    roots: Mutex<Vec<PathBuf>>,
    config: CheckpointConfig,
    state: Mutex<State>,
    first_backup_done: Condvar,
    #[cfg(test)]
    hook: Mutex<Option<PhaseHook>>,
}

/// Clears the restore lock however the restore ends.
struct RestoreGuard<'a>(&'a FileHistory);

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().restoring = None;
    }
}

impl FileHistory {
    /// Opens (or starts) the history kept in `dir`; `roots` are the folders a rewind may write in.
    pub fn open(
        dir: impl Into<PathBuf>,
        roots: Vec<PathBuf>,
        config: CheckpointConfig,
    ) -> Result<Self, CheckpointError> {
        let dir = dir.into();
        let mut state = State::default();
        if config.enabled {
            create_private_dir(&dir)?;
            let replayed = log::replay(&dir)?;
            state.snapshots = replayed.snapshots;
            state.rewinds = replayed.rewinds;
            state.last_rewind = replayed.last_rewind;
            let keep_from = state.snapshots.len().saturating_sub(config.max_snapshots.max(1));
            state.snapshots.drain(..keep_from);
            state.tracked = state
                .snapshots
                .iter()
                .flat_map(|snapshot| snapshot.backups.keys().cloned())
                .collect();
        }
        Ok(Self {
            dir,
            roots: Mutex::new(roots),
            config,
            state: Mutex::new(state),
            first_backup_done: Condvar::new(),
            #[cfg(test)]
            hook: Mutex::new(None),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The folders a rewind may write in (the thread's work folders, which can change between turns).
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        *self.roots.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = roots;
    }

    /// Sets the folders unless some are set already (the turn loop sets the turn's own; this is the fallback
    /// from the thread's configuration for a history no turn of this process has used yet).
    pub fn set_roots_if_unset(&self, roots: Vec<PathBuf>) {
        let mut current = self.roots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_empty() {
            *current = roots;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn require_enabled(&self) -> Result<(), CheckpointError> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(CheckpointError::Disabled)
        }
    }

    #[cfg(test)]
    fn phase(&self, name: &'static str, path: &Path) {
        let hook = self.hook.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(hook) = hook {
            hook(name, path);
        }
    }

    #[cfg(not(test))]
    fn phase(&self, _name: &'static str, _path: &Path) {}

    fn budget(&self) -> Budget {
        Budget {
            max_file_bytes: self.config.max_file_bytes,
            remaining_bytes: self.config.max_total_bytes.saturating_sub(store::stored_bytes(&self.dir)),
        }
    }

    /// Records the tracked files' state as the start of `turn_id` (before the turn changes anything).
    ///
    /// Three steps (Claude Code's `makeSnapshot`): read what is tracked; back up the files that changed since
    /// their last backup without holding the lock; commit, taking in the files `track_edit` added meanwhile.
    pub fn make_snapshot(&self, turn_id: &str) -> Result<(), CheckpointError> {
        self.require_enabled()?;
        let (tracked, previous) = {
            let mut state = self.lock();
            state.last_rewind = None;
            let previous = state.snapshots.last().map(|s| s.backups.clone()).unwrap_or_default();
            (state.tracked.clone(), previous)
        };
        let mut budget = self.budget();
        let mut backups = BTreeMap::new();
        for path in tracked {
            let last = previous.get(&path);
            self.phase("make_snapshot:backup", &path);
            let backup = match last {
                Some(last) if !store::changed_since(&self.dir, &path, last) => last.clone(),
                _ => {
                    let version = last.map_or(1, |last| last.version + 1);
                    let (backup, used) = store::create_backup(&self.dir, &path, version, &budget)?;
                    budget.remaining_bytes = budget.remaining_bytes.saturating_sub(used);
                    backup
                }
            };
            backups.insert(path, backup);
        }
        let evicted = {
            let mut state = self.lock();
            // Files `track_edit` added while the backups above were taken.
            if let Some(latest) = state.snapshots.last() {
                for (path, backup) in &latest.backups {
                    backups.entry(path.clone()).or_insert_with(|| backup.clone());
                }
            }
            let snapshot = Snapshot {
                turn_id: turn_id.to_string(),
                timestamp_ms: store::now_ms(),
                backups,
            };
            log::append(&self.dir, &LogLine::snapshot(&snapshot))?;
            state.snapshots.push(snapshot);
            let excess = state.snapshots.len().saturating_sub(self.config.max_snapshots.max(1));
            let evicted = excess > 0;
            if evicted {
                state.snapshots.drain(..excess);
                log::rewrite(&self.dir, &state.snapshots, state.rewinds, None)?;
            }
            evicted.then(|| referenced(&state))
        };
        if let Some(keep) = evicted {
            store::remove_unreferenced(&self.dir, &keep);
        }
        Ok(())
    }

    /// Backs up `path` before its first change (version 1, or "did not exist"), into the latest snapshot.
    ///
    /// Returns once the backup is taken: the edit may then go ahead. A file already tracked is left alone (its
    /// next state is recorded by the next snapshot), so repeated edits never overwrite version 1.
    pub fn track_edit(&self, path: &Path) -> Result<(), CheckpointError> {
        self.require_enabled()?;
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(CheckpointError::PathOutsideRoots);
        }
        {
            let mut state = self.lock();
            loop {
                let Some(latest) = state.snapshots.last() else {
                    return Err(CheckpointError::NoSnapshot);
                };
                if latest.backups.contains_key(path) {
                    return Ok(());
                }
                if !state.pending.contains(path) {
                    break;
                }
                state = self
                    .first_backup_done
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state.pending.insert(path.to_path_buf());
            state.last_rewind = None;
        }
        self.phase("track_edit:backup", path);
        let backed_up = store::create_backup(&self.dir, path, 1, &self.budget());
        let mut state = self.lock();
        state.pending.remove(path);
        self.first_backup_done.notify_all();
        let (backup, _) = backed_up?;
        let Some(latest) = state.snapshots.last_mut() else {
            return Err(CheckpointError::NoSnapshot);
        };
        if latest.backups.contains_key(path) {
            return Ok(());
        }
        let line = LogLine::delta(&latest.turn_id, path, &backup);
        latest.backups.insert(path.to_path_buf(), backup);
        state.tracked.insert(path.to_path_buf());
        log::append(&self.dir, &line)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Listing, CheckpointError> {
        self.require_enabled()?;
        let state = self.lock();
        Ok(Listing {
            checkpoints: state
                .snapshots
                .iter()
                .map(|snapshot| CheckpointSummary {
                    turn_id: snapshot.turn_id.clone(),
                    timestamp_ms: snapshot.timestamp_ms,
                    file_count: snapshot.backups.len(),
                })
                .collect(),
            can_undo: state.last_rewind.is_some(),
        })
    }

    /// A rewind or undo of this thread is running (turns and conversation reverts wait for it).
    pub fn restore_in_progress(&self) -> bool {
        self.lock()
            .restoring
            .is_some_and(|started| started.elapsed() < RESTORE_LOCK)
    }

    fn begin_restore(&self) -> Result<RestoreGuard<'_>, CheckpointError> {
        let mut state = self.lock();
        if state.restoring.is_some_and(|started| started.elapsed() < RESTORE_LOCK) {
            return Err(CheckpointError::RestoreInProgress);
        }
        state.restoring = Some(Instant::now());
        Ok(RestoreGuard(self))
    }

    /// Puts every tracked file back as it was when `turn_id` started (`dry_run`: only reports what would change).
    ///
    /// A file first changed after that turn gets its version 1 (its state before the first tracked change). A
    /// file that cannot be restored is left as it is and listed in `skipped`; the rest still are.
    pub fn rewind(
        &self,
        turn_id: &str,
        dry_run: bool,
        timeout: Duration,
    ) -> Result<RewindReport, CheckpointError> {
        self.require_enabled()?;
        let _guard = self.begin_restore()?;
        let deadline = Instant::now() + timeout;
        let targets = {
            let state = self.lock();
            let target = state
                .snapshots
                .iter()
                .find(|snapshot| snapshot.turn_id == turn_id)
                .ok_or(CheckpointError::CheckpointNotFound)?;
            state
                .tracked
                .iter()
                .map(|path| {
                    let backup = target
                        .backups
                        .get(path)
                        .or_else(|| first_backup(&state.snapshots, path))
                        .cloned();
                    (path.clone(), backup)
                })
                .collect::<Vec<_>>()
        };
        let (plan, mut report) = self.plan(targets, deadline)?;
        if dry_run || plan.is_empty() {
            return Ok(report);
        }
        let seq = self.lock().rewinds + 1;
        let mut budget = self.budget();
        let mut kept = BTreeMap::new();
        for planned in &plan {
            let (backup, used) = store::create_pre_rewind(&self.dir, &planned.path, seq, &budget)?;
            budget.remaining_bytes = budget.remaining_bytes.saturating_sub(used);
            kept.insert(planned.path.clone(), backup);
        }
        let pre_rewind = PreRewind { seq, backups: kept };
        log::append(&self.dir, &LogLine::pre_rewind(&pre_rewind))?;
        {
            let mut state = self.lock();
            state.rewinds = seq;
            state.last_rewind = Some(pre_rewind);
        }
        self.apply(plan, &mut report, deadline)?;
        Ok(report)
    }

    /// Takes the last rewind back (the files it changed get the content they had just before it).
    pub fn undo_rewind(&self, dry_run: bool, timeout: Duration) -> Result<RewindReport, CheckpointError> {
        self.require_enabled()?;
        let _guard = self.begin_restore()?;
        let deadline = Instant::now() + timeout;
        let pre_rewind = self.lock().last_rewind.clone().ok_or(CheckpointError::NothingToUndo)?;
        let targets = pre_rewind
            .backups
            .iter()
            .map(|(path, backup)| (path.clone(), Some(backup.clone())))
            .collect();
        let (plan, mut report) = self.plan(targets, deadline)?;
        if dry_run {
            return Ok(report);
        }
        log::append(&self.dir, &LogLine::undo(pre_rewind.seq))?;
        self.lock().last_rewind = None;
        self.apply(plan, &mut report, deadline)?;
        Ok(report)
    }

    /// Which files differ from their target state, and how; files that cannot be restored go to `skipped`.
    fn plan(
        &self,
        targets: Vec<(PathBuf, Option<Backup>)>,
        deadline: Instant,
    ) -> Result<(Vec<Planned>, RewindReport), CheckpointError> {
        let roots = self.roots.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let mut plan = Vec::new();
        let mut report = RewindReport::default();
        for (path, backup) in targets {
            if Instant::now() >= deadline {
                return Err(CheckpointError::TimedOut);
            }
            let skip = |reason| Skipped { path: path.clone(), reason };
            if let Err(reason) = store::writable_target(&path, &roots) {
                report.skipped.push(skip(reason));
                continue;
            }
            let Some(backup) = backup.filter(|backup| !backup.unavailable) else {
                report.skipped.push(skip(SkipReason::BackupMissing));
                continue;
            };
            let Ok(wanted) = store::backup_bytes(&self.dir, &backup) else {
                report.skipped.push(skip(SkipReason::BackupMissing));
                continue;
            };
            let Ok(current) = store::current_bytes(&path) else {
                report.skipped.push(skip(SkipReason::Io));
                continue;
            };
            let change = match (&current, &wanted) {
                (current, wanted) if current == wanted => continue,
                (_, None) => Change::Delete,
                (None, Some(_)) => Change::Recreate,
                (Some(_), Some(_)) => Change::Restore,
            };
            let (insertions, deletions) = store::line_stats(current.as_deref(), wanted.as_deref());
            report.files_changed.push(FileChange {
                path: path.clone(),
                change,
                insertions,
                deletions,
            });
            plan.push(Planned { path, backup });
        }
        Ok((plan, report))
    }

    /// Writes the planned files; one that fails moves from `files_changed` to `skipped`.
    fn apply(
        &self,
        plan: Vec<Planned>,
        report: &mut RewindReport,
        deadline: Instant,
    ) -> Result<(), CheckpointError> {
        let roots = self.roots.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        for planned in plan {
            if Instant::now() >= deadline {
                return Err(CheckpointError::TimedOut);
            }
            // Checked again: a folder on the way may have become a link since the plan.
            let written = store::writable_target(&planned.path, &roots).and_then(|()| {
                store::restore(&self.dir, &planned.path, &planned.backup).map_err(|_| SkipReason::Io)
            });
            if let Err(reason) = written {
                report.files_changed.retain(|change| change.path != planned.path);
                report.skipped.push(Skipped {
                    path: planned.path,
                    reason,
                });
            }
        }
        Ok(())
    }

    /// Copies the history to `dir` for a forked thread (which starts with the same checkpoints).
    pub fn copy_to(&self, dir: &Path) -> Result<(), CheckpointError> {
        self.require_enabled()?;
        let _state = self.lock();
        create_private_dir(dir)?;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::copy(entry.path(), dir.join(entry.file_name()))?;
            }
        }
        Ok(())
    }
}

struct Planned {
    path: PathBuf,
    backup: Backup,
}

/// The backup a file had in the earliest snapshot that has it: its version 1 while that snapshot is kept (a
/// snapshot without the file is older than that one).
fn first_backup<'a>(snapshots: &'a [Snapshot], path: &Path) -> Option<&'a Backup> {
    snapshots.iter().find_map(|snapshot| snapshot.backups.get(path))
}

/// Backup file names the kept snapshots and the undoable rewind still use.
fn referenced(state: &State) -> BTreeSet<String> {
    state
        .snapshots
        .iter()
        .flat_map(|snapshot| snapshot.backups.values())
        .chain(state.last_rewind.iter().flat_map(|pre| pre.backups.values()))
        .filter_map(|backup| backup.file.clone())
        .collect()
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
