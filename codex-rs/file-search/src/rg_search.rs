//! The search engine behind the `Glob` and `Grep` model tools (core `tools/handlers/search_tools.rs`). Those tools
//! follow Claude Code's tools of the same names, which run the `rg` binary; this module runs ripgrep's own
//! libraries in process instead (`ignore` for the walk, globs and file types, `grep-regex`, `grep-searcher` and
//! `grep-printer` for matching and output), configured the way those tools configure `rg`, so the regex syntax,
//! ignore rules, binary handling and output lines are ripgrep's:
//!
//! - [`glob_files`] = `rg --files --glob <pattern> --sort=modified --no-ignore --hidden <dir>`: every file under
//!   `dir` (ignore files not read, hidden entries included) whose path matches the glob, oldest first.
//! - [`grep`] = `rg --hidden --glob !.git … --max-columns 500 [-U --multiline-dotall] [-i] [-l|-c] [-n]
//!   [-A/-B/-C] <pattern> [--type T] [--glob G…] <path>`: `.gitignore`/`.ignore`/`.rgignore` honored (inside a git
//!   repository for `.gitignore`), version-control folders skipped.
//!
//! Globs are matched like `rg --glob`: relative to the working directory (`cwd`), a glob without a `/` matches the
//! name at any depth. The caller's `readable` filter drops paths the sandbox may not read (Claude Code passes its
//! read-deny rules to `rg` as `--glob !…`). Blocking: run off the async runtime. Stops at the deadline or when
//! `cancelled` is set.

use grep_printer::StandardBuilder;
use grep_printer::SummaryBuilder;
use grep_printer::SummaryKind;
use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::BinaryDetection;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use ignore::WalkBuilder;
use ignore::overrides::Override;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

/// How long a search may run (Claude Code kills `rg` after 20 s).
pub const DEADLINE: Duration = Duration::from_secs(20);
/// Output kept from one search (Claude Code's `maxBuffer` for `rg`'s stdout).
pub const MAX_OUTPUT_BYTES: usize = 20_000_000;
/// `--max-columns 500`: longer matching lines are replaced by ripgrep's "[Omitted long line …]".
pub const MAX_COLUMNS: u64 = 500;
/// Version-control folders `Grep` never searches (`--glob !<dir>` each).
pub const VCS_DIRECTORIES: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Whether the sandbox lets the tool read a path (directories are pruned, files dropped).
pub type Readable = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// A glob (`pattern` of `Glob`, `glob` of `Grep`) ripgrep rejects.
    InvalidGlob(String),
    /// The regular expression does not parse.
    InvalidPattern(String),
    /// `type` is not one of ripgrep's file types.
    UnknownType(String),
    /// The deadline passed before anything was found.
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGlob(message) => write!(f, "invalid glob: {message}"),
            Self::InvalidPattern(message) => write!(f, "invalid regex pattern: {message}"),
            Self::UnknownType(name) => write!(
                f,
                "unrecognized file type: {name} (use a ripgrep type such as js, py, rust, go, java)"
            ),
            Self::TimedOut => write!(
                f,
                "Ripgrep search timed out after {} seconds. The search may have matched files but did not \
                 complete in time. Try searching a more specific path or pattern.",
                DEADLINE.as_secs()
            ),
            Self::Cancelled => write!(f, "the search was cancelled"),
        }
    }
}

impl std::error::Error for SearchError {}

/// Bounds of one search; [`Limits::default`] is Claude Code's.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub deadline: Duration,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: DEADLINE,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

struct Clock<'a> {
    started: Instant,
    deadline: Duration,
    cancelled: &'a AtomicBool,
}

impl Clock<'_> {
    fn stop(&self) -> Option<SearchError> {
        if self.cancelled.load(Ordering::Relaxed) {
            Some(SearchError::Cancelled)
        } else if self.started.elapsed() > self.deadline {
            Some(SearchError::TimedOut)
        } else {
            None
        }
    }
}

/// How a path is shown to the model: relative to `cwd` when it is inside it, absolute otherwise (Claude Code's
/// `toRelativePath`).
pub fn display_path(cwd: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}

/// Splits an absolute glob into the folder to search and the glob relative to it (Claude Code's
/// `extractGlobBaseDirectory`: the base is everything before the last separator ahead of the first `*?[{`).
pub fn split_absolute_glob(pattern: &str) -> (PathBuf, String) {
    let is_separator = |c: char| c == '/' || c == std::path::MAIN_SEPARATOR;
    let Some(first_glob) = pattern.find(['*', '?', '[', '{']) else {
        let path = Path::new(pattern);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let file = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        return (dir, file);
    };
    let prefix = &pattern[..first_glob];
    let Some(last_separator) = prefix.rfind(is_separator) else {
        return (PathBuf::new(), pattern.to_string());
    };
    let mut base = prefix[..last_separator].to_string();
    if base.is_empty() && last_separator == 0 {
        base = "/".to_string();
    }
    // `C:` alone is "the current folder on drive C"; the root is `C:\`.
    if cfg!(windows) && base.len() == 2 && base.ends_with(':') {
        base.push(std::path::MAIN_SEPARATOR);
    }
    (
        PathBuf::from(base),
        pattern[last_separator + 1..].to_string(),
    )
}

fn overrides(cwd: &Path, globs: &[String]) -> Result<Override, SearchError> {
    let mut builder = OverrideBuilder::new(cwd);
    for glob in globs {
        builder
            .add(glob)
            .map_err(|err| SearchError::InvalidGlob(err.to_string()))?;
    }
    builder
        .build()
        .map_err(|err| SearchError::InvalidGlob(err.to_string()))
}

fn prune_unreadable(walk: &mut WalkBuilder, readable: &Readable) {
    let readable = Arc::clone(readable);
    walk.filter_entry(move |entry| readable(entry.path()));
}

fn is_file(entry: &ignore::DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_file())
}

/// What [`glob_files`] found: at most `limit` files, oldest first (absolute paths).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GlobFiles {
    pub files: Vec<PathBuf>,
    pub truncated: bool,
}

/// `rg --files --glob <pattern> --sort=modified --no-ignore --hidden <search_dir>`, the first `limit` files.
pub fn glob_files(
    cwd: &Path,
    search_dir: &Path,
    pattern: &str,
    limit: usize,
    readable: &Readable,
    limits: Limits,
    cancelled: &AtomicBool,
) -> Result<GlobFiles, SearchError> {
    let clock = Clock {
        started: Instant::now(),
        deadline: limits.deadline,
        cancelled,
    };
    let mut walk = WalkBuilder::new(search_dir);
    walk.standard_filters(false)
        .follow_links(false)
        .overrides(overrides(cwd, &[pattern.to_string()])?)
        .sort_by_file_name(std::cmp::Ord::cmp);
    prune_unreadable(&mut walk, readable);
    let mut found: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    for entry in walk.build() {
        if let Some(stop) = clock.stop() {
            // `rg --sort` prints nothing before it has every file.
            return Err(stop);
        }
        let Ok(entry) = entry else { continue };
        if !is_file(&entry) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        found.push((entry.into_path(), modified));
    }
    // ripgrep's `--sort=modified`: stable, oldest first, files without a time last.
    found.sort_by(|(_, a), (_, b)| match (a, b) {
        (Some(a), Some(b)) => a.cmp(b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let truncated = found.len() > limit;
    let files = found
        .into_iter()
        .take(limit)
        .map(|(path, _)| path)
        .collect();
    Ok(GlobFiles { files, truncated })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepMode {
    /// Matching lines (`rg` without `-l`/`-c`).
    Content,
    /// Files with a match (`rg -l`).
    FilesWithMatches,
    /// Matching lines per file (`rg -c`).
    Count,
}

/// One `Grep` search, in `rg`'s terms.
#[derive(Debug, Clone)]
pub struct GrepRequest {
    /// The working directory: globs are relative to it and shown paths are relative to it.
    pub cwd: PathBuf,
    /// The file or folder to search (absolute).
    pub target: PathBuf,
    pub pattern: String,
    /// `--glob` patterns (already split).
    pub globs: Vec<String>,
    /// `--type`.
    pub file_type: Option<String>,
    pub mode: GrepMode,
    pub before_context: usize,
    pub after_context: usize,
    /// `-n` (content mode only).
    pub line_numbers: bool,
    /// `-i`.
    pub case_insensitive: bool,
    /// `-U --multiline-dotall`.
    pub multiline: bool,
}

/// What [`grep`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GrepOutput {
    /// Content and count modes: ripgrep's output lines (paths as [`display_path`] shows them); empty lines dropped.
    pub lines: Vec<String>,
    /// Count mode: matching lines over all files, and how many files had a match.
    pub total_matches: u64,
    pub files_with_counts: u64,
    /// Files-with-matches mode: absolute paths, in search order.
    pub files: Vec<PathBuf>,
    /// The deadline or the output cap cut the search short (the last, possibly partial, line is dropped).
    pub cut_short: bool,
}

fn matcher(request: &GrepRequest) -> Result<RegexMatcher, SearchError> {
    let mut builder = RegexMatcherBuilder::new();
    // ripgrep: `^`/`$` always match at line boundaries; without -U a match never spans lines.
    builder
        .multi_line(true)
        .case_insensitive(request.case_insensitive)
        .octal(false);
    if request.multiline {
        builder.dot_matches_new_line(true);
    } else {
        builder
            .line_terminator(Some(b'\n'))
            .dot_matches_new_line(false);
    }
    builder
        .build(&request.pattern)
        .map_err(|err| SearchError::InvalidPattern(err.to_string()))
}

fn searcher(request: &GrepRequest, explicit: bool) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(request.mode == GrepMode::Content && request.line_numbers)
        .multi_line(request.multiline)
        .before_context(if request.mode == GrepMode::Content {
            request.before_context
        } else {
            0
        })
        .after_context(if request.mode == GrepMode::Content {
            request.after_context
        } else {
            0
        })
        // ripgrep: files found by the walk are skipped at the first NUL byte; a file named on the command line
        // is searched with NULs replaced and a "binary file matches" note.
        .binary_detection(if explicit {
            BinaryDetection::convert(b'\x00')
        } else {
            BinaryDetection::quit(b'\x00')
        });
    builder.build()
}

/// The files to search: `target` itself when it is a file, else the walk under it.
fn haystacks(
    request: &GrepRequest,
    readable: &Readable,
) -> Result<Box<dyn Iterator<Item = PathBuf>>, SearchError> {
    if !request.target.is_dir() {
        let target = request.target.clone();
        let allowed = readable(&target);
        return Ok(Box::new(allowed.then_some(target).into_iter()));
    }
    let mut globs: Vec<String> = VCS_DIRECTORIES
        .iter()
        .map(|dir| format!("!{dir}"))
        .collect();
    globs.extend(request.globs.iter().cloned());
    let mut walk = WalkBuilder::new(&request.target);
    walk.hidden(false)
        .follow_links(false)
        .require_git(true)
        .add_custom_ignore_filename(".rgignore")
        .overrides(overrides(&request.cwd, &globs)?)
        .sort_by_file_name(std::cmp::Ord::cmp);
    if let Some(name) = &request.file_type {
        let mut types = TypesBuilder::new();
        types.add_defaults();
        types.select(name);
        let types = types
            .build()
            .map_err(|_| SearchError::UnknownType(name.clone()))?;
        walk.types(types);
    }
    prune_unreadable(&mut walk, readable);
    Ok(Box::new(walk.build().filter_map(|entry| {
        let entry = entry.ok()?;
        is_file(&entry).then(|| entry.into_path())
    })))
}

/// `rg` as Claude Code's `Grep` runs it.
pub fn grep(
    request: &GrepRequest,
    readable: &Readable,
    limits: Limits,
    cancelled: &AtomicBool,
) -> Result<GrepOutput, SearchError> {
    let clock = Clock {
        started: Instant::now(),
        deadline: limits.deadline,
        cancelled,
    };
    let matcher = matcher(request)?;
    if let Some(name) = &request.file_type
        && !ignore::types::TypesBuilder::new()
            .add_defaults()
            .definitions()
            .iter()
            .any(|definition| definition.name() == name)
    {
        return Err(SearchError::UnknownType(name.clone()));
    }
    let explicit = !request.target.is_dir();
    // ripgrep prints the path when it searches a folder (a single named file is shown without it).
    let with_path = !explicit;
    let mut searcher = searcher(request, explicit);
    let mut output = GrepOutput::default();
    let mut standard = StandardBuilder::new()
        .path(with_path)
        .heading(false)
        .max_columns(Some(MAX_COLUMNS))
        .max_columns_preview(false)
        .separator_context(Some(b"--".to_vec()))
        // ripgrep also puts `--` between files when context lines are shown.
        .separator_search(
            (request.before_context > 0 || request.after_context > 0).then(|| b"--".to_vec()),
        )
        .separator_field_match(b":".to_vec())
        .separator_field_context(b"-".to_vec())
        .build_no_color(Vec::new());
    let mut summary = SummaryBuilder::new()
        .kind(match request.mode {
            GrepMode::Count => SummaryKind::Count,
            _ => SummaryKind::QuietWithMatch,
        })
        .path(with_path && request.mode == GrepMode::Count)
        .exclude_zero(true)
        .stats(request.mode == GrepMode::Count)
        .separator_field(b":".to_vec())
        .build_no_color(Vec::new());

    for path in haystacks(request, readable)? {
        if let Some(stop) = clock.stop() {
            if stop == SearchError::Cancelled {
                return Err(stop);
            }
            output.cut_short = true;
            break;
        }
        let shown = display_path(&request.cwd, &path);
        match request.mode {
            GrepMode::Content => {
                let mut sink = standard.sink_with_path(&matcher, &shown);
                // Unreadable files are skipped, as ripgrep reports them on stderr and goes on.
                let _ = searcher.search_path(&matcher, &path, &mut sink);
            }
            GrepMode::Count | GrepMode::FilesWithMatches => {
                let mut sink = summary.sink_with_path(&matcher, &shown);
                if searcher.search_path(&matcher, &path, &mut sink).is_err() {
                    continue;
                }
                if !sink.has_match() {
                    continue;
                }
                if request.mode == GrepMode::FilesWithMatches {
                    output.files.push(path);
                } else {
                    output.files_with_counts += 1;
                    output.total_matches +=
                        sink.stats().map_or(0, grep_printer::Stats::matched_lines);
                }
            }
        }
        let written = match request.mode {
            GrepMode::Content => standard.get_mut().get_ref().len(),
            _ => summary.get_mut().get_ref().len(),
        };
        if written > limits.max_output_bytes {
            output.cut_short = true;
            break;
        }
    }

    if request.mode != GrepMode::FilesWithMatches {
        let bytes = match request.mode {
            GrepMode::Content => standard.into_inner().into_inner(),
            _ => summary.into_inner().into_inner(),
        };
        let bytes = &bytes[..bytes.len().min(limits.max_output_bytes)];
        let text = String::from_utf8_lossy(bytes);
        output.lines = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if output.cut_short {
            output.lines.pop();
        }
    }
    if output.cut_short && output.lines.is_empty() && output.files.is_empty() {
        return Err(SearchError::TimedOut);
    }
    Ok(output)
}

#[cfg(test)]
#[path = "rg_search_tests.rs"]
mod tests;
