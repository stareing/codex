//! Credentials and recovery bound to one remote-control login lifetime.
//! A request can refresh credentials, but cannot adopt a replacement authentication owner.

use super::protocol::RemoteControlTarget;
use super::protocol::normalize_remote_control_url_trusting;
use super::site_relay::SiteRelay;
use super::site_relay::SiteRelayAuthProvider;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::UnauthorizedRecovery;
use std::io;
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;

#[derive(Clone)]
pub(super) struct RemoteControlAuth {
    manager: Arc<AuthManager>,
    pub(super) owner: crate::ConnectionAuth,
    pub(super) network_policy: codex_http_client::NetworkPolicy,
    /// Onigio fork: when set, credentials come from the site relay's device token, not ChatGPT.
    site_relay: Option<Arc<SiteRelay>>,
}

pub(super) struct RemoteControlRecovery {
    auth: RemoteControlAuth,
    /// `None` for the site relay: its device token has no refresh step; the launcher replaces it.
    recovery: Option<UnauthorizedRecovery>,
}

impl RemoteControlAuth {
    #[cfg(test)]
    pub(super) fn capture(manager: Arc<AuthManager>) -> (Self, bool) {
        Self::capture_with_site_relay(manager, /*site_relay*/ None)
    }

    pub(super) fn capture_with_site_relay(
        manager: Arc<AuthManager>,
        site_relay: Option<Arc<SiteRelay>>,
    ) -> (Self, bool) {
        loop {
            let owner = crate::ConnectionAuth::capture(&manager);
            let authenticated = match site_relay.as_deref() {
                Some(site_relay) => site_relay.has_token(),
                None => manager.auth_cached().is_some_and(|auth| {
                    auth.uses_codex_backend() && auth.get_account_id().is_some()
                }),
            };
            if owner.is_current() {
                return (
                    Self {
                        network_policy: manager.application_network_policy(),
                        manager,
                        owner,
                        site_relay,
                    },
                    authenticated,
                );
            }
        }
    }

    pub(super) fn ensure_current(&self) -> io::Result<()> {
        self.owner.ensure_current()
    }

    pub(super) fn unauthorized_recovery(&self) -> RemoteControlRecovery {
        RemoteControlRecovery {
            auth: self.clone(),
            recovery: self
                .site_relay
                .is_none()
                .then(|| self.manager.unauthorized_recovery()),
        }
    }

    pub(super) fn site_relay(&self) -> Option<&SiteRelay> {
        self.site_relay.as_deref()
    }

    /// Validates a remote control URL, trusting the site relay's host when one is configured.
    pub(super) fn target(&self, remote_control_url: &str) -> io::Result<RemoteControlTarget> {
        normalize_remote_control_url_trusting(remote_control_url, self.site_relay())
    }

    pub(super) fn auth_change_receiver(&self) -> watch::Receiver<u64> {
        self.manager.auth_change_receiver()
    }
}

pub(super) const REMOTE_CONTROL_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";
/// Account header a site relay receives instead of the upstream one (`site_relay.rs`).
pub(super) const SITE_RELAY_ACCOUNT_ID_HEADER: &str = "x-relay-account-id";

pub(super) struct RemoteControlConnectionAuth {
    pub(super) http_client_factory: codex_http_client::HttpClientFactory,
    pub(super) auth_provider: SharedAuthProvider,
    pub(super) account_id: String,
    pub(super) account_header: &'static str,
}

impl RemoteControlConnectionAuth {
    pub(super) fn request_headers(&self) -> io::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        self.auth_provider.add_auth_headers(&mut headers);
        headers.insert(
            self.account_header,
            HeaderValue::from_str(&self.account_id).map_err(|err| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid remote control account id header: {err}"),
                )
            })?,
        );
        Ok(headers)
    }
}

pub(super) async fn load_remote_control_auth(
    auth: &RemoteControlAuth,
) -> io::Result<RemoteControlConnectionAuth> {
    auth.ensure_current()?;
    let credentials = match auth.site_relay() {
        Some(site_relay) => load_site_relay(&auth.manager, site_relay)?,
        None => load_auth_manager(&auth.manager).await?,
    };
    auth.ensure_current()?;
    Ok(credentials)
}

fn load_site_relay(
    auth_manager: &AuthManager,
    site_relay: &SiteRelay,
) -> io::Result<RemoteControlConnectionAuth> {
    let token = site_relay.read_token()?;
    Ok(RemoteControlConnectionAuth {
        http_client_factory: auth_manager.http_client_factory(),
        auth_provider: Arc::new(SiteRelayAuthProvider::new(&token)?),
        account_id: site_relay.account_id().to_string(),
        account_header: SITE_RELAY_ACCOUNT_ID_HEADER,
    })
}

async fn load_auth_manager(
    auth_manager: &Arc<AuthManager>,
) -> io::Result<RemoteControlConnectionAuth> {
    let mut reloaded = false;
    let (auth, http_client_factory) = loop {
        let Some((auth, http_client_factory)) = auth_manager.auth_with_http_client_factory().await
        else {
            if reloaded {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "remote control requires ChatGPT authentication",
                ));
            }
            auth_manager.reload().await;
            reloaded = true;
            continue;
        };
        if !auth.uses_codex_backend() {
            break (auth, http_client_factory);
        }
        if auth.get_account_id().is_none() && !reloaded {
            auth_manager.reload().await;
            reloaded = true;
            continue;
        }
        break (auth, http_client_factory);
    };

    if !auth.uses_codex_backend() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "remote control requires ChatGPT authentication; API key auth is not supported",
        ));
    }

    Ok(RemoteControlConnectionAuth {
        http_client_factory,
        auth_provider: codex_model_provider::auth_provider_from_auth(&auth),
        account_id: auth.get_account_id().ok_or_else(|| {
            io::Error::new(
                ErrorKind::WouldBlock,
                "remote control enrollment is waiting for a ChatGPT account id",
            )
        })?,
        account_header: REMOTE_CONTROL_ACCOUNT_ID_HEADER,
    })
}

pub(super) async fn request_client(
    factory: &codex_http_client::HttpClientFactory,
    endpoint: &str,
) -> io::Result<codex_http_client::HttpClient> {
    codex_login::default_client::create_client_for_route_without_request_logging_async(
        factory.clone(),
        endpoint.to_string(),
        codex_http_client::ClientRouteClass::Api,
    )
    .await
}

pub(super) fn is_policy_denial(error: &io::Error) -> bool {
    let Some(cause) = error.get_ref() else {
        return false;
    };
    cause.is::<codex_http_client::NetworkPolicyDenied>()
}

pub(super) fn is_auth_error(error: &io::Error) -> bool {
    error.kind() == ErrorKind::PermissionDenied && !is_policy_denial(error)
}

pub(super) fn request_error(error: codex_http_client::HttpError) -> io::Error {
    match error {
        codex_http_client::HttpError::Policy(denied) => {
            io::Error::new(ErrorKind::PermissionDenied, denied)
        }
        error => io::Error::other(error),
    }
}

pub(super) async fn recover_remote_control_auth(
    recovery: &mut RemoteControlRecovery,
    auth_change_rx: &mut watch::Receiver<u64>,
) -> bool {
    if recovery.auth.ensure_current().is_err() {
        return false;
    }
    let Some(auth_recovery) = recovery.recovery.as_mut() else {
        return false;
    };
    if !auth_recovery.has_next() {
        return false;
    }

    let mode = auth_recovery.mode_name();
    let step = auth_recovery.step_name();
    let auth_change_revision_before_recovery = *auth_change_rx.borrow();
    match auth_recovery.next().await {
        Ok(step_result) => {
            if recovery.auth.ensure_current().is_err() {
                return false;
            }
            if step_result.auth_state_changed() == Some(true) {
                mark_recovery_auth_change_seen(
                    auth_change_rx,
                    auth_change_revision_before_recovery,
                );
            }
            info!(
                "remote control auth recovery succeeded: mode={mode}, step={step}, auth_state_changed={:?}",
                step_result.auth_state_changed()
            );
            true
        }
        Err(err) => {
            warn!("remote control auth recovery failed: mode={mode}, step={step}: {err}");
            false
        }
    }
}

pub(super) fn mark_recovery_auth_change_seen(
    auth_change_rx: &mut watch::Receiver<u64>,
    auth_change_revision_before_recovery: u64,
) {
    let auth_change_revision_after_recovery = *auth_change_rx.borrow();
    if auth_change_revision_after_recovery == auth_change_revision_before_recovery.wrapping_add(1) {
        // Recovery updated the same watch that wakes the outer reconnect
        // loop. Mark only that single revision seen; if more revisions
        // arrived while recovery was in flight, leave them pending so the
        // reconnect loop still reacts to the later external auth change.
        auth_change_rx.borrow_and_update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_api::AuthProvider;
    use pretty_assertions::assert_eq;

    #[derive(Debug)]
    struct TestAuthProvider {
        account_ids: Vec<&'static str>,
    }

    impl AuthProvider for TestAuthProvider {
        fn add_auth_headers(&self, headers: &mut HeaderMap) {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer test-token"),
            );
            headers.insert("x-openai-fedramp", HeaderValue::from_static("true"));
            for account_id in &self.account_ids {
                headers.append("ChatGPT-Account-ID", HeaderValue::from_static(account_id));
            }
        }
    }

    fn remote_control_auth(
        account_id: &str,
        provider_account_ids: Vec<&'static str>,
    ) -> RemoteControlConnectionAuth {
        RemoteControlConnectionAuth {
            http_client_factory: codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ),
            auth_provider: Arc::new(TestAuthProvider {
                account_ids: provider_account_ids,
            }),
            account_id: account_id.to_string(),
            account_header: REMOTE_CONTROL_ACCOUNT_ID_HEADER,
        }
    }

    #[test]
    fn request_headers_adds_account_header_when_provider_omits_it() {
        let headers = remote_control_auth("selected-account", Vec::new())
            .request_headers()
            .expect("request headers should build");

        assert_eq!(
            headers
                .get_all(REMOTE_CONTROL_ACCOUNT_ID_HEADER)
                .iter()
                .map(|value| value.to_str().expect("account header should be text"))
                .collect::<Vec<_>>(),
            vec!["selected-account"]
        );
    }

    #[test]
    fn request_headers_replaces_provider_accounts_and_preserves_other_headers() {
        let headers = remote_control_auth(
            "selected-account",
            vec!["provider-account-a", "provider-account-b"],
        )
        .request_headers()
        .expect("request headers should build");

        assert_eq!(
            headers
                .get_all(REMOTE_CONTROL_ACCOUNT_ID_HEADER)
                .iter()
                .map(|value| value.to_str().expect("account header should be text"))
                .collect::<Vec<_>>(),
            vec!["selected-account"]
        );
        assert_eq!(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(
            headers
                .get("x-openai-fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn request_headers_rejects_invalid_account_header_value() {
        let err = remote_control_auth("invalid\naccount", Vec::new())
            .request_headers()
            .expect_err("invalid account header should fail");

        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string()
                .starts_with("invalid remote control account id header:")
        );
    }
}
