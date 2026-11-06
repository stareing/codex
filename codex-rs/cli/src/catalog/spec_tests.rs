use super::*;
use pretty_assertions::assert_eq;

pub(crate) fn stdio_spec_json() -> serde_json::Value {
    serde_json::json!({
        "ref": "store:mcp/context7", "kind": "mcp", "slug": "context7", "name": "Context7",
        "version": "1.2.0", "trust": "reviewed",
        "source_url": "https://onigio.com/store/mcp/context7",
        "mcp": {"transport": "stdio", "command": "npx",
                "args": ["-y", "@upstash/context7-mcp@1.2.0"],
                "env": {"FOO": "bar"}, "secret_env": ["CONTEXT7_API_KEY"],
                "url": null, "secret_headers": {}},
        "skill": null,
        "bundle": {"sha256": "a".repeat(64), "size": 1234}
    })
}

fn spec_from(value: serde_json::Value) -> InstallSpec {
    serde_json::from_value(value).unwrap()
}

#[test]
fn references_parse_in_all_three_forms() {
    assert_eq!(
        parse_ref("store:mcp/context7").unwrap(),
        CatalogRef::store(Kind::Mcp, "context7")
    );
    assert_eq!(
        parse_ref(" store:skill/pdf-tools ").unwrap(),
        CatalogRef::store(Kind::Skill, "pdf-tools")
    );
    let bare = parse_ref("context7").unwrap();
    assert_eq!(bare.kind, None);
    assert_eq!(bare.to_string(), "context7");
    assert_eq!(
        parse_ref("store:skill/pdf-tools").unwrap().to_string(),
        "store:skill/pdf-tools"
    );
}

#[test]
fn bad_references_are_refused() {
    for bad in [
        "",
        "store:plugin/x",
        "tools:12",
        "store:mcp/",
        "store:mcp",
        "store:mcp/../x",
        "store:skill/a/b",
        "../etc",
        ".hidden",
        "-flag",
        "a b",
        "store:mcp/x?y",
    ] {
        assert!(parse_ref(bad).is_err(), "{bad:?} should be refused");
    }
    assert!(parse_ref(&"a".repeat(129)).is_err());
}

#[test]
fn the_contract_example_is_a_valid_stdio_spec() {
    let spec = spec_from(stdio_spec_json());
    assert_eq!(spec.validate(), Ok(()));
    assert!(spec.reviewed());
    let mcp = spec.mcp.unwrap();
    assert_eq!(mcp.transport, Transport::Stdio);
    assert_eq!(mcp.command.as_deref(), Some("npx"));
    assert_eq!(mcp.secret_env, vec!["CONTEXT7_API_KEY".to_string()]);
}

#[test]
fn http_and_skill_specs_validate() {
    let http = spec_from(serde_json::json!({
        "ref": "store:mcp/remote", "kind": "mcp", "slug": "remote", "name": "Remote",
        "version": "2", "trust": "reviewed",
        "mcp": {"transport": "http", "command": null, "url": "https://mcp.example.com/mcp",
                "secret_headers": {"X-Api-Key": "REMOTE_API_KEY"}},
    }));
    assert_eq!(http.validate(), Ok(()));
    let skill = spec_from(serde_json::json!({
        "ref": "store:skill/pdf", "kind": "skill", "slug": "pdf", "name": "PDF",
        "version": "0.1.0", "trust": "reviewed", "mcp": null,
        "skill": {"name": "pdf"}, "bundle": {"sha256": "b".repeat(64), "size": 10},
    }));
    assert_eq!(skill.validate(), Ok(()));
}

#[test]
fn unsafe_specs_are_refused() {
    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, Mutation)> = vec![
        ("slug traversal", Box::new(|v| v["slug"] = "../x".into())),
        (
            "ref does not match",
            Box::new(|v| v["ref"] = "store:mcp/other".into()),
        ),
        ("kind mismatch", Box::new(|v| v["kind"] = "skill".into())),
        (
            "http url on stdio",
            Box::new(|v| v["mcp"]["url"] = "https://x".into()),
        ),
        (
            "no command",
            Box::new(|v| v["mcp"]["command"] = serde_json::Value::Null),
        ),
        (
            "bad env name",
            Box::new(|v| v["mcp"]["env"] = serde_json::json!({"A=B": "x"})),
        ),
        (
            "bad secret name",
            Box::new(|v| v["mcp"]["secret_env"] = serde_json::json!(["1X"])),
        ),
        (
            "control char in arg",
            Box::new(|v| v["mcp"]["args"] = serde_json::json!(["a\nb"])),
        ),
        (
            "headers on stdio",
            Box::new(|v| v["mcp"]["secret_headers"] = serde_json::json!({"X-K": "K"})),
        ),
        ("empty version", Box::new(|v| v["version"] = " ".into())),
    ];
    for (label, mutate) in cases {
        let mut value = stdio_spec_json();
        mutate(&mut value);
        let spec = spec_from(value);
        assert!(spec.validate().is_err(), "{label} should be refused");
    }
}

#[test]
fn http_must_be_https_and_skills_need_a_sane_bundle() {
    let http = |url: &str| {
        spec_from(serde_json::json!({
            "ref": "store:mcp/r", "kind": "mcp", "slug": "r", "name": "R", "version": "1",
            "mcp": {"transport": "http", "url": url},
        }))
    };
    assert!(http("http://mcp.example.com").validate().is_err());
    assert!(http("file:///etc/passwd").validate().is_err());
    assert_eq!(http("http://127.0.0.1:9000/mcp").validate(), Ok(()));
    let skill = |bundle: serde_json::Value| {
        spec_from(serde_json::json!({
            "ref": "store:skill/s", "kind": "skill", "slug": "s", "name": "S", "version": "1",
            "skill": {"name": "s"}, "bundle": bundle,
        }))
    };
    assert!(skill(serde_json::Value::Null).validate().is_err());
    assert!(
        skill(serde_json::json!({"sha256": "xyz", "size": 1}))
            .validate()
            .is_err()
    );
    assert!(
        skill(serde_json::json!({"sha256": "a".repeat(64), "size": MAX_BUNDLE_BYTES + 1}))
            .validate()
            .is_err()
    );
    assert!(
        skill(serde_json::json!({"sha256": "a".repeat(64), "size": 0}))
            .validate()
            .is_err()
    );
    // Untrusted text never decides trust: anything but "reviewed" is unreviewed.
    let mut value = stdio_spec_json();
    value["trust"] = "Reviewed".into();
    assert!(!spec_from(value).reviewed());
}

#[test]
fn placeholders_are_found_and_filled_once() {
    assert_eq!(
        placeholders("--key=${API_KEY} ${bad-name} ${X}${Y"),
        vec!["API_KEY".to_string(), "X".to_string()]
    );
    let values: BTreeMap<String, String> = [
        ("API_KEY".to_string(), "v${X}".to_string()),
        ("X".to_string(), "x".to_string()),
    ]
    .into();
    // A value containing `${…}` is not expanded again; unknown names stay as written.
    assert_eq!(
        substitute("--key=${API_KEY}&${X}&${OTHER}&${", &values),
        "--key=v${X}&x&${OTHER}&${"
    );
}

#[test]
fn placeholders_must_be_asked_for() {
    let mut value = stdio_spec_json();
    value["mcp"]["args"] = serde_json::json!(["--key=${CONTEXT7_API_KEY}"]);
    assert_eq!(spec_from(value.clone()).validate(), Ok(()));
    value["mcp"]["args"] = serde_json::json!(["--key=${UNLISTED}"]);
    assert!(spec_from(value).validate().is_err());
    // A remote server's address may carry a secret too.
    let http = spec_from(serde_json::json!({
        "ref": "store:mcp/r", "kind": "mcp", "slug": "r", "name": "R", "version": "1",
        "mcp": {"transport": "http", "command": null, "url": "https://x.example/mcp?key=${R_KEY}",
                "env": {}, "secret_env": ["R_KEY"], "secret_headers": {}},
    }));
    assert_eq!(http.validate(), Ok(()));
}

#[test]
fn a_skill_comes_either_as_text_or_as_a_bundle() {
    let skill = |skill_md: serde_json::Value, bundle: serde_json::Value| {
        spec_from(serde_json::json!({
            "ref": "store:skill/s", "kind": "skill", "slug": "s", "name": "S", "version": "1",
            "skill": {"name": "s", "skill_md": skill_md}, "bundle": bundle,
        }))
    };
    let bundle = serde_json::json!({"sha256": "a".repeat(64), "size": 10});
    assert_eq!(
        skill("---\nname: s\n---\n".into(), serde_json::Value::Null).validate(),
        Ok(())
    );
    assert_eq!(
        skill(serde_json::Value::Null, bundle.clone()).validate(),
        Ok(())
    );
    assert!(skill("text".into(), bundle).validate().is_err());
    assert!(
        skill(serde_json::Value::Null, serde_json::Value::Null)
            .validate()
            .is_err()
    );
    assert!(
        skill(
            "x".repeat(MAX_SKILL_MD_BYTES as usize + 1).into(),
            serde_json::Value::Null
        )
        .validate()
        .is_err()
    );
}

fn tools_stdio_json() -> serde_json::Value {
    serde_json::json!({
        "ref": "tools:mcp/io.github.acme/weather", "kind": "mcp", "slug": "weather",
        "name": "Weather", "version": "1.2.0", "trust": "unreviewed",
        "source_url": "https://github.com/acme/weather",
        "mcp": {"transport": "stdio", "command": "npx",
                "args": ["-y", "@acme/weather@1.2.0", "--port", "3000"],
                "env": {"REGION": "eu"}, "secret_env": ["API_KEY"], "url": null,
                "headers": {}, "secret_headers": {}},
        "skill": null, "bundle": null, "install_instructions": null
    })
}

#[test]
fn tool_directory_references_keep_their_key() {
    for (raw, kind, key) in [
        (
            "tools:mcp/io.github.acme/weather",
            Kind::Mcp,
            "io.github.acme/weather",
        ),
        (
            "tools:skill/Owner/Repo.Name",
            Kind::Skill,
            "Owner/Repo.Name",
        ),
        (
            " tools:cli/burntsushi/ripgrep ",
            Kind::Cli,
            "burntsushi/ripgrep",
        ),
        ("tools:mcp/a..b/c_d-e", Kind::Mcp, "a..b/c_d-e"),
    ] {
        let parsed = parse_ref(raw).unwrap();
        assert_eq!(parsed, CatalogRef::tools(kind, key), "{raw}");
        assert_eq!(parsed.to_string(), raw.trim());
    }
    for bad in [
        "tools:mcp/weather",
        "tools:mcp/a/b/c",
        "tools:mcp//b",
        "tools:mcp/a/",
        "tools:plugin/a/b",
        "tools:mcp/a b/c",
        "tools:mcp/a/b?x=1",
        "store:cli/x",
    ] {
        assert!(parse_ref(bad).is_err(), "{bad:?} should be refused");
    }
    let long = format!("tools:mcp/{}/{}", "a".repeat(150), "b".repeat(50));
    assert!(parse_ref(&long).is_err(), "over 200 characters in all");
    assert!(parse_ref(&format!("tools:mcp/{}/b", "a".repeat(151))).is_err());
}

#[test]
fn a_tool_directory_spec_validates_and_answers_its_reference() {
    let spec = spec_from(tools_stdio_json());
    assert_eq!(spec.validate(), Ok(()));
    assert!(!spec.reviewed());
    assert!(spec.answers(&parse_ref("tools:mcp/io.github.acme/weather").unwrap()));
    // GitHub keys are case-insensitive; the site answers lower-cased.
    assert!(spec.answers(&parse_ref("tools:mcp/IO.github.Acme/Weather").unwrap()));
    for other in [
        "tools:mcp/io.github.acme/other",
        "tools:skill/io.github.acme/weather",
        "store:mcp/weather",
        "weather",
    ] {
        assert!(!spec.answers(&parse_ref(other).unwrap()), "{other}");
    }
    // A store spec still answers its store and bare references only.
    let store = spec_from(stdio_spec_json());
    assert!(store.answers(&parse_ref("context7").unwrap()));
    assert!(store.answers(&parse_ref("store:mcp/context7").unwrap()));
    assert!(!store.answers(&parse_ref("store:skill/context7").unwrap()));
    assert!(!store.answers(&parse_ref("tools:mcp/x/context7").unwrap()));
}

#[test]
fn tool_directory_specs_with_mismatched_or_unusable_fields_are_refused() {
    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "kind differs from ref",
            Box::new(|v| v["ref"] = "tools:skill/io.github.acme/weather".into()),
        ),
        (
            "ref not a reference",
            Box::new(|v| v["ref"] = "tools:mcp/weather".into()),
        ),
        (
            "slug with a dot",
            Box::new(|v| v["slug"] = "weather.tool".into()),
        ),
        ("slug traversal", Box::new(|v| v["slug"] = "../x".into())),
        (
            "source not a web page",
            Box::new(|v| v["source_url"] = "javascript:alert(1)".into()),
        ),
        (
            "literal header on stdio",
            Box::new(|v| v["mcp"]["headers"] = serde_json::json!({"X": "y"})),
        ),
    ];
    for (label, mutate) in cases {
        let mut value = tools_stdio_json();
        mutate(&mut value);
        assert!(
            spec_from(value).validate().is_err(),
            "{label} should be refused"
        );
    }
}

#[test]
fn headers_and_install_instructions_are_optional_for_older_answers() {
    // The P1 answer shape (no `headers`, no `install_instructions`) still parses and validates.
    let spec = spec_from(stdio_spec_json());
    assert_eq!(spec.install_instructions, None);
    assert!(spec.mcp.unwrap().headers.is_empty());

    let mut http = serde_json::json!({
        "ref": "tools:mcp/io.github.acme/remote", "kind": "mcp", "slug": "remote", "name": "R",
        "version": "1", "trust": "unreviewed",
        "mcp": {"transport": "http", "url": "https://mcp.acme.dev/mcp",
                "headers": {"X-Client": "onigio"}, "secret_headers": {"X-Key": "REMOTE_X_KEY"}},
        "install_instructions": null,
    });
    assert_eq!(spec_from(http.clone()).validate(), Ok(()));
    http["mcp"]["headers"] = serde_json::json!({"X-Key": "dup"});
    assert!(
        spec_from(http.clone()).validate().is_err(),
        "a header both literal and secret"
    );
    http["mcp"]["headers"] = serde_json::json!({"Bad Header": "v"});
    assert!(spec_from(http.clone()).validate().is_err());
    http["mcp"]["headers"] = serde_json::json!({"X-Client": "a\r\nX-Evil: 1"});
    assert!(spec_from(http).validate().is_err());
}

#[test]
fn a_command_line_tool_needs_instructions_and_nothing_to_run() {
    let cli = |instructions: serde_json::Value| {
        spec_from(serde_json::json!({
            "ref": "tools:cli/burntsushi/ripgrep", "kind": "cli", "slug": "ripgrep",
            "name": "ripgrep", "version": "14.1.0", "trust": "unreviewed",
            "source_url": "https://github.com/burntsushi/ripgrep",
            "mcp": null, "skill": null, "bundle": null, "install_instructions": instructions,
        }))
    };
    let ok = cli(serde_json::json!({"url": "https://github.com/burntsushi/ripgrep#readme"}));
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(cli(serde_json::json!({"url": null})).validate(), Ok(()));
    assert!(cli(serde_json::Value::Null).validate().is_err());
    assert!(
        cli(serde_json::json!({"url": "file:///etc/passwd"}))
            .validate()
            .is_err()
    );
    let mut with_mcp = serde_json::json!({
        "ref": "tools:cli/a/b", "kind": "cli", "slug": "b", "name": "b", "version": "1",
        "mcp": {"transport": "stdio", "command": "b"}, "install_instructions": {"url": null},
    });
    assert!(spec_from(with_mcp.clone()).validate().is_err());
    with_mcp["mcp"] = serde_json::Value::Null;
    assert_eq!(spec_from(with_mcp).validate(), Ok(()));
}
