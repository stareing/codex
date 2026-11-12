//! `Read`, `Edit` and `Write`: plain JSON function tools for reading and changing files, for models that do not
//! get `apply_patch` (`model_info.apply_patch_tool_type` is `None`, e.g. third-party models behind an
//! OpenAI-compatible gateway, which rarely write the patch grammar correctly).
//!
//! The model never writes a patch: `Edit` (replace an exact string) and `Write` (create or overwrite a file) are
//! turned here into a whole-file `apply_patch` change that this code generates, then verified and applied by the
//! same path as `apply_patch` (`execute_verified_patch`): approvals, sandbox write permissions, turn diff tracking
//! and the patch events the clients show stay exactly as they are.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_file_system::FileSystemSandboxContext;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::apply_patch::apply_patch_file_update_mode;
use crate::tools::handlers::apply_patch::execute_verified_patch;
use crate::tools::handlers::file_access::Access;
use crate::tools::handlers::file_access::ensure_access;
use crate::tools::handlers::file_reads::FileKey;
use crate::tools::handlers::file_reads::FileReadLedger;
use crate::tools::handlers::file_reads::ReadOutcome;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::shown_tool::shown;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::sandboxing::ToolCtx;

pub const READ_TOOL_NAME: &str = "Read";
pub const EDIT_TOOL_NAME: &str = "Edit";
pub const WRITE_TOOL_NAME: &str = "Write";

/// Lines `Read` returns when no `limit` is given.
pub(crate) const DEFAULT_READ_LINES: usize = 2000;
/// Longer lines are cut in `Read`'s output.
const MAX_LINE_CHARS: usize = 2000;
/// Files larger than this are refused by all three tools (read in parts with the shell instead).
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

const ENVIRONMENT_ID_DESCRIPTION: &str =
    "Environment id from <environment_context>. Omit to use the primary environment.";

pub(crate) fn properties(
    fields: Vec<(&str, JsonSchema)>,
    include_environment_id: bool,
) -> BTreeMap<String, JsonSchema> {
    let mut map: BTreeMap<String, JsonSchema> = fields
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect();
    if include_environment_id {
        map.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(ENVIRONMENT_ID_DESCRIPTION.to_string())),
        );
    }
    map
}

pub(crate) fn function_tool(
    name: &str,
    description: &str,
    properties: BTreeMap<String, JsonSchema>,
    required: &[&str],
) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(required.iter().map(|r| r.to_string()).collect()),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn text(description: &str) -> JsonSchema {
    JsonSchema::string(Some(description.to_string()))
}

pub(crate) fn function_arguments(
    payload: ToolPayload,
    tool: &str,
) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(format!(
            "{tool} handler received unsupported payload"
        ))),
    }
}

pub(crate) fn refuse(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

/// The environment a file or search tool call runs in (`environment_id`, or the primary one).
pub(crate) fn turn_environment(
    invocation: &ToolInvocation,
    tool: &str,
    environment_id: Option<&str>,
    multi_environment: bool,
) -> Result<TurnEnvironment, FunctionCallError> {
    if environment_id.is_some() && !multi_environment {
        return Err(refuse(format!(
            "{tool}: environment selection is unavailable for this turn"
        )));
    }
    resolve_tool_environment(&invocation.step_context.environments, environment_id)?
        .cloned()
        .ok_or_else(|| refuse(format!("{tool} is unavailable in this session")))
}

/// One file the tools work on, resolved against the selected environment.
struct Target {
    environment: TurnEnvironment,
    path: PathUri,
    shown: String,
}

impl Target {
    fn resolve(
        invocation: &ToolInvocation,
        tool: &str,
        file_path: &str,
        environment_id: Option<&str>,
        multi_environment: bool,
    ) -> Result<Self, FunctionCallError> {
        if file_path.trim().is_empty() {
            return Err(refuse(format!("{tool}: file_path is empty")));
        }
        if file_path.contains(['\n', '\r', '\0']) {
            return Err(refuse(format!("{tool}: file_path must be a single line")));
        }
        let environment = turn_environment(invocation, tool, environment_id, multi_environment)?;
        let path = environment.cwd().join(file_path).map_err(|err| {
            refuse(format!(
                "{tool}: unable to resolve `{file_path}` against `{}`: {err}",
                environment.cwd()
            ))
        })?;
        let shown = path.inferred_native_path_string();
        Ok(Self {
            environment,
            path,
            shown,
        })
    }

    /// The permission check of `file_access.rs` for this file.
    async fn ensure(
        &self,
        invocation: &ToolInvocation,
        tool: &str,
        raw: &str,
        access: Access,
    ) -> Result<(), FunctionCallError> {
        ensure_access(
            invocation,
            &self.environment,
            tool,
            raw,
            &self.path.to_path_buf(),
            access,
        )
        .await
    }

    fn key(&self) -> FileKey {
        (
            self.environment.selection.environment_id.clone(),
            self.shown.clone(),
        )
    }

    fn sandbox(&self) -> FileSystemSandboxContext {
        self.environment
            .sandbox_context(/*additional_permissions*/ None)
    }

    /// The file's text; `Ok(None)` when there is no file at the path.
    async fn read_text(
        &self,
        fs: &dyn ExecutorFileSystem,
    ) -> Result<Option<String>, FunctionCallError> {
        let sandbox = self.sandbox();
        let metadata = match fs
            .get_metadata(&self.path, GetMetadataOptions::default(), Some(&sandbox))
            .await
        {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(refuse(format!("unable to access `{}`: {err}", self.shown))),
        };
        if metadata.is_directory {
            return Err(refuse(format!(
                "`{}` is a directory, not a file",
                self.shown
            )));
        }
        if metadata.size > MAX_FILE_BYTES {
            return Err(refuse(format!(
                "`{}` is {} bytes, larger than the {MAX_FILE_BYTES}-byte limit; use the shell to work on it in parts",
                self.shown, metadata.size
            )));
        }
        let bytes = fs
            .read_file(&self.path, ReadFileOptions::default(), Some(&sandbox))
            .await
            .map_err(|err| refuse(format!("unable to read `{}`: {err}", self.shown)))?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| refuse(format!("`{}` is not UTF-8 text", self.shown)))
    }
}

fn lock_ledger(
    session: &crate::session::session::Session,
) -> std::sync::MutexGuard<'_, FileReadLedger> {
    session
        .file_reads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ledger(invocation: &ToolInvocation) -> std::sync::MutexGuard<'_, FileReadLedger> {
    lock_ledger(&invocation.session)
}

/// Lines of `text` as the patch grammar sees them (no trailing empty line for a final newline).
fn patch_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// A patch replacing the whole of `old` by `new` in `path` (one chunk: it can only match at the file's start, so
/// it is never applied to the wrong place); `old = None` adds the file.
pub(crate) fn whole_file_patch(path: &str, old: Option<&str>, new: &str) -> String {
    let mut patch = String::from("*** Begin Patch\n");
    match old {
        None => {
            patch.push_str(&format!("*** Add File: {path}\n"));
            for line in patch_lines(new) {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
        }
        Some(old) => {
            patch.push_str(&format!("*** Update File: {path}\n@@\n"));
            for line in patch_lines(old) {
                patch.push('-');
                patch.push_str(line);
                patch.push('\n');
            }
            for line in patch_lines(new) {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
        }
    }
    patch.push_str("*** End Patch");
    patch
}

/// `text` with `old` replaced by `new` (all of them or exactly one), trying the file's own `\r\n` endings when the
/// model wrote `\n`.
pub(crate) fn replaced(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string is empty; use Write to create or overwrite a file".to_string());
    }
    if old == new {
        return Err("old_string and new_string are the same; nothing to change".to_string());
    }
    let crlf = text.contains("\r\n") && !old.contains("\r\n");
    let (old, new) = if crlf && !text.contains(old) {
        (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
    } else {
        (old.to_string(), new.to_string())
    };
    if !text.contains(old.as_str())
        && let Some(result) = replaced_ignoring_quote_style(text, &old, &new, replace_all)
    {
        return result;
    }
    match text.matches(old.as_str()).count() {
        0 => Err("old_string was not found in the file; Read it again and copy the text exactly (including whitespace and indentation)".to_string()),
        1 => Ok(text.replacen(old.as_str(), &new, 1)),
        _ if replace_all => Ok(text.replace(old.as_str(), &new)),
        n => Err(format!(
            "old_string appears {n} times; include more surrounding lines to make it unique, or set replace_all to true"
        )),
    }
}

/// Typographic quotes as their ASCII counterparts (one char for one char, so char positions line up).
fn fold_quote(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        c => c,
    }
}

/// The replacement when `old` only differs from the file in quote style (the model wrote `'`/`"` where the file has
/// curly quotes, or the other way round); `None` when that does not find it either. `new` takes the curly quotes
/// the matched text used.
fn replaced_ignoring_quote_style(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Option<Result<String, String>> {
    let folded_old: String = old.chars().map(fold_quote).collect();
    if folded_old == old && !text.chars().any(|c| fold_quote(c) != c) {
        return None;
    }
    // Byte offsets of each char boundary in the folded text and in `text`.
    let mut folded = String::with_capacity(text.len());
    let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(text.len() + 1);
    for (at, c) in text.char_indices() {
        boundaries.push((folded.len(), at));
        folded.push(fold_quote(c));
    }
    boundaries.push((folded.len(), text.len()));
    let original_at = |folded_at: usize| {
        boundaries
            .binary_search_by_key(&folded_at, |(f, _)| *f)
            .ok()
            .map(|i| boundaries[i].1)
    };
    let ranges: Vec<(usize, usize)> = folded
        .match_indices(folded_old.as_str())
        .filter_map(|(at, m)| Some((original_at(at)?, original_at(at + m.len())?)))
        .collect();
    match ranges.len() {
        0 => return None,
        1 => {}
        _ if replace_all => {}
        n => {
            return Some(Err(format!(
                "old_string appears {n} times; include more surrounding lines to make it unique, or set replace_all to true"
            )));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end) in ranges {
        out.push_str(&text[last..start]);
        out.push_str(&with_quote_style_of(&text[start..end], new));
        last = end;
    }
    out.push_str(&text[last..]);
    Some(Ok(out))
}

/// `new` with its ASCII quotes made curly where `matched` (the file's text it replaces) uses curly ones: opening
/// after a space or bracket or at the start, closing otherwise (an apostrophe is a closing single quote).
fn with_quote_style_of(matched: &str, new: &str) -> String {
    let curly_single = matched.contains(['\u{2018}', '\u{2019}']);
    let curly_double = matched.contains(['\u{201C}', '\u{201D}']);
    if !curly_single && !curly_double {
        return new.to_string();
    }
    let mut out = String::with_capacity(new.len());
    let mut previous: Option<char> = None;
    for c in new.chars() {
        let opens =
            previous.is_none_or(|p| p.is_whitespace() || "([{<\u{2014}\u{2013}".contains(p));
        let styled = match c {
            '\'' if curly_single => {
                if opens {
                    '\u{2018}'
                } else {
                    '\u{2019}'
                }
            }
            '"' if curly_double => {
                if opens {
                    '\u{201C}'
                } else {
                    '\u{201D}'
                }
            }
            c => c,
        };
        out.push(styled);
        previous = Some(c);
    }
    out
}

/// `Read`'s output: `cat -n` style numbered lines from `offset` (1-based), at most `max_bytes` of them (whole lines,
/// at least one): the history would otherwise cut the middle out of a long read and the model would never know
/// which lines it lost.
pub(crate) fn numbered(text: &str, offset: usize, limit: usize, max_bytes: usize) -> String {
    let lines = patch_lines(text);
    if lines.is_empty() {
        return "(the file is empty)".to_string();
    }
    let start = offset.max(1);
    if start > lines.len() {
        return format!(
            "(the file has {} lines; offset {start} is past the end)",
            lines.len()
        );
    }
    let mut out = String::new();
    let mut end = (start - 1 + limit).min(lines.len());
    for (index, line) in lines[start - 1..end].iter().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
        let mut row = format!("{:>6}\t{shown}", start + index);
        if shown.len() < line.len() {
            row.push_str(" … (line cut)");
        }
        row.push('\n');
        if index > 0 && out.len() + row.len() > max_bytes {
            end = start - 1 + index;
            out.push_str(&format!(
                "(output size limit: showing lines {start}-{end} of {}; pass offset {} to read on, or Grep for what you need)\n",
                lines.len(),
                end + 1
            ));
            return out;
        }
        out.push_str(&row);
    }
    if end < lines.len() {
        out.push_str(&format!(
            "(showing lines {start}-{end} of {}; pass offset {} to read on)\n",
            lines.len(),
            end + 1
        ));
    }
    out
}

/// Verifies `patch` against the environment and applies it through `apply_patch`'s own path.
async fn apply_generated_patch(
    invocation: ToolInvocation,
    target: Target,
    patch: String,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        step_context,
        cancellation_token,
        tracker,
        call_id,
        tool_name,
        ..
    } = invocation;
    let args = codex_apply_patch::parse_patch(&patch).map_err(|err| {
        refuse(format!(
            "internal patch for `{}` is invalid: {err}",
            target.shown
        ))
    })?;
    let fs = target.environment.environment.get_filesystem();
    let sandbox = target.sandbox();
    let action = match codex_apply_patch::verify_apply_patch_args_with_mode(
        args,
        target.environment.cwd(),
        apply_patch_file_update_mode(&turn),
        fs.as_ref(),
        Some(&sandbox),
    )
    .await
    {
        codex_apply_patch::MaybeApplyPatchVerified::Body(action) => action,
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(err) => {
            return Err(refuse(format!(
                "could not change `{}`: {err}",
                target.shown
            )));
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(_)
        | codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
            return Err(refuse(format!("could not change `{}`", target.shown)));
        }
    };
    let tool_ctx = ToolCtx {
        session,
        step_context: Arc::clone(&step_context),
        cancellation_token,
        call_id,
        tool_name,
    };
    let content =
        execute_verified_patch(action, target.environment, Some(&tracker), tool_ctx).await?;
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        content,
        Some(true),
    )))
}

#[derive(Deserialize)]
struct ReadArgs {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Deserialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Deserialize)]
struct WriteArgs {
    file_path: String,
    content: String,
    #[serde(default)]
    environment_id: Option<String>,
}

pub struct ReadFileHandler {
    multi_environment: bool,
}

pub struct EditFileHandler {
    multi_environment: bool,
}

pub struct WriteFileHandler {
    multi_environment: bool,
}

impl ReadFileHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

impl EditFileHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

impl WriteFileHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        function_tool(
            READ_TOOL_NAME,
            "Reads a text file. Returns its lines numbered like `cat -n` (number, tab, text), up to 2000 lines \
             from `offset`; pass `offset`/`limit` to read long files in parts. Read a file before you Edit it, and \
             copy old_string from this output without the line-number prefix.",
            properties(
                vec![
                    (
                        "file_path",
                        text("Path of the file (absolute, or relative to the working directory)."),
                    ),
                    (
                        "offset",
                        JsonSchema::integer(Some(
                            "1-based line to start at. Defaults to 1.".to_string(),
                        )),
                    ),
                    (
                        "limit",
                        JsonSchema::integer(Some(
                            "How many lines to return. Defaults to 2000.".to_string(),
                        )),
                    ),
                ],
                self.multi_environment,
            ),
            &["file_path"],
        )
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let work = async {
                let arguments = function_arguments(invocation.payload.clone(), READ_TOOL_NAME)?;
                let args: ReadArgs = parse_arguments(&arguments)?;
                let target = Target::resolve(
                    &invocation,
                    READ_TOOL_NAME,
                    &args.file_path,
                    args.environment_id.as_deref(),
                    self.multi_environment,
                )?;
                target
                    .ensure(&invocation, READ_TOOL_NAME, &args.file_path, Access::Read)
                    .await?;
                let fs = target.environment.environment.get_filesystem();
                let Some(text) = target.read_text(fs.as_ref()).await? else {
                    return Err(refuse(format!("`{}` does not exist", target.shown)));
                };
                let offset = args.offset.unwrap_or(1).max(1);
                let limit = args.limit.filter(|l| *l > 0).unwrap_or(DEFAULT_READ_LINES);
                if ledger(&invocation).read(target.key(), target.path.clone(), &text, offset, limit)
                    == ReadOutcome::Unchanged
                {
                    return Ok(format!(
                        "`{}` is unchanged since the last Read of these lines in this conversation; that output is still above, use it instead of reading again",
                        target.shown
                    ));
                }
                let budget = codex_protocol::protocol::TruncationPolicy::from(
                    invocation.turn.model_info().truncation_policy,
                )
                .byte_budget();
                let body = numbered(&text, offset, limit, budget);
                Ok::<String, FunctionCallError>(body)
            };
            shown(&invocation, READ_TOOL_NAME, work).await
        })
    }
}

impl ToolExecutor<ToolInvocation> for EditFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(EDIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        function_tool(
            EDIT_TOOL_NAME,
            "Replaces an exact string in a file. `old_string` must match the file exactly (whitespace and \
             indentation included) and appear once, unless `replace_all` is true; include enough surrounding \
             lines to make it unique. Read the file first. To create a file or replace all of it, use Write.",
            properties(
                vec![
                    (
                        "file_path",
                        text(
                            "Path of the file to change (absolute, or relative to the working directory).",
                        ),
                    ),
                    ("old_string", text("The exact text to replace.")),
                    (
                        "new_string",
                        text("The text to put in its place (must differ from old_string)."),
                    ),
                    (
                        "replace_all",
                        JsonSchema::boolean(Some(
                            "Replace every occurrence. Defaults to false.".to_string(),
                        )),
                    ),
                ],
                self.multi_environment,
            ),
            &["file_path", "old_string", "new_string"],
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = function_arguments(invocation.payload.clone(), EDIT_TOOL_NAME)?;
            let args: EditArgs = parse_arguments(&arguments)?;
            let target = Target::resolve(
                &invocation,
                EDIT_TOOL_NAME,
                &args.file_path,
                args.environment_id.as_deref(),
                self.multi_environment,
            )?;
            target
                .ensure(&invocation, EDIT_TOOL_NAME, &args.file_path, Access::Write)
                .await?;
            let fs = target.environment.environment.get_filesystem();
            let Some(text) = target.read_text(fs.as_ref()).await? else {
                return Err(refuse(format!(
                    "`{}` does not exist; use Write to create it",
                    target.shown
                )));
            };
            let key = target.key();
            ledger(&invocation)
                .may_change(&key, Some(&text))
                .map_err(|stale| refuse(format!("Edit: {}", stale.message(&target.shown))))?;
            let next = replaced(&text, &args.old_string, &args.new_string, args.replace_all)
                .map_err(|reason| refuse(format!("Edit `{}`: {reason}", target.shown)))?;
            let patch = whole_file_patch(&args.file_path, Some(&text), &next);
            let session = Arc::clone(&invocation.session);
            let uri = target.path.clone();
            let output = apply_generated_patch(invocation, target, patch).await?;
            lock_ledger(&session).wrote(key, uri, &next);
            Ok(output)
        })
    }
}

impl ToolExecutor<ToolInvocation> for WriteFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WRITE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        function_tool(
            WRITE_TOOL_NAME,
            "Creates a file, or replaces the whole of an existing one, with `content` (parent folders are created). \
             Prefer Edit for changing part of an existing file. To replace an existing file, Read it first.",
            properties(
                vec![
                    (
                        "file_path",
                        text(
                            "Path of the file to write (absolute, or relative to the working directory).",
                        ),
                    ),
                    ("content", text("The complete text of the file.")),
                ],
                self.multi_environment,
            ),
            &["file_path", "content"],
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = function_arguments(invocation.payload.clone(), WRITE_TOOL_NAME)?;
            let args: WriteArgs = parse_arguments(&arguments)?;
            let target = Target::resolve(
                &invocation,
                WRITE_TOOL_NAME,
                &args.file_path,
                args.environment_id.as_deref(),
                self.multi_environment,
            )?;
            target
                .ensure(&invocation, WRITE_TOOL_NAME, &args.file_path, Access::Write)
                .await?;
            let fs = target.environment.environment.get_filesystem();
            let old = target.read_text(fs.as_ref()).await?;
            if old.as_deref() == Some(args.content.as_str()) {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!(
                        "`{}` already has this content; nothing changed",
                        target.shown
                    ),
                    Some(true),
                )));
            }
            let key = target.key();
            ledger(&invocation)
                .may_change(&key, old.as_deref())
                .map_err(|stale| refuse(format!("Write: {}", stale.message(&target.shown))))?;
            let patch = whole_file_patch(&args.file_path, old.as_deref(), &args.content);
            let session = Arc::clone(&invocation.session);
            let uri = target.path.clone();
            let output = apply_generated_patch(invocation, target, patch).await?;
            lock_ledger(&session).wrote(key, uri, &args.content);
            Ok(output)
        })
    }
}

impl CoreToolRuntime for ReadFileHandler {}
impl CoreToolRuntime for EditFileHandler {}
impl CoreToolRuntime for WriteFileHandler {}

#[cfg(test)]
#[path = "file_tools_tests.rs"]
mod tests;
