use super::*;
use pretty_assertions::assert_eq;

fn folder() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    // TODO: Hello\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.md"), "hello world\nnothing\n").unwrap();
    std::fs::write(root.join("blob.bin"), b"hello\0binary").unwrap();
    dir
}

fn run(root: &Path, query: &str, case_sensitive: bool, max_results: usize) -> TextSearchResults {
    search_text(
        root,
        query,
        case_sensitive,
        TextSearchLimits {
            max_results,
            deadline: DEADLINE,
        },
        &AtomicBool::new(false),
    )
}

#[test]
fn finds_every_line_case_insensitively_in_path_order_and_skips_binary_files() {
    let dir = folder();
    let found = run(dir.path(), "HELLO", false, 10);
    assert_eq!(
        found,
        TextSearchResults {
            matches: vec![
                TextMatch {
                    path: PathBuf::from("notes.md"),
                    line_number: 1,
                    line: "hello world".to_string(),
                },
                TextMatch {
                    path: PathBuf::from("src/main.rs"),
                    line_number: 2,
                    line: "// TODO: Hello".to_string(),
                },
            ],
            truncated: false,
        }
    );
}

#[test]
fn case_sensitive_search_only_finds_the_exact_case() {
    let dir = folder();
    let found = run(dir.path(), "Hello", true, 10);
    assert_eq!(
        found
            .matches
            .iter()
            .map(|m| m.path.clone())
            .collect::<Vec<_>>(),
        vec![PathBuf::from("src/main.rs")]
    );
}

#[test]
fn stops_at_the_result_limit_and_says_so() {
    let dir = folder();
    let found = run(dir.path(), "o", false, 2);
    assert_eq!(found.matches.len(), 2);
    assert!(found.truncated);
}

#[test]
fn skips_large_files_cuts_long_lines_and_ignores_an_empty_query() {
    let dir = folder();
    let big = "needle\n".repeat((MAX_FILE_BYTES as usize) / 7 + 1);
    std::fs::write(dir.path().join("big.txt"), big).unwrap();
    std::fs::write(
        dir.path().join("long.txt"),
        format!("needle{}", "x".repeat(1000)),
    )
    .unwrap();
    let found = run(dir.path(), "needle", false, 10);
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches[0].path, PathBuf::from("long.txt"));
    assert_eq!(found.matches[0].line.chars().count(), MAX_LINE_CHARS);
    assert_eq!(run(dir.path(), "", false, 10), TextSearchResults::default());
}

#[test]
fn cancelled_or_past_the_deadline_stops_early() {
    let dir = folder();
    let cancelled = AtomicBool::new(true);
    let found = search_text(
        dir.path(),
        "hello",
        false,
        TextSearchLimits::default(),
        &cancelled,
    );
    assert!(found.truncated);
    assert!(found.matches.is_empty());
    let late = search_text(
        dir.path(),
        "hello",
        false,
        TextSearchLimits {
            max_results: 10,
            deadline: Duration::ZERO,
        },
        &AtomicBool::new(false),
    );
    assert!(late.truncated);
}

#[cfg(unix)]
#[test]
fn does_not_follow_a_symlink_out_of_the_folder() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "hello secret").unwrap();
    let dir = folder();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("secret-link.txt"),
    )
    .unwrap();
    let found = run(dir.path(), "secret", false, 10);
    assert_eq!(found.matches, vec![]);
}
