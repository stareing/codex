//! `log.jsonl`: snapshot lines never change; a first backup taken during a turn is appended as a delta of that
//! turn's snapshot (Claude Code's `file-history-snapshot` / `file-history-delta`); rewinds append their pre-rewind
//! backups and undos. Opening a thread replays it.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::PreRewind;
use crate::Snapshot;
use crate::error::CheckpointError;
use crate::store::Backup;

const LOG_FILE: &str = "log.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum LogLine {
    Snapshot {
        turn_id: String,
        timestamp_ms: u64,
        backups: BTreeMap<PathBuf, Backup>,
    },
    Delta {
        turn_id: String,
        path: PathBuf,
        backup: Backup,
    },
    PreRewind {
        seq: u64,
        backups: BTreeMap<PathBuf, Backup>,
    },
    Undo {
        seq: u64,
    },
}

impl LogLine {
    pub(crate) fn snapshot(snapshot: &Snapshot) -> Self {
        LogLine::Snapshot {
            turn_id: snapshot.turn_id.clone(),
            timestamp_ms: snapshot.timestamp_ms,
            backups: snapshot.backups.clone(),
        }
    }

    pub(crate) fn delta(turn_id: &str, path: &Path, backup: &Backup) -> Self {
        LogLine::Delta {
            turn_id: turn_id.to_string(),
            path: path.to_path_buf(),
            backup: backup.clone(),
        }
    }

    pub(crate) fn pre_rewind(pre: &PreRewind) -> Self {
        LogLine::PreRewind {
            seq: pre.seq,
            backups: pre.backups.clone(),
        }
    }

    pub(crate) fn undo(seq: u64) -> Self {
        LogLine::Undo { seq }
    }
}

fn encode(line: &LogLine) -> Result<String, CheckpointError> {
    serde_json::to_string(line).map_err(|err| CheckpointError::Corrupt(err.to_string()))
}

pub(crate) fn append(dir: &Path, line: &LogLine) -> Result<(), CheckpointError> {
    let text = encode(line)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))?;
    file.write_all(format!("{text}\n").as_bytes())?;
    Ok(())
}

/// Replaces the log with the kept snapshots (through a temporary file renamed over it), after old ones were dropped.
pub(crate) fn rewrite(
    dir: &Path,
    snapshots: &[Snapshot],
    rewinds: u64,
    last_rewind: Option<&PreRewind>,
) -> Result<(), CheckpointError> {
    let mut text = String::new();
    for snapshot in snapshots {
        text.push_str(&encode(&LogLine::snapshot(snapshot))?);
        text.push('\n');
    }
    // The rewind count survives (pre-rewind backup names must not repeat): as the undoable rewind, or as an undo.
    let last = match last_rewind {
        Some(pre) => Some(LogLine::pre_rewind(pre)),
        None => (rewinds > 0).then_some(LogLine::Undo { seq: rewinds }),
    };
    if let Some(last) = last {
        text.push_str(&encode(&last)?);
        text.push('\n');
    }
    let temporary = dir.join(format!("{LOG_FILE}.tmp"));
    fs::write(&temporary, text)?;
    fs::rename(&temporary, dir.join(LOG_FILE))?;
    Ok(())
}

#[derive(Default)]
pub(crate) struct Replayed {
    pub snapshots: Vec<Snapshot>,
    pub rewinds: u64,
    pub last_rewind: Option<PreRewind>,
}

/// Reads the log back. A last line cut short (the process stopped while writing it) is ignored; any other line
/// that does not parse fails the whole replay.
pub(crate) fn replay(dir: &Path) -> Result<Replayed, CheckpointError> {
    let text = match fs::read_to_string(dir.join(LOG_FILE)) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Replayed::default()),
        Err(err) => return Err(err.into()),
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut replayed = Replayed::default();
    for (index, raw) in lines.iter().enumerate() {
        let line = match serde_json::from_str::<LogLine>(raw) {
            Ok(line) => line,
            Err(_) if index + 1 == lines.len() && !text.ends_with('\n') => break,
            Err(err) => {
                return Err(CheckpointError::Corrupt(format!(
                    "line {}: {err}",
                    index + 1
                )));
            }
        };
        apply(&mut replayed, line);
    }
    Ok(replayed)
}

fn apply(replayed: &mut Replayed, line: LogLine) {
    match line {
        LogLine::Snapshot {
            turn_id,
            timestamp_ms,
            backups,
        } => {
            replayed.last_rewind = None;
            replayed.snapshots.push(Snapshot {
                turn_id,
                timestamp_ms,
                backups,
            });
        }
        LogLine::Delta {
            turn_id,
            path,
            backup,
        } => {
            replayed.last_rewind = None;
            if let Some(snapshot) = replayed
                .snapshots
                .iter_mut()
                .rev()
                .find(|s| s.turn_id == turn_id)
            {
                snapshot.backups.entry(path).or_insert(backup);
            }
        }
        LogLine::PreRewind { seq, backups } => {
            replayed.rewinds = replayed.rewinds.max(seq);
            replayed.last_rewind = Some(PreRewind { seq, backups });
        }
        LogLine::Undo { seq } => {
            replayed.rewinds = replayed.rewinds.max(seq);
            replayed.last_rewind = None;
        }
    }
}
