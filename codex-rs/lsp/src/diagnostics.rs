//! Passive diagnostics: what servers publish (`textDocument/publishDiagnostics`) is kept per conversation and handed
//! to the model once, deduplicated and capped.
//!
//! Behaviour follows the reference agent: at most 10 diagnostics per file and 30 per delivery, errors first, a
//! diagnostic already delivered for a file is not delivered again until that file is edited, and the rendered summary
//! is cut at 4000 characters. Unlike the reference, a newer publish for a file replaces the older one (servers always
//! publish the full set for a file), and an empty publish clears what was pending for it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

use lru::LruCache;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;

use crate::uri::display_path;

pub const DEFAULT_MAX_PER_FILE: usize = 10;
pub const DEFAULT_MAX_TOTAL: usize = 30;
pub const DEFAULT_MAX_SUMMARY_CHARS: usize = 4000;
/// Files whose delivered-diagnostic keys are remembered (least recently used are forgotten).
pub const DEFAULT_MAX_DELIVERED_FILES: usize = 500;
/// Files with undelivered diagnostics kept per conversation.
pub const DEFAULT_MAX_PENDING_FILES: usize = 500;
/// Longest diagnostic message kept (servers can attach whole type dumps).
pub const MAX_MESSAGE_CHARS: usize = 2000;

const TRUNCATION_MARKER: &str = "…[truncated]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticLimits {
    pub max_per_file: usize,
    pub max_total: usize,
    pub max_summary_chars: usize,
    pub max_delivered_files: usize,
    pub max_pending_files: usize,
}

impl Default for DiagnosticLimits {
    fn default() -> Self {
        Self {
            max_per_file: DEFAULT_MAX_PER_FILE,
            max_total: DEFAULT_MAX_TOTAL,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
            max_delivered_files: DEFAULT_MAX_DELIVERED_FILES,
            max_pending_files: DEFAULT_MAX_PENDING_FILES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl Severity {
    /// LSP severity number; a missing or unknown one counts as an error (as in the reference agent).
    pub fn from_lsp(value: Option<i64>) -> Self {
        match value {
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Error,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub message: String,
    pub severity: Severity,
    /// Zero-based start line and character, as sent by the server.
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub source: Option<String>,
    pub code: Option<String>,
}

impl Diagnostic {
    /// One entry of `publishDiagnostics.diagnostics`; `None` when it has no message or range.
    pub fn from_lsp(value: &Value) -> Option<Self> {
        let message = value.get("message")?.as_str()?;
        let range = value.get("range")?;
        let position = |key: &str, field: &str| {
            range
                .get(key)
                .and_then(|p| p.get(field))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        let code = match value.get("code") {
            Some(Value::String(code)) => Some(code.clone()),
            Some(Value::Number(code)) => Some(code.to_string()),
            _ => None,
        };
        Some(Self {
            message: truncate_chars(message, MAX_MESSAGE_CHARS),
            severity: Severity::from_lsp(value.get("severity").and_then(Value::as_i64)),
            line: position("start", "line")?,
            character: position("start", "character")?,
            end_line: position("end", "line")?,
            end_character: position("end", "character")?,
            source: value
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string),
            code,
        })
    }

    fn key(&self) -> String {
        // Same identity as the reference agent: message, severity, range, source and code.
        serde_json::to_string(self).unwrap_or_else(|_| self.message.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiagnostics {
    /// The document URI as the server sent it.
    pub uri: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// One batch for the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticDelivery {
    pub servers: Vec<String>,
    pub files: Vec<FileDiagnostics>,
    /// Diagnostics left out by the caps.
    pub truncated: usize,
}

impl DiagnosticDelivery {
    pub fn count(&self) -> usize {
        self.files.iter().map(|file| file.diagnostics.len()).sum()
    }
}

struct PendingFile {
    server: String,
    diagnostics: Vec<Diagnostic>,
}

struct SinkState {
    limits: DiagnosticLimits,
    pending: BTreeMap<String, PendingFile>,
    /// Edited documents not yet answered by a publish, with the time to stop waiting for them.
    awaiting: BTreeMap<String, Instant>,
    delivered: LruCache<String, HashSet<String>>,
}

/// The diagnostics of one conversation.
pub struct DiagnosticSink {
    state: Mutex<SinkState>,
    changed: Notify,
}

impl DiagnosticSink {
    pub fn new(limits: DiagnosticLimits) -> Self {
        let delivered_capacity =
            NonZeroUsize::new(limits.max_delivered_files.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            state: Mutex::new(SinkState {
                limits,
                pending: BTreeMap::new(),
                awaiting: BTreeMap::new(),
                delivered: LruCache::new(delivered_capacity),
            }),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SinkState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A server's full set of diagnostics for `uri`.
    pub fn publish(&self, server: &str, uri: &str, diagnostics: Vec<Diagnostic>) {
        {
            let mut state = self.lock();
            state.awaiting.remove(uri);
            if diagnostics.is_empty() {
                state.pending.remove(uri);
            } else if state.pending.contains_key(uri)
                || state.pending.len() < state.limits.max_pending_files
            {
                state.pending.insert(
                    uri.to_string(),
                    PendingFile {
                        server: server.to_string(),
                        diagnostics,
                    },
                );
            } else {
                tracing::debug!("lsp: dropping diagnostics for {uri}: pending file limit reached");
            }
        }
        self.changed.notify_waiters();
    }

    /// `uri` was edited: what was delivered for it may be delivered again, and the next delivery waits (up to
    /// `wait`) for the server's answer.
    pub fn file_edited(&self, uri: &str, wait: Duration) {
        let mut state = self.lock();
        state.delivered.pop(uri);
        state
            .awaiting
            .insert(uri.to_string(), Instant::now() + wait);
    }

    /// Stop waiting for `uri` (its server could not be reached).
    pub fn stop_waiting(&self, uri: &str) {
        self.lock().awaiting.remove(uri);
        self.changed.notify_waiters();
    }

    /// Waits until every edited document has been answered or its deadline has passed.
    pub async fn wait_for_edits(&self) {
        loop {
            let notified = self.changed.notified();
            let next_deadline = {
                let mut state = self.lock();
                let now = Instant::now();
                state.awaiting.retain(|_, deadline| *deadline > now);
                state.awaiting.values().min().copied()
            };
            let Some(deadline) = next_deadline else {
                return;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    pub fn pending_files(&self) -> usize {
        self.lock().pending.len()
    }

    /// Takes everything pending: deduplicated against itself and earlier deliveries, sorted by severity, capped.
    pub fn take_delivery(&self) -> Option<DiagnosticDelivery> {
        let mut state = self.lock();
        if state.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut state.pending);
        let limits = state.limits.clone();
        let mut servers = BTreeSet::new();
        let mut files = Vec::new();
        let mut truncated = 0usize;
        let mut total = 0usize;
        for (uri, file) in pending {
            let previously = state.delivered.get(&uri).cloned().unwrap_or_default();
            let mut seen = HashSet::new();
            let mut diagnostics: Vec<Diagnostic> = file
                .diagnostics
                .into_iter()
                .filter(|diagnostic| {
                    let key = diagnostic.key();
                    !previously.contains(&key) && seen.insert(key)
                })
                .collect();
            diagnostics.sort_by_key(|diagnostic| diagnostic.severity);
            if diagnostics.len() > limits.max_per_file {
                truncated += diagnostics.len() - limits.max_per_file;
                diagnostics.truncate(limits.max_per_file);
            }
            let room = limits.max_total.saturating_sub(total);
            if diagnostics.len() > room {
                truncated += diagnostics.len() - room;
                diagnostics.truncate(room);
            }
            if diagnostics.is_empty() {
                continue;
            }
            total += diagnostics.len();
            servers.insert(file.server);
            files.push(FileDiagnostics { uri, diagnostics });
        }
        for file in &files {
            let keys = file.diagnostics.iter().map(Diagnostic::key);
            match state.delivered.get_mut(&file.uri) {
                Some(delivered) => delivered.extend(keys),
                None => {
                    state.delivered.put(file.uri.clone(), keys.collect());
                }
            }
        }
        if files.is_empty() {
            return None;
        }
        Some(DiagnosticDelivery {
            servers: servers.into_iter().collect(),
            files,
            truncated,
        })
    }
}

/// The text the model sees, cut at `max_chars`.
pub fn format_delivery(
    delivery: &DiagnosticDelivery,
    cwd: Option<&std::path::Path>,
    max_chars: usize,
) -> String {
    let mut text = delivery
        .files
        .iter()
        .map(|file| {
            let lines: Vec<String> = file
                .diagnostics
                .iter()
                .map(|d| {
                    let code = d
                        .code
                        .as_deref()
                        .map(|c| format!(" [{c}]"))
                        .unwrap_or_default();
                    let source = d
                        .source
                        .as_deref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default();
                    format!(
                        "  {} {}:{} {}{code}{source}",
                        d.severity.label(),
                        d.line.saturating_add(1),
                        d.character.saturating_add(1),
                        d.message
                    )
                })
                .collect();
            format!("{}:\n{}", display_path(&file.uri, cwd), lines.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if delivery.truncated > 0 {
        text.push_str(&format!(
            "\n\n({} more diagnostics not shown)",
            delivery.truncated
        ));
    }
    if text.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(TRUNCATION_MARKER.chars().count());
        text = text.chars().take(keep).collect::<String>() + TRUNCATION_MARKER;
    }
    text
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + TRUNCATION_MARKER
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn diag(message: &str, severity: i64, line: u32) -> Diagnostic {
        Diagnostic::from_lsp(&json!({
            "message": message,
            "severity": severity,
            "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 1}},
        }))
        .unwrap()
    }

    #[test]
    fn parses_lsp_diagnostics_leniently() {
        let parsed = Diagnostic::from_lsp(&json!({
            "message": "x", "code": 42, "source": "rustc",
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 3}},
        }))
        .unwrap();
        assert_eq!(parsed.severity, Severity::Error);
        assert_eq!(parsed.code.as_deref(), Some("42"));
        assert_eq!(Diagnostic::from_lsp(&json!({"message": "no range"})), None);
    }

    #[test]
    fn caps_per_file_and_total_and_sorts_errors_first() {
        let sink = DiagnosticSink::new(DiagnosticLimits::default());
        for file in 0..4 {
            let mut list: Vec<Diagnostic> =
                (0..12).map(|i| diag(&format!("hint {i}"), 4, i)).collect();
            list.push(diag("the error", 1, 99));
            sink.publish("srv", &format!("file:///w/f{file}.rs"), list);
        }
        let delivery = sink.take_delivery().unwrap();
        assert_eq!(delivery.count(), DEFAULT_MAX_TOTAL);
        assert!(
            delivery
                .files
                .iter()
                .all(|f| f.diagnostics.len() <= DEFAULT_MAX_PER_FILE)
        );
        assert!(
            delivery
                .files
                .iter()
                .all(|f| f.diagnostics[0].message == "the error")
        );
        // 4 files × 13 = 52 published, 30 delivered.
        assert_eq!(delivery.truncated, 52 - DEFAULT_MAX_TOTAL);
        assert_eq!(delivery.files.len(), 3);
        assert!(sink.take_delivery().is_none());
    }

    #[test]
    fn delivered_once_until_the_file_is_edited_and_empty_publish_clears() {
        let sink = DiagnosticSink::new(DiagnosticLimits::default());
        let uri = "file:///w/a.rs";
        sink.publish("srv", uri, vec![diag("e", 1, 0), diag("e", 1, 0)]);
        assert_eq!(sink.take_delivery().unwrap().count(), 1);
        sink.publish("srv", uri, vec![diag("e", 1, 0)]);
        assert_eq!(sink.take_delivery(), None);
        sink.file_edited(uri, Duration::ZERO);
        sink.publish("srv", uri, vec![diag("e", 1, 0)]);
        assert_eq!(sink.take_delivery().unwrap().count(), 1);
        sink.publish("srv", uri, vec![diag("other", 2, 3)]);
        sink.publish("srv", uri, Vec::new());
        assert_eq!(sink.take_delivery(), None);
    }

    #[test]
    fn pending_files_are_bounded() {
        let limits = DiagnosticLimits {
            max_pending_files: 2,
            ..DiagnosticLimits::default()
        };
        let sink = DiagnosticSink::new(limits);
        for i in 0..5 {
            sink.publish("srv", &format!("file:///w/{i}.rs"), vec![diag("e", 1, 0)]);
        }
        assert_eq!(sink.pending_files(), 2);
    }

    #[test]
    fn formats_one_based_positions_and_cuts_long_summaries() {
        let delivery = DiagnosticDelivery {
            servers: vec!["srv".into()],
            files: vec![FileDiagnostics {
                uri: "file:///w/src/a.rs".into(),
                diagnostics: vec![Diagnostic {
                    code: Some("E0308".into()),
                    source: Some("rustc".into()),
                    ..diag("mismatched types", 1, 4)
                }],
            }],
            truncated: 2,
        };
        let text = format_delivery(
            &delivery,
            Some(std::path::Path::new("/w")),
            DEFAULT_MAX_SUMMARY_CHARS,
        );
        assert_eq!(
            text,
            "src/a.rs:\n  error 5:1 mismatched types [E0308] (rustc)\n\n(2 more diagnostics not shown)"
        );
        let cut = format_delivery(&delivery, None, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with(TRUNCATION_MARKER));
    }

    #[tokio::test]
    async fn waits_for_an_edited_file_until_published_or_deadline() {
        let sink = std::sync::Arc::new(DiagnosticSink::new(DiagnosticLimits::default()));
        let uri = "file:///w/a.rs";
        sink.file_edited(uri, Duration::from_secs(30));
        let publisher = {
            let sink = sink.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                sink.publish("srv", uri, vec![diag("e", 1, 0)]);
            })
        };
        let started = Instant::now();
        sink.wait_for_edits().await;
        assert!(started.elapsed() < Duration::from_secs(5));
        publisher.await.unwrap();

        sink.file_edited("file:///w/b.rs", Duration::from_millis(30));
        let started = Instant::now();
        sink.wait_for_edits().await;
        assert!(started.elapsed() >= Duration::from_millis(25));
    }
}
