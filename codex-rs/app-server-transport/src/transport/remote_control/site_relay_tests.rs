use super::*;
use crate::transport::remote_control::auth::REMOTE_CONTROL_ACCOUNT_ID_HEADER;
use crate::transport::remote_control::auth::RemoteControlAuth;
use crate::transport::remote_control::auth::load_remote_control_auth;
use crate::transport::remote_control::auth::recover_remote_control_auth;
use crate::transport::remote_control::protocol::normalize_remote_control_url;
use crate::transport::remote_control::protocol::normalize_remote_control_url_trusting;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

fn relay(dir: &TempDir) -> SiteRelay {
    SiteRelay::new(
        "https://relay.onigio.com/",
        dir.path().join("device-token"),
        "u42",
    )
    .expect("relay should be valid")
}

fn lookup(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let values: HashMap<String, OsString> = values
        .iter()
        .map(|(name, value)| (name.to_string(), OsString::from(value)))
        .collect();
    move |name| values.get(name).cloned()
}

async fn signed_out_auth_manager(codex_home: &TempDir) -> Arc<AuthManager> {
    AuthManager::shared(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await
}

#[test]
fn environment_selects_the_relay_only_when_every_variable_is_set() {
    assert_eq!(
        SiteRelay::from_lookup(lookup(&[])).expect("none is fine"),
        None
    );
    assert_eq!(
        SiteRelay::from_lookup(lookup(&[(SITE_RELAY_URL_ENV_VAR, "")])).expect("empty is unset"),
        None
    );

    let token_file = std::env::temp_dir().join("device-token");
    let token_file = token_file.to_str().expect("temp path should be UTF-8");
    let partial = SiteRelay::from_lookup(lookup(&[
        (SITE_RELAY_URL_ENV_VAR, "https://relay.onigio.com/"),
        (SITE_RELAY_TOKEN_FILE_ENV_VAR, token_file),
    ]))
    .expect_err("a partial configuration must not start");
    assert_eq!(partial.kind(), ErrorKind::InvalidInput);

    let relay = SiteRelay::from_lookup(lookup(&[
        (SITE_RELAY_URL_ENV_VAR, "https://relay.onigio.com/"),
        (SITE_RELAY_TOKEN_FILE_ENV_VAR, token_file),
        (SITE_RELAY_ACCOUNT_ID_ENV_VAR, "u42"),
    ]))
    .expect("full configuration should parse")
    .expect("full configuration selects the relay");
    assert_eq!(relay.url(), "https://relay.onigio.com/");
    assert_eq!(relay.account_id(), "u42");
}

#[test]
fn relay_settings_are_validated() {
    let token_file = std::env::temp_dir().join("device-token");
    let accepted = [
        ("https://relay.onigio.com/", "u42"),
        ("https://relay.onigio.com:8443/base/", "user_1-a"),
        ("http://localhost:9000/", "u1"),
        ("http://127.0.0.1:9000/", "u1"),
        ("http://[::1]:9000/", "u1"),
    ];
    for (url, account_id) in accepted {
        SiteRelay::new(url, token_file.clone(), account_id)
            .unwrap_or_else(|err| panic!("{url} / {account_id} should be accepted: {err}"));
    }

    let rejected = [
        ("http://relay.onigio.com/", "u42"),
        ("ftp://relay.onigio.com/", "u42"),
        ("https://user:pass@relay.onigio.com/", "u42"),
        ("https://relay.onigio.com/?token=x", "u42"),
        ("https://relay.onigio.com/#x", "u42"),
        ("not a url", "u42"),
        ("https://relay.onigio.com/", ""),
        ("https://relay.onigio.com/", "u 42"),
        ("https://relay.onigio.com/", "u42\n"),
        ("https://relay.onigio.com/", &"u".repeat(65)),
    ];
    for (url, account_id) in rejected {
        let err = SiteRelay::new(url, token_file.clone(), account_id)
            .expect_err(&format!("{url} / {account_id:?} should be rejected"));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    let relative = SiteRelay::new("https://relay.onigio.com/", PathBuf::from("token"), "u42")
        .expect_err("a relative token path should be rejected");
    assert_eq!(relative.kind(), ErrorKind::InvalidInput);
}

#[test]
fn device_token_is_read_trimmed_and_refused_when_unusable() {
    let dir = TempDir::new().expect("temp dir should create");
    let relay = relay(&dir);
    let token_file = dir.path().join("device-token");

    let missing = relay.read_token().expect_err("a missing token is refused");
    assert_eq!(missing.kind(), ErrorKind::PermissionDenied);
    assert!(!relay.has_token());

    std::fs::write(&token_file, "dt_abc.123\n").expect("token should write");
    assert_eq!(relay.read_token().expect("token should read"), "dt_abc.123");
    assert!(relay.has_token());

    for unusable in [
        "".to_string(),
        "  \n".to_string(),
        "two words".to_string(),
        "tökén".to_string(),
        "x".repeat(MAX_TOKEN_BYTES as usize + 1),
    ] {
        std::fs::write(&token_file, &unusable).expect("token should write");
        let err = relay
            .read_token()
            .expect_err(&format!("{:?} should be refused", unusable.get(..20)));
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        // Fixed messages only: the token itself never reaches logs.
        assert!(err.to_string().starts_with("site relay device token "));
    }
}

#[test]
fn only_the_configured_relay_host_is_added_to_the_allowlist() {
    let dir = TempDir::new().expect("temp dir should create");
    let relay = relay(&dir);

    normalize_remote_control_url("https://relay.onigio.com/")
        .expect_err("upstream still rejects hosts outside chatgpt.com");
    let target = normalize_remote_control_url_trusting("https://relay.onigio.com/", Some(&relay))
        .expect("the configured relay is trusted");
    assert_eq!(
        target.websocket_url,
        "wss://relay.onigio.com/wham/remote/control/server"
    );
    assert_eq!(
        target.enroll_url,
        "https://relay.onigio.com/wham/remote/control/server/enroll"
    );
    normalize_remote_control_url_trusting("https://other.onigio.com/", Some(&relay))
        .expect_err("another host is still rejected");
    normalize_remote_control_url_trusting("http://relay.onigio.com/", Some(&relay))
        .expect_err("plain HTTP to the relay host is still rejected");
}

#[tokio::test]
async fn site_relay_auth_uses_the_device_token_without_a_chatgpt_login() {
    let dir = TempDir::new().expect("temp dir should create");
    let codex_home = TempDir::new().expect("temp dir should create");
    let relay = Arc::new(relay(&dir));
    let manager = signed_out_auth_manager(&codex_home).await;

    let (auth, authenticated) =
        RemoteControlAuth::capture_with_site_relay(manager.clone(), Some(relay.clone()));
    assert!(!authenticated, "no token file yet");
    let missing = load_remote_control_auth(&auth)
        .await
        .err()
        .expect("loading without a token fails");
    assert_eq!(missing.kind(), ErrorKind::PermissionDenied);

    std::fs::write(dir.path().join("device-token"), "dt_first\n").expect("token should write");
    let (auth, authenticated) =
        RemoteControlAuth::capture_with_site_relay(manager.clone(), Some(relay.clone()));
    assert!(authenticated);
    let headers = load_remote_control_auth(&auth)
        .await
        .expect("credentials should load")
        .request_headers()
        .expect("headers should build");
    assert_eq!(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer dt_first")
    );
    assert_eq!(
        headers
            .get(REMOTE_CONTROL_ACCOUNT_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("u42")
    );

    // The launcher rotates the token in place; the next load sends the new one.
    std::fs::write(dir.path().join("device-token"), "dt_second").expect("token should write");
    let headers = load_remote_control_auth(&auth)
        .await
        .expect("credentials should load")
        .request_headers()
        .expect("headers should build");
    assert_eq!(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer dt_second")
    );

    // A rejected device token has no refresh step: recovery gives up instead of looping.
    let mut recovery = auth.unauthorized_recovery();
    let mut auth_change_rx = auth.auth_change_receiver();
    assert!(!recover_remote_control_auth(&mut recovery, &mut auth_change_rx).await);
}

#[tokio::test]
async fn chatgpt_mode_is_unchanged_when_no_relay_is_configured() {
    let manager = codex_core::test_support::auth_manager_from_auth(
        codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing(),
    );
    let (auth, authenticated) = RemoteControlAuth::capture(manager);
    assert!(authenticated);
    assert!(auth.site_relay().is_none());
    normalize_remote_control_url("https://chatgpt.com/backend-api")
        .expect("upstream hosts still pass");
}
