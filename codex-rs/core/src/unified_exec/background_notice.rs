//! Tells the model about a command it left running in the background once it is no longer watching it:
//! when the command ends, and when it seems to wait for input that will never come.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;
use std::time::Duration;

use regex_lite::Regex;

use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use crate::session::session::Session;
use crate::tools::exit_code_meaning::exit_code_note;
use crate::unified_exec::process::UnifiedExecProcess;

/// How often a running background command is looked at.
pub(super) const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// No new output for this long, ending in what looks like a question, counts as waiting for input.
pub(super) const STALL_AFTER: Duration = Duration::from_secs(45);
/// Output shown with a notice, from the end.
const TAIL_BYTES: usize = 1024;
const TRIGGER: &str = "background_command";

/// Last lines that ask for an answer: yes/no choices, "Press Enter", and questions such as
/// "Do you want to continue?".
static PROMPT: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)[\(\[]\s*y(es)?\s*/\s*n(o)?\s*[\)\]]",
        r"(?i)\bpress (any key|enter|return)\b",
        r"(?i)\b(continue|proceed|overwrite|replace)\?\s*$",
        r"(?i)\b(do you|would you|shall i|are you sure|ready to)\b.*\?\s*$",
    ]
    .into_iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// The end of `output`, at most `TAIL_BYTES`, without a split character.
pub(super) fn tail(output: &[u8]) -> String {
    let start = output.len().saturating_sub(TAIL_BYTES);
    let text = String::from_utf8_lossy(&output[start..]);
    text.trim_start_matches('\u{FFFD}').to_string()
}

/// Whether the last non-empty line of `tail` asks for an answer.
pub(super) fn looks_like_prompt(tail: &str) -> bool {
    let Some(line) = tail.lines().rev().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let line = line.trim_end();
    PROMPT.iter().any(|pattern| pattern.is_match(line))
}

fn output_block(tail: &str) -> String {
    if tail.trim().is_empty() {
        "It printed nothing.".to_string()
    } else {
        format!("The end of its output:\n```\n{}\n```", tail.trim_end())
    }
}

pub(super) fn exit_notice(process_id: i32, command: &str, exit_code: i32, tail: &str) -> String {
    let meaning = exit_code_note(command, exit_code)
        .map(|note| format!(" ({note})"))
        .unwrap_or_default();
    format!(
        "The background command `{command}` (session {process_id}) has finished with exit code \
         {exit_code}{meaning}. {}\nUse it as needed; there is no need to poll that session again.",
        output_block(tail)
    )
}

pub(super) fn failure_notice(process_id: i32, command: &str, message: &str) -> String {
    format!("The background command `{command}` (session {process_id}) has stopped: {message}")
}

pub(super) fn stall_notice(process_id: i32, command: &str, tail: &str) -> String {
    format!(
        "The background command `{command}` (session {process_id}) has printed nothing for {} \
         seconds and its last line looks like a question waiting for an answer. {}\nIf it does need \
         input, answer with write_stdin on session {process_id}, or stop it and run it again \
         non-interactively (for example with a `--yes` flag or input piped in); otherwise ignore \
         this.",
        STALL_AFTER.as_secs(),
        output_block(tail)
    )
}

/// Hands `body` to the model: into the running turn, or as a new automatic turn when idle.
pub(super) async fn deliver(session: &Arc<Session>, body: String) {
    let item = ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static(TRIGGER),
        body,
    ));
    session.deliver_or_start_turn(item, TRIGGER).await;
}

/// Watches a background command for a stall until it ends. Holds the session only weakly.
pub(super) fn spawn_stall_watcher(
    session: Weak<Session>,
    process: Arc<UnifiedExecProcess>,
    process_id: i32,
    command: String,
) {
    tokio::spawn(async move {
        let exit_token = process.cancellation_token();
        let mut last_total = None;
        let mut quiet_for = Duration::ZERO;
        let mut told_at_total = None;
        loop {
            tokio::select! {
                _ = exit_token.cancelled() => return,
                _ = tokio::time::sleep(STALL_CHECK_INTERVAL) => {}
            }
            let Some(session) = session.upgrade() else {
                return;
            };
            let (total, output) = {
                let buffers = process.output_handles().output_buffer.lock().await;
                (
                    buffers.transcript.total_bytes(),
                    buffers.transcript.to_bytes(),
                )
            };
            if last_total != Some(total) {
                last_total = Some(total);
                quiet_for = Duration::ZERO;
                continue;
            }
            quiet_for += STALL_CHECK_INTERVAL;
            if quiet_for < STALL_AFTER || told_at_total == Some(total) {
                continue;
            }
            let tail = tail(&output);
            // The model polling it right now sees the prompt itself.
            let polling = process.interaction_lock().try_lock().is_err();
            if polling
                || !looks_like_prompt(&tail)
                || !session
                    .services
                    .unified_exec_manager
                    .runs_unwatched(process_id, &process)
                    .await
            {
                continue;
            }
            told_at_total = Some(total);
            deliver(&session, stall_notice(process_id, &command, &tail)).await;
        }
    });
}

#[cfg(test)]
#[path = "background_notice_tests.rs"]
mod tests;
