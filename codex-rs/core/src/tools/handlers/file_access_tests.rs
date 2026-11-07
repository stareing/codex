use super::*;
use pretty_assertions::assert_eq;

fn root() -> PathBuf {
    std::env::temp_dir().join("file-access-rules")
}

fn facts() -> Facts {
    let root = root();
    let denied = root.join("project").join("secrets");
    let writable = root.join("project").join("out");
    Facts {
        working_dirs: vec![root.join("project"), root.join("granted")],
        agent_dirs: vec![root.join("codex-home").join("skills")],
        read_denied: Box::new(move |path: &Path| path.starts_with(&denied)),
        writable: Box::new(move |path: &Path| path.starts_with(&writable)),
        windows_paths: false,
    }
}

fn probe(raw: &str) -> Probe<'_> {
    let path = root().join(raw);
    Probe {
        raw,
        resolved: path.clone(),
        path,
    }
}

fn read(raw: &str) -> Verdict {
    decide(Access::Read, &facts(), &probe(raw))
}

fn write(raw: &str) -> Verdict {
    decide(Access::Write, &facts(), &probe(raw))
}

#[test]
fn reads_follow_the_ladder() {
    assert_eq!(
        read("project/src/main.rs"),
        Verdict::Allow,
        "working directory"
    );
    assert_eq!(read("granted/notes.md"), Verdict::Allow, "granted folder");
    assert_eq!(
        read("codex-home/skills/x/SKILL.md"),
        Verdict::Allow,
        "agent folder"
    );
    let elsewhere = root().join("elsewhere/a.txt");
    assert_eq!(
        read("elsewhere/a.txt"),
        Verdict::Ask(format!(
            "The agent requested permissions to read from {}, but you haven't granted it yet.",
            elsewhere.display()
        ))
    );
    let secret = root().join("project/secrets/key");
    assert_eq!(
        read("project/secrets/key"),
        Verdict::Deny(format!(
            "Permission to read {} has been denied.",
            secret.display()
        )),
        "deny beats the working directory"
    );
    assert!(matches!(read("//host/share/a"), Verdict::Ask(q) if q.contains("UNC path")));
    assert!(matches!(read("project/GIT~1/config"), Verdict::Ask(q) if q.contains("suspicious")));
}

#[test]
fn writable_paths_are_readable_and_case_does_not_matter() {
    let facts = Facts {
        working_dirs: vec![],
        ..facts()
    };
    assert_eq!(
        decide(Access::Read, &facts, &probe("project/out/x")),
        Verdict::Allow
    );
    assert!(matches!(
        decide(Access::Read, &facts, &probe("project/src/x")),
        Verdict::Ask(_)
    ));
    assert_eq!(read("PROJECT/Src/main.rs"), Verdict::Allow);
}

#[test]
fn a_symlink_out_of_the_working_directory_asks() {
    let path = root().join("project/link/file");
    let probe = Probe {
        raw: "project/link/file",
        path,
        resolved: root().join("elsewhere/file"),
    };
    assert!(matches!(
        decide(Access::Read, &facts(), &probe),
        Verdict::Ask(_)
    ));
}

#[test]
fn writes_ask_only_for_sensitive_and_odd_paths() {
    assert_eq!(write("project/src/main.rs"), Verdict::Allow);
    assert_eq!(
        write("elsewhere/a.txt"),
        Verdict::Allow,
        "left to apply_patch"
    );
    for sensitive in [
        "project/.git/config",
        "project/.VSCode/settings.json",
        "project/.idea/x.xml",
        "project/.codex/config.toml",
        "project/.agents/skills/a/SKILL.md",
        "home/.bashrc",
        "project/.gitmodules",
        "project/.mcp.json",
    ] {
        assert!(
            matches!(write(sensitive), Verdict::Ask(q) if q.ends_with("which is a sensitive file.")),
            "{sensitive}"
        );
    }
    assert_eq!(
        write("project/.claude/worktrees/a/src/x.rs"),
        Verdict::Allow
    );
    assert!(matches!(
        write("project/.claude/worktrees/a/.claude/x"),
        Verdict::Ask(_)
    ));
    assert_eq!(
        write("codex-home/.codex/worktrees/1a2b/src/x.rs"),
        Verdict::Allow
    );
    assert!(matches!(write("\\\\host\\share\\a"), Verdict::Ask(q) if q.contains("write to")));
    assert!(matches!(write("project/a.txt."), Verdict::Ask(q) if q.contains("suspicious")));
}

#[test]
fn suspicious_windows_patterns() {
    for raw in [
        "GIT~1/config",
        "\\\\?\\C:\\x",
        "//./C:/x",
        ".git.",
        "a.txt ",
        "settings.json.PRN",
        "x/.../y",
        "...",
    ] {
        assert!(is_suspicious_windows_path(raw, false), "{raw}");
    }
    for raw in [
        "src/[...slug]/page.tsx",
        "a/b.txt",
        "C:\\x\\y",
        "~/notes",
        "file.txt:stream",
    ] {
        assert!(!is_suspicious_windows_path(raw, false), "{raw}");
    }
    assert!(is_suspicious_windows_path("file.txt:stream", true));
    assert!(
        !is_suspicious_windows_path("C:\\x", true),
        "a drive colon is not a stream"
    );
}

#[test]
fn within_compares_whole_components() {
    assert!(within(Path::new("/a/b/c"), Path::new("/a/b")));
    assert!(within(Path::new("/a/b"), Path::new("/a/b")));
    assert!(!within(Path::new("/a/bc"), Path::new("/a/b")));
    assert!(!within(Path::new("/a"), Path::new("/a/b")));
}

#[test]
fn resolved_follows_the_existing_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    let missing = dir.path().join("new").join("file.txt");
    assert_eq!(resolved(&missing), real.join("new").join("file.txt"));
}

#[test]
fn granted_entries_become_working_dirs() {
    let folder = root().join("granted");
    let uri = PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&folder).unwrap());
    let profile = AdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry::new(
                FileSystemPath::Path { path: uri },
                FileSystemAccessMode::Read,
            )],
            glob_scan_max_depth: None,
        }),
    };
    assert_eq!(granted_dirs(Some(&profile), Access::Read), vec![folder]);
    assert_eq!(
        granted_dirs(Some(&profile), Access::Write),
        Vec::<PathBuf>::new()
    );
    assert_eq!(granted_dirs(None, Access::Read), Vec::<PathBuf>::new());
}
