//! Folder allow-list for connections from this computer (stdio, websocket).
//!
//! Upstream never checks local connections. A launcher that drives the app-server on behalf of a
//! web page (a desktop app relaying its chat page) can hold them to the folders its user allowed by
//! naming a roots file in `SITE_LOCAL_ALLOWED_ROOTS_FILE` (a JSON list of folders, the same format
//! as the remote-control roots file). The check is the remote-control one (`remote_roots`); only the
//! wording of refusals differs.
//!
//! The launcher may change the list while the server runs, so the file is re-read whenever its
//! size or modification time changes. A file that is named but cannot be read or parsed refuses
//! everything (fail closed) and is retried on the next request. Unset: unchecked, as upstream.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use codex_app_server_transport::SITE_LOCAL_ALLOWED_ROOTS_FILE_ENV_VAR;
use codex_app_server_transport::read_roots_json;

use crate::remote_roots::RemoteRoots;

/// What identifies one version of the roots file.
type Stamp = (u64, Option<SystemTime>);

pub(crate) struct LocalRoots {
    file: Option<PathBuf>,
    base: PathBuf,
    codex_home: PathBuf,
    cached: Mutex<Option<(Stamp, RemoteRoots)>>,
}

impl LocalRoots {
    /// The roots file from [`SITE_LOCAL_ALLOWED_ROOTS_FILE_ENV_VAR`], if set.
    pub(crate) fn from_env(base: PathBuf, codex_home: PathBuf) -> Self {
        let file = std::env::var_os(SITE_LOCAL_ALLOWED_ROOTS_FILE_ENV_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self::new(file, base, codex_home)
    }

    pub(crate) fn new(file: Option<PathBuf>, base: PathBuf, codex_home: PathBuf) -> Self {
        Self {
            file,
            base,
            codex_home,
            cached: Mutex::new(None),
        }
    }

    pub(crate) fn unchecked() -> Self {
        Self::new(None, PathBuf::new(), PathBuf::new())
    }

    /// The roots local requests are held to now; `None` when no roots file was named.
    pub(crate) fn current(&self) -> Option<RemoteRoots> {
        let file = self.file.as_ref()?;
        let stamp = std::fs::metadata(file)
            .ok()
            .map(|meta| (meta.len(), meta.modified().ok()));
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let (Some(stamp), Some((seen, roots))) = (stamp, cached.as_ref())
            && stamp == *seen
        {
            return Some(roots.clone());
        }
        let roots = match read_roots_json(file) {
            Ok(roots) => roots,
            Err(err) => {
                tracing::warn!(file = %file.display(), %err, "allowed-folders file unreadable; refusing local requests that name folders");
                *cached = None;
                return Some(self.roots(Vec::new()));
            }
        };
        let roots = self.roots(roots);
        if let Some(stamp) = stamp {
            *cached = Some((stamp, roots.clone()));
        }
        Some(roots)
    }

    fn roots(&self, roots: Vec<PathBuf>) -> RemoteRoots {
        RemoteRoots::new(Some(roots), self.base.clone(), self.codex_home.clone()).local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(path: &Path, roots: &[&Path]) {
        std::fs::write(path, serde_json::to_vec(roots).unwrap()).unwrap();
    }

    #[test]
    fn unnamed_means_unchecked() {
        assert!(LocalRoots::unchecked().current().is_none());
    }

    #[test]
    fn the_file_is_followed_as_it_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let file = tmp.path().join("roots.json");
        write(&file, &[&a]);
        let local = LocalRoots::new(
            Some(file.clone()),
            tmp.path().into(),
            tmp.path().join("home"),
        );

        let roots = local.current().unwrap();
        assert!(roots.check(&a.join("x.txt")).is_ok());
        let refused = roots.check(&b).unwrap_err();
        assert!(
            refused.starts_with("this session may only use"),
            "{refused}"
        );

        // A folder allowed later counts at once (a different size is a different version).
        write(&file, &[&a, &b]);
        assert!(local.current().unwrap().check(&b).is_ok());
    }

    #[test]
    fn an_unreadable_file_refuses_everything_until_fixed() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let file = tmp.path().join("roots.json");
        let local = LocalRoots::new(
            Some(file.clone()),
            tmp.path().into(),
            tmp.path().join("home"),
        );
        assert!(local.current().unwrap().check(&a).is_err(), "missing file");
        std::fs::write(&file, "not json").unwrap();
        assert!(
            local.current().unwrap().check(&a).is_err(),
            "malformed file"
        );
        write(&file, &[&a]);
        assert!(local.current().unwrap().check(&a).is_ok());
    }
}
