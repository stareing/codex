//! Backup files on disk: naming, copying, change detection, and writing a file back without following links.

use std::fs;
use std::io;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::error::SkipReason;

/// One file's state at a snapshot. `file: None` and not `unavailable` = the file did not exist then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    pub file: Option<String>,
    pub version: u32,
    pub time_ms: u64,
    /// No copy was kept (the file was over the size limit, or the store over its total): restoring it reports
    /// `backup_missing`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unavailable: bool,
}

impl Backup {
    pub(crate) fn absent(version: u32) -> Self {
        Self {
            file: None,
            version,
            time_ms: now_ms(),
            unavailable: false,
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `<first 16 hex of sha256(path)>@v<version>`: only the hash, never the path itself.
pub(crate) fn backup_name(path: &Path, version: u32) -> String {
    format!("{}@v{version}", path_hash(path))
}

fn path_hash(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A backup file's name (`<hash>@v<n>` or `<hash>@r<n>`), as opposed to the log and anything else in the folder.
fn is_backup_file(name: &str) -> bool {
    name.split_once('@').is_some_and(|(hash, rest)| {
        hash.len() == 16 && (rest.starts_with('v') || rest.starts_with('r'))
    })
}

/// Limits a backup must fit in: the file's size and what the store may still hold.
pub(crate) struct Budget {
    pub max_file_bytes: u64,
    pub remaining_bytes: u64,
}

/// Copies `path` as backup `version` into `dir`; returns the backup and the bytes it took.
pub(crate) fn create_backup(
    dir: &Path,
    path: &Path,
    version: u32,
    budget: &Budget,
) -> io::Result<(Backup, u64)> {
    copy_as(dir, path, version, backup_name(path, version), budget)
}

/// The file's state just before rewind `seq` (`<hash>@r<seq>`, apart from the versions).
pub(crate) fn create_pre_rewind(
    dir: &Path,
    path: &Path,
    seq: u64,
    budget: &Budget,
) -> io::Result<(Backup, u64)> {
    let name = format!("{}@r{seq}", path_hash(path));
    copy_as(dir, path, 0, name, budget)
}

fn copy_as(
    dir: &Path,
    path: &Path,
    version: u32,
    name: String,
    budget: &Budget,
) -> io::Result<(Backup, u64)> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok((Backup::absent(version), 0));
        }
        Err(err) => return Err(err),
    };
    let size = metadata.len();
    if !metadata.is_file() || size > budget.max_file_bytes || size > budget.remaining_bytes {
        return Ok((
            Backup {
                file: None,
                version,
                time_ms: now_ms(),
                unavailable: true,
            },
            0,
        ));
    }
    // `fs::copy` keeps the permission bits; the copy's mtime is now (the change check relies on it).
    let copied = fs::copy(path, dir.join(&name))?;
    Ok((
        Backup {
            file: Some(name),
            version,
            time_ms: now_ms(),
            unavailable: false,
        },
        copied,
    ))
}

/// Claude Code's `checkOriginFileChanged`: whether `path` differs from `backup`, cheapest test first.
pub(crate) fn changed_since(dir: &Path, path: &Path, backup: &Backup) -> bool {
    let current = fs::symlink_metadata(path).ok();
    if backup.unavailable {
        return true;
    }
    let Some(name) = &backup.file else {
        return current.is_some();
    };
    let Some(current) = current else {
        return true;
    };
    let stored_path = dir.join(name);
    let Ok(stored) = fs::metadata(&stored_path) else {
        return true;
    };
    if current.len() != stored.len() || current.permissions() != stored.permissions() {
        return true;
    }
    if let (Ok(original), Ok(copied)) = (current.modified(), stored.modified())
        && original < copied
    {
        return false;
    }
    !same_bytes(path, &stored_path).unwrap_or(false)
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let mut left = fs::File::open(a)?;
    let mut right = fs::File::open(b)?;
    let mut left_buf = vec![0u8; 64 * 1024];
    let mut right_buf = vec![0u8; 64 * 1024];
    loop {
        let read = left.read(&mut left_buf)?;
        if read == 0 {
            return Ok(right.read(&mut right_buf[..1])? == 0);
        }
        right.read_exact(&mut right_buf[..read])?;
        if left_buf[..read] != right_buf[..read] {
            return Ok(false);
        }
    }
}

/// `path` lexically inside `root` (whole components; on Windows regardless of case).
pub(crate) fn within(path: &Path, root: &Path) -> bool {
    let mut parts = path.components();
    root.components().all(|part| {
        parts
            .next()
            .is_some_and(|other| same_component(part, other))
    })
}

#[cfg(windows)]
fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

#[cfg(not(windows))]
fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    a == b
}

/// `path` may be written: inside one of `roots`, absolute, no `..`, and no symbolic link on the way below the
/// root (folders still missing are fine: they are created).
pub(crate) fn writable_target(path: &Path, roots: &[PathBuf]) -> Result<(), SkipReason> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(SkipReason::OutsideRoots);
    }
    let Some(root) = roots.iter().find(|root| within(path, root)) else {
        return Err(SkipReason::OutsideRoots);
    };
    let mut current = root.clone();
    for part in path.components().skip(root.components().count()) {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(SkipReason::Symlink),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(SkipReason::Io),
        }
    }
    Ok(())
}

/// The bytes of a backup (`None` = the file did not exist).
pub(crate) fn backup_bytes(dir: &Path, backup: &Backup) -> io::Result<Option<Vec<u8>>> {
    match &backup.file {
        Some(name) => fs::read(dir.join(name)).map(Some),
        None => Ok(None),
    }
}

/// The file's current bytes (`None` = it does not exist).
pub(crate) fn current_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Puts the backup back at `path` (through a temporary file in the same folder, renamed over it), or deletes
/// `path` when the backup says it did not exist.
pub(crate) fn restore(dir: &Path, path: &Path, backup: &Backup) -> io::Result<()> {
    let Some(name) = &backup.file else {
        return match fs::remove_file(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let temporary = parent.join(format!(
        ".{}.checkpoint-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    fs::copy(dir.join(name), &temporary)?;
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Lines added and removed going from `from` to `to` (a missing file is empty).
pub(crate) fn line_stats(from: Option<&[u8]>, to: Option<&[u8]>) -> (u64, u64) {
    let from = String::from_utf8_lossy(from.unwrap_or_default());
    let to = String::from_utf8_lossy(to.unwrap_or_default());
    let diff = similar::TextDiff::from_lines(from.as_ref(), to.as_ref());
    let mut insertions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => insertions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (insertions, deletions)
}

/// Bytes the store's backup files take.
pub(crate) fn stored_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| is_backup_file(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// Deletes the backup files not in `keep` (those of dropped snapshots and of rewinds no longer undoable).
pub(crate) fn remove_unreferenced(dir: &Path, keep: &std::collections::BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_backup_file(&name) && !keep.contains(&name) {
            // Best effort: a file left behind only takes space (it is counted against the store's limit).
            let _ = fs::remove_file(entry.path());
        }
    }
}
