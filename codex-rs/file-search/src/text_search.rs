//! Text search under one folder (`fs/searchText`, the site's "Find in Files"): every line that
//! contains the query, as a literal, in the folder's files. Walks like the name search (hidden
//! entries included, `.gitignore` honored inside a git repository) but does not follow symlinks,
//! so a link cannot lead the search outside the folder it was given. Bounded: files over
//! [`MAX_FILE_BYTES`] and files with a NUL byte in their first [`BINARY_SNIFF_BYTES`] are skipped,
//! each line is cut to [`MAX_LINE_CHARS`], and the search stops at `max_results` matches, at the
//! deadline or when cancelled — `truncated` says it stopped early.

use ignore::WalkBuilder;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;
pub const MAX_LINE_CHARS: usize = 300;
pub const DEFAULT_MAX_RESULTS: usize = 200;
pub const MAX_RESULTS: usize = 1000;
pub const MAX_QUERY_CHARS: usize = 200;
pub const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    /// Relative to the searched folder.
    pub path: PathBuf,
    /// 1-based.
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TextSearchResults {
    pub matches: Vec<TextMatch>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct TextSearchLimits {
    pub max_results: usize,
    pub deadline: Duration,
}

impl Default for TextSearchLimits {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            deadline: DEADLINE,
        }
    }
}

/// Blocking: run it off the async runtime. An empty query finds nothing.
pub fn search_text(
    root: &Path,
    query: &str,
    case_sensitive: bool,
    limits: TextSearchLimits,
    cancelled: &AtomicBool,
) -> TextSearchResults {
    let mut results = TextSearchResults::default();
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    if query.is_empty() {
        return results;
    }
    let needle = if case_sensitive {
        query
    } else {
        query.to_lowercase()
    };
    let max_results = limits.max_results.clamp(1, MAX_RESULTS);
    let started = Instant::now();

    let mut walk = WalkBuilder::new(root);
    walk.hidden(false).follow_links(false).require_git(true);
    // Sorted so the same folder gives the same matches in the same order.
    walk.sort_by_file_path(Path::cmp);
    for entry in walk.build() {
        if cancelled.load(Ordering::Relaxed) || started.elapsed() > limits.deadline {
            results.truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if entry
            .metadata()
            .map_or(true, |metadata| metadata.len() > MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // Grew past the limit after the metadata was read.
        if bytes.len() as u64 > MAX_FILE_BYTES
            || bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
        {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            let found = if case_sensitive {
                line.contains(needle.as_str())
            } else {
                line.to_lowercase().contains(needle.as_str())
            };
            if !found {
                continue;
            }
            if results.matches.len() == max_results {
                results.truncated = true;
                return results;
            }
            results.matches.push(TextMatch {
                path: relative.clone(),
                line_number: index + 1,
                line: line.trim().chars().take(MAX_LINE_CHARS).collect(),
            });
        }
    }
    results
}

#[cfg(test)]
#[path = "text_search_tests.rs"]
mod tests;
