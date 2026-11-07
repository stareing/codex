use super::*;
use pretty_assertions::assert_eq;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

#[test]
fn head_limit_defaults_to_250_and_zero_means_all() {
    let items: Vec<usize> = (0..300).collect();
    let (kept, limit) = head_limited(items.clone(), None, 0);
    assert_eq!((kept.len(), limit), (250, Some(250)));
    let (kept, limit) = head_limited(items.clone(), Some(0), 0);
    assert_eq!((kept.len(), limit), (300, None));
    let (kept, limit) = head_limited(items.clone(), Some(10), 295);
    assert_eq!(
        (kept, limit),
        (vec![295, 296, 297, 298, 299], None),
        "nothing cut: no limit reported"
    );
    let (kept, limit) = head_limited(items, Some(2), 5);
    assert_eq!((kept, limit), (vec![5, 6], Some(2)));
}

#[test]
fn limit_info_like_claude_code() {
    assert_eq!(limit_info(None, 0), "");
    assert_eq!(limit_info(Some(250), 0), "limit: 250");
    assert_eq!(limit_info(None, 5), "offset: 5");
    assert_eq!(limit_info(Some(2), 5), "limit: 2, offset: 5");
}

#[test]
fn glob_result_text() {
    assert_eq!(glob_result(&[], false), "No files found");
    assert_eq!(
        glob_result(&strings(&["a.rs", "b.rs"]), false),
        "a.rs\nb.rs"
    );
    assert_eq!(
        glob_result(&strings(&["a.rs"]), true),
        "a.rs\n(Results are truncated. Consider using a more specific path or pattern.)"
    );
}

#[test]
fn grep_result_texts() {
    assert_eq!(content_result(&[], None, 0), "No matches found");
    assert_eq!(
        content_result(&strings(&["a.rs:1:x"]), Some(250), 3),
        "a.rs:1:x\n\n[Showing results with pagination = limit: 250, offset: 3]"
    );
    assert_eq!(
        count_result(&strings(&["a.rs:2", "b/c.rs:1"]), None, 0),
        "a.rs:2\nb/c.rs:1\n\nFound 3 total occurrences across 2 files."
    );
    assert_eq!(
        count_result(&strings(&["1"]), Some(1), 0),
        "1\n\nFound 1 total occurrence across 1 file. with pagination = limit: 1"
    );
    assert_eq!(
        count_result(&[], None, 0),
        "No matches found\n\nFound 0 total occurrences across 0 files."
    );
    assert_eq!(files_result(&[], None, 0), "No files found");
    assert_eq!(
        files_result(&strings(&["a.rs"]), None, 0),
        "Found 1 file\na.rs"
    );
    assert_eq!(
        files_result(&strings(&["a.rs", "b.rs"]), Some(2), 0),
        "Found 2 files limit: 2\na.rs\nb.rs"
    );
}

#[test]
fn globs_split_like_claude_code() {
    assert_eq!(split_globs("*.js"), strings(&["*.js"]));
    assert_eq!(
        split_globs("*.js,*.ts  *.md"),
        strings(&["*.js", "*.ts", "*.md"])
    );
    assert_eq!(
        split_globs("*.{ts,tsx} !dist/**"),
        strings(&["*.{ts,tsx}", "!dist/**"])
    );
    assert_eq!(split_globs(" , "), Vec::<String>::new());
}

#[test]
fn paths_expand_like_claude_code() {
    let root = std::env::temp_dir();
    let cwd = root.join("project");
    let home = root.join("home");
    let scope = SearchScope {
        cwd: cwd.clone(),
        home: Some(home.clone()),
        readable: Arc::new(|_: &Path| true),
    };
    assert_eq!(scope.expand("  "), cwd);
    assert_eq!(scope.expand("src/../lib/./x"), cwd.join("lib").join("x"));
    assert_eq!(scope.expand("~"), home);
    assert_eq!(scope.expand("~/notes"), home.join("notes"));
    let elsewhere = root.join("elsewhere");
    assert_eq!(scope.expand(&elsewhere.to_string_lossy()), elsewhere);
    assert_eq!(
        scope.shown(&cwd.join("a").join("b.rs")),
        Path::new("a").join("b.rs").to_string_lossy()
    );
    assert_eq!(scope.shown(&elsewhere), elsewhere.to_string_lossy());
}

#[test]
fn missing_paths_suggest_the_one_under_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    let scope = SearchScope {
        cwd: cwd.clone(),
        home: None,
        readable: Arc::new(|_: &Path| true),
    };
    // `<parent>/src` was meant as `<parent>/project/src`.
    let requested = dir.path().join("src");
    assert_eq!(scope.suggestion(&requested), Some(cwd.join("src")));
    assert_eq!(scope.suggestion(&dir.path().join("nope")), None);
    let FunctionCallError::RespondToModel(message) = scope.missing("Path", "../src", &requested)
    else {
        panic!("not a model-facing error");
    };
    assert_eq!(
        message,
        format!(
            "Path does not exist: ../src. Note: your current working directory is {}. Did you mean {}?",
            cwd.display(),
            cwd.join("src").display()
        )
    );
}

#[test]
fn numbers_and_flags_accept_strings() {
    let map: Map<String, Value> = serde_json::from_str(
        r#"{"-A": "3", "-B": 2, "-n": "false", "-i": true, "head_limit": 1.5, "offset": -1, "-C": "x"}"#,
    )
    .unwrap();
    assert_eq!(count_arg(&map, "Grep", "-A").unwrap(), Some(3));
    assert_eq!(count_arg(&map, "Grep", "-B").unwrap(), Some(2));
    assert_eq!(count_arg(&map, "Grep", "context").unwrap(), None);
    assert!(count_arg(&map, "Grep", "head_limit").is_err());
    assert!(count_arg(&map, "Grep", "offset").is_err());
    assert!(count_arg(&map, "Grep", "-C").is_err());
    assert_eq!(flag_arg(&map, "Grep", "-n").unwrap(), Some(false));
    assert_eq!(flag_arg(&map, "Grep", "-i").unwrap(), Some(true));
    assert!(flag_arg(&map, "Grep", "-A").is_err());
}
