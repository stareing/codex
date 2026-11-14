//! New diagnostics as a per-step context section.
//!
//! Each sampling step first waits (bounded by `lsp.diagnostics_wait_ms`) for diagnostics of files edited since the
//! last step, then hands over whatever is new, once. The section is never replayed when the context is rebuilt
//! (compaction): old diagnostics may no longer be true.

use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use codex_lsp::diagnostics::format_delivery;
use serde_json::Value;

use crate::LspThread;

pub(crate) const WORLD_STATE_ID: &str = "lsp_diagnostics";
pub(crate) const START_MARKER: &str = "<new-diagnostics>";
pub(crate) const END_MARKER: &str = "</new-diagnostics>";
const INTRO: &str = "Language servers reported these new problems (line:character, 1-based):";

/// The body the model sees for one delivery.
pub(crate) fn diagnostics_text(thread: &LspThread) -> Option<String> {
    let delivery = thread.ctx.sink.take_delivery()?;
    let summary = format_delivery(
        &delivery,
        Some(&thread.ctx.cwd),
        thread.ctx.limits.diagnostics.max_summary_chars,
    );
    Some(format!("{INTRO}\n\n{summary}"))
}

pub(crate) async fn diagnostics_section(thread: &LspThread) -> WorldStateSectionContribution {
    thread.ctx.sink.wait_for_edits().await;
    let text = diagnostics_text(thread);
    WorldStateSectionContribution::new(WORLD_STATE_ID, move |_previous| match &text {
        // The snapshot only records that something was delivered; the text itself is the update.
        Some(text) => (
            Some(Value::Bool(true)),
            Some(RenderedWorldStateFragment::new(
                "developer",
                (START_MARKER, END_MARKER),
                input_text(text),
            )),
        ),
        None => (None, None),
    })
    .without_full_context()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_extension_api::PreviousWorldStateSection;
    use codex_lsp::DiagnosticSink;
    use codex_lsp::LspContext;
    use codex_lsp::LspLimits;
    use codex_lsp::LspManager;
    use codex_lsp::ServerTable;
    use codex_lsp::diagnostics::Diagnostic;
    use std::sync::Arc;

    fn thread() -> LspThread {
        let limits = LspLimits::default();
        LspThread {
            enabled: true,
            ctx: LspContext {
                table: Arc::new(ServerTable::default()),
                sink: Arc::new(DiagnosticSink::new(limits.diagnostics.clone())),
                limits,
                cwd: std::env::temp_dir(),
            },
            manager: LspManager::new(),
            plugin_servers: Arc::default(),
        }
    }

    #[tokio::test]
    async fn new_diagnostics_become_one_update_and_then_nothing() {
        let thread = thread();
        let diagnostic = Diagnostic::from_lsp(&serde_json::json!({
            "message": "unused variable", "severity": 2,
            "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}},
        }))
        .unwrap();
        thread
            .ctx
            .sink
            .publish("srv", "file:///w/a.rs", vec![diagnostic]);

        let section = diagnostics_section(&thread).await;
        assert!(!section.include_in_full_context());
        let (snapshot, fragment) = section.render_diff(PreviousWorldStateSection::Absent);
        assert_eq!(snapshot, Some(Value::Bool(true)));
        let fragment = fragment.expect("an update");
        assert_eq!(fragment.markers(), (START_MARKER, END_MARKER));
        let codex_extension_api::ContentItem::InputText { text } = fragment.body() else {
            panic!("text body");
        };
        assert!(text.starts_with(INTRO));
        assert!(text.contains("warning 1:5 unused variable"), "{text}");

        let quiet = diagnostics_section(&thread).await;
        assert_eq!(
            quiet.render_diff(PreviousWorldStateSection::Absent),
            (None, None)
        );
    }
}
