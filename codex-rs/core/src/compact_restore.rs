//! Context put back right after a local compaction, so the model can carry on without first reading everything
//! again: the files it read or changed last (as they are on disk now), its unfinished plan, and the instructions of
//! the skills it was using (both would otherwise be lost: the compacted history keeps only the user's own messages
//! and the summary). One hidden context message after the summary; the limits follow the reference agent's
//! (5 files of at most 5k tokens, 50k in all; skills at most 5k tokens each, 25k in all), and on a small context
//! window they shrink to a share of it so a compaction is not followed straight away by another.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use codex_utils_path_uri::PathUri;

use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use crate::session::session::Session;
use crate::tools::handlers::file_reads::FileKey;
use crate::tools::handlers::file_tools::DEFAULT_READ_LINES;
use crate::tools::handlers::file_tools::numbered;
use codex_history::ResponseItemEnvelope;

pub(crate) const MAX_FILES: usize = 5;
pub(crate) const MAX_TOKENS_PER_FILE: usize = 5_000;
pub(crate) const FILES_TOKEN_BUDGET: usize = 50_000;
pub(crate) const MAX_TOKENS_PER_SKILL: usize = 5_000;
pub(crate) const SKILLS_TOKEN_BUDGET: usize = 25_000;
/// Restored files take at most this share (1/N) of the context window, skills half of that.
const WINDOW_SHARE: usize = 8;
/// Larger files are not put back (the model reads the part it needs).
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const PLAN_TOOL: &str = "update_plan";
const SKILL_OPEN_TAG: &str = "<skill>";
const SKILL_CLOSE_TAG: &str = "</skill>";
/// Source label of the restored context message.
const SOURCE: &str = "compaction_restore";
const PLAN_OPEN_TAG: &str = "<plan>";
const PLAN_CLOSE_TAG: &str = "</plan>";
const FILES_HEADING: &str = "\nFiles read or changed last";

/// One file put back, to record as seen once the history is replaced.
pub(crate) struct RestoredFile {
    pub(crate) key: FileKey,
    pub(crate) uri: PathUri,
    pub(crate) text: String,
}

pub(crate) struct Restored {
    pub(crate) item: Option<ResponseItemEnvelope>,
    pub(crate) files: Vec<RestoredFile>,
}

impl Restored {
    /// Appends the restored context to the replacement history, as part of turn `turn_id`.
    pub(crate) fn append_to(&mut self, history: &mut Vec<ResponseItemEnvelope>, turn_id: &str) {
        if let Some(mut item) = self.item.take() {
            item.set_turn_id_if_missing(turn_id);
            history.push(item);
        }
    }

    /// After the history was replaced: the restored files are in the context again, so `Edit`/`Write` may
    /// change them without a new `Read`.
    pub(crate) fn mark_files_seen(self, sess: &Session) {
        let mut reads = sess
            .file_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for file in self.files {
            reads.wrote(file.key, file.uri, &file.text);
        }
    }
}

/// The token budgets for files and skills on a model with `context_window` tokens.
pub(crate) fn budgets(context_window: Option<i64>) -> (usize, usize) {
    let share = context_window
        .and_then(|w| usize::try_from(w).ok())
        .map_or(usize::MAX, |w| w / WINDOW_SHARE);
    (
        FILES_TOKEN_BUDGET.min(share),
        SKILLS_TOKEN_BUDGET.min(share / 2),
    )
}

/// The context to put back after replacing `history` (the history before compaction).
pub(crate) async fn restore<'a, I>(
    sess: &Session,
    history: I,
    context_window: Option<i64>,
) -> Restored
where
    I: DoubleEndedIterator<Item = &'a ResponseItem> + Clone,
{
    let (files_budget, skills_budget) = budgets(context_window);
    let files = read_recent_files(sess, files_budget).await;
    let plan = unfinished_plan(history.clone());
    let skills = used_skills(history, skills_budget);
    let body = render(&files, plan.as_deref(), &skills);
    let item = body.map(|body| {
        ResponseItemEnvelope::new(ContextualUserFragment::into(
            InternalModelContextFragment::new(InternalContextSource::from_static(SOURCE), body),
        ))
    });
    Restored {
        item,
        files: files.into_iter().map(|(file, _)| file).collect(),
    }
}

/// The latest files, numbered like `Read`, newest first, within the budget (a file that does not fit is left out
/// and the next one tried).
async fn read_recent_files(sess: &Session, budget: usize) -> Vec<(RestoredFile, String)> {
    let recent = sess
        .file_reads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recent(MAX_FILES);
    if recent.is_empty() || budget == 0 {
        return Vec::new();
    }
    let environments = sess.services.turn_environments.snapshot().await;
    let mut used = 0;
    let mut out = Vec::new();
    for (key, uri) in recent {
        let Some(environment) = environments
            .turn_environments()
            .find(|e| e.selection.environment_id == key.0)
        else {
            continue;
        };
        let fs = environment.environment().get_filesystem();
        let sandbox = environment.sandbox_context(/*additional_permissions*/ None);
        let Ok(metadata) = fs
            .get_metadata(
                &uri,
                codex_exec_server::GetMetadataOptions::default(),
                Some(&sandbox),
            )
            .await
        else {
            continue;
        };
        if metadata.is_directory || metadata.size > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = fs
            .read_file(
                &uri,
                codex_exec_server::ReadFileOptions::default(),
                Some(&sandbox),
            )
            .await
        else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let shown = numbered(
            &text,
            1,
            DEFAULT_READ_LINES,
            approx_bytes(MAX_TOKENS_PER_FILE),
        );
        let tokens = approx_token_count(&shown);
        if used + tokens > budget {
            continue;
        }
        used += tokens;
        out.push((RestoredFile { key, uri, text }, shown));
    }
    out
}

fn approx_bytes(tokens: usize) -> usize {
    TruncationPolicy::Tokens(tokens).byte_budget()
}

/// The text of a context message an earlier compaction put back, when `item` is one.
fn restored_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }
    content.iter().find_map(|part| match part {
        ContentItem::InputText { text }
            if crate::context::internal_context_source(text) == Some(SOURCE) =>
        {
            Some(text.as_str())
        }
        _ => None,
    })
}

/// Whether `item` is context an earlier compaction put back: the next compaction leaves it out of the summary
/// request and puts back what is still in use itself.
pub(crate) fn is_restored_context(item: &ResponseItem) -> bool {
    restored_text(item).is_some()
}

/// The plan and skills part of a restored context message (the files come last and are not searched, so their
/// contents cannot be taken for a plan or a skill).
fn restored_head(text: &str) -> &str {
    text.split(FILES_HEADING).next().unwrap_or(text)
}

fn between<'t>(text: &'t str, open: &str, close: &str) -> Option<&'t str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// The plan to put back: the steps of the last `update_plan` call when any is not completed, or, when an earlier
/// compaction came after that call, the plan it put back.
pub(crate) fn unfinished_plan<'a>(
    items: impl DoubleEndedIterator<Item = &'a ResponseItem>,
) -> Option<String> {
    for item in items.rev() {
        if let ResponseItem::FunctionCall {
            name, arguments, ..
        } = item
            && name == PLAN_TOOL
        {
            return plan_lines(arguments);
        }
        if let Some(text) = restored_text(item) {
            return between(restored_head(text), PLAN_OPEN_TAG, PLAN_CLOSE_TAG)
                .map(|plan| plan.trim().to_string())
                .filter(|plan| !plan.is_empty());
        }
    }
    None
}

fn plan_lines(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let steps = value.get("plan")?.as_array()?;
    let status = |step: &serde_json::Value| {
        step.get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pending")
            .to_string()
    };
    if steps.iter().all(|step| status(step) == "completed") {
        return None;
    }
    let lines: Vec<String> = steps
        .iter()
        .filter_map(|step| {
            Some(format!(
                "- [{}] {}",
                status(step),
                step.get("step")?.as_str()?
            ))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The `<skill>` blocks of a restored context message, in order.
fn restored_skills(text: &str) -> Vec<&str> {
    let mut head = restored_head(text);
    let mut out = Vec::new();
    while let Some(start) = head.find(SKILL_OPEN_TAG) {
        let rest = &head[start..];
        let Some(end) = rest.find(SKILL_CLOSE_TAG) else {
            break;
        };
        let end = end + SKILL_CLOSE_TAG.len();
        out.push(&rest[..end]);
        head = &rest[end..];
    }
    out
}

/// The instructions of the skills used in the conversation (including those an earlier compaction put back),
/// latest first, each cut to its limit, within the budget.
pub(crate) fn used_skills<'a>(
    items: impl DoubleEndedIterator<Item = &'a ResponseItem>,
    budget: usize,
) -> Vec<String> {
    let mut used = 0;
    let mut out: Vec<String> = Vec::new();
    for item in items.rev() {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "user" {
            continue;
        }
        let candidates: Vec<&str> = match restored_text(item) {
            Some(text) => restored_skills(text),
            None => content
                .iter()
                .filter_map(|part| match part {
                    ContentItem::InputText { text }
                        if text.trim_start().starts_with(SKILL_OPEN_TAG) =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect(),
        };
        for text in candidates {
            let cut = truncate_text(text, TruncationPolicy::Tokens(MAX_TOKENS_PER_SKILL));
            if out.contains(&cut) {
                continue;
            }
            let tokens = approx_token_count(&cut);
            if used + tokens > budget {
                return out;
            }
            used += tokens;
            out.push(cut);
        }
    }
    out
}

fn render(
    files: &[(RestoredFile, String)],
    plan: Option<&str>,
    skills: &[String],
) -> Option<String> {
    if files.is_empty() && plan.is_none() && skills.is_empty() {
        return None;
    }
    let mut body = String::from(
        "The conversation was just compacted. To carry on without reading them again, here is what was in use \
         before it, restored as it is now.\n",
    );
    if let Some(plan) = plan {
        body.push_str(&format!(
            "\nYour plan (last update_plan; keep it up to date with update_plan):\n{PLAN_OPEN_TAG}\n{plan}\n{PLAN_CLOSE_TAG}\n"
        ));
    }
    if !skills.is_empty() {
        body.push_str("\nSkills used in this conversation (keep following them):\n");
        for skill in skills {
            body.push('\n');
            body.push_str(skill);
            body.push('\n');
        }
    }
    if !files.is_empty() {
        body.push_str(FILES_HEADING);
        body.push_str(
            " (current contents, numbered like Read; a cut file says where to continue):\n",
        );
        for (file, shown) in files {
            body.push_str(&format!(
                "\n<file path=\"{}\">\n{shown}</file>\n",
                file.key.1
            ));
        }
    }
    Some(body)
}

#[cfg(test)]
#[path = "compact_restore_tests.rs"]
mod tests;
