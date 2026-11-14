use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn plugin(dir: &Path, name: &str) -> PluginDir {
    PluginDir {
        name: name.to_string(),
        root: dir.join(name),
        data_root: dir.join("data").join(name),
    }
}

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn manifest(plugin: &PluginDir, lsp_servers: Value) {
    write(
        &plugin.root.join(".claude-plugin/plugin.json"),
        &json!({"name": plugin.name, "version": "1.0.0", "lspServers": lsp_servers}),
    );
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn names(servers: &PluginServers) -> Vec<&str> {
    servers.specs.iter().map(|s| s.name.as_str()).collect()
}

#[test]
fn an_official_style_plugin_loads_as_claude_code_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "rust-analyzer-lsp");
    // The shape of the official `rust-analyzer-lsp` plugin's manifest.
    manifest(
        &p,
        json!({"rust-analyzer": {"command": "rust-analyzer", "extensionToLanguage": {".rs": "rust"}}}),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert_eq!(out.errors, Vec::<String>::new());
    let spec = &out.specs[0];
    assert_eq!(spec.name, "plugin:rust-analyzer-lsp:rust-analyzer");
    assert_eq!(spec.command, "rust-analyzer");
    assert_eq!(
        spec.extensions,
        BTreeMap::from([(".rs".to_string(), "rust".to_string())])
    );
    assert_eq!(spec.priority, PLUGIN_PRIORITY);
    assert!(
        spec.priority < codex_lsp::config::DEFAULT_PRIORITY,
        "config entries beat plugins"
    );
    assert!(spec.optional);
    assert_eq!(spec.source.as_deref(), Some("plugin rust-analyzer-lsp"));
    assert_eq!(spec.env["CLAUDE_PLUGIN_ROOT"], folder(&p, false));
    assert_eq!(spec.env["CLAUDE_PLUGIN_DATA"], folder(&p, true));
}

#[test]
fn every_declaration_form_is_read_and_later_ones_win() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "multi");
    let server =
        |command: &str, ext: &str| json!({"command": command, "extensionToLanguage": {ext: "x"}});
    write(
        &p.root.join(".lsp.json"),
        &json!({"a": server("from-lsp-json", ".a"), "b": server("b", ".b")}),
    );
    write(
        &p.root.join("config/more.json"),
        &json!({"c": server("c", ".c")}),
    );
    manifest(
        &p,
        json!(["./config/more.json", {"a": server("from-manifest", ".a")}]),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert_eq!(out.errors, Vec::<String>::new());
    assert_eq!(
        names(&out),
        ["plugin:multi:a", "plugin:multi:b", "plugin:multi:c"]
    );
    assert_eq!(out.specs[0].command, "from-manifest");

    // A single path (not in an array) works too.
    let q = plugin(dir.path(), "single");
    write(&q.root.join("lsp.json"), &json!({"d": server("d", ".d")}));
    manifest(&q, json!("lsp.json"));
    assert_eq!(
        names(&load(std::slice::from_ref(&q), &no_env)),
        ["plugin:single:d"]
    );
}

#[test]
fn paths_outside_the_plugin_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "escape");
    write(
        &dir.path().join("outside.json"),
        &json!({"x": {"command": "x", "extensionToLanguage": {".x": "x"}}}),
    );
    let absolute = dir
        .path()
        .join("outside.json")
        .to_string_lossy()
        .into_owned();
    manifest(
        &p,
        json!(["../outside.json", "a/../../outside.json", absolute, "."]),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert!(out.specs.is_empty());
    assert_eq!(out.errors.len(), 4, "{:?}", out.errors);
    assert!(
        out.errors
            .iter()
            .all(|e| e.contains("inside the plugin folder"))
    );
    // `a/../b.json` stays inside.
    assert_eq!(
        within(Path::new("/p"), "a/../b.json"),
        Some(PathBuf::from("/p/b.json"))
    );
}

#[test]
fn claude_codes_schema_rules_reject_each_bad_server_and_keep_the_rest() {
    let ok = json!({"command": "ok", "extensionToLanguage": {".ok": "ok"}});
    let cases = [
        (
            json!({"command": "", "extensionToLanguage": {".x": "x"}}),
            "empty",
        ),
        (
            json!({"command": "my server", "extensionToLanguage": {".x": "x"}}),
            "spaces",
        ),
        (
            json!({"command": "x", "args": [""], "extensionToLanguage": {".x": "x"}}),
            "empty strings",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {}}),
            "at least one",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {"x": "x"}}),
            "dot",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".": "x"}}),
            "dot",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": ""}}),
            "must not be empty",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "startupTimeout": 0}),
            "positive",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "startupTimeout": -5}),
            "invalid",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "maxRestarts": -1}),
            "invalid",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "restartOnCrash": false}),
            "restartOnCrash",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "shutdownTimeout": 5}),
            "shutdownTimeout",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "transport": "pipe"}),
            "unknown variant",
        ),
        (
            json!({"command": "x", "extensionToLanguage": {".x": "x"}, "extra": 1}),
            "unknown field",
        ),
        (json!({"extensionToLanguage": {".x": "x"}}), "missing field"),
    ];
    for (bad, why) in cases {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path(), "p");
        manifest(&p, json!({"bad": bad, "good": ok}));
        let out = load(std::slice::from_ref(&p), &no_env);
        assert_eq!(names(&out), ["plugin:p:good"], "{why}");
        assert_eq!(out.errors.len(), 1, "{why}: {:?}", out.errors);
        assert!(
            out.errors[0].contains("`bad`") && out.errors[0].contains(why),
            "{why}: {}",
            out.errors[0]
        );
    }
    // An absolute command may contain spaces; socket is accepted (and, as in Claude Code, ignored).
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "p");
    manifest(
        &p,
        json!({"s": {"command": "/opt/My Tools/ls", "transport": "socket", "extensionToLanguage": {".X": "x"}}}),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert_eq!(out.errors, Vec::<String>::new());
    assert_eq!(
        out.specs[0].extensions.keys().collect::<Vec<_>>(),
        [".x"],
        "lower-cased like Claude Code"
    );
}

#[test]
fn variables_are_replaced_in_command_args_env_and_workspace_folder() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "vars");
    let root = folder(&p, false);
    let data = folder(&p, true);
    let workspace = dir.path().join("ws").to_string_lossy().into_owned();
    manifest(
        &p,
        json!({"v": {
            "command": "${CLAUDE_PLUGIN_ROOT}/bin/server",
            "args": ["--data=${CLAUDE_PLUGIN_DATA}", "--home=${HOME_X}", "--mode=${MODE:-fast}", "${NOPE}", "${PLUGIN_ROOT}", "${open"],
            "env": {"CACHE": "${PLUGIN_DATA}/cache", "CLAUDE_PLUGIN_ROOT": "${KEEP}"},
            "workspaceFolder": "${WS}",
            "extensionToLanguage": {".v": "v"},
            "startupTimeout": 1500,
            "maxRestarts": 0,
            "initializationOptions": {"a": 1},
            "settings": {"v": {"b": true}}
        }}),
    );
    let env = |name: &str| match name {
        "HOME_X" => Some("/home/x".to_string()),
        "WS" => Some(workspace.clone()),
        _ => None,
    };
    let out = load(std::slice::from_ref(&p), &env);
    let spec = &out.specs[0];
    assert_eq!(spec.command, format!("{root}/bin/server"));
    assert_eq!(
        spec.args,
        [
            format!("--data={data}"),
            "--home=/home/x".into(),
            "--mode=fast".into(),
            "${NOPE}".into(),
            root.clone(),
            "${open".into()
        ]
    );
    assert_eq!(spec.env["CACHE"], format!("{data}/cache"));
    assert_eq!(
        spec.env["CLAUDE_PLUGIN_ROOT"], "${KEEP}",
        "a plugin's own value for a reserved name is kept"
    );
    assert_eq!(spec.env["PLUGIN_ROOT"], root);
    assert_eq!(spec.workspace_folder, Some(PathBuf::from(&workspace)));
    assert_eq!(spec.startup_timeout, Some(Duration::from_millis(1500)));
    assert_eq!(spec.max_restarts, Some(0));
    assert_eq!(spec.initialization_options, Some(json!({"a": 1})));
    assert_eq!(spec.settings, Some(json!({"v": {"b": true}})));
    assert_eq!(out.errors.len(), 1);
    assert!(out.errors[0].contains("NOPE"), "{:?}", out.errors);
}

#[test]
fn a_relative_workspace_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "p");
    manifest(
        &p,
        json!({"w": {"command": "w", "workspaceFolder": "src", "extensionToLanguage": {".w": "w"}}}),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert!(out.specs.is_empty());
    assert!(out.errors[0].contains("absolute"), "{:?}", out.errors);
}

#[test]
fn unreadable_files_are_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let p = plugin(dir.path(), "p");
    std::fs::create_dir_all(&p.root).unwrap();
    std::fs::write(p.root.join(".lsp.json"), "{ not json").unwrap();
    std::fs::write(
        p.root.join("big.json"),
        vec![b' '; MAX_CONFIG_BYTES as usize + 1],
    )
    .unwrap();
    manifest(
        &p,
        json!(["missing.json", "big.json", 7, {"ok": {"command": "ok", "extensionToLanguage": {".ok": "ok"}}}]),
    );
    let out = load(std::slice::from_ref(&p), &no_env);
    assert_eq!(names(&out), ["plugin:p:ok"]);
    assert_eq!(out.errors.len(), 4, "{:?}", out.errors);
    assert!(out.errors[0].contains("invalid JSON"));
    assert!(out.errors[2].contains("larger than"));
    // A plugin without a manifest or `.lsp.json` declares nothing, silently.
    let empty = plugin(dir.path(), "empty");
    std::fs::create_dir_all(&empty.root).unwrap();
    let out = load(std::slice::from_ref(&empty), &no_env);
    assert!(out.specs.is_empty() && out.errors.is_empty());
}
