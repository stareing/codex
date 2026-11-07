use super::*;
use pretty_assertions::assert_eq;
use std::fs;
use std::fs::File;
use std::time::UNIX_EPOCH;

/// A small repository: ignored, hidden, binary, long-line, CRLF and nested files, with fixed modification times.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let files: [(&str, &[u8], u64); 12] = [
        (".git/HEAD", b"ref: needle\n", 1),
        (".gitignore", b"ignored.txt\nbuild/\n", 2),
        ("src/main.rs", b"fn main() {\n    let needle = 1;\n}\n", 30),
        (
            "src/lib.rs",
            b"// Needle here\npub fn f() {}\nfn needle() {}\n",
            20,
        ),
        ("ignored.txt", b"needle\n", 40),
        ("build/out.js", b"needle\n", 50),
        (".hidden/conf.txt", b"needle\n", 10),
        ("bin.dat", b"needle\0rest\n", 60),
        ("crlf.txt", b"a\r\nneedle\r\nb\r\n", 70),
        ("multi.txt", b"start\nmiddle\nend\n", 80),
        (
            "ctx.txt",
            b"one\ntwo\nneedle\nthree\nfour\nfive\nsix\nneedle\nseven\n",
            90,
        ),
        ("-dash.txt", b"--flag\n", 100),
    ];
    for (path, body, seconds) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        set_mtime(&path, seconds);
    }
    let long = root.join("long.txt");
    fs::write(&long, format!("{}needle\nshort needle\n", "x".repeat(600))).unwrap();
    set_mtime(&long, 110);
    dir
}

fn set_mtime(path: &Path, seconds: u64) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000 + seconds))
        .unwrap();
}

fn everything() -> Readable {
    Arc::new(|_: &Path| true)
}

fn names(cwd: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|path| display_path(cwd, path).to_string_lossy().replace('\\', "/"))
        .collect()
}

fn run_glob(root: &Path, dir: &Path, pattern: &str, limit: usize) -> GlobFiles {
    glob_files(
        root,
        dir,
        pattern,
        limit,
        &everything(),
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap()
}

fn request(root: &Path, pattern: &str, mode: GrepMode) -> GrepRequest {
    GrepRequest {
        cwd: root.to_path_buf(),
        target: root.to_path_buf(),
        pattern: pattern.to_string(),
        globs: Vec::new(),
        file_type: None,
        mode,
        before_context: 0,
        after_context: 0,
        line_numbers: true,
        case_insensitive: false,
        multiline: false,
    }
}

fn run_grep(request: &GrepRequest) -> GrepOutput {
    grep(
        request,
        &everything(),
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap()
}

#[test]
fn glob_lists_ignored_and_hidden_files_oldest_first() {
    let dir = fixture();
    let root = dir.path();
    let found = run_glob(root, root, "*.txt", 100);
    assert_eq!(
        names(root, &found.files),
        [
            ".hidden/conf.txt",
            "ignored.txt",
            "crlf.txt",
            "multi.txt",
            "ctx.txt",
            "-dash.txt",
            "long.txt"
        ]
    );
    assert!(!found.truncated);

    let found = run_glob(root, root, "src/**/*.rs", 100);
    assert_eq!(names(root, &found.files), ["src/lib.rs", "src/main.rs"]);
    // `.git` is not skipped by Glob (`--no-ignore --hidden`).
    assert_eq!(
        names(root, &run_glob(root, root, "**/HEAD", 100).files),
        [".git/HEAD"]
    );
}

#[test]
fn glob_limits_and_reports_truncation() {
    let dir = fixture();
    let root = dir.path();
    let found = run_glob(root, root, "*.txt", 2);
    assert_eq!(
        names(root, &found.files),
        [".hidden/conf.txt", "ignored.txt"]
    );
    assert!(found.truncated);
}

#[test]
fn glob_patterns_are_relative_to_the_working_directory() {
    let dir = fixture();
    let root = dir.path();
    // Like `rg --glob 'main.rs' src`: a name glob matches at any depth.
    assert_eq!(
        names(
            root,
            &run_glob(root, &root.join("src"), "main.rs", 100).files
        ),
        ["src/main.rs"]
    );
    // A glob with a `/` is anchored at the working directory, not at the searched folder.
    assert!(
        run_glob(root, &root.join("src"), "src/*.rs", 100)
            .files
            .len()
            == 2
    );
    assert!(
        run_glob(root, &root.join("src"), "*/main.rs", 100)
            .files
            .len()
            == 1
    );
}

#[test]
fn glob_drops_unreadable_paths_and_rejects_bad_globs() {
    let dir = fixture();
    let root = dir.path().to_path_buf();
    let hidden = root.join("src");
    let readable: Readable = Arc::new(move |path: &Path| !path.starts_with(&hidden));
    let found = glob_files(
        &root,
        &root,
        "**/*.rs",
        100,
        &readable,
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(found.files.is_empty());
    let err = glob_files(
        &root,
        &root,
        "a[",
        100,
        &everything(),
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(matches!(err, SearchError::InvalidGlob(_)), "{err:?}");
}

#[test]
fn glob_stops_at_the_deadline_and_when_cancelled() {
    let dir = fixture();
    let root = dir.path();
    let expired = Limits {
        deadline: Duration::ZERO,
        ..Limits::default()
    };
    let err = glob_files(
        root,
        root,
        "*",
        100,
        &everything(),
        expired,
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, SearchError::TimedOut);
    let err = glob_files(
        root,
        root,
        "*",
        100,
        &everything(),
        Limits::default(),
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert_eq!(err, SearchError::Cancelled);
}

#[test]
fn split_absolute_glob_like_claude_code() {
    assert_eq!(
        split_absolute_glob("/a/b/**/*.ts"),
        (PathBuf::from("/a/b"), "**/*.ts".to_string())
    );
    assert_eq!(
        split_absolute_glob("/*.txt"),
        (PathBuf::from("/"), "*.txt".to_string())
    );
    assert_eq!(
        split_absolute_glob("/a/b/c.txt"),
        (PathBuf::from("/a/b"), "c.txt".to_string())
    );
    assert_eq!(
        split_absolute_glob("*.rs"),
        (PathBuf::new(), "*.rs".to_string())
    );
}

#[test]
fn grep_files_with_matches_honors_gitignore_and_skips_vcs_and_binary() {
    let dir = fixture();
    let root = dir.path();
    let found = run_grep(&request(root, "needle", GrepMode::FilesWithMatches));
    let mut shown = names(root, &found.files);
    shown.sort();
    // Not: ignored.txt / build/ (.gitignore), .git/HEAD (VCS), bin.dat (binary), src/lib.rs (case).
    assert_eq!(
        shown,
        [
            ".hidden/conf.txt",
            "crlf.txt",
            "ctx.txt",
            "long.txt",
            "src/lib.rs",
            "src/main.rs"
        ]
    );
}

#[test]
fn grep_content_lines_are_ripgreps() {
    let dir = fixture();
    let root = dir.path();
    let mut req = request(root, "needle", GrepMode::Content);
    req.target = root.join("src");
    assert_eq!(
        run_grep(&req).lines,
        [
            "src/lib.rs:3:fn needle() {}",
            "src/main.rs:2:    let needle = 1;"
        ]
    );
    req.line_numbers = false;
    req.case_insensitive = true;
    assert_eq!(
        run_grep(&req).lines,
        [
            "src/lib.rs:// Needle here",
            "src/lib.rs:fn needle() {}",
            "src/main.rs:    let needle = 1;"
        ]
    );
}

#[test]
fn grep_long_lines_crlf_and_single_files() {
    let dir = fixture();
    let root = dir.path();
    let mut req = request(root, "needle", GrepMode::Content);
    req.target = root.join("long.txt");
    // A single named file: no path prefix; lines over 500 columns are omitted.
    assert_eq!(
        run_grep(&req).lines,
        ["1:[Omitted long matching line]", "2:short needle"]
    );
    req.target = root.join("crlf.txt");
    assert_eq!(run_grep(&req).lines, ["2:needle"]);
    req.target = root.join("bin.dat");
    // A named binary file: ripgrep only says that it matches.
    assert_eq!(
        run_grep(&req).lines,
        ["binary file matches (found \"\\0\" byte around offset 6)"]
    );
}

#[test]
fn grep_context_and_separators() {
    let dir = fixture();
    let root = dir.path();
    let mut req = request(root, "needle", GrepMode::Content);
    req.target = root.join("ctx.txt");
    req.before_context = 1;
    req.after_context = 1;
    assert_eq!(
        run_grep(&req).lines,
        [
            "2-two", "3:needle", "4-three", "--", "7-six", "8:needle", "9-seven"
        ]
    );
}

#[test]
fn grep_count_mode() {
    let dir = fixture();
    let root = dir.path();
    let mut req = request(root, "needle", GrepMode::Count);
    req.target = root.join("src");
    let found = run_grep(&req);
    assert_eq!(found.lines, ["src/lib.rs:1", "src/main.rs:1"]);
    assert_eq!((found.total_matches, found.files_with_counts), (2, 2));
    req.target = root.join("ctx.txt");
    let found = run_grep(&req);
    assert_eq!(found.lines, ["2"]);
    assert_eq!((found.total_matches, found.files_with_counts), (2, 1));
}

#[test]
fn grep_multiline_globs_types_and_errors() {
    let dir = fixture();
    let root = dir.path();
    let mut req = request(root, "start.*end", GrepMode::FilesWithMatches);
    assert!(run_grep(&req).files.is_empty(), "single-line by default");
    req.multiline = true;
    assert_eq!(names(root, &run_grep(&req).files), ["multi.txt"]);

    let mut req = request(root, "needle", GrepMode::FilesWithMatches);
    req.globs = vec!["*.rs".to_string()];
    let mut shown = names(root, &run_grep(&req).files);
    shown.sort();
    assert_eq!(shown, ["src/lib.rs", "src/main.rs"]);
    req.globs = Vec::new();
    req.file_type = Some("rust".to_string());
    assert_eq!(run_grep(&req).files.len(), 2);

    req.file_type = Some("nosuchtype".to_string());
    let err = grep(
        &req,
        &everything(),
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, SearchError::UnknownType("nosuchtype".to_string()));
    let req = request(root, "(", GrepMode::Content);
    let err = grep(
        &req,
        &everything(),
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(matches!(err, SearchError::InvalidPattern(_)), "{err:?}");
}

#[test]
fn grep_pattern_starting_with_a_dash_is_a_pattern() {
    let dir = fixture();
    let root = dir.path();
    let found = run_grep(&request(root, "--flag", GrepMode::FilesWithMatches));
    assert_eq!(names(root, &found.files), ["-dash.txt"]);
}

#[test]
fn grep_drops_unreadable_files_and_stops_at_the_deadline() {
    let dir = fixture();
    let root = dir.path().to_path_buf();
    let src = root.join("src");
    let readable: Readable = Arc::new(move |path: &Path| !path.starts_with(&src));
    let found = grep(
        &request(&root, "fn ", GrepMode::FilesWithMatches),
        &readable,
        Limits::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(found.files.is_empty());
    let expired = Limits {
        deadline: Duration::ZERO,
        ..Limits::default()
    };
    let err = grep(
        &request(&root, "needle", GrepMode::Content),
        &everything(),
        expired,
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, SearchError::TimedOut);
    let cut = Limits {
        max_output_bytes: 1,
        ..Limits::default()
    };
    let found = grep(
        &request(&root, "needle", GrepMode::Content),
        &everything(),
        cut,
        &AtomicBool::new(false),
    );
    // The first file's output passes the cap: the search stops and the partial line is dropped.
    assert!(matches!(
        found,
        Err(SearchError::TimedOut)
            | Ok(GrepOutput {
                cut_short: true,
                ..
            })
    ));
}

/// With `RG_BIN` set to a ripgrep binary, the engine's output must equal `rg`'s for the same arguments
/// (Claude Code's argument lists, `--sort path` so the order is fixed).
#[test]
fn matches_the_rg_binary_when_available() {
    let Ok(rg) = std::env::var("RG_BIN") else {
        return;
    };
    let dir = fixture();
    let root = dir.path();
    let prefix = format!("{}{}", root.display(), std::path::MAIN_SEPARATOR);
    let rg_lines = |args: &[&str], target: &Path| -> Vec<String> {
        let mut all = vec!["--sort", "path"];
        all.extend_from_slice(args);
        let output = std::process::Command::new(&rg)
            .args(&all)
            .arg(target)
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.strip_prefix(prefix.as_str())
                    .unwrap_or(line)
                    .to_string()
            })
            .collect()
    };
    let vcs: Vec<String> = VCS_DIRECTORIES
        .iter()
        .map(|dir| format!("!{dir}"))
        .collect();
    let base = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--hidden".to_string()];
        for glob in &vcs {
            args.push("--glob".to_string());
            args.push(glob.clone());
        }
        args.push("--max-columns".to_string());
        args.push("500".to_string());
        args.extend(extra.iter().map(ToString::to_string));
        args
    };
    struct Case {
        args: Vec<&'static str>,
        target: &'static str,
        edit: fn(&mut GrepRequest),
    }
    let cases = [
        Case {
            args: vec!["-n", "needle"],
            target: "",
            edit: |_| {},
        },
        Case {
            args: vec!["-i", "-n", "needle"],
            target: "",
            edit: |r| r.case_insensitive = true,
        },
        Case {
            args: vec!["needle"],
            target: "",
            edit: |r| r.line_numbers = false,
        },
        Case {
            args: vec!["-n", "-C", "1", "needle"],
            target: "ctx.txt",
            edit: |r| {
                r.before_context = 1;
                r.after_context = 1;
            },
        },
        Case {
            args: vec!["-n", "-B", "2", "needle"],
            target: "",
            edit: |r| r.before_context = 2,
        },
        Case {
            args: vec!["-n", "needle"],
            target: "long.txt",
            edit: |_| {},
        },
        Case {
            args: vec!["-n", "needle"],
            target: "bin.dat",
            edit: |_| {},
        },
        Case {
            args: vec!["-c", "needle"],
            target: "",
            edit: |r| r.mode = GrepMode::Count,
        },
        Case {
            args: vec!["-c", "needle"],
            target: "ctx.txt",
            edit: |r| r.mode = GrepMode::Count,
        },
        Case {
            args: vec!["-l", "needle"],
            target: "",
            edit: |r| r.mode = GrepMode::FilesWithMatches,
        },
        Case {
            args: vec!["-l", "-U", "--multiline-dotall", "start.*end"],
            target: "",
            edit: |r| {
                r.mode = GrepMode::FilesWithMatches;
                r.multiline = true;
                r.pattern = "start.*end".to_string();
            },
        },
        Case {
            args: vec!["-n", "needle", "--glob", "*.rs"],
            target: "",
            edit: |r| r.globs = vec!["*.rs".to_string()],
        },
        Case {
            args: vec!["-n", "needle", "--type", "rust"],
            target: "",
            edit: |r| r.file_type = Some("rust".to_string()),
        },
        Case {
            args: vec!["-n", "-e", "--flag"],
            target: "",
            edit: |r| r.pattern = "--flag".to_string(),
        },
        Case {
            args: vec!["-n", "-A", "1", "needle"],
            target: "",
            edit: |r| r.after_context = 1,
        },
        Case {
            args: vec![
                "-n",
                "needle",
                "--glob",
                "*.{rs,txt}",
                "--glob",
                "!src/main.rs",
            ],
            target: "",
            edit: |r| r.globs = vec!["*.{rs,txt}".to_string(), "!src/main.rs".to_string()],
        },
        Case {
            args: vec!["-i", "-l", "NEEDLE"],
            target: "src",
            edit: |r| {
                r.mode = GrepMode::FilesWithMatches;
                r.case_insensitive = true;
                r.pattern = "NEEDLE".to_string();
            },
        },
        Case {
            args: vec!["-n", "^needle$"],
            target: "",
            edit: |r| r.pattern = "^needle$".to_string(),
        },
        Case {
            args: vec!["-n", "needle"],
            target: "ignored.txt",
            edit: |_| {},
        },
    ];
    for case in cases {
        let target = if case.target.is_empty() {
            root.to_path_buf()
        } else {
            root.join(case.target)
        };
        let mut req = request(root, "needle", GrepMode::Content);
        req.target = target.clone();
        (case.edit)(&mut req);
        let ours = run_grep(&req);
        let theirs = rg_lines(
            &base(&case.args)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            &target,
        );
        let ours = if req.mode == GrepMode::FilesWithMatches {
            names(root, &ours.files)
        } else {
            ours.lines
        };
        assert_eq!(ours, theirs, "rg {:?} {}", case.args, case.target);
    }
    for pattern in ["*.txt", "src/**/*.rs", "**/HEAD", "*"] {
        let mut theirs = rg_lines(
            &["--files", "--glob", pattern, "--no-ignore", "--hidden"],
            root,
        );
        let mut ours = names(root, &run_glob(root, root, pattern, 1000).files);
        // `--sort path` here; the tool's own order is by modification time.
        theirs.sort();
        ours.sort();
        assert_eq!(ours, theirs, "rg --files --glob {pattern}");
    }
}
