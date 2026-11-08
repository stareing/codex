//! The whole flow against a fake site: sign-in files → catalog API → config.toml, skills/ and the
//! ledger on disk.

use super::*;
use crate::catalog::ledger::Ledger;
use crate::catalog::test_support::SKILL_MD;
use crate::catalog::test_support::ScriptedPrompter;
use crate::catalog::test_support::sign_in;
use crate::catalog::test_support::zip_of;
use crate::catalog::unpack::sha256_hex;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

const TOKEN: &str = "oad_e2e";

fn stdio_spec() -> serde_json::Value {
    serde_json::json!({
        "ref": "store:mcp/context7", "kind": "mcp", "slug": "context7", "name": "Context7",
        "version": "1.2.0", "trust": "reviewed",
        "source_url": "https://onigio.com/store/mcp/context7",
        "mcp": {"transport": "stdio", "command": "npx",
                "args": ["-y", "@upstash/context7-mcp@1.2.0"],
                "env": {"FOO": "bar"}, "secret_env": ["CONTEXT7_API_KEY"],
                "url": null, "secret_headers": {}},
        "skill": null, "bundle": null
    })
}

fn skill_spec(bundle: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "ref": "store:skill/pdf", "kind": "skill", "slug": "pdf", "name": "PDF helper",
        "version": "0.3.0", "trust": "reviewed", "mcp": null,
        "skill": {"name": "pdf-helper"},
        "bundle": {"sha256": sha256_hex(bundle), "size": bundle.len()}
    })
}

async fn site() -> (MockServer, tempfile::TempDir) {
    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    sign_in(home.path(), &server.uri(), TOKEN);
    (server, home)
}

async fn mount_resolve(server: &MockServer, reference: &str, spec: &serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/resolve/"))
        .and(query_param("ref", reference))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"spec": spec})))
        .mount(server)
        .await;
}

async fn mount_install(server: &MockServer, reference: &str, answer: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/install/"))
        .and(body_json(serde_json::json!({"ref": reference})))
        .respond_with(ResponseTemplate::new(201).set_body_json(answer))
        .expect(1)
        .mount(server)
        .await;
}

fn config_toml(home: &Path) -> toml::Value {
    toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap_or_default()).unwrap()
}

async fn run_install(
    home: &Path,
    raw_ref: &str,
    prompter: &mut ScriptedPrompter,
    options: InstallOptions,
) -> Result<install::InstallOutcome, CatalogError> {
    let client = signed_in_client(home).unwrap();
    let mut shown = Vec::new();
    let mut show = |line: &str| shown.push(line.to_string());
    install::install(home, &client, prompter, raw_ref, &options, &mut show).await
}

#[tokio::test]
async fn installing_an_mcp_server_writes_config_and_ledger_and_keeps_secrets_local() {
    let (server, home) = site().await;
    mount_resolve(&server, "context7", &stdio_spec()).await;
    mount_install(
        &server,
        "store:mcp/context7",
        serde_json::json!({"spec": stdio_spec(), "installed_version": "1.2.0"}),
    )
    .await;
    let mut prompter = ScriptedPrompter {
        interactive: true,
        confirms: [true].into(),
        secrets: [("CONTEXT7_API_KEY".to_string(), "typed-secret".to_string())].into(),
        ..Default::default()
    };
    let outcome = run_install(
        home.path(),
        "context7",
        &mut prompter,
        InstallOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.entry.reference, "store:mcp/context7");
    assert_eq!(prompter.asked, vec!["Install it?", "CONTEXT7_API_KEY"]);

    let toml = config_toml(home.path());
    let server_entry = &toml["mcp_servers"]["context7"];
    assert_eq!(server_entry["command"].as_str(), Some("npx"));
    assert_eq!(
        server_entry["env"]["CONTEXT7_API_KEY"].as_str(),
        Some("typed-secret")
    );
    assert_eq!(server_entry["env"]["FOO"].as_str(), Some("bar"));

    let ledger = Ledger::load(home.path()).unwrap();
    assert_eq!(ledger.items.len(), 1);
    assert_eq!(ledger.items[0].version, "1.2.0");
    assert_eq!(ledger.items[0].name, "context7");

    // The secret never went to the site.
    for request in server.received_requests().await.unwrap() {
        let body = String::from_utf8_lossy(&request.body).to_string();
        assert!(!body.contains("typed-secret"), "{body}");
        assert!(!request.url.as_str().contains("typed-secret"));
    }
}

#[tokio::test]
async fn an_alias_and_env_flags_install_without_prompts() {
    let (server, home) = site().await;
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    mount_install(
        &server,
        "store:mcp/context7",
        serde_json::json!({"spec": stdio_spec(), "installed_version": "1.2.0"}),
    )
    .await;
    let mut prompter = ScriptedPrompter::default();
    let options = InstallOptions {
        yes: true,
        alias: Some("ctx".to_string()),
        env: [("CONTEXT7_API_KEY".to_string(), "flag".to_string())].into(),
    };
    run_install(home.path(), "store:mcp/context7", &mut prompter, options)
        .await
        .unwrap();
    assert_eq!(
        config_toml(home.path())["mcp_servers"]["ctx"]["env"]["CONTEXT7_API_KEY"].as_str(),
        Some("flag")
    );
}

#[tokio::test]
async fn without_a_terminal_missing_secrets_fail_before_anything_is_recorded() {
    let (server, home) = site().await;
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&server)
        .await;
    let mut prompter = ScriptedPrompter::default();
    let options = InstallOptions {
        yes: true,
        ..Default::default()
    };
    let err = run_install(home.path(), "store:mcp/context7", &mut prompter, options)
        .await
        .unwrap_err();
    assert_eq!(err.code, "missing_secrets");
    assert!(err.message().contains("CONTEXT7_API_KEY"));
    assert!(!home.path().join("config.toml").exists());

    // And without --yes there is nothing to confirm on.
    let err = run_install(
        home.path(),
        "store:mcp/context7",
        &mut ScriptedPrompter::default(),
        InstallOptions::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "confirmation_required");
}

#[tokio::test]
async fn declining_installs_nothing() {
    let (server, home) = site().await;
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&server)
        .await;
    let mut prompter = ScriptedPrompter {
        interactive: true,
        confirms: [false].into(),
        ..Default::default()
    };
    let err = run_install(
        home.path(),
        "store:mcp/context7",
        &mut prompter,
        InstallOptions::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "cancelled");
}

#[tokio::test]
async fn a_name_used_by_another_server_is_not_overwritten() {
    let (server, home) = site().await;
    std::fs::write(
        home.path().join("config.toml"),
        "[mcp_servers.context7]\ncommand = \"my-own\"\n",
    )
    .unwrap();
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    let err = run_install(
        home.path(),
        "store:mcp/context7",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "name_taken");
    assert_eq!(
        config_toml(home.path())["mcp_servers"]["context7"]["command"].as_str(),
        Some("my-own")
    );
}

#[tokio::test]
async fn an_ambiguous_slug_lists_the_candidates() {
    let (server, home) = site().await;
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/resolve/"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "code": "ambiguous_ref", "detail": "Both an MCP server and a skill are named pdf.",
            "candidates": ["store:mcp/pdf", "store:skill/pdf"]
        })))
        .mount(&server)
        .await;
    let err = run_install(
        home.path(),
        "pdf",
        &mut ScriptedPrompter::default(),
        InstallOptions::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "ambiguous_ref");
    assert_eq!(err.candidates, vec!["store:mcp/pdf", "store:skill/pdf"]);
    assert_eq!(error_json(&err)["candidates"][1], "store:skill/pdf");
}

#[tokio::test]
async fn a_skill_is_downloaded_verified_unpacked_and_toggled() {
    let (server, home) = site().await;
    let bundle = zip_of(&[("SKILL.md", SKILL_MD), ("notes.txt", b"n")]);
    let spec = skill_spec(&bundle);
    mount_resolve(&server, "store:skill/pdf", &spec).await;
    mount_install(
        &server,
        "store:skill/pdf",
        serde_json::json!({"spec": spec, "installed_version": "0.3.0",
                           "download_url": "/api/skills/download/one-time/"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/skills/download/one-time/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bundle.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = run_install(
        home.path(),
        "store:skill/pdf",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.entry.skill_name.as_deref(), Some("pdf-helper"));
    assert_eq!(
        outcome.entry.sha256.as_deref(),
        Some(sha256_hex(&bundle).as_str())
    );
    assert_eq!(
        std::fs::read(home.path().join("skills/pdf/SKILL.md")).unwrap(),
        SKILL_MD
    );

    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/set-enabled/"))
        .and(body_json(
            serde_json::json!({"ref": "store:skill/pdf", "enabled": false}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"enabled": false})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = signed_in_client(home.path()).unwrap();
    let change = install::set_enabled(home.path(), &client, "pdf", false)
        .await
        .unwrap();
    assert!(change.site_warning.is_none());
    let skills = config_toml(home.path())["skills"]["config"].clone();
    assert_eq!(skills[0]["name"].as_str(), Some("pdf-helper"));
    assert_eq!(skills[0]["enabled"].as_bool(), Some(false));
    assert!(!Ledger::load(home.path()).unwrap().items[0].enabled);

    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .and(body_json(serde_json::json!({"ref": "store:skill/pdf"})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let change = install::remove(home.path(), &client, "store:skill/pdf")
        .await
        .unwrap();
    assert!(change.site_warning.is_none());
    assert!(!home.path().join("skills/pdf").exists());
    assert!(Ledger::load(home.path()).unwrap().items.is_empty());
    assert!(
        !std::fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("pdf-helper")
    );
}

#[tokio::test]
async fn a_tampered_bundle_is_refused_and_the_site_record_withdrawn() {
    let (server, home) = site().await;
    let bundle = zip_of(&[("SKILL.md", SKILL_MD)]);
    let spec = skill_spec(&bundle);
    mount_resolve(&server, "store:skill/pdf", &spec).await;
    mount_install(
        &server,
        "store:skill/pdf",
        serde_json::json!({"spec": spec, "download_url": "/api/skills/download/t/"}),
    )
    .await;
    let mut tampered = bundle.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    Mock::given(method("GET"))
        .and(path("/api/skills/download/t/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tampered))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let err = run_install(
        home.path(),
        "store:skill/pdf",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "bad_bundle");
    assert!(err.message().contains("sha256"), "{}", err.message());
    assert!(!home.path().join("skills/pdf").exists());
    assert!(Ledger::load(home.path()).unwrap().items.is_empty());
}

#[tokio::test]
async fn unequip_and_remove_an_mcp_server_and_list_what_is_left() {
    let (server, home) = site().await;
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    mount_install(
        &server,
        "store:mcp/context7",
        serde_json::json!({"spec": stdio_spec(), "installed_version": "1.2.0"}),
    )
    .await;
    run_install(
        home.path(),
        "store:mcp/context7",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            env: [("CONTEXT7_API_KEY".to_string(), "k".to_string())].into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let client = signed_in_client(home.path()).unwrap();

    // The site being down does not undo the local change; it is reported.
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/set-enabled/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let change = install::set_enabled(home.path(), &client, "context7", false)
        .await
        .unwrap();
    assert_eq!(change.site_warning.map(|w| w.status), Some(Some(503)));
    assert_eq!(
        config_toml(home.path())["mcp_servers"]["context7"]["enabled"].as_bool(),
        Some(false)
    );

    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/installed/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                {"ref": "store:mcp/context7", "kind": "mcp", "slug": "context7", "name": "Context7",
                 "installed_version": "1.2.0", "latest_version": "1.3.0", "update_available": true,
                 "enabled": false}
            ]})),
        )
        .mount(&server)
        .await;
    let (items, warning) = install::list(home.path(), Some(&client)).await.unwrap();
    assert!(warning.is_none());
    assert_eq!(items.len(), 1);
    assert!(items[0].update_available);
    assert_eq!(items[0].enabled, Some(false));

    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            serde_json::json!({"code": "not_installed", "detail": "Not installed."}),
        ))
        .mount(&server)
        .await;
    let change = install::remove(home.path(), &client, "context7")
        .await
        .unwrap();
    assert!(change.site_warning.is_none(), "404 means already gone");
    let toml = config_toml(home.path());
    assert!(
        toml.get("mcp_servers")
            .and_then(|t| t.get("context7"))
            .is_none()
    );
    let err = install::remove(home.path(), &client, "context7")
        .await
        .unwrap_err();
    assert_eq!(err.code, "not_installed");
}

/// The site's `installed/` answer for context7 (MCP) and pdf (skill) with the given statuses.
async fn mount_installed(server: &MockServer, context7: &str, pdf: &str, reason: &str) {
    server.reset().await;
    let row = |reference: &str, status: &str| {
        let withdrawn = status == "withdrawn";
        serde_json::json!({"ref": reference, "kind": "mcp", "status": status,
                           "withdrawn_reason": withdrawn.then_some(reason)})
    };
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/installed/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                row("store:mcp/context7", context7), row("store:skill/pdf", pdf),
            ]})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn sync_turns_withdrawn_items_off_and_restores_them_when_offered_again() {
    let (server, home) = site().await;
    mount_resolve(&server, "store:mcp/context7", &stdio_spec()).await;
    mount_install(
        &server,
        "store:mcp/context7",
        serde_json::json!({"spec": stdio_spec(), "installed_version": "1.2.0"}),
    )
    .await;
    run_install(
        home.path(),
        "store:mcp/context7",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            env: [("CONTEXT7_API_KEY".to_string(), "k".to_string())].into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut ledger = Ledger::load(home.path()).unwrap();
    let mut pdf = ledger.items[0].clone();
    pdf.reference = "store:skill/pdf".to_string();
    pdf.kind = crate::catalog::spec::Kind::Skill;
    pdf.name = "pdf".to_string();
    pdf.skill_name = Some("pdf-helper".to_string());
    pdf.enabled = false;
    ledger.upsert(pdf);
    ledger.save(home.path()).unwrap();
    let client = signed_in_client(home.path()).unwrap();
    // `enabled = true` is the default and is left out of config.toml.
    let enabled = |home: &Path| {
        let server = config_toml(home)["mcp_servers"]["context7"].clone();
        Some(
            server
                .get("enabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true),
        )
    };

    // Offline: nothing changes.
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/installed/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    assert!(sync::sync(home.path(), &client).await.is_err());
    assert_eq!(enabled(home.path()), Some(true));
    assert!(
        Ledger::load(home.path())
            .unwrap()
            .items
            .iter()
            .all(|i| i.withdrawn.is_none())
    );

    // Withdrawn: off, and the site's own choice is not touched (no set-enabled call is mounted).
    mount_installed(&server, "withdrawn", "withdrawn", "version_yanked").await;
    let done = sync::sync(home.path(), &client).await.unwrap();
    let actions: Vec<_> = done
        .iter()
        .map(|d| (d.reference.as_str(), d.action, d.enabled))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("store:mcp/context7", "withdrawn", false),
            ("store:skill/pdf", "withdrawn", false)
        ]
    );
    assert_eq!(enabled(home.path()), Some(false));
    let held = Ledger::load(home.path()).unwrap();
    let record = held.items[0].withdrawn.clone().unwrap();
    assert_eq!(
        (record.reason.as_str(), record.enabled_before),
        ("version_yanked", true)
    );
    assert!(!held.items[1].withdrawn.as_ref().unwrap().enabled_before);
    let err = install::set_enabled(home.path(), &client, "context7", true)
        .await
        .unwrap_err();
    assert_eq!(err.code, "withdrawn");
    assert!(
        sync::sync(home.path(), &client).await.unwrap().is_empty(),
        "a second run changes nothing"
    );
    let (items, _) = install::list(home.path(), Some(&client)).await.unwrap();
    assert_eq!(items[0].withdrawn_reason.as_deref(), Some("version_yanked"));

    // A new reason is recorded; when it was first withdrawn is kept.
    let since = record.since.clone();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    mount_installed(&server, "withdrawn", "withdrawn", "package_unavailable").await;
    assert_eq!(sync::sync(home.path(), &client).await.unwrap().len(), 2);
    let record = Ledger::load(home.path()).unwrap().items[0]
        .withdrawn
        .clone()
        .unwrap();
    assert_eq!(
        (record.reason.as_str(), record.since),
        ("package_unavailable", since)
    );

    // Offered again: back to what each was before.
    mount_installed(&server, "available", "available", "").await;
    let done = sync::sync(home.path(), &client).await.unwrap();
    let actions: Vec<_> = done
        .iter()
        .map(|d| (d.reference.as_str(), d.action, d.enabled))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("store:mcp/context7", "restored", true),
            ("store:skill/pdf", "restored", false)
        ]
    );
    assert_eq!(enabled(home.path()), Some(true));
    assert!(
        Ledger::load(home.path())
            .unwrap()
            .items
            .iter()
            .all(|i| i.withdrawn.is_none())
    );
    let (items, _) = install::list(home.path(), Some(&client)).await.unwrap();
    assert!(
        items.iter().all(|i| i.withdrawn_reason.is_none()),
        "{items:?}"
    );
}

#[test]
fn commands_need_a_signed_in_site_account() {
    let home = tempfile::tempdir().unwrap();
    let err = signed_in_client(home.path()).err().unwrap();
    assert_eq!(err.code, "not_signed_in");
    assert!(err.message().contains("onigio login"), "{}", err.message());
    sign_in(home.path(), "https://onigio.com", "  ");
    assert_eq!(
        signed_in_client(home.path()).err().map(|e| e.code),
        Some("not_signed_in".to_string())
    );
    sign_in(home.path(), "https://onigio.com", "oad_x");
    assert_eq!(
        signed_in_client(home.path()).unwrap().site(),
        "https://onigio.com"
    );
}

#[test]
fn the_commands_parse_as_top_level_subcommands() {
    use clap::Parser;
    #[derive(clap::Parser, Debug)]
    struct Root {
        #[command(subcommand)]
        command: CatalogCommand,
    }
    let parsed = Root::try_parse_from([
        "onigio",
        "install",
        "store:mcp/context7",
        "--yes",
        "--name",
        "ctx",
        "--env",
        "A=b=c",
        "--json",
    ])
    .unwrap();
    match parsed.command {
        CatalogCommand::Install(args) => {
            assert!(args.yes && args.json);
            assert_eq!(args.name.as_deref(), Some("ctx"));
            assert_eq!(args.env, vec![("A".to_string(), "b=c".to_string())]);
        }
        other => panic!("{other:?}"),
    }
    let parsed = Root::try_parse_from(["x", "search", "pdf", "tools", "--kind", "skill"]).unwrap();
    assert_eq!(parsed.command.name(), "search");
    for name in [
        "list",
        "equip a",
        "unequip a",
        "remove a",
        "sync-installed --json",
    ] {
        let args: Vec<&str> = std::iter::once("x").chain(name.split(' ')).collect();
        assert!(Root::try_parse_from(args).is_ok(), "{name}");
    }
    assert!(Root::try_parse_from(["x", "install", "a", "--env", "noequals"]).is_err());
}

#[tokio::test]
async fn a_text_only_skill_is_written_from_its_skill_md() {
    let (server, home) = site().await;
    let text = String::from_utf8(SKILL_MD.to_vec()).unwrap();
    let spec = serde_json::json!({
        "ref": "store:skill/notes", "kind": "skill", "slug": "notes", "name": "Notes",
        "version": "1.0.0", "trust": "reviewed", "source_url": "https://onigio.com/store/skills/notes",
        "mcp": null, "skill": {"name": "pdf-helper", "skill_md": text}, "bundle": null
    });
    mount_resolve(&server, "notes", &spec).await;
    mount_install(
        &server,
        "store:skill/notes",
        serde_json::json!({"spec": spec, "installed_version": "1.0.0", "download_url": null}),
    )
    .await;
    let outcome = run_install(
        home.path(),
        "notes",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.entry.skill_name.as_deref(), Some("pdf-helper"));
    assert_eq!(
        std::fs::read(home.path().join("skills/notes/SKILL.md")).unwrap(),
        SKILL_MD
    );
    assert_eq!(outcome.entry.sha256, Some(sha256_hex(SKILL_MD)));
}

#[tokio::test]
async fn the_download_is_authenticated_and_follows_the_redirect() {
    let (server, home) = site().await;
    let bundle = zip_of(&[("SKILL.md", SKILL_MD)]);
    let spec = skill_spec(&bundle);
    mount_resolve(&server, "store:skill/pdf", &spec).await;
    mount_install(
        &server,
        "store:skill/pdf",
        serde_json::json!({"spec": spec, "installed_version": "0.3.0",
                           "download_url": "/api/skills/download/ocd_abc/"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/skills/download/ocd_abc/"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "/media/store/pdf.zip?t=signed"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/store/pdf.zip"))
        .and(query_param("t", "signed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bundle))
        .expect(1)
        .mount(&server)
        .await;
    run_install(
        home.path(),
        "store:skill/pdf",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(home.path().join("skills/pdf/SKILL.md").exists());
}

#[tokio::test]
async fn an_expired_download_link_says_to_install_again() {
    let (server, home) = site().await;
    let bundle = zip_of(&[("SKILL.md", SKILL_MD)]);
    let spec = skill_spec(&bundle);
    mount_resolve(&server, "store:skill/pdf", &spec).await;
    mount_install(
        &server,
        "store:skill/pdf",
        serde_json::json!({"spec": spec, "download_url": "/api/skills/download/ocd_old/"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/skills/download/ocd_old/"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "code": "download_token_invalid",
            "detail": "This download link is invalid, expired, already used or not yours."
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let err = run_install(
        home.path(),
        "store:skill/pdf",
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "download_token_invalid");
    assert!(
        err.message().contains("run install again"),
        "{}",
        err.message()
    );
}

// --- P2: the open-source tool directory (`tools:` references, unreviewed) -----------------------

const TOOLS_NPM_REF: &str = "tools:mcp/io.github.acme/weather";
const TOOLS_HTTP_REF: &str = "tools:mcp/io.github.acme/remote";

fn tools_npm_spec() -> serde_json::Value {
    serde_json::json!({
        "ref": TOOLS_NPM_REF, "kind": "mcp", "slug": "weather", "name": "Weather",
        "version": "1.2.0", "trust": "unreviewed", "source_url": "https://github.com/acme/weather",
        "mcp": {"transport": "stdio", "command": "npx",
                "args": ["-y", "@acme/weather@1.2.0", "--port", "3000"],
                "env": {"REGION": "eu"}, "secret_env": ["API_KEY", "WORKSPACE"], "url": null,
                "headers": {}, "secret_headers": {}},
        "skill": null, "bundle": null, "install_instructions": null
    })
}

fn tools_http_spec() -> serde_json::Value {
    serde_json::json!({
        "ref": TOOLS_HTTP_REF, "kind": "mcp", "slug": "remote", "name": "Remote",
        "version": "2.0.0", "trust": "unreviewed", "source_url": "https://github.com/acme/remote",
        "mcp": {"transport": "http", "command": null, "args": [], "env": {}, "secret_env": [],
                "url": "https://mcp.acme.dev/mcp", "headers": {"X-Client": "onigio"},
                "secret_headers": {"X-API-Key": "REMOTE_X_API_KEY"}},
        "skill": null, "bundle": null, "install_instructions": null
    })
}

fn no_installs_expected() -> Mock {
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/install/"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
}

#[tokio::test]
async fn a_tool_directory_npm_server_installs_under_its_slug_and_keeps_the_ref() {
    let (server, home) = site().await;
    mount_resolve(&server, TOOLS_NPM_REF, &tools_npm_spec()).await;
    mount_install(
        &server,
        TOOLS_NPM_REF,
        serde_json::json!({"spec": tools_npm_spec(), "installed_version": "1.2.0",
                           "download_url": null}),
    )
    .await;
    // `secret_env` also names required settings that are not secret (WORKSPACE): each is asked.
    let mut prompter = ScriptedPrompter {
        interactive: true,
        confirms: [true].into(),
        secrets: [
            ("API_KEY".to_string(), "k-123".to_string()),
            ("WORKSPACE".to_string(), "/srv/w".to_string()),
        ]
        .into(),
        ..Default::default()
    };
    let client = signed_in_client(home.path()).unwrap();
    let mut shown = Vec::new();
    let outcome = install::install(
        home.path(),
        &client,
        &mut prompter,
        TOOLS_NPM_REF,
        &InstallOptions::default(),
        &mut |line: &str| shown.push(line.to_string()),
    )
    .await
    .unwrap();
    assert_eq!(
        prompter.asked,
        vec![install::UNREVIEWED_QUESTION, "API_KEY", "WORKSPACE"]
    );
    let shown = shown.join("\n");
    assert!(
        shown.contains("source:  https://github.com/acme/weather"),
        "{shown}"
    );
    assert!(
        shown.contains("runs:    npx -y @acme/weather@1.2.0 --port 3000"),
        "{shown}"
    );

    let toml = config_toml(home.path());
    let entry = &toml["mcp_servers"]["weather"];
    assert_eq!(entry["command"].as_str(), Some("npx"));
    assert_eq!(entry["args"][1].as_str(), Some("@acme/weather@1.2.0"));
    assert_eq!(entry["env"]["API_KEY"].as_str(), Some("k-123"));
    assert_eq!(entry["env"]["WORKSPACE"].as_str(), Some("/srv/w"));
    assert_eq!(entry["env"]["REGION"].as_str(), Some("eu"));
    assert!(
        toml["mcp_servers"].get("io.github.acme/weather").is_none(),
        "the key is never a table name"
    );

    let ledger = Ledger::load(home.path()).unwrap();
    assert_eq!(ledger.items[0].reference, TOOLS_NPM_REF);
    assert_eq!(ledger.items[0].name, "weather");
    assert_eq!(ledger.items[0].trust, "unreviewed");
    assert!(ledger.items[0].reconfirm_on_update());
    assert_eq!(outcome.entry, ledger.items[0]);

    // equip/unequip/remove address the site with the ref exactly as recorded.
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .and(body_json(serde_json::json!({"ref": TOOLS_NPM_REF})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    install::remove(home.path(), &client, "weather")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_remote_server_writes_headers_and_keeps_the_secret_header_value_in_the_env_file() {
    let (server, home) = site().await;
    mount_resolve(&server, TOOLS_HTTP_REF, &tools_http_spec()).await;
    mount_install(
        &server,
        TOOLS_HTTP_REF,
        serde_json::json!({"spec": tools_http_spec(), "installed_version": "2.0.0"}),
    )
    .await;
    let mut prompter = ScriptedPrompter {
        interactive: true,
        confirms: [true].into(),
        secrets: [(
            "REMOTE_X_API_KEY (X-API-Key header)".to_string(),
            "Bearer s3cret".to_string(),
        )]
        .into(),
        ..Default::default()
    };
    let outcome = run_install(
        home.path(),
        TOOLS_HTTP_REF,
        &mut prompter,
        InstallOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.entry.env_file, vec!["REMOTE_X_API_KEY"]);

    let toml = config_toml(home.path());
    let entry = &toml["mcp_servers"]["remote"];
    assert_eq!(entry["url"].as_str(), Some("https://mcp.acme.dev/mcp"));
    assert_eq!(entry["http_headers"]["X-Client"].as_str(), Some("onigio"));
    assert_eq!(
        entry["env_http_headers"]["X-API-Key"].as_str(),
        Some("REMOTE_X_API_KEY")
    );
    let config_text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(!config_text.contains("s3cret"), "{config_text}");
    // The agent loads $CODEX_HOME/.env at start-up; the value reads back unchanged.
    let loaded: Vec<(String, String)> = dotenvy::from_path_iter(home.path().join(".env"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        loaded,
        vec![("REMOTE_X_API_KEY".to_string(), "Bearer s3cret".to_string())]
    );
    for request in server.received_requests().await.unwrap() {
        assert!(!String::from_utf8_lossy(&request.body).contains("s3cret"));
    }

    // Removing it takes the value out of .env again.
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/uninstall/"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = signed_in_client(home.path()).unwrap();
    install::remove(home.path(), &client, TOOLS_HTTP_REF)
        .await
        .unwrap();
    let left = std::fs::read_to_string(home.path().join(".env")).unwrap();
    assert!(!left.contains("REMOTE_X_API_KEY"), "{left}");
}

#[tokio::test]
async fn a_command_line_tool_only_points_to_its_instructions() {
    let (server, home) = site().await;
    let reference = "tools:cli/burntsushi/ripgrep";
    let spec = serde_json::json!({
        "ref": reference, "kind": "cli", "slug": "ripgrep", "name": "ripgrep",
        "version": "14.1.0", "trust": "unreviewed",
        "source_url": "https://github.com/burntsushi/ripgrep",
        "mcp": null, "skill": null, "bundle": null,
        "install_instructions": {"url": "https://github.com/burntsushi/ripgrep#readme"}
    });
    mount_resolve(&server, reference, &spec).await;
    no_installs_expected().mount(&server).await;
    let err = run_install(
        home.path(),
        reference,
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "not_installable");
    assert_eq!(
        err.instructions_url.as_deref(),
        Some("https://github.com/burntsushi/ripgrep#readme")
    );
    assert!(
        err.message()
            .contains("Official instructions: https://github.com/burntsushi/ripgrep#readme"),
        "{}",
        err.message()
    );
    assert_eq!(
        error_json(&err)["install_instructions"]["url"],
        "https://github.com/burntsushi/ripgrep#readme"
    );
    assert!(!home.path().join("config.toml").exists());
    assert!(Ledger::load(home.path()).unwrap().items.is_empty());
}

#[tokio::test]
async fn an_unreviewed_entry_without_a_terminal_needs_yes() {
    let (server, home) = site().await;
    mount_resolve(&server, TOOLS_NPM_REF, &tools_npm_spec()).await;
    no_installs_expected().mount(&server).await;
    let options = InstallOptions {
        env: [
            ("API_KEY".to_string(), "k".to_string()),
            ("WORKSPACE".to_string(), "w".to_string()),
        ]
        .into(),
        ..Default::default()
    };
    let err = run_install(
        home.path(),
        TOOLS_NPM_REF,
        &mut ScriptedPrompter::default(),
        options.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "confirmation_required");

    // On a terminal, declining the unreviewed warning installs nothing either.
    let mut prompter = ScriptedPrompter {
        interactive: true,
        confirms: [false].into(),
        ..Default::default()
    };
    let err = run_install(home.path(), TOOLS_NPM_REF, &mut prompter, options)
        .await
        .unwrap_err();
    assert_eq!(err.code, "cancelled");
    assert_eq!(prompter.asked, vec![install::UNREVIEWED_QUESTION]);
    assert!(!home.path().join("config.toml").exists());
}

#[tokio::test]
async fn a_spec_for_another_entry_than_the_one_asked_for_is_refused() {
    let (server, home) = site().await;
    let mut other = tools_npm_spec();
    other["ref"] = "tools:mcp/io.github.evil/weather".into();
    mount_resolve(&server, TOOLS_NPM_REF, &other).await;
    no_installs_expected().mount(&server).await;
    let err = run_install(
        home.path(),
        TOOLS_NPM_REF,
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "unexpected_response");
}

#[tokio::test]
async fn tool_directory_refusals_show_the_sites_detail_and_what_to_do() {
    let (server, home) = site().await;
    let reference = "tools:skill/acme/skills";
    let detail = "This entry cannot be installed from the tool directory until it is reviewed.";
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/resolve/"))
        .and(query_param("ref", reference))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(serde_json::json!({"code": "needs_review", "detail": detail})),
        )
        .mount(&server)
        .await;
    no_installs_expected().mount(&server).await;
    let err = run_install(
        home.path(),
        reference,
        &mut ScriptedPrompter::default(),
        InstallOptions {
            yes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "needs_review");
    assert_eq!(err.status, Some(422));
    let message = err.message();
    assert!(message.starts_with(detail), "{message}");
    assert!(
        message.contains("reviewed before it can be installed"),
        "{message}"
    );
    assert_eq!(error_json(&err)["code"], "needs_review");

    // Every tool directory refusal code has its own next step.
    for code in [
        "unsupported_registry",
        "unpinned_version",
        "needs_input",
        "needs_review",
        "no_install_target",
        "pending_sync",
        "not_installable",
        "unsupported_transport",
    ] {
        let err = CatalogError::from_response(
            422,
            &serde_json::json!({"code": code, "detail": "Site says why."}),
        );
        let message = err.message();
        assert!(
            message.starts_with("Site says why.") && message.len() > "Site says why.".len() + 1,
            "{code}: {message}"
        );
    }
}

#[tokio::test]
async fn search_takes_a_source_and_shows_unreviewed_rows() {
    let (server, home) = site().await;
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/search/"))
        .and(query_param("q", "weather"))
        .and(query_param("source", "tools"))
        .and(query_param("kind", "cli"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"ref": TOOLS_NPM_REF, "kind": "mcp", "slug": "weather", "name": "Weather",
                 "summary": "Forecasts", "trust": "unreviewed", "installs": null,
                 "supported": true},
                {"ref": "tools:cli/acme/wx", "kind": "cli", "slug": "wx", "name": "wx",
                 "summary": "", "trust": "unreviewed", "installs": null, "supported": false,
                 "unsupported_reason": "not_installable"}
            ],
            "pagination": {"page": 1, "page_size": 20, "total": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/search/"))
        .and(query_param("source", "store"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"ref": "store:mcp/context7", "kind": "mcp", "slug": "context7",
                      "name": "Context7", "summary": "", "trust": "reviewed", "installs": 12,
                      "supported": true}],
            "pagination": {"page": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = signed_in_client(home.path()).unwrap();
    let page = client
        .search("weather", Some("cli"), "tools", None)
        .await
        .unwrap();
    let lines = search_lines(&page.data);
    assert_eq!(
        lines,
        vec![
            format!("{TOOLS_NPM_REF}  Weather  (unreviewed)"),
            "    Forecasts".to_string(),
            "tools:cli/acme/wx  wx  (unreviewed)  [command-line tool: instructions only]"
                .to_string(),
        ]
    );
    let store = client.search("ctx", None, "store", None).await.unwrap();
    assert_eq!(
        search_lines(&store.data),
        vec!["store:mcp/context7  Context7  (12 installs)"]
    );

    // On the command line `--source` defaults to the store.
    use clap::Parser;
    #[derive(clap::Parser, Debug)]
    struct Root {
        #[command(subcommand)]
        command: CatalogCommand,
    }
    let parse = |args: &[&str]| match Root::try_parse_from(args).unwrap().command {
        CatalogCommand::Search(args) => args.source,
        other => panic!("{other:?}"),
    };
    assert_eq!(parse(&["x", "search", "a"]), SourceArg::Store);
    assert_eq!(
        parse(&["x", "search", "a", "--source", "all"]),
        SourceArg::All
    );
    assert!(Root::try_parse_from(["x", "search", "a", "--source", "web"]).is_err());
}
