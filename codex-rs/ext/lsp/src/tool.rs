//! The model-callable `lsp` tool.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema;
use codex_extension_items::ExtensionItem;
use codex_extension_items::lsp::LspItem;
use codex_extension_items::lsp::LspItemStatus;
use codex_lsp::EditorPosition;
use codex_lsp::Operation;
use codex_lsp::QueryOutcome;
use codex_lsp::config::file_extension;
use codex_lsp::format::format_result;
use codex_lsp::format::result_uris;
use codex_lsp::format::without_uris;
use codex_lsp::gitignore::ignored_uris;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use serde::Deserialize;
use serde_json::json;

use crate::LspThread;

pub(crate) const TOOL_NAME: &str = "lsp";
/// Longest answer given to the model, in bytes.
pub(crate) const MAX_RESULT_BYTES: usize = 100_000;
/// Longest answer kept on the display item, in bytes.
pub(crate) const MAX_ITEM_RESULT_BYTES: usize = 4_000;
const TRUNCATION_MARKER: &str = "\n…[truncated]";

const DESCRIPTION: &str = "\
Ask a language server about code: where a symbol is defined or implemented, every place it is referenced, its type \
and documentation (hover), the symbols of one file or of the whole workspace, and call hierarchies (who calls a \
function, and what it calls). Answers come from the compiler's view of the code, so they are exact where text search \
only guesses. Give the file and a position on the symbol: `line` and `character` are 1-based, as shown in editors. \
`documentSymbol` and `workspaceSymbol` ignore the position (pass 1 and 1). Results in git-ignored files are left out. \
The first request to a server can be slow while it indexes the project.";

pub(crate) struct LspTool {
    pub(crate) thread: Arc<LspThread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Args {
    operation: Operation,
    file_path: String,
    line: i64,
    character: i64,
}

impl LspTool {
    fn description(&self) -> String {
        let table = &self.thread.ctx.table;
        let mut text = format!(
            "{DESCRIPTION}\n\nLanguage servers: {}.",
            table.describe().join("; ")
        );
        if !table.skipped().is_empty() {
            text.push_str(&format!(" Not installed: {}.", table.skipped().join(", ")));
        }
        text
    }

    fn parameters() -> serde_json::Value {
        let operations: Vec<&str> = Operation::ALL.iter().map(|op| op.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": operations, "description": "What to ask the language server."},
                "filePath": {"type": "string", "description": "The file, absolute or relative to the working directory."},
                "line": {"type": "integer", "description": "1-based line of the symbol."},
                "character": {"type": "integer", "description": "1-based character offset of the symbol within the line."},
            },
            "required": ["operation", "filePath", "line", "character"],
            "additionalProperties": false,
        })
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for LspTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let parameters = match parse_tool_input_schema(&Self::parameters()) {
            Ok(parameters) => parameters,
            Err(err) => panic!("lsp tool schema should parse: {err}"),
        };
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: self.description(),
            strict: false,
            parameters,
            output_schema: None,
            defer_loading: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

/// `text` cut to at most `max_bytes` bytes (on a character boundary), marked when cut.
fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut keep = max_bytes.saturating_sub(TRUNCATION_MARKER.len());
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &text[..keep])
}

/// The file the model named, as an absolute path.
fn resolve_path(file_path: &str, base: &Path) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

struct Answer {
    text: String,
    ok: bool,
    server: Option<String>,
    result_count: Option<usize>,
    file_count: Option<usize>,
}

impl Answer {
    fn failed(text: String) -> Self {
        Self {
            text,
            ok: false,
            server: None,
            result_count: None,
            file_count: None,
        }
    }
}

impl LspTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: Args = serde_json::from_str(call.function_arguments()?)
            .map_err(|err| FunctionCallError::RespondToModel(format!("Invalid input: {err}")))?;
        let (Ok(line), Ok(character)) = (u32::try_from(args.line), u32::try_from(args.character))
        else {
            return Err(FunctionCallError::RespondToModel(
                "Invalid input: line and character must be positive 1-based numbers".to_string(),
            ));
        };
        let Some(position) = EditorPosition::new(line, character) else {
            return Err(FunctionCallError::RespondToModel(
                "Invalid input: line and character are 1-based (the first line is 1)".to_string(),
            ));
        };
        let base = call
            .environments
            .first()
            .map(|environment| environment.cwd.to_path_buf())
            .filter(|cwd| cwd.is_absolute())
            .unwrap_or_else(|| self.thread.ctx.cwd.clone());
        let path = resolve_path(&args.file_path, &base);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "Path is not a file: {}",
                    args.file_path
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "File does not exist: {}",
                    args.file_path
                )));
            }
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "Cannot access file: {}. {err}",
                    args.file_path
                )));
            }
        }

        let mut item = LspItem {
            id: call.call_id.clone(),
            operation: args.operation.as_str().to_string(),
            file_path: args.file_path.clone(),
            line,
            character,
            status: LspItemStatus::InProgress,
            server: None,
            result_count: None,
            file_count: None,
            result: None,
        };
        call.turn_item_emitter
            .emit_started(turn_item(item.clone()))
            .await;

        let answer = self.answer(&path, args.operation, position, &base).await;

        item.status = if answer.ok {
            LspItemStatus::Completed
        } else {
            LspItemStatus::Failed
        };
        item.server = answer.server.clone();
        item.result_count = answer.result_count.map(saturating_u32);
        item.file_count = answer.file_count.map(saturating_u32);
        item.result = Some(truncate(&answer.text, MAX_ITEM_RESULT_BYTES));
        call.turn_item_emitter.emit_completed(turn_item(item)).await;

        let budget = call.response_byte_budget(MAX_RESULT_BYTES);
        Ok(Box::new(LspOutput::new(
            truncate(&answer.text, budget),
            answer.ok,
        )))
    }

    async fn answer(
        &self,
        path: &Path,
        operation: Operation,
        position: EditorPosition,
        cwd: &Path,
    ) -> Answer {
        let thread = &self.thread;
        let outcome = thread
            .manager
            .query(&thread.ctx, path, operation, position)
            .await;
        match outcome {
            Ok(QueryOutcome::NoServer) => {
                let ext = file_extension(path).unwrap_or_else(|| "(none)".to_string());
                Answer::failed(format!(
                    "No language server is configured for files of type {ext}. Configured: {}.",
                    thread.ctx.table.describe().join("; ")
                ))
            }
            Ok(QueryOutcome::TooLarge { bytes }) => Answer::failed(format!(
                "File too large for the language server ({} MB; the limit is {} MB)",
                bytes.div_ceil(1_000_000),
                thread.ctx.limits.max_file_bytes / 1_000_000
            )),
            Ok(QueryOutcome::Result { server, value }) => {
                let value = if operation.filters_ignored_locations() {
                    let ignored = ignored_uris(&result_uris(operation, &value), cwd).await;
                    without_uris(operation, value, &ignored)
                } else {
                    value
                };
                let formatted = format_result(operation, &value, Some(cwd));
                Answer {
                    text: formatted.text,
                    ok: true,
                    server: Some(server),
                    result_count: Some(formatted.result_count),
                    file_count: Some(formatted.file_count),
                }
            }
            Err(err) => Answer::failed(format!("Error performing {}: {err}", operation.as_str())),
        }
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn turn_item(item: LspItem) -> ExtensionTurnItem {
    ExtensionTurnItem {
        item: ExtensionItem::Lsp(item),
        legacy_events: Vec::new(),
    }
}

struct LspOutput {
    payload: FunctionCallOutputPayload,
}

impl LspOutput {
    fn new(text: String, ok: bool) -> Self {
        let payload = if ok {
            FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText { text },
            ])
        } else {
            FunctionCallOutputPayload {
                success: Some(false),
                ..FunctionCallOutputPayload::from_text(text)
            }
        };
        Self { payload }
    }
}

impl ToolOutput for LspOutput {
    fn log_output(&self) -> String {
        "[lsp output]".to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.payload.success.unwrap_or(true)
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.payload.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn truncation_keeps_the_limit_and_marks_the_cut() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate(&"x".repeat(50), 20);
        assert_eq!(cut.len(), 20);
        assert!(cut.ends_with(TRUNCATION_MARKER));
        let wide = truncate(&"字".repeat(30), 25);
        assert!(
            wide.len() <= 25 && wide.ends_with(TRUNCATION_MARKER),
            "{wide}"
        );
    }

    #[test]
    fn schema_parses_and_lists_every_operation() {
        let schema = LspTool::parameters();
        assert!(parse_tool_input_schema(&schema).is_ok());
        assert_eq!(
            schema["properties"]["operation"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            Operation::ALL.len()
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let base = std::env::temp_dir();
        assert_eq!(resolve_path("src/a.rs", &base), base.join("src/a.rs"));
        let absolute = base.join("x.rs");
        assert_eq!(resolve_path(absolute.to_str().unwrap(), &base), absolute);
    }
}
