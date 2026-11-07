use super::*;
use pretty_assertions::assert_eq;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("project");
    let outside = base.join("other");
    let home = base.join("home");
    for path in [&root, &outside, &home] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::create_dir_all(root.join("src")).unwrap();
    Fixture {
        _dir: dir,
        root,
        outside,
        home,
    }
}

fn policy(fx: &Fixture) -> RemoteRoots {
    RemoteRoots::new(
        Some(vec![fx.root.clone()]),
        fx.root.clone(),
        fx.home.clone(),
    )
}

#[test]
fn unrestricted_without_roots() {
    let roots = RemoteRoots::new(None, PathBuf::from("/"), PathBuf::from("/"));
    assert!(!roots.is_restricted());
    assert_eq!(roots.check(Path::new("/etc")), Ok(()));
    assert!(!RemoteRoots::unrestricted().is_restricted());
}

#[test]
fn inside_and_outside() {
    let fx = fixture();
    let roots = policy(&fx);
    assert!(roots.is_restricted());
    assert_eq!(roots.check(&fx.root), Ok(()));
    assert_eq!(roots.check(&fx.root.join("src")), Ok(()));
    // Not created yet, but inside: fs/writeFile and fs/createDirectory need this.
    assert_eq!(roots.check(&fx.root.join("new/dir/file.txt")), Ok(()));
    let err = roots.check(&fx.outside).unwrap_err();
    assert!(err.contains("outside"), "{err}");
    assert!(roots.check(Path::new("/")).is_err());
    // A sibling whose name merely starts with the root's name is outside.
    let sibling = PathBuf::from(format!("{}-evil", fx.root.display()));
    std::fs::create_dir_all(&sibling).unwrap();
    assert!(roots.check(&sibling).is_err());
}

#[test]
fn relative_paths_and_omitted_cwd_resolve_against_the_server_cwd() {
    let fx = fixture();
    let roots = policy(&fx);
    assert_eq!(roots.check(Path::new("src")), Ok(()));
    assert_eq!(roots.check_or_base(None), Ok(()));
    assert!(roots.check(Path::new("../other")).is_err());

    // Server started outside the allowed folders: an omitted cwd is refused.
    let elsewhere = RemoteRoots::new(Some(vec![fx.root.clone()]), fx.outside.clone(), fx.home);
    assert!(elsewhere.check_or_base(None).is_err());
    assert!(elsewhere.check(Path::new("x")).is_err());
}

#[test]
fn dot_dot_cannot_escape() {
    let fx = fixture();
    let roots = policy(&fx);
    // Existing path with `..`: canonicalized to the parent's sibling.
    assert!(roots.check(&fx.root.join("src/../../other")).is_err());
    assert_eq!(roots.check(&fx.root.join("src/..")), Ok(()));
    // Missing tail with `..` is refused outright.
    assert!(roots.check(&fx.root.join("missing/../../other")).is_err());
    assert!(roots.check(&fx.root.join("missing/..")).is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_are_followed() {
    let fx = fixture();
    let roots = policy(&fx);
    let link = fx.root.join("escape");
    std::os::unix::fs::symlink(&fx.outside, &link).unwrap();
    assert!(roots.check(&link).is_err());
    assert!(roots.check(&link.join("file.txt")).is_err());
    // A link from outside into the root is fine: the real place is inside.
    let inbound = fx.outside.join("into-project");
    std::os::unix::fs::symlink(&fx.root, &inbound).unwrap();
    assert_eq!(roots.check(&inbound.join("src")), Ok(()));
    // A symlinked root is canonicalized too.
    let root_link = fx.outside.join("root-link");
    std::os::unix::fs::symlink(&fx.root, &root_link).unwrap();
    let via_link = RemoteRoots::new(Some(vec![root_link]), fx.root.clone(), fx.home.clone());
    assert_eq!(via_link.check(&fx.root.join("src")), Ok(()));
}

#[test]
fn set_but_unusable_roots_refuse_everything() {
    let fx = fixture();
    let none = RemoteRoots::new(Some(vec![]), fx.root.clone(), fx.home.clone());
    assert!(none.is_restricted());
    assert!(none.check(&fx.root).is_err());
    let missing = RemoteRoots::new(
        Some(vec![fx.root.join("does-not-exist")]),
        fx.root.clone(),
        fx.home.clone(),
    );
    assert!(missing.check(&fx.root).is_err());
}

#[test]
fn rollouts_may_come_from_the_codex_home() {
    let fx = fixture();
    let roots = policy(&fx);
    assert_eq!(
        roots.check_rollout(&fx.home.join("sessions/x.jsonl")),
        Ok(())
    );
    assert_eq!(roots.check_rollout(&fx.root.join("x.jsonl")), Ok(()));
    assert!(roots.check_rollout(&fx.outside.join("x.jsonl")).is_err());
}

#[test]
fn explicit_option_wins_over_the_environment() {
    let fx = fixture();
    // Explicit option wins regardless of the environment.
    let roots = RemoteRoots::resolve(
        Some(vec![fx.root.clone()]),
        fx.root.clone(),
        fx.home.clone(),
    );
    assert!(roots.check(&fx.outside).is_err());
}

fn scope_of(method: &str, params: serde_json::Value) -> RequestScope {
    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "method": method, "id": 1, "params": params,
    }))
    .unwrap_or_else(|err| panic!("{method}: {err}"));
    request_scope(&request)
}

#[test]
fn request_scopes_name_every_folder_the_request_uses() {
    let thread_id = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    assert_eq!(
        scope_of("thread/start", serde_json::json!({})),
        RequestScope {
            server_cwd: true,
            ..RequestScope::default()
        }
    );
    assert_eq!(
        scope_of(
            "thread/start",
            serde_json::json!({"cwd": "/w", "runtimeWorkspaceRoots": ["/r"]})
        )
        .paths,
        vec![PathBuf::from("/w"), PathBuf::from("/r")]
    );
    let resume = scope_of(
        "thread/resume",
        serde_json::json!({"threadId": thread_id, "path": "/h/rollout.jsonl"}),
    );
    assert_eq!(resume.thread_cwd.as_deref(), Some(thread_id));
    assert_eq!(resume.rollouts, vec![PathBuf::from("/h/rollout.jsonl")]);
    assert_eq!(
        scope_of(
            "turn/start",
            serde_json::json!({"threadId": thread_id, "input": []})
        )
        .thread_cwd
        .as_deref(),
        Some(thread_id)
    );
    let turn = scope_of(
        "turn/start",
        serde_json::json!({
            "threadId": thread_id, "input": [], "cwd": "/c",
            "environments": [{"environmentId": "local", "cwd": "/e", "runtimeWorkspaceRoots": ["/er"]}],
            "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": ["/wr"]},
        }),
    );
    assert_eq!(turn.thread_cwd, None);
    assert_eq!(
        turn.paths,
        ["/c", "/e", "/er", "/wr"].map(PathBuf::from).to_vec()
    );
    assert_eq!(
        scope_of(
            "thread/shellCommand",
            serde_json::json!({"threadId": thread_id, "command": "ls"})
        )
        .thread_cwd
        .as_deref(),
        Some(thread_id)
    );
    assert!(scope_of("command/exec", serde_json::json!({"command": ["ls"]})).server_cwd);
    assert_eq!(
        scope_of(
            "command/exec",
            serde_json::json!({"command": ["ls"], "cwd": "sub"})
        )
        .paths,
        vec![PathBuf::from("sub")]
    );
    assert_eq!(
        scope_of(
            "fs/copy",
            serde_json::json!({"sourcePath": "/a", "destinationPath": "/b"})
        )
        .paths,
        vec![PathBuf::from("/a"), PathBuf::from("/b")]
    );
    assert_eq!(
        scope_of("fs/readFile", serde_json::json!({"path": "/a"})).paths,
        vec![PathBuf::from("/a")]
    );
    // Searches, listings and plugin files name folders too; none listed means the server cwd.
    assert_eq!(
        scope_of(
            "fuzzyFileSearch",
            serde_json::json!({"query": "a", "roots": ["/w", "/x"]})
        )
        .paths,
        vec![PathBuf::from("/w"), PathBuf::from("/x")]
    );
    assert!(
        scope_of(
            "fuzzyFileSearch",
            serde_json::json!({"query": "a", "roots": []})
        )
        .server_cwd
    );
    assert_eq!(
        scope_of(
            "fs/searchText",
            serde_json::json!({"root": "/srv", "query": "a"})
        )
        .paths,
        vec![PathBuf::from("/srv")]
    );
    assert_eq!(
        scope_of(
            "fuzzyFileSearch/sessionStart",
            serde_json::json!({"sessionId": "s", "roots": ["/w"]})
        )
        .paths,
        vec![PathBuf::from("/w")]
    );
    assert!(scope_of("skills/list", serde_json::json!({})).server_cwd);
    assert_eq!(
        scope_of("hooks/list", serde_json::json!({"cwds": ["/h"]})).paths,
        vec![PathBuf::from("/h")]
    );
    assert_eq!(
        scope_of("plugin/list", serde_json::json!({"cwds": ["/p"]})).paths,
        vec![PathBuf::from("/p")]
    );
    assert_eq!(
        scope_of(
            "plugin/read",
            serde_json::json!({"marketplacePath": "/m/marketplace.json", "pluginName": "x"})
        )
        .paths,
        vec![PathBuf::from("/m/marketplace.json")]
    );
    assert_eq!(
        scope_of(
            "plugin/share/save",
            serde_json::json!({"pluginPath": "/pp"})
        )
        .paths,
        vec![PathBuf::from("/pp")]
    );
    assert_eq!(
        scope_of("config/read", serde_json::json!({"cwd": "/cfg"})).paths,
        vec![PathBuf::from("/cfg")]
    );
    assert_eq!(
        scope_of(
            "feedback/upload",
            serde_json::json!({"classification": "bug", "includeLogs": true, "extraLogFiles": ["/etc/shadow"]})
        )
        .paths,
        vec![PathBuf::from("/etc/shadow")]
    );
    assert_eq!(
        scope_of("gitDiffToRemote", serde_json::json!({"cwd": "/g"})).paths,
        vec![PathBuf::from("/g")]
    );
    assert_eq!(
        scope_of(
            "getConversationSummary",
            serde_json::json!({"rolloutPath": "/r.jsonl"})
        )
        .rollouts,
        vec![PathBuf::from("/r.jsonl")]
    );
    // A command's extra writable folders, and the files a turn's input reads.
    assert_eq!(
        scope_of(
            "command/exec",
            serde_json::json!({"command": ["ls"], "cwd": "/c",
                "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": ["/wr"]}})
        )
        .paths,
        vec![PathBuf::from("/c"), PathBuf::from("/wr")]
    );
    let steer = scope_of(
        "turn/steer",
        serde_json::json!({"threadId": thread_id, "expectedTurnId": "t", "input": [
            {"type": "localImage", "path": "/i.png"},
            {"type": "localAudio", "path": "/a.wav"},
            {"type": "skill", "name": "s", "path": "/s/SKILL.md"},
            {"type": "mention", "name": "m", "path": "app://m"},
            {"type": "text", "text": "hi"},
        ]}),
    );
    assert_eq!(steer.thread_cwd.as_deref(), Some(thread_id));
    assert_eq!(
        steer.paths,
        ["/i.png", "/a.wav"].map(PathBuf::from).to_vec()
    );
    assert_eq!(steer.rollouts, vec![PathBuf::from("/s/SKILL.md")]);
    assert_eq!(
        scope_of(
            "thread/start",
            serde_json::json!({"environments": [{"environmentId": "local", "cwd": "/e"}]})
        )
        .paths,
        vec![PathBuf::from("/e")]
    );
    // What the allow-list cannot judge is refused.
    for (method, params) in [
        ("marketplace/add", serde_json::json!({"source": "~/market"})),
        (
            "marketplace/add",
            serde_json::json!({"source": "../market"}),
        ),
        ("marketplace/add", serde_json::json!({"source": "/market"})),
        (
            "externalAgentConfig/detect",
            serde_json::json!({"includeHome": true}),
        ),
        (
            "externalAgentConfig/import",
            serde_json::json!({"migrationItems": []}),
        ),
    ] {
        assert!(
            scope_of(method, params.clone()).refused.is_some(),
            "{method} {params}"
        );
    }
    assert_eq!(
        scope_of(
            "marketplace/add",
            serde_json::json!({"source": "owner/repo"})
        ),
        RequestScope::default()
    );
    // Requests that name no folder are not checked.
    assert_eq!(
        scope_of("thread/list", serde_json::json!({})),
        RequestScope::default()
    );
}
