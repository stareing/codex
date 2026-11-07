//! `Glob` and `Grep`: Claude Code's search tools of the same names, offered with `Read`/`Edit`/`Write`
//! (`file_tools.rs`) to models that do not get `apply_patch`. Parameters, defaults, validation messages, paging and
//! result text are Claude Code's (`GlobTool.ts`, `GrepTool.ts`); the search itself is ripgrep's libraries
//! configured as those tools configure `rg` (`codex_file_search::rg_search`), so no `rg` binary is needed.
//!
//! Differences, all deliberate: an unknown `type`, a bad regex or glob is an error here (Claude Code's `rg` call
//! returns "No files found" for them); `count` mode on a single file counts it (Claude Code reports 0, as `rg -c`
//! prints no file name there); paths in context lines are shown relative like every other line. Searches run on
//! the session's own computer only (local environments); paths the sandbox may not read are left out, as
//! Claude Code leaves out its read-deny rules.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use codex_file_search::rg_search;
use codex_file_search::rg_search::GrepMode;
use codex_file_search::rg_search::GrepRequest;
use codex_file_search::rg_search::Limits;
use codex_file_search::rg_search::Readable;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_tools::JsonSchema;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Map;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::file_access::Access;
use crate::tools::handlers::file_access::ensure_access;
use crate::tools::handlers::file_tools::function_arguments;
use crate::tools::handlers::file_tools::function_tool;
use crate::tools::handlers::file_tools::properties;
use crate::tools::handlers::file_tools::refuse;
use crate::tools::handlers::file_tools::text;
use crate::tools::handlers::file_tools::turn_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub const GLOB_TOOL_NAME: &str = "Glob";
pub const GREP_TOOL_NAME: &str = "Grep";

/// Files `Glob` returns (Claude Code's `globLimits.maxResults` default).
const GLOB_MAX_RESULTS: usize = 100;
/// `Grep`'s `head_limit` when none is given; 0 means no limit.
const DEFAULT_HEAD_LIMIT: usize = 250;
const CWD_NOTE: &str = "Note: your current working directory is";
const GLOB_TRUNCATED: &str =
    "(Results are truncated. Consider using a more specific path or pattern.)";

const GLOB_DESCRIPTION: &str = "- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"
- Returns matching file paths sorted by modification time
- Use this tool when you need to find files by name patterns
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use a sub-agent (spawn_agent) instead when that tool is available";

const GREP_DESCRIPTION: &str = "A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a shell command. The Grep tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., \"log.*Error\", \"function\\s+\\w+\")
  - Filter files with glob parameter (e.g., \"*.js\", \"**/*.tsx\") or type parameter (e.g., \"js\", \"py\", \"rust\")
  - Output modes: \"content\" shows matching lines, \"files_with_matches\" shows only file paths (default), \"count\" shows match counts
  - Use a sub-agent (spawn_agent, when available) for open-ended searches requiring multiple rounds
  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\\{\\}` to find `interface{}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \\{[\\s\\S]*?field`, use `multiline: true`
";

const GLOB_PATH_DESCRIPTION: &str = "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided.";

/// The search's view of one call: the computer's working directory, home and what the sandbox lets it read.
struct SearchScope {
    cwd: PathBuf,
    home: Option<PathBuf>,
    readable: Readable,
}

impl SearchScope {
    fn new(tool: &str, environment: &TurnEnvironment) -> Result<Self, FunctionCallError> {
        if environment.environment.is_remote() {
            return Err(refuse(format!(
                "{tool} searches files on this computer only; in a remote environment run `rg` with the shell"
            )));
        }
        let cwd = environment.cwd().to_path_buf();
        let home = environment
            .user_home_dir
            .as_ref()
            .map(codex_utils_path_uri::PathUri::to_path_buf);
        let policy = environment
            .sandbox_context(/*additional_permissions*/ None)
            .permissions
            .file_system_sandbox_policy();
        let readable: Readable = if policy.has_full_disk_read_access() {
            match ReadDenyMatcher::try_new_for_local_paths(&policy, &cwd)
                .map_err(|err| refuse(format!("{tool}: {err}")))?
            {
                Some(denied) => {
                    Arc::new(move |path: &Path| !denied.is_local_path_read_denied(path))
                }
                None => Arc::new(|_: &Path| true),
            }
        } else {
            let cwd = cwd.clone();
            Arc::new(move |path: &Path| policy.can_read_local_path_with_cwd(path, &cwd))
        };
        Ok(Self {
            cwd,
            home,
            readable,
        })
    }

    /// Claude Code's `expandPath`: trimmed, `~` expanded, relative to the working directory, normalized.
    fn expand(&self, raw: &str) -> PathBuf {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return self.cwd.clone();
        }
        if let Some(home) = &self.home {
            if trimmed == "~" {
                return home.clone();
            }
            if let Some(rest) = trimmed.strip_prefix("~/") {
                return normalized(&home.join(rest));
            }
        }
        normalized(&self.cwd.join(trimmed))
    }

    /// Claude Code's `suggestPathUnderCwd`: a path under the working directory's parent but not under it, when the
    /// same relative path exists under the working directory (a missing project folder in the path).
    fn suggestion(&self, requested: &Path) -> Option<PathBuf> {
        let parent = self.cwd.parent()?;
        if !requested.starts_with(parent) || requested.starts_with(&self.cwd) {
            return None;
        }
        let corrected = self.cwd.join(requested.strip_prefix(parent).ok()?);
        corrected.exists().then_some(corrected)
    }

    fn missing(&self, what: &str, raw: &str, requested: &Path) -> FunctionCallError {
        let mut message = format!(
            "{what} does not exist: {raw}. {CWD_NOTE} {}.",
            self.cwd.display()
        );
        if let Some(suggestion) = self.suggestion(requested) {
            message.push_str(&format!(" Did you mean {}?", suggestion.display()));
        }
        refuse(message)
    }

    fn shown(&self, path: &Path) -> String {
        rg_search::display_path(&self.cwd, path)
            .to_string_lossy()
            .into_owned()
    }
}

fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Runs a blocking search off the runtime; cancelling the turn stops it.
async fn run_search<T: Send + 'static>(
    invocation: &ToolInvocation,
    search: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> Result<T, FunctionCallError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let job = tokio::task::spawn_blocking(move || search(&flag));
    tokio::select! {
        result = job => result.map_err(|err| refuse(format!("the search failed: {err}"))),
        () = invocation.cancellation_token.cancelled() => {
            cancelled.store(true, Ordering::Relaxed);
            Err(refuse("the search was cancelled"))
        }
    }
}

/// The arguments object, refusing keys the tool does not take (Claude Code's schemas are strict).
fn arguments(
    invocation: &ToolInvocation,
    tool: &str,
    allowed: &[&str],
) -> Result<Map<String, Value>, FunctionCallError> {
    let raw = function_arguments(invocation.payload.clone(), tool)?;
    let Value::Object(map) = serde_json::from_str::<Value>(&raw)
        .map_err(|err| refuse(format!("{tool}: arguments are not JSON: {err}")))?
    else {
        return Err(refuse(format!("{tool}: arguments must be an object")));
    };
    if let Some(unknown) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(refuse(format!("{tool}: unknown parameter `{unknown}`")));
    }
    Ok(map)
}

fn string_arg(
    map: &Map<String, Value>,
    tool: &str,
    key: &str,
) -> Result<Option<String>, FunctionCallError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(refuse(format!("{tool}: `{key}` must be a string"))),
    }
}

/// A number, or a string holding one (Claude Code's `semanticNumber`); must be a whole number ≥ 0.
fn count_arg(
    map: &Map<String, Value>,
    tool: &str,
    key: &str,
) -> Result<Option<usize>, FunctionCallError> {
    let number = match map.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(value)) => value.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    match number {
        Some(n) if n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64 => Ok(Some(n as usize)),
        _ => Err(refuse(format!(
            "{tool}: `{key}` must be a whole number, 0 or more"
        ))),
    }
}

/// A boolean, or `"true"`/`"false"` (Claude Code's `semanticBoolean`).
fn flag_arg(
    map: &Map<String, Value>,
    tool: &str,
    key: &str,
) -> Result<Option<bool>, FunctionCallError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(Value::String(value)) if value == "true" => Ok(Some(true)),
        Some(Value::String(value)) if value == "false" => Ok(Some(false)),
        Some(_) => Err(refuse(format!("{tool}: `{key}` must be true or false"))),
    }
}

fn search_error(tool: &str, err: rg_search::SearchError) -> FunctionCallError {
    refuse(format!("{tool}: {err}"))
}

/// Claude Code's `applyHeadLimit`: `offset` items skipped, then `limit` kept (0 = all); the limit is reported only
/// when it cut something off.
pub(crate) fn head_limited<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: usize,
) -> (Vec<T>, Option<usize>) {
    let rest: Vec<T> = items.into_iter().skip(offset).collect();
    let limit = match limit {
        Some(0) => return (rest, None),
        Some(limit) => limit,
        None => DEFAULT_HEAD_LIMIT,
    };
    let truncated = rest.len() > limit;
    let kept = rest.into_iter().take(limit).collect();
    (kept, truncated.then_some(limit))
}

/// Claude Code's `formatLimitInfo`.
pub(crate) fn limit_info(limit: Option<usize>, offset: usize) -> String {
    let mut parts = Vec::new();
    if let Some(limit) = limit {
        parts.push(format!("limit: {limit}"));
    }
    if offset > 0 {
        parts.push(format!("offset: {offset}"));
    }
    parts.join(", ")
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// `Glob`'s result text.
pub(crate) fn glob_result(files: &[String], truncated: bool) -> String {
    if files.is_empty() {
        return "No files found".to_string();
    }
    let mut lines = files.to_vec();
    if truncated {
        lines.push(GLOB_TRUNCATED.to_string());
    }
    lines.join("\n")
}

/// `Grep`'s result text for content mode.
pub(crate) fn content_result(lines: &[String], limit: Option<usize>, offset: usize) -> String {
    let content = if lines.is_empty() {
        "No matches found".to_string()
    } else {
        lines.join("\n")
    };
    let info = limit_info(limit, offset);
    if info.is_empty() {
        content
    } else {
        format!("{content}\n\n[Showing results with pagination = {info}]")
    }
}

/// `Grep`'s result text for count mode (`path:count` lines, or a bare count for a single file).
pub(crate) fn count_result(lines: &[String], limit: Option<usize>, offset: usize) -> String {
    let (mut matches, mut files) = (0u64, 0usize);
    for line in lines {
        let count = line
            .rsplit_once(':')
            .map_or(line.as_str(), |(_, count)| count);
        if let Ok(count) = count.parse::<u64>() {
            matches += count;
            files += 1;
        }
    }
    let content = if lines.is_empty() {
        "No matches found".to_string()
    } else {
        lines.join("\n")
    };
    let info = limit_info(limit, offset);
    let occurrences = if matches == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    let pagination = if info.is_empty() {
        String::new()
    } else {
        format!(" with pagination = {info}")
    };
    format!(
        "{content}\n\nFound {matches} total {occurrences} across {files} {}.{pagination}",
        plural(files, "file")
    )
}

/// `Grep`'s result text for files_with_matches mode.
pub(crate) fn files_result(files: &[String], limit: Option<usize>, offset: usize) -> String {
    if files.is_empty() {
        return "No files found".to_string();
    }
    let info = limit_info(limit, offset);
    let info = if info.is_empty() {
        String::new()
    } else {
        format!(" {info}")
    };
    format!(
        "Found {} {}{info}\n{}",
        files.len(),
        plural(files.len(), "file"),
        files.join("\n")
    )
}

/// Claude Code's `glob` splitting: on whitespace, then on commas unless the part has braces.
pub(crate) fn split_globs(raw: &str) -> Vec<String> {
    let mut globs = Vec::new();
    for part in raw.split_whitespace() {
        if part.contains('{') && part.contains('}') {
            globs.push(part.to_string());
        } else {
            globs.extend(
                part.split(',')
                    .filter(|glob| !glob.is_empty())
                    .map(str::to_string),
            );
        }
    }
    globs
}

/// Newest first, ties by path (Claude Code sorts `files_with_matches` by modification time).
fn newest_first(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut timed: Vec<(PathBuf, SystemTime)> = files
        .into_iter()
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (path, modified)
        })
        .collect();
    timed.sort_by(|(a, ta), (b, tb)| tb.cmp(ta).then_with(|| a.cmp(b)));
    timed.into_iter().map(|(path, _)| path).collect()
}

pub struct GlobHandler {
    multi_environment: bool,
}

pub struct GrepHandler {
    multi_environment: bool,
}

impl GlobHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

impl GrepHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

fn allowed_keys<'a>(keys: &[&'a str], multi_environment: bool) -> Vec<&'a str> {
    let mut keys = keys.to_vec();
    if multi_environment {
        keys.push("environment_id");
    }
    keys
}

impl ToolExecutor<ToolInvocation> for GlobHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(GLOB_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        function_tool(
            GLOB_TOOL_NAME,
            GLOB_DESCRIPTION,
            properties(
                vec![
                    ("pattern", text("The glob pattern to match files against")),
                    ("path", text(GLOB_PATH_DESCRIPTION)),
                ],
                self.multi_environment,
            ),
            &["pattern"],
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
            let tool = GLOB_TOOL_NAME;
            let map = arguments(
                &invocation,
                tool,
                &allowed_keys(&["pattern", "path"], self.multi_environment),
            )?;
            let Some(pattern) = string_arg(&map, tool, "pattern")? else {
                return Err(refuse(format!("{tool}: `pattern` is required")));
            };
            let path = string_arg(&map, tool, "path")?;
            let environment_id = string_arg(&map, tool, "environment_id")?;
            let environment = turn_environment(
                &invocation,
                tool,
                environment_id.as_deref(),
                self.multi_environment,
            )?;
            let scope = SearchScope::new(tool, &environment)?;
            let mut search_dir = match &path {
                Some(raw) => {
                    let dir = scope.expand(raw);
                    if !dir.exists() {
                        return Err(scope.missing("Directory", raw, &dir));
                    }
                    if !dir.is_dir() {
                        return Err(refuse(format!("Path is not a directory: {raw}")));
                    }
                    dir
                }
                None => scope.cwd.clone(),
            };
            let mut relative_pattern = pattern;
            if Path::new(&relative_pattern).is_absolute() {
                let (base, rest) = rg_search::split_absolute_glob(&relative_pattern);
                if !base.as_os_str().is_empty() {
                    search_dir = base;
                    relative_pattern = rest;
                }
            }
            let asked = path.as_deref().unwrap_or(".");
            ensure_access(
                &invocation,
                &environment,
                tool,
                asked,
                &search_dir,
                Access::Read,
            )
            .await?;
            let cwd = scope.cwd.clone();
            let readable = Arc::clone(&scope.readable);
            let found = run_search(&invocation, move |cancelled| {
                rg_search::glob_files(
                    &cwd,
                    &search_dir,
                    &relative_pattern,
                    GLOB_MAX_RESULTS,
                    &readable,
                    Limits::default(),
                    cancelled,
                )
            })
            .await?
            .map_err(|err| search_error(tool, err))?;
            let files: Vec<String> = found.files.iter().map(|path| scope.shown(path)).collect();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                glob_result(&files, found.truncated),
                Some(true),
            )))
        })
    }
}

const GREP_KEYS: [&str; 14] = [
    "pattern",
    "path",
    "glob",
    "output_mode",
    "-B",
    "-A",
    "-C",
    "context",
    "-n",
    "-i",
    "type",
    "head_limit",
    "offset",
    "multiline",
];

impl ToolExecutor<ToolInvocation> for GrepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(GREP_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let number = |description: &str| JsonSchema::number(Some(description.to_string()));
        let boolean = |description: &str| JsonSchema::boolean(Some(description.to_string()));
        function_tool(
            GREP_TOOL_NAME,
            GREP_DESCRIPTION,
            properties(
                vec![
                    ("pattern", text("The regular expression pattern to search for in file contents")),
                    (
                        "path",
                        text("File or directory to search in (rg PATH). Defaults to current working directory."),
                    ),
                    (
                        "glob",
                        text("Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob"),
                    ),
                    (
                        "output_mode",
                        JsonSchema::string_enum(
                            vec![
                                Value::String("content".to_string()),
                                Value::String("files_with_matches".to_string()),
                                Value::String("count".to_string()),
                            ],
                            Some(
                                "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"files_with_matches\".".to_string(),
                            ),
                        ),
                    ),
                    (
                        "-B",
                        number("Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise."),
                    ),
                    (
                        "-A",
                        number("Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise."),
                    ),
                    ("-C", number("Alias for context.")),
                    (
                        "context",
                        number("Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise."),
                    ),
                    (
                        "-n",
                        boolean("Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true."),
                    ),
                    ("-i", boolean("Case insensitive search (rg -i)")),
                    (
                        "type",
                        text("File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types."),
                    ),
                    (
                        "head_limit",
                        number("Limit output to first N lines/entries, equivalent to \"| head -N\". Works across all output modes: content (limits output lines), files_with_matches (limits file paths), count (limits count entries). Defaults to 250 when unspecified. Pass 0 for unlimited (use sparingly — large result sets waste context)."),
                    ),
                    (
                        "offset",
                        number("Skip first N lines/entries before applying head_limit, equivalent to \"| tail -n +N | head -N\". Works across all output modes. Defaults to 0."),
                    ),
                    (
                        "multiline",
                        boolean("Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false."),
                    ),
                ],
                self.multi_environment,
            ),
            &["pattern"],
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
            let tool = GREP_TOOL_NAME;
            let map = arguments(
                &invocation,
                tool,
                &allowed_keys(&GREP_KEYS, self.multi_environment),
            )?;
            let Some(pattern) = string_arg(&map, tool, "pattern")? else {
                return Err(refuse(format!("{tool}: `pattern` is required")));
            };
            let mode = match string_arg(&map, tool, "output_mode")?.as_deref() {
                None | Some("files_with_matches") => GrepMode::FilesWithMatches,
                Some("content") => GrepMode::Content,
                Some("count") => GrepMode::Count,
                Some(other) => {
                    return Err(refuse(format!(
                        "{tool}: output_mode must be content, files_with_matches or count, not `{other}`"
                    )));
                }
            };
            // `context`, then `-C`, then `-B`/`-A` (Claude Code's precedence).
            let (before, after) = match (
                count_arg(&map, tool, "context")?,
                count_arg(&map, tool, "-C")?,
            ) {
                (Some(both), _) | (None, Some(both)) => (both, both),
                (None, None) => (
                    count_arg(&map, tool, "-B")?.unwrap_or(0),
                    count_arg(&map, tool, "-A")?.unwrap_or(0),
                ),
            };
            let head_limit = count_arg(&map, tool, "head_limit")?;
            let offset = count_arg(&map, tool, "offset")?.unwrap_or(0);
            let environment_id = string_arg(&map, tool, "environment_id")?;
            let environment = turn_environment(
                &invocation,
                tool,
                environment_id.as_deref(),
                self.multi_environment,
            )?;
            let scope = SearchScope::new(tool, &environment)?;
            let raw_path = string_arg(&map, tool, "path")?;
            let target = match &raw_path {
                Some(raw) => {
                    let target = scope.expand(raw);
                    if !target.exists() {
                        return Err(scope.missing("Path", raw, &target));
                    }
                    target
                }
                None => scope.cwd.clone(),
            };
            let asked = raw_path.as_deref().unwrap_or(".");
            ensure_access(
                &invocation,
                &environment,
                tool,
                asked,
                &target,
                Access::Read,
            )
            .await?;
            let request = GrepRequest {
                cwd: scope.cwd.clone(),
                target,
                pattern,
                globs: string_arg(&map, tool, "glob")?
                    .map_or_else(Vec::new, |raw| split_globs(&raw)),
                file_type: string_arg(&map, tool, "type")?.filter(|name| !name.is_empty()),
                mode,
                before_context: before,
                after_context: after,
                line_numbers: flag_arg(&map, tool, "-n")?.unwrap_or(true),
                case_insensitive: flag_arg(&map, tool, "-i")?.unwrap_or(false),
                multiline: flag_arg(&map, tool, "multiline")?.unwrap_or(false),
            };
            let readable = Arc::clone(&scope.readable);
            let found = run_search(&invocation, move |cancelled| {
                rg_search::grep(&request, &readable, Limits::default(), cancelled).map(
                    |mut found| {
                        if request.mode == GrepMode::FilesWithMatches {
                            found.files = newest_first(found.files);
                        }
                        found
                    },
                )
            })
            .await?
            .map_err(|err| search_error(tool, err))?;
            let body = match mode {
                GrepMode::Content => {
                    let (lines, limit) = head_limited(found.lines, head_limit, offset);
                    content_result(&lines, limit, offset)
                }
                GrepMode::Count => {
                    let (lines, limit) = head_limited(found.lines, head_limit, offset);
                    count_result(&lines, limit, offset)
                }
                GrepMode::FilesWithMatches => {
                    let (files, limit) = head_limited(found.files, head_limit, offset);
                    let files: Vec<String> = files.iter().map(|path| scope.shown(path)).collect();
                    files_result(&files, limit, offset)
                }
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                body,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for GlobHandler {}
impl CoreToolRuntime for GrepHandler {}

#[cfg(test)]
#[path = "search_tools_tests.rs"]
mod tests;
