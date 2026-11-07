use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

const TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    _temp: TempDir,
    store: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("work");
        fs::create_dir_all(&root).unwrap();
        Self {
            store: base.join("checkpoints").join("thread-1"),
            root,
            _temp: temp,
        }
    }

    fn open(&self) -> FileHistory {
        self.open_with(CheckpointConfig::default())
    }

    fn open_with(&self, config: CheckpointConfig) -> FileHistory {
        FileHistory::open(&self.store, vec![self.root.clone()], config).unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path(name)).ok()
    }
}

fn changes(report: &RewindReport) -> Vec<(String, Change)> {
    report
        .files_changed
        .iter()
        .map(|c| (c.path.file_name().unwrap().to_string_lossy().into_owned(), c.change))
        .collect()
}

fn skipped(report: &RewindReport) -> Vec<(String, SkipReason)> {
    report
        .skipped
        .iter()
        .map(|s| (s.path.file_name().unwrap().to_string_lossy().into_owned(), s.reason))
        .collect()
}

fn set_hook(history: &FileHistory, hook: impl Fn(&'static str, &Path) + Send + Sync + 'static) {
    *history.hook.lock().unwrap() = Some(Arc::new(hook));
}

#[test]
fn rewind_undoes_an_edit_a_new_file_and_a_deletion() {
    let fx = Fixture::new();
    fx.write("edited.txt", "one\ntwo\n");
    fx.write("deleted.txt", "keep me\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();

    history.track_edit(&fx.path("edited.txt")).unwrap();
    fx.write("edited.txt", "one\nTWO\nthree\n");
    history.track_edit(&fx.path("created.txt")).unwrap();
    fx.write("created.txt", "new\n");
    history.track_edit(&fx.path("deleted.txt")).unwrap();
    fs::remove_file(fx.path("deleted.txt")).unwrap();

    let report = history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(
        changes(&report),
        vec![
            ("created.txt".into(), Change::Delete),
            ("deleted.txt".into(), Change::Recreate),
            ("edited.txt".into(), Change::Restore),
        ]
    );
    assert_eq!(report.skipped, vec![]);
    assert_eq!(fx.read("edited.txt").as_deref(), Some("one\ntwo\n"));
    assert_eq!(fx.read("created.txt"), None);
    assert_eq!(fx.read("deleted.txt").as_deref(), Some("keep me\n"));
}

#[test]
fn preview_counts_lines_and_changes_nothing() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\ntwo\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "one\nTWO\nthree\n");

    let report = history.rewind("t1", true, TIMEOUT).unwrap();
    let change = &report.files_changed[0];
    // Back to the snapshot: "TWO" and "three" go, "two" comes back.
    assert_eq!((change.change, change.insertions, change.deletions), (Change::Restore, 1, 2));
    assert_eq!(fx.read("a.txt").as_deref(), Some("one\nTWO\nthree\n"));
    assert!(!history.list().unwrap().can_undo);
}

#[test]
fn a_file_first_changed_after_the_target_gets_its_first_version() {
    let fx = Fixture::new();
    fx.write("late.txt", "original\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.make_snapshot("t2").unwrap();
    history.track_edit(&fx.path("late.txt")).unwrap();
    fx.write("late.txt", "changed in t2\n");
    history.make_snapshot("t3").unwrap();
    fx.write("late.txt", "changed in t3\n");
    history.make_snapshot("t4").unwrap();

    history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("late.txt").as_deref(), Some("original\n"));
    history.rewind("t3", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("late.txt").as_deref(), Some("changed in t2\n"));
}

#[test]
fn a_missing_backup_leaves_the_file_alone() {
    let fx = Fixture::new();
    fx.write("big.txt", "0123456789");
    fx.write("small.txt", "sm\n");
    let history = fx.open_with(CheckpointConfig {
        max_file_bytes: 5,
        ..CheckpointConfig::default()
    });
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("big.txt")).unwrap();
    fx.write("big.txt", "changed");
    history.track_edit(&fx.path("small.txt")).unwrap();
    fx.write("small.txt", "edited\n");
    // The small file's copy is deleted behind the history's back.
    fs::remove_file(fx.store.join(store::backup_name(&fx.path("small.txt"), 1))).unwrap();

    let report = history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(
        skipped(&report),
        vec![
            ("big.txt".into(), SkipReason::BackupMissing),
            ("small.txt".into(), SkipReason::BackupMissing),
        ]
    );
    assert_eq!(fx.read("big.txt").as_deref(), Some("changed"));
    assert_eq!(fx.read("small.txt").as_deref(), Some("edited\n"));
}

#[test]
fn repeated_edits_in_a_turn_keep_the_first_version() {
    let fx = Fixture::new();
    fx.write("a.txt", "v0\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "v1\n");
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "v2\n");

    history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("v0\n"));
}

#[test]
fn snapshots_reuse_unchanged_backups_and_version_changed_ones() {
    let fx = Fixture::new();
    fx.write("same.txt", "same\n");
    fx.write("moving.txt", "m0\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("same.txt")).unwrap();
    history.track_edit(&fx.path("moving.txt")).unwrap();
    fx.write("moving.txt", "m1\n");
    history.make_snapshot("t2").unwrap();

    let state = history.lock();
    let latest = &state.snapshots[1].backups;
    assert_eq!(latest[&fx.path("same.txt")].version, 1);
    assert_eq!(latest[&fx.path("moving.txt")].version, 2);
}

/// Two `track_edit`s of one new path at once: the second waits for the first's backup instead of taking its own
/// after the first edit already happened (which would overwrite version 1 with edited content).
#[test]
fn concurrent_first_edits_take_one_first_version() {
    let fx = Fixture::new();
    fx.write("a.txt", "original\n");
    let history = Arc::new(fx.open());
    history.make_snapshot("t1").unwrap();
    let path = fx.path("a.txt");

    let (second_done_tx, second_done_rx) = mpsc::channel::<()>();
    let (first_in_window_tx, first_in_window_rx) = mpsc::channel::<()>();
    let first_seen = Arc::new(AtomicBool::new(false));
    let second_done = Mutex::new(Some(second_done_rx));
    set_hook(&history, {
        let first_seen = Arc::clone(&first_seen);
        move |phase, _| {
            if phase == "track_edit:backup" && !first_seen.swap(true, Ordering::SeqCst) {
                let _ = first_in_window_tx.send(());
                // Give the second caller every chance to finish (and edit) before this backup is taken.
                if let Some(rx) = second_done.lock().unwrap().take() {
                    let _ = rx.recv_timeout(Duration::from_secs(2));
                }
            }
        }
    });

    let first = std::thread::spawn({
        let history = Arc::clone(&history);
        let path = path.clone();
        move || history.track_edit(&path).unwrap()
    });
    first_in_window_rx.recv().unwrap();
    let second = std::thread::spawn({
        let history = Arc::clone(&history);
        move || {
            history.track_edit(&path).unwrap();
            fs::write(&path, "edited by the second\n").unwrap();
            let _ = second_done_tx.send(());
        }
    });
    first.join().unwrap();
    second.join().unwrap();

    history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("original\n"));
}

#[test]
fn a_snapshot_takes_in_files_tracked_while_it_was_taken() {
    let fx = Fixture::new();
    fx.write("tracked.txt", "t\n");
    fx.write("late.txt", "late original\n");
    let history = Arc::new(fx.open());
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("tracked.txt")).unwrap();
    let late = fx.path("late.txt");
    set_hook(&history, {
        let history = Arc::downgrade(&history);
        let done = AtomicBool::new(false);
        move |phase, _| {
            if phase == "make_snapshot:backup" && !done.swap(true, Ordering::SeqCst) {
                let history = history.upgrade().unwrap();
                history.track_edit(&late).unwrap();
                fs::write(&late, "late edited\n").unwrap();
            }
        }
    });
    history.make_snapshot("t2").unwrap();

    let listing = history.list().unwrap();
    assert_eq!(listing.checkpoints[1].file_count, 2, "{listing:?}");
    *history.hook.lock().unwrap() = None;
    history.rewind("t2", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("late.txt").as_deref(), Some("late original\n"));
}

#[test]
fn old_snapshots_are_dropped_with_their_backups() {
    let fx = Fixture::new();
    fx.write("a.txt", "0");
    let history = fx.open_with(CheckpointConfig {
        max_snapshots: 3,
        ..CheckpointConfig::default()
    });
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    for turn in 2..=5 {
        fx.write("a.txt", &turn.to_string());
        history.make_snapshot(&format!("t{turn}")).unwrap();
    }

    let turns: Vec<String> = history.list().unwrap().checkpoints.into_iter().map(|c| c.turn_id).collect();
    assert_eq!(turns, vec!["t3", "t4", "t5"]);
    assert_eq!(history.rewind("t1", true, TIMEOUT), Err(CheckpointError::CheckpointNotFound));
    let mut files: Vec<String> = fs::read_dir(&fx.store)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains('@'))
        .collect();
    files.sort();
    let hash = store::backup_name(&fx.path("a.txt"), 0);
    let hash = hash.trim_end_matches("@v0");
    assert_eq!(files, vec![format!("{hash}@v3"), format!("{hash}@v4"), format!("{hash}@v5")]);

    // The log was compacted to the kept snapshots.
    drop(history);
    let reopened = fx.open_with(CheckpointConfig {
        max_snapshots: 3,
        ..CheckpointConfig::default()
    });
    assert_eq!(reopened.list().unwrap().checkpoints.len(), 3);
    reopened.rewind("t3", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("3"));
}

#[test]
fn a_reopened_history_replays_snapshots_and_deltas() {
    let fx = Fixture::new();
    fx.write("a.txt", "a0\n");
    {
        let history = fx.open();
        history.make_snapshot("t1").unwrap();
        history.track_edit(&fx.path("a.txt")).unwrap();
        fx.write("a.txt", "a1\n");
        history.make_snapshot("t2").unwrap();
        history.track_edit(&fx.path("b.txt")).unwrap();
        fx.write("b.txt", "b1\n");
    }
    let history = fx.open();
    let listing = history.list().unwrap();
    assert_eq!(
        listing
            .checkpoints
            .iter()
            .map(|c| (c.turn_id.as_str(), c.file_count))
            .collect::<Vec<_>>(),
        vec![("t1", 1), ("t2", 2)]
    );
    history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("a0\n"));
    assert_eq!(fx.read("b.txt"), None);
}

#[test]
fn replay_ignores_a_cut_last_line_and_refuses_a_broken_one_before() {
    let fx = Fixture::new();
    {
        let history = fx.open();
        history.make_snapshot("t1").unwrap();
    }
    let log = fx.store.join("log.jsonl");
    let mut text = fs::read_to_string(&log).unwrap();
    text.push_str("{\"type\":\"snapshot\",\"turn_");
    fs::write(&log, &text).unwrap();
    assert_eq!(fx.open().list().unwrap().checkpoints.len(), 1);

    text.push_str("\n{\"type\":\"undo\",\"seq\":0}\n");
    fs::write(&log, &text).unwrap();
    let opened = FileHistory::open(&fx.store, vec![fx.root.clone()], CheckpointConfig::default());
    assert!(matches!(opened, Err(CheckpointError::Corrupt(_))));
}

#[test]
fn undo_takes_the_rewind_back() {
    let fx = Fixture::new();
    fx.write("a.txt", "before\n");
    let history = fx.open();
    assert_eq!(history.undo_rewind(false, TIMEOUT), Err(CheckpointError::NothingToUndo));
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "after\n");
    history.track_edit(&fx.path("new.txt")).unwrap();
    fx.write("new.txt", "new\n");

    history.rewind("t1", false, TIMEOUT).unwrap();
    assert!(history.list().unwrap().can_undo);
    let report = history.undo_rewind(false, TIMEOUT).unwrap();
    assert_eq!(
        changes(&report),
        vec![("a.txt".into(), Change::Restore), ("new.txt".into(), Change::Recreate)]
    );
    assert_eq!(fx.read("a.txt").as_deref(), Some("after\n"));
    assert_eq!(fx.read("new.txt").as_deref(), Some("new\n"));
    assert_eq!(history.undo_rewind(false, TIMEOUT), Err(CheckpointError::NothingToUndo));
}

#[test]
fn undo_survives_a_reopen_until_the_next_turn() {
    let fx = Fixture::new();
    fx.write("a.txt", "before\n");
    {
        let history = fx.open();
        history.make_snapshot("t1").unwrap();
        history.track_edit(&fx.path("a.txt")).unwrap();
        fx.write("a.txt", "after\n");
        history.rewind("t1", false, TIMEOUT).unwrap();
    }
    let history = fx.open();
    assert!(history.list().unwrap().can_undo);
    history.make_snapshot("t2").unwrap();
    assert!(!history.list().unwrap().can_undo, "a new turn may have changed the files again");
    drop(history);
    assert!(!fx.open().list().unwrap().can_undo);
}

#[test]
fn paths_outside_the_folders_are_refused() {
    let fx = Fixture::new();
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    assert_eq!(history.track_edit(Path::new("relative.txt")), Err(CheckpointError::PathOutsideRoots));
    assert_eq!(
        history.track_edit(&fx.root.join("..").join("escape.txt")),
        Err(CheckpointError::PathOutsideRoots)
    );

    // Tracked while the folder was a work folder, then the thread's folders changed.
    let outside = fx.root.parent().unwrap().join("outside.txt");
    fs::write(&outside, "o\n").unwrap();
    history.track_edit(&outside).unwrap();
    fs::write(&outside, "changed\n").unwrap();
    let report = history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(skipped(&report), vec![("outside.txt".into(), SkipReason::OutsideRoots)]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "changed\n");
}

#[cfg(unix)]
#[test]
fn a_link_on_the_way_is_not_followed() {
    let fx = Fixture::new();
    fx.write("dir/a.txt", "a0\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("dir/a.txt")).unwrap();
    fx.write("dir/a.txt", "a1\n");
    // The folder is swapped for a link to somewhere else.
    let elsewhere = fx.root.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("a.txt"), "not ours\n").unwrap();
    fs::rename(fx.path("dir"), fx.root.join("moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.path("dir")).unwrap();

    let report = history.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(skipped(&report), vec![("a.txt".into(), SkipReason::Symlink)]);
    assert_eq!(fs::read_to_string(elsewhere.join("a.txt")).unwrap(), "not ours\n");
}

#[test]
fn a_running_restore_blocks_another() {
    let fx = Fixture::new();
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.lock().restoring = Some(Instant::now());
    assert!(history.restore_in_progress());
    assert_eq!(history.rewind("t1", true, TIMEOUT), Err(CheckpointError::RestoreInProgress));
    assert_eq!(history.undo_rewind(true, TIMEOUT), Err(CheckpointError::RestoreInProgress));

    // A lock older than the limit was left behind: it no longer counts.
    if let Some(stale) = Instant::now().checked_sub(RESTORE_LOCK + Duration::from_secs(1)) {
        history.lock().restoring = Some(stale);
        assert!(!history.restore_in_progress());
        assert!(history.rewind("t1", true, TIMEOUT).is_ok());
    }
    history.rewind("t1", true, TIMEOUT).unwrap();
    assert!(!history.restore_in_progress(), "the lock is released after a rewind");
}

#[test]
fn a_rewind_past_its_time_stops() {
    let fx = Fixture::new();
    fx.write("a.txt", "a\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "b\n");
    assert_eq!(history.rewind("t1", false, Duration::ZERO), Err(CheckpointError::TimedOut));
    assert_eq!(fx.read("a.txt").as_deref(), Some("b\n"));
}

#[test]
fn track_edit_needs_a_snapshot() {
    let fx = Fixture::new();
    let history = fx.open();
    assert_eq!(history.track_edit(&fx.path("a.txt")), Err(CheckpointError::NoSnapshot));
}

#[test]
fn disabled_does_nothing() {
    let fx = Fixture::new();
    let history = fx.open_with(CheckpointConfig {
        enabled: false,
        ..CheckpointConfig::default()
    });
    assert!(!history.enabled());
    assert_eq!(history.make_snapshot("t1"), Err(CheckpointError::Disabled));
    assert_eq!(history.track_edit(&fx.path("a.txt")), Err(CheckpointError::Disabled));
    assert_eq!(history.list(), Err(CheckpointError::Disabled));
    assert_eq!(history.rewind("t1", true, TIMEOUT), Err(CheckpointError::Disabled));
    assert_eq!(history.undo_rewind(true, TIMEOUT), Err(CheckpointError::Disabled));
    assert!(!fx.store.exists());
}

#[test]
fn a_fork_starts_with_the_same_checkpoints() {
    let fx = Fixture::new();
    fx.write("a.txt", "a0\n");
    let history = fx.open();
    history.make_snapshot("t1").unwrap();
    history.track_edit(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "a1\n");
    let forked_dir = fx.store.parent().unwrap().join("thread-2");
    history.copy_to(&forked_dir).unwrap();

    let forked = FileHistory::open(&forked_dir, vec![fx.root.clone()], CheckpointConfig::default()).unwrap();
    forked.rewind("t1", false, TIMEOUT).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("a0\n"));
}

#[test]
fn codes_are_stable() {
    assert_eq!(CheckpointError::TurnInProgress.code(), "turn_in_progress");
    assert_eq!(CheckpointError::BackupMissing.code(), "backup_missing");
    assert_eq!(SkipReason::OutsideRoots.code(), "outside_roots");
    assert_eq!(
        serde_json::to_value(SkipReason::BackupMissing).unwrap(),
        serde_json::json!("backup_missing")
    );
    assert_eq!(serde_json::to_value(Change::Recreate).unwrap(), serde_json::json!("recreate"));
}

#[test]
fn one_history_per_folder_in_a_process() {
    let fx = Fixture::new();
    let first = shared(&fx.store, CheckpointConfig::default()).unwrap();
    let second = shared(&fx.store, CheckpointConfig::default()).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    first.make_snapshot("t1").unwrap();
    assert_eq!(second.list().unwrap().checkpoints.len(), 1);

    assert!(!restore_running(&fx.store));
    first.lock().restoring = Some(Instant::now());
    assert!(restore_running(&fx.store));
    assert!(!restore_running(&fx.store.with_file_name("other")), "another thread");
    first.lock().restoring = None;
}

#[test]
fn roots_from_the_configuration_do_not_replace_the_turns() {
    let fx = Fixture::new();
    let history = FileHistory::open(&fx.store, Vec::new(), CheckpointConfig::default()).unwrap();
    history.set_roots_if_unset(vec![fx.root.clone()]);
    history.set_roots_if_unset(vec![PathBuf::from("/elsewhere")]);
    assert_eq!(*history.roots.lock().unwrap(), vec![fx.root]);
}

#[test]
fn thread_folders_are_plain_names() {
    let home = Path::new("/home/a/.onigio");
    assert_eq!(
        thread_dir(home, "019a-ff_2").unwrap(),
        PathBuf::from("/home/a/.onigio/checkpoints/019a-ff_2")
    );
    for bad in ["", "..", "a/b", "a\\b", "x.y"] {
        assert_eq!(thread_dir(home, bad), Err(CheckpointError::PathOutsideRoots), "{bad}");
    }
}
