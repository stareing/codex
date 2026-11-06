use super::*;
use crate::catalog::ledger::Ledger;
use crate::catalog::test_support::ScriptedPrompter;
use codex_config::types::McpServerTransportConfig;
use pretty_assertions::assert_eq;

fn stdio_mcp() -> McpSpec {
    serde_json::from_value(serde_json::json!({
        "transport": "stdio", "command": "npx", "args": ["-y", "pkg@1.2.0"],
        "env": {"FOO": "bar"}, "secret_env": ["API_KEY"],
    }))
    .unwrap()
}

fn http_mcp() -> McpSpec {
    serde_json::from_value(serde_json::json!({
        "transport": "http", "url": "https://mcp.example.com/mcp",
        "secret_headers": {"X-Api-Key": "REMOTE_KEY"},
    }))
    .unwrap()
}

fn given(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn stdio_entry_has_command_args_env_and_secrets() {
    let config = mcp_server_config(&stdio_mcp(), &given(&[("API_KEY", "s3cret")]), true).unwrap();
    assert!(config.enabled);
    match config.transport {
        McpServerTransportConfig::Stdio {
            command, args, env, ..
        } => {
            assert_eq!(command, "npx");
            assert_eq!(args, vec!["-y", "pkg@1.2.0"]);
            let env = env.unwrap();
            assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
            assert_eq!(env.get("API_KEY").map(String::as_str), Some("s3cret"));
        }
        other => panic!("expected stdio, got {other:?}"),
    }
}

#[test]
fn http_entry_maps_secret_headers_to_environment_variables() {
    let config = mcp_server_config(&http_mcp(), &BTreeMap::new(), false).unwrap();
    assert!(!config.enabled);
    match config.transport {
        McpServerTransportConfig::StreamableHttp {
            url,
            env_http_headers,
            http_headers,
            bearer_token_env_var,
            ..
        } => {
            assert_eq!(url, "https://mcp.example.com/mcp");
            let headers = env_http_headers.unwrap();
            assert_eq!(
                headers.get("X-Api-Key").map(String::as_str),
                Some("REMOTE_KEY")
            );
            assert_eq!(http_headers, None);
            assert_eq!(bearer_token_env_var, None);
        }
        other => panic!("expected http, got {other:?}"),
    }
}

#[test]
fn secrets_come_from_env_flags_then_the_terminal() {
    let mut quiet = ScriptedPrompter::default();
    let values = collect_secrets(&stdio_mcp(), &given(&[("API_KEY", "k")]), &mut quiet).unwrap();
    assert_eq!(values, given(&[("API_KEY", "k")]));
    assert!(quiet.asked.is_empty());

    let mut terminal = ScriptedPrompter {
        interactive: true,
        secrets: [("API_KEY".to_string(), "typed".to_string())].into(),
        ..Default::default()
    };
    let values = collect_secrets(&stdio_mcp(), &BTreeMap::new(), &mut terminal).unwrap();
    assert_eq!(values, given(&[("API_KEY", "typed")]));
}

#[test]
fn missing_secrets_without_a_terminal_fail_with_their_names() {
    let mut quiet = ScriptedPrompter::default();
    let err = collect_secrets(&stdio_mcp(), &BTreeMap::new(), &mut quiet).unwrap_err();
    assert_eq!(err.code, "missing_secrets");
    assert!(err.detail.contains("API_KEY"), "{}", err.detail);
}

#[test]
fn unknown_or_misplaced_env_flags_are_refused() {
    let mut quiet = ScriptedPrompter::default();
    let err = collect_secrets(&stdio_mcp(), &given(&[("OTHER", "x")]), &mut quiet).unwrap_err();
    assert_eq!(err.code, "invalid_env");
    let err = collect_secrets(&http_mcp(), &given(&[("REMOTE_KEY", "x")]), &mut quiet).unwrap_err();
    assert_eq!(err.code, "invalid_env");
    // Overriding one of the server's own variables is allowed.
    let values = collect_secrets(
        &stdio_mcp(),
        &given(&[("API_KEY", "k"), ("FOO", "baz")]),
        &mut quiet,
    )
    .unwrap();
    assert_eq!(values.get("FOO").map(String::as_str), Some("baz"));
}

#[tokio::test]
async fn config_writes_round_trip_through_config_toml() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "model = \"keep-me\"\n\n[mcp_servers.mine]\ncommand = \"mine\"\n",
    )
    .unwrap();
    let mut servers = load_servers(home.path()).await.unwrap();
    servers.insert(
        "ctx".to_string(),
        mcp_server_config(&stdio_mcp(), &given(&[("API_KEY", "k")]), true).unwrap(),
    );
    save_servers(home.path(), &servers).await.unwrap();
    set_skill_enabled(home.path(), "pdf-helper", false)
        .await
        .unwrap();

    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    let toml: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(toml["model"].as_str(), Some("keep-me"));
    assert_eq!(
        toml["mcp_servers"]["mine"]["command"].as_str(),
        Some("mine")
    );
    assert_eq!(toml["mcp_servers"]["ctx"]["command"].as_str(), Some("npx"));
    assert_eq!(
        toml["mcp_servers"]["ctx"]["env"]["API_KEY"].as_str(),
        Some("k")
    );
    let skills = toml["skills"]["config"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"].as_str(), Some("pdf-helper"));
    assert_eq!(skills[0]["enabled"].as_bool(), Some(false));

    // Turning the skill back on drops the override.
    set_skill_enabled(home.path(), "pdf-helper", true)
        .await
        .unwrap();
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(!text.contains("pdf-helper"), "{text}");
}

#[test]
fn the_summary_shows_trust_command_and_secret_names_but_no_values() {
    let spec: InstallSpec = serde_json::from_value(serde_json::json!({
        "ref": "store:mcp/ctx", "kind": "mcp", "slug": "ctx", "name": "Ctx", "version": "1.2.0",
        "trust": "unreviewed",
        "mcp": {"transport": "stdio", "command": "npx", "args": ["-y", "pkg@1.2.0"],
                "env": {"FOO": "bar-value"}, "secret_env": ["API_KEY"]},
    }))
    .unwrap();
    let text = summary_lines(&spec, "ctx").join("\n");
    assert!(text.contains("NOT reviewed"), "{text}");
    assert!(text.contains("npx -y pkg@1.2.0"), "{text}");
    assert!(text.contains("API_KEY"), "{text}");
    assert!(text.contains("`ctx`"), "{text}");
    assert!(!text.contains("bar-value"), "{text}");
}

#[test]
fn listing_merges_this_computer_with_the_site() {
    let mut ledger = Ledger::default();
    ledger.upsert(crate::catalog::ledger::LedgerEntry {
        reference: "store:mcp/ctx".to_string(),
        kind: Kind::Mcp,
        slug: "ctx".to_string(),
        name: "ctx".to_string(),
        title: "Ctx".to_string(),
        version: "1.0.0".to_string(),
        sha256: None,
        skill_name: None,
        installed_at: "2026-11-06T00:00:00Z".to_string(),
        enabled: false,
    });
    let site: Vec<SiteInstalled> = serde_json::from_value(serde_json::json!([
        {"ref": "store:mcp/ctx", "kind": "mcp", "slug": "ctx", "name": "Ctx",
         "installed_version": "1.0.0", "latest_version": "1.1.0", "update_available": true,
         "enabled": true},
        {"ref": "store:skill/pdf", "kind": "skill", "slug": "pdf", "name": "PDF",
         "installed_version": "0.1.0", "latest_version": "0.1.0", "update_available": false,
         "enabled": true},
    ]))
    .unwrap();
    let items = merge_listing(&ledger, Some(&site));
    assert_eq!(items.len(), 2);
    assert!(items[0].local && items[0].on_site && items[0].update_available);
    assert_eq!(items[0].enabled, Some(false));
    assert_eq!(items[0].latest_version.as_deref(), Some("1.1.0"));
    assert!(!items[1].local && items[1].on_site);
    assert_eq!(items[1].enabled, None);
    let offline = merge_listing(&ledger, None);
    assert_eq!(offline.len(), 1);
    assert!(!offline[0].on_site);
}

#[test]
fn placeholders_in_args_and_urls_are_filled_locally() {
    let mcp: McpSpec = serde_json::from_value(serde_json::json!({
        "transport": "stdio", "command": "npx", "args": ["-y", "pkg", "--key=${API_KEY}"],
        "env": {}, "secret_env": ["API_KEY"],
    }))
    .unwrap();
    let config = mcp_server_config(&mcp, &given(&[("API_KEY", "k1")]), true).unwrap();
    match config.transport {
        McpServerTransportConfig::Stdio { args, .. } => {
            assert_eq!(args, vec!["-y", "pkg", "--key=k1"]);
        }
        other => panic!("{other:?}"),
    }
    let http: McpSpec = serde_json::from_value(serde_json::json!({
        "transport": "http", "url": "https://x.example/mcp?key=${R_KEY}", "secret_env": ["R_KEY"],
    }))
    .unwrap();
    let mut quiet = ScriptedPrompter::default();
    let values = collect_secrets(&http, &given(&[("R_KEY", "abc")]), &mut quiet).unwrap();
    let config = mcp_server_config(&http, &values, true).unwrap();
    match config.transport {
        McpServerTransportConfig::StreamableHttp { url, .. } => {
            assert_eq!(url, "https://x.example/mcp?key=abc");
        }
        other => panic!("{other:?}"),
    }
    // A secret cannot turn the address into something other than HTTPS.
    let host: McpSpec = serde_json::from_value(serde_json::json!({
        "transport": "http", "url": "${HOST}/mcp", "secret_env": ["HOST"],
    }))
    .unwrap();
    let err =
        mcp_server_config(&host, &given(&[("HOST", "http://evil.example")]), true).unwrap_err();
    assert_eq!(err.code, "invalid_env");
}
