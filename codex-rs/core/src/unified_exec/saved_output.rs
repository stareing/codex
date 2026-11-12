//! Keeps the whole collected output of a command whose model-facing copy is cut (head and tail kept, the middle
//! dropped), in `<temp>/onigio-output/<thread>/<call>.txt`, and tells the model where it is, so it can Read or Grep
//! the dropped part instead of running the command again. Only for commands run on this machine (the file must be
//! where the model's own tools look); each thread keeps its newest `MAX_FILES_PER_THREAD` files.

use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;

use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::SavedOutput;

const DIR_NAME: &str = "onigio-output";
const MAX_FILES_PER_THREAD: usize = 50;

pub(crate) fn thread_dir(temp: &Path, thread: ThreadId) -> PathBuf {
    temp.join(DIR_NAME).join(thread.to_string())
}

/// Saves `output`'s collected bytes when the model will only see part of them and records where in
/// `output.saved_output`. A failure to save only means the note is left out.
pub(crate) async fn save_if_truncated(
    output: &mut ExecCommandToolOutput,
    dir: PathBuf,
    call_id: &str,
) {
    if output.saved_output.is_some() || !output.will_be_truncated() {
        return;
    }
    let name: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let chunk = if output.chunk_id.is_empty() {
        String::new()
    } else {
        format!("-{}", output.chunk_id)
    };
    let path = dir.join(format!("{name}{chunk}.txt"));
    let bytes = output.raw_output.text.clone().into_bytes();
    let written = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, &bytes)?;
            prune(&dir);
            Ok(())
        }
    })
    .await;
    match written {
        Ok(Ok(())) => {
            output.saved_output = Some(SavedOutput {
                path: path.to_string_lossy().into_owned(),
                bytes: output.raw_output.text.len(),
            });
        }
        Ok(Err(err)) => tracing::warn!("could not save truncated command output: {err}"),
        Err(err) => tracing::warn!("could not save truncated command output: {err}"),
    }
}

/// Removes all but the newest `MAX_FILES_PER_THREAD` files in `dir`.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if files.len() <= MAX_FILES_PER_THREAD {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(MAX_FILES_PER_THREAD) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use codex_utils_output_truncation::TruncationPolicy;
    use pretty_assertions::assert_eq;

    fn output(raw: &str) -> ExecCommandToolOutput {
        ExecCommandToolOutput {
            saved_output: None,
            event_call_id: "call-1".to_string(),
            chunk_id: "c1".to_string(),
            wall_time: std::time::Duration::ZERO,
            raw_output: raw.into(),
            truncation_policy: TruncationPolicy::Bytes(200),
            max_output_tokens: None,
            process_id: None,
            exit_code: Some(0),
            original_token_count: None,
            hook_command: None,
            tool_cwd: None,
        }
    }

    #[tokio::test]
    async fn a_cut_output_is_saved_whole_and_the_model_is_told_where() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("thread");
        let long: String = (0..200).map(|n| format!("line {n}\n")).collect();
        let mut cut = output(&long);
        save_if_truncated(&mut cut, dir.clone(), "call/1").await;
        let saved = cut.saved_output.clone().expect("saved");
        assert_eq!(saved.path, dir.join("call_1-c1.txt").to_string_lossy());
        assert_eq!(std::fs::read_to_string(&saved.path).expect("read"), long);
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        let text = format!("{:?}", cut.to_response_item("call-1", &payload));
        assert!(text.contains(&saved.path.replace('\\', "\\\\")), "{text}");

        let mut short = output("ok\n");
        save_if_truncated(&mut short, dir, "call-2").await;
        assert_eq!(
            short.saved_output, None,
            "nothing is saved when the model sees it all"
        );
    }

    #[test]
    fn a_thread_keeps_only_its_newest_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        for n in 0..MAX_FILES_PER_THREAD + 3 {
            let path = temp.path().join(format!("{n}.txt"));
            std::fs::write(&path, "x").expect("write");
            let when = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000 + n as u64);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(when))
                .expect("set mtime");
        }
        prune(temp.path());
        let mut left: Vec<String> = std::fs::read_dir(temp.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), MAX_FILES_PER_THREAD);
        assert!(!left.contains(&"0.txt".to_string()) && !left.contains(&"2.txt".to_string()));
        assert!(left.contains(&"3.txt".to_string()));
    }
}
