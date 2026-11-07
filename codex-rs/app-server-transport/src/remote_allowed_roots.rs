//! Folders a remote-control session may work in (the single definition of how they are passed).
//!
//! `remote-control --allow <DIR>` (default: the current directory) hands the roots to the
//! app-server in one of two ways:
//! - foreground: directly through the app-server runtime options;
//! - daemon (`remote-control start`): written to [`REMOTE_ALLOWED_ROOTS_FILE`] in the daemon's state
//!   directory; every daemon launch (start, restart, update) exports them to the child as
//!   [`SITE_RELAY_ALLOWED_ROOTS_ENV_VAR`] (platform path-list separator, as in `PATH`).
//!
//! The app-server enforces them on remote-control connections only (`app-server/src/remote_roots.rs`).

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

/// Path list (`PATH`-style separator) of the folders remote-control requests may touch.
pub const SITE_RELAY_ALLOWED_ROOTS_ENV_VAR: &str = "SITE_RELAY_ALLOWED_ROOTS";
/// Path of a JSON array of folders (same format as [`REMOTE_ALLOWED_ROOTS_FILE`]) that local
/// connections (stdio, websocket) are held to as well; the launcher keeps the file current and the
/// app-server re-reads it when it changes (`app-server/src/local_roots.rs`). Unset: unchecked.
pub const SITE_LOCAL_ALLOWED_ROOTS_FILE_ENV_VAR: &str = "SITE_LOCAL_ALLOWED_ROOTS_FILE";
/// JSON array of absolute folder paths, in the daemon state directory.
pub const REMOTE_ALLOWED_ROOTS_FILE: &str = "remote-control-allowed-roots.json";
const MAX_ROOTS_FILE_BYTES: u64 = 64 * 1024;

/// Encodes roots for [`SITE_RELAY_ALLOWED_ROOTS_ENV_VAR`].
pub fn encode_allowed_roots(roots: &[PathBuf]) -> io::Result<OsString> {
    std::env::join_paths(roots).map_err(|err| io::Error::new(ErrorKind::InvalidInput, err))
}

/// Decodes [`SITE_RELAY_ALLOWED_ROOTS_ENV_VAR`]; empty entries are dropped, so an empty value
/// means "no folder" (everything refused), not "unrestricted".
pub fn decode_allowed_roots(value: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

/// Reads the daemon's saved roots; `Ok(None)` when none were saved.
pub fn read_allowed_roots_file(state_dir: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    match read_roots_json(&state_dir.join(REMOTE_ALLOWED_ROOTS_FILE)) {
        Ok(roots) => Ok(Some(roots)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Reads a JSON list of folders (at most 64 KiB).
pub fn read_roots_json(path: &Path) -> io::Result<Vec<PathBuf>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut raw = String::new();
    file.take(MAX_ROOTS_FILE_BYTES).read_to_string(&mut raw)?;
    serde_json::from_str(&raw).map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{} is not a list of folders: {err}", path.display()),
        )
    })
}

/// Saves the daemon's roots; returns whether they differ from what was saved before.
pub fn write_allowed_roots_file(state_dir: &Path, roots: &[PathBuf]) -> io::Result<bool> {
    if roots.iter().any(|root| !root.is_absolute()) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "allowed folders must be absolute paths",
        ));
    }
    // An unreadable or malformed previous file counts as a change.
    let previous = read_allowed_roots_file(state_dir).ok().flatten();
    if previous.as_deref() == Some(roots) {
        return Ok(false);
    }
    std::fs::create_dir_all(state_dir)?;
    let json = serde_json::to_vec_pretty(roots).map_err(io::Error::other)?;
    let path = state_dir.join(REMOTE_ALLOWED_ROOTS_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn env_value_round_trips_and_empty_means_none() {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().join("a b"), dir.path().join("c")];
        let encoded = encode_allowed_roots(&roots).unwrap();
        assert_eq!(decode_allowed_roots(&encoded), roots);
        assert_eq!(decode_allowed_roots(OsStr::new("")), Vec::<PathBuf>::new());
    }

    #[test]
    fn file_reports_changes_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_allowed_roots_file(dir.path()).unwrap(), None);
        let roots = vec![dir.path().join("project")];
        assert!(write_allowed_roots_file(dir.path(), &roots).unwrap());
        assert!(!write_allowed_roots_file(dir.path(), &roots).unwrap());
        assert_eq!(read_allowed_roots_file(dir.path()).unwrap(), Some(roots));
        assert!(write_allowed_roots_file(dir.path(), &[dir.path().to_path_buf()]).unwrap());
        assert!(write_allowed_roots_file(dir.path(), &[PathBuf::from("relative")]).is_err());
    }
}
