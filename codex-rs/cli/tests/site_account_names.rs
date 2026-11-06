//! `codex_core` reads the site sign-in for model requests without depending on the transport
//! crate that writes it; these checks keep the file and variable names of the two in step.

use codex_app_server_transport::SiteAccount;
use codex_app_server_transport::SiteRelay;
use codex_core::config::site_account_provider as core_site;
use std::ffi::OsString;

#[test]
fn saved_account_files_have_the_names_core_reads() {
    let home = tempfile::TempDir::new().unwrap();
    let account = SiteAccount {
        site_url: "https://onigio.com".into(),
        relay_url: "https://onigio.com/agent-relay/".into(),
        account_id: "u7".into(),
        device_id: 3,
        name: "studio".into(),
        model: Some("site-model".into()),
    };
    account.save(home.path(), "oad_secret").unwrap();
    assert!(home.path().join(core_site::SITE_ACCOUNT_FILE).is_file());
    assert_eq!(
        SiteAccount::token_path(home.path()),
        home.path().join(core_site::SITE_ACCOUNT_TOKEN_FILE)
    );
}

#[test]
fn launcher_variables_have_the_names_core_reads() {
    let home = tempfile::TempDir::new().unwrap();
    let token_file = home.path().join("t");
    let lookup = |name: &str| -> Option<OsString> {
        match name {
            core_site::SITE_RELAY_URL_ENV_VAR => Some("https://onigio.com/agent-relay/".into()),
            core_site::SITE_RELAY_TOKEN_FILE_ENV_VAR => Some(token_file.clone().into_os_string()),
            core_site::SITE_RELAY_ACCOUNT_ID_ENV_VAR => Some("u7".into()),
            _ => None,
        }
    };
    let relay = SiteRelay::from_lookup(lookup).unwrap().expect("all three variables recognised");
    assert_eq!(relay.url(), "https://onigio.com/agent-relay/");
}
