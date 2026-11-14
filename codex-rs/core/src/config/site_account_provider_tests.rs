use super::*;
use crate::config::Config;
use crate::config::ConfigOverrides;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;

const RELAY: &str = "https://onigio.com/agent-relay/";

fn sign_in(home: &Path, model: Option<&str>) {
    let mut account = serde_json::json!({
        "site_url": "https://onigio.com",
        "relay_url": RELAY,
        "account_id": "u7",
        "device_id": 3,
        "name": "studio",
    });
    if let Some(model) = model {
        account["model"] = model.into();
    }
    std::fs::write(home.join(SITE_ACCOUNT_FILE), account.to_string()).unwrap();
    std::fs::write(home.join(SITE_ACCOUNT_TOKEN_FILE), "oad_secret\n").unwrap();
}

fn no_env(_: &str) -> Option<OsString> {
    None
}

async fn load(home: &tempfile::TempDir, cfg: ConfigToml, overrides: ConfigOverrides) -> Config {
    Config::load_from_base_config_with_overrides(cfg, overrides, home.abs())
        .await
        .expect("config loads")
}

#[test]
fn provider_targets_the_relay_without_upstream_auth() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), Some("gpt-5.4-mini"));
    let access = SiteModelAccess::load_with(home.path(), no_env).expect("signed in");
    let provider = access.provider();
    assert_eq!(provider.name, "Onigio");
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://onigio.com/agent-relay/v1")
    );
    assert_eq!(provider.wire_api, WireApi::Responses);
    assert!(!provider.requires_openai_auth);
    assert!(!provider.supports_websockets);
    assert_eq!(
        provider
            .experimental_bearer_token
            .as_deref()
            .map(String::as_str),
        Some("oad_secret")
    );
    let headers = provider.http_headers.clone().expect("headers");
    assert_eq!(headers[RELAY_ACCOUNT_HEADER].as_str(), "u7");
    provider.validate().expect("valid provider");
}

#[test]
fn relay_url_without_trailing_slash_gets_one_v1_segment() {
    let access = SiteModelAccess::new("https://onigio.com/agent-relay", "u7", None, None).unwrap();
    assert_eq!(access.base_url(), "https://onigio.com/agent-relay/v1");
}

#[test]
fn not_signed_in_means_nothing_to_inject() {
    let home = tempfile::TempDir::new().unwrap();
    assert_eq!(SiteModelAccess::load_with(home.path(), no_env), None);
}

#[test]
fn missing_token_still_installs_the_provider_without_a_bearer() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), None);
    std::fs::remove_file(home.path().join(SITE_ACCOUNT_TOKEN_FILE)).unwrap();
    let access = SiteModelAccess::load_with(home.path(), no_env).expect("account present");
    assert_eq!(access.provider().experimental_bearer_token, None);
}

#[test]
fn launcher_variables_win_over_the_saved_account() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), Some("saved-model"));
    let token_file = home.path().join("launcher.token");
    std::fs::write(&token_file, "oad_launcher").unwrap();
    let env = |name: &str| match name {
        SITE_RELAY_URL_ENV_VAR => Some(OsString::from("http://127.0.0.1:9000/relay/")),
        SITE_RELAY_TOKEN_FILE_ENV_VAR => Some(token_file.clone().into_os_string()),
        SITE_RELAY_ACCOUNT_ID_ENV_VAR => Some(OsString::from("u9")),
        _ => None,
    };
    let access = SiteModelAccess::load_with(home.path(), env).expect("env relay");
    assert_eq!(access.base_url(), "http://127.0.0.1:9000/relay/v1");
    assert_eq!(access.account_id, "u9");
    assert_eq!(access.token.as_deref(), Some("oad_launcher"));
    assert_eq!(access.model, None);
}

#[test]
fn partial_or_invalid_launcher_variables_fall_back_to_the_saved_account() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), None);
    let partial = |name: &str| (name == SITE_RELAY_URL_ENV_VAR).then(|| OsString::from(RELAY));
    let access = SiteModelAccess::load_with(home.path(), partial).expect("saved account");
    assert_eq!(access.account_id, "u7");
    for (url, account) in [
        ("http://onigio.com/relay/", "u7"),
        ("https://user:pw@onigio.com/", "u7"),
        (RELAY, "u 7"),
    ] {
        assert!(
            SiteModelAccess::new(url, account, None, None).is_err(),
            "{url} {account} should be refused"
        );
    }
}

#[tokio::test]
async fn signed_in_user_defaults_to_the_site_provider_and_model() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), Some("gpt-5.4-mini"));
    let config = load(&home, ConfigToml::default(), ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_id, SITE_PROVIDER_ID);
    assert_eq!(config.model_provider.name, SITE_PROVIDER_NAME);
    // The TUI and app-server skip the upstream login screen for such providers.
    assert!(!config.model_provider.requires_openai_auth);
    assert_eq!(config.model.as_deref(), Some("gpt-5.4-mini"));
}

#[tokio::test]
async fn explicit_choices_win_over_the_site_account() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), Some("site-model"));

    // A configured provider and model stay as they are.
    let cfg: ConfigToml =
        toml::from_str("model_provider = \"openai\"\nmodel = \"mine\"").expect("toml");
    let config = load(&home, cfg, ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_id, "openai");
    assert_eq!(config.model.as_deref(), Some("mine"));

    // A command-line provider wins and the site's model suggestion does not leak into it.
    let overrides = ConfigOverrides {
        model_provider: Some("openai".to_string()),
        ..Default::default()
    };
    let config = load(&home, ConfigToml::default(), overrides).await;
    assert_eq!(config.model_provider_id, "openai");
    assert_ne!(config.model.as_deref(), Some("site-model"));

    // A configured model wins over the suggestion while the site provider is used.
    let cfg: ConfigToml = toml::from_str("model = \"mine\"").expect("toml");
    let config = load(&home, cfg, ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_id, SITE_PROVIDER_ID);
    assert_eq!(config.model.as_deref(), Some("mine"));

    // A user-defined provider with the same id is never replaced.
    let cfg: ConfigToml = toml::from_str(
        "[model_providers.onigio]\nname = \"Mine\"\nbase_url = \"https://example.test/v1\"",
    )
    .expect("toml");
    let config = load(&home, cfg, ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_id, SITE_PROVIDER_ID);
    assert_eq!(config.model_provider.name, "Mine");
}

#[tokio::test]
async fn without_a_site_account_upstream_defaults_are_unchanged() {
    let home = tempfile::TempDir::new().unwrap();
    let config = load(&home, ConfigToml::default(), ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_id, "openai");
}

#[test]
fn provider_source_rule_table() {
    let inputs = |required, overridden, configured, signed_in| ProviderInputs {
        required,
        overridden,
        configured,
        signed_in,
    };
    let cases = [
        (inputs(true, true, true, true), ProviderSource::Required),
        (inputs(false, true, true, true), ProviderSource::Override),
        (inputs(false, false, true, true), ProviderSource::Configured),
        (
            inputs(false, false, false, true),
            ProviderSource::SiteAccount,
        ),
        (
            inputs(false, false, false, false),
            ProviderSource::UpstreamDefault,
        ),
    ];
    for (inputs, expected) in cases {
        assert_eq!(provider_source(inputs), expected, "{inputs:?}");
    }
}

#[test]
fn branded_binary_refuses_only_the_silent_upstream_default() {
    for source in [
        ProviderSource::Required,
        ProviderSource::Override,
        ProviderSource::Configured,
        ProviderSource::SiteAccount,
    ] {
        assert_eq!(check_model_configured(true, source), Ok(()), "{source:?}");
    }
    assert_eq!(
        check_model_configured(true, ProviderSource::UpstreamDefault),
        Err(NO_MODEL_CONFIGURED.to_string())
    );
    // Libraries and upstream tests never set the requirement.
    assert_eq!(
        check_model_configured(false, ProviderSource::UpstreamDefault),
        Ok(())
    );
}

#[tokio::test]
async fn config_records_where_the_provider_came_from() {
    let home = tempfile::TempDir::new().unwrap();
    let config = load(&home, ConfigToml::default(), ConfigOverrides::default()).await;
    assert_eq!(
        config.model_provider_source,
        ProviderSource::UpstreamDefault
    );

    // A client echoing the default provider id back is not a choice; naming another one is.
    let echoed = ConfigOverrides {
        model_provider: Some("openai".to_string()),
        ..ConfigOverrides::default()
    };
    let config = load(&home, ConfigToml::default(), echoed).await;
    assert_eq!(
        config.model_provider_source,
        ProviderSource::UpstreamDefault
    );
    let chosen = ConfigOverrides {
        model_provider: Some("ollama".to_string()),
        ..ConfigOverrides::default()
    };
    let config = load(&home, ConfigToml::default(), chosen).await;
    assert_eq!(config.model_provider_source, ProviderSource::Override);

    let cfg: ConfigToml = toml::from_str("model_provider = \"openai\"").expect("toml");
    let config = load(&home, cfg, ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_source, ProviderSource::Configured);

    sign_in(home.path(), Some("site-model"));
    let config = load(&home, ConfigToml::default(), ConfigOverrides::default()).await;
    assert_eq!(config.model_provider_source, ProviderSource::SiteAccount);
}

#[tokio::test]
async fn site_provider_uses_its_own_catalog_seeded_with_the_suggested_model() {
    let home = tempfile::TempDir::new().unwrap();
    sign_in(home.path(), Some("site-model"));
    let config = load(&home, ConfigToml::default(), ConfigOverrides::default()).await;
    assert_eq!(
        config
            .model_provider
            .model_catalog_url
            .as_deref()
            .map(String::as_str),
        Some("https://onigio.com/agent-relay/v1/models")
    );
    let fallback = own_catalog_fallback(
        &config.model_provider_id,
        &config.model_provider,
        config.model.as_deref(),
        config.model_catalog.as_ref(),
    )
    .expect("site provider serves its own catalog");
    let slugs: Vec<_> = fallback.models.iter().map(|m| m.slug.as_str()).collect();
    assert_eq!(slugs, ["site-model"]);
    assert_eq!(fallback.models[0].visibility, ModelVisibility::List);

    // No suggestion: an empty catalog, still not the upstream presets.
    let empty = own_catalog_fallback(SITE_PROVIDER_ID, &config.model_provider, None, None)
        .expect("site provider");
    assert!(empty.models.is_empty());

    // Any provider naming its own catalog URL gets the same treatment (the cloud sandbox's gateway).
    let gateway = own_catalog_fallback("gateway", &config.model_provider, Some("x"), None)
        .expect("a provider with its own catalog URL");
    assert_eq!(gateway.models.len(), 1);

    // Providers without one, or an explicit model_catalog_json, keep upstream behaviour.
    let mut upstream = config.model_provider.clone();
    upstream.model_catalog_url = None;
    assert_eq!(
        own_catalog_fallback("openai", &upstream, Some("x"), None),
        None
    );
    let configured = ModelsResponse { models: Vec::new() };
    assert_eq!(
        own_catalog_fallback(
            SITE_PROVIDER_ID,
            &config.model_provider,
            Some("x"),
            Some(&configured)
        ),
        None
    );
}

#[test]
fn desktop_custom_model_lists_only_itself() {
    let catalog = desktop_custom_catalog(
        DESKTOP_CUSTOM_PROVIDER_ID,
        Some("claude-opus-5-5"),
        &[],
        None,
    )
    .expect("a custom model has a catalog");
    let slugs: Vec<&str> = catalog.models.iter().map(|m| m.slug.as_str()).collect();
    assert_eq!(slugs, vec!["claude-opus-5-5"]);
    assert_eq!(catalog.models[0].visibility, ModelVisibility::List);
    // other providers, no model, or a catalog the user set keep the usual list
    assert_eq!(
        desktop_custom_catalog("openai", Some("gpt-5.5"), &[], None),
        None
    );
    assert_eq!(
        desktop_custom_catalog(DESKTOP_CUSTOM_PROVIDER_ID, None, &[], None),
        None
    );
    assert_eq!(
        desktop_custom_catalog(DESKTOP_CUSTOM_PROVIDER_ID, Some(" "), &[], None),
        None
    );
    assert_eq!(
        desktop_custom_catalog(DESKTOP_CUSTOM_PROVIDER_ID, Some("m"), &[], Some(&catalog)),
        None
    );
}

#[test]
fn desktop_custom_endpoint_lists_its_other_models_after_the_default() {
    let extra: Vec<String> = [
        "claude-sonnet-5-5",
        " claude-opus-5-5 ",
        "",
        "a\nb",
        "claude-sonnet-5-5",
        " claude-haiku-5-5",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let catalog = desktop_custom_catalog(
        DESKTOP_CUSTOM_PROVIDER_ID,
        Some("claude-opus-5-5"),
        &extra,
        None,
    )
    .expect("a custom model has a catalog");
    let slugs: Vec<&str> = catalog.models.iter().map(|m| m.slug.as_str()).collect();
    // the default first; blanks, control characters and repeats dropped
    assert_eq!(
        slugs,
        vec!["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
    );
    assert!(
        catalog
            .models
            .iter()
            .all(|m| m.visibility == ModelVisibility::List)
    );
    // other providers ignore the list
    assert_eq!(
        desktop_custom_catalog("openai", Some("gpt-5.5"), &extra, None),
        None
    );
    // capped
    let many: Vec<String> = (0..1000).map(|i| format!("m{i}")).collect();
    let capped =
        desktop_custom_catalog(DESKTOP_CUSTOM_PROVIDER_ID, Some("x"), &many, None).unwrap();
    assert_eq!(capped.models.len(), MAX_DESKTOP_CUSTOM_MODELS + 1);
}

#[test]
fn desktop_custom_labels_name_the_models_of_other_endpoints() {
    let extra = vec![
        "custom-b2/deepseek-chat".to_string(),
        "custom-c3/qwen/qwen3".to_string(),
    ];
    let catalog = desktop_custom_catalog(DESKTOP_CUSTOM_PROVIDER_ID, Some("opus"), &extra, None)
        .expect("a custom model has a catalog");
    let labels: HashMap<String, DesktopModelLabel> = serde_json::from_str(
        r#"{"custom-b2/deepseek-chat":{"display_name":"deepseek-chat（DeepSeek）","description":"deepseek-chat"},
            "custom-c3/qwen/qwen3":{"display_name":"  ","description":"a\nb"},
            "not-listed":{"display_name":"x"}}"#,
    )
    .expect("labels parse");
    let named = with_desktop_custom_labels(catalog, &labels);
    let shown: Vec<(&str, &str, Option<&str>)> = named
        .models
        .iter()
        .map(|m| {
            (
                m.slug.as_str(),
                m.display_name.as_str(),
                m.description.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            ("opus", "opus", None),
            (
                "custom-b2/deepseek-chat",
                "deepseek-chat（DeepSeek）",
                Some("deepseek-chat")
            ),
            // a blank name or a description with a control character keeps the slug
            ("custom-c3/qwen/qwen3", "custom-c3/qwen/qwen3", None),
        ]
    );
}
