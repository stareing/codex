//! Drops results in git-ignored files (build output, vendored dependencies) with `git check-ignore`.
//! Outside a repository, without git, or on a timeout nothing is dropped.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use codex_utils_process::background_command;

use crate::uri::uri_to_path;

/// Paths per `git check-ignore` call.
pub const BATCH_SIZE: usize = 50;
/// Time one `git check-ignore` call may take.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The subset of `uris` that git ignores in the repository around `cwd`.
pub async fn ignored_uris(uris: &[String], cwd: &Path) -> BTreeSet<String> {
    let by_path: BTreeMap<String, &String> = uris
        .iter()
        .filter_map(|uri| uri_to_path(uri).map(|path| (path.to_string_lossy().into_owned(), uri)))
        .collect();
    let paths: Vec<&String> = by_path.keys().collect();
    let mut ignored = BTreeSet::new();
    for batch in paths.chunks(BATCH_SIZE) {
        let mut command = tokio::process::Command::from(background_command("git"));
        command
            .arg("check-ignore")
            .arg("--")
            .args(batch.iter().map(|p| p.as_str()))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let Ok(Ok(output)) = tokio::time::timeout(GIT_TIMEOUT, command.output()).await else {
            continue;
        };
        // Exit 0: some paths are ignored; 1: none; 128: not a repository.
        if output.status.code() != Some(0) {
            continue;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(uri) = by_path.get(line.trim()) {
                ignored.insert((*uri).clone());
            }
        }
    }
    ignored
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::uri::path_to_uri;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn reports_ignored_files_inside_a_repository_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inited = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status();
        if !inited.is_ok_and(|status| status.success()) {
            // No git on this machine: the filter is a no-op by design.
            return;
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let kept = path_to_uri(&root.join("src/a.rs")).unwrap();
        let dropped = path_to_uri(&root.join("target/b.rs")).unwrap();
        let ignored = ignored_uris(&[kept, dropped.clone()], &root).await;
        assert_eq!(ignored, BTreeSet::from([dropped]));

        let outside = tempfile::tempdir().unwrap();
        let none = ignored_uris(
            &[path_to_uri(&outside.path().join("x.rs")).unwrap()],
            outside.path(),
        )
        .await;
        assert!(none.is_empty());
    }
}
