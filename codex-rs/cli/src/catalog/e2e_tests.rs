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

#[test]
fn commands_need_a_signed_in_site_account() {
    let home = tempfile::tempdir().unwrap();
    let err = signed_in_client(home.path()).err().unwrap();
    assert_eq!(err.code, "not_signed_in");
    assert!(
        err.message().contains("onigio-agent login"),
        "{}",
        err.message()
    );
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
        "onigio-agent",
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
    for name in ["list", "equip a", "unequip a", "remove a"] {
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
