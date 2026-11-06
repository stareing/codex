//! Site relay: remote control through a relay run by the site the user signed in to, instead of
//! the upstream hosted service.
//!
//! The desktop client launches `app-server --remote-control` with the three `SITE_RELAY_*`
//! variables. Enroll and refresh then go to the named relay (any HTTPS host, or HTTP on loopback)
//! with `Authorization: Bearer <device token>` and the account in `x-relay-account-id`; no upstream
//! account sign-in is needed.
//! The token is re-read from its file for every request, so the client can rotate it in place.
//! When none of the variables is set, remote control behaves exactly as upstream.

use axum::http::HeaderMap;
use axum::http::HeaderValue;
use codex_api::AuthProvider;
use std::ffi::OsString;
use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use url::Host;
use url::Url;

pub const SITE_RELAY_URL_ENV_VAR: &str = "SITE_RELAY_URL";
pub const SITE_RELAY_TOKEN_FILE_ENV_VAR: &str = "SITE_RELAY_TOKEN_FILE";
pub const SITE_RELAY_ACCOUNT_ID_ENV_VAR: &str = "SITE_RELAY_ACCOUNT_ID";

const MAX_TOKEN_BYTES: u64 = 4096;
const MAX_ACCOUNT_ID_CHARS: usize = 64;

/// A relay named by the launcher; built only through [`SiteRelay::new`], so every instance is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRelay {
    url: String,
    host: Host<String>,
    token_file: PathBuf,
    account_id: String,
}

impl SiteRelay {
    /// `Ok(None)` when no variable is set; an error when only some are, or a value is invalid.
    pub fn from_env() -> io::Result<Option<Self>> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// The variables win; otherwise the account saved by `login` under `codex_home`, so a daemon
    /// started without the launcher's environment still reaches the relay.
    pub fn from_env_or_account(codex_home: &Path) -> io::Result<Option<Self>> {
        if let Some(relay) = Self::from_env()? {
            return Ok(Some(relay));
        }
        SiteAccount::load(codex_home)?
            .map(|account| account.relay(codex_home))
            .transpose()
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> io::Result<Option<Self>> {
        let names = [
            SITE_RELAY_URL_ENV_VAR,
            SITE_RELAY_TOKEN_FILE_ENV_VAR,
            SITE_RELAY_ACCOUNT_ID_ENV_VAR,
        ];
        let values = names.map(|name| lookup(name).filter(|value| !value.is_empty()));
        match values {
            [None, None, None] => Ok(None),
            [Some(url), Some(token_file), Some(account_id)] => {
                let text = |name: &str, value: OsString| {
                    value.into_string().map_err(|_| {
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            format!("{name} is not valid UTF-8"),
                        )
                    })
                };
                Self::new(
                    &text(SITE_RELAY_URL_ENV_VAR, url)?,
                    PathBuf::from(token_file),
                    &text(SITE_RELAY_ACCOUNT_ID_ENV_VAR, account_id)?,
                )
                .map(Some)
            }
            _ => Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("set all of {} or none of them", names.join(", ")),
            )),
        }
    }

    pub fn new(url: &str, token_file: PathBuf, account_id: &str) -> io::Result<Self> {
        let invalid = |reason: &str| io::Error::new(ErrorKind::InvalidInput, reason.to_string());
        let parsed = Url::parse(url).map_err(|_| invalid("site relay URL does not parse"))?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(invalid(
                "site relay URL must not carry credentials, a query or a fragment",
            ));
        }
        let host = parsed
            .host()
            .map(|host| host.to_owned())
            .ok_or_else(|| invalid("site relay URL has no host"))?;
        match parsed.scheme() {
            "https" => {}
            "http" if is_loopback(&host) => {}
            _ => {
                return Err(invalid(
                    "site relay URL must be HTTPS (HTTP only on loopback)",
                ));
            }
        }
        if account_id.is_empty()
            || account_id.chars().count() > MAX_ACCOUNT_ID_CHARS
            || !account_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(invalid(
                "site relay account id must be 1-64 characters of [A-Za-z0-9_-]",
            ));
        }
        if !token_file.is_absolute() {
            return Err(invalid("site relay token file must be an absolute path"));
        }
        Ok(Self {
            url: url.to_string(),
            host,
            token_file,
            account_id: account_id.to_string(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub(super) fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The relay's own host is trusted for remote control in addition to upstream's allowlist.
    pub(super) fn trusts(&self, host: &Option<Host<&str>>) -> bool {
        host.as_ref()
            .is_some_and(|host| host.to_owned() == self.host)
    }

    /// Whether a usable token is present right now (the session's "signed in" state).
    pub(super) fn has_token(&self) -> bool {
        self.read_token().is_ok()
    }

    /// Reads the device token; `PermissionDenied` when it is missing, empty or malformed, so the
    /// transport treats it like a signed-out upstream account and waits instead of retrying hot.
    pub(super) fn read_token(&self) -> io::Result<String> {
        let denied = |reason: &str| {
            io::Error::new(
                ErrorKind::PermissionDenied,
                format!("site relay device token {reason}"),
            )
        };
        let file = std::fs::File::open(&self.token_file).map_err(|_| denied("is unavailable"))?;
        let mut raw = String::new();
        file.take(MAX_TOKEN_BYTES + 1)
            .read_to_string(&mut raw)
            .map_err(|_| denied("is not readable text"))?;
        if raw.len() as u64 > MAX_TOKEN_BYTES {
            return Err(denied("is too long"));
        }
        let token = raw.trim();
        if token.is_empty() {
            return Err(denied("is empty"));
        }
        if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(denied("contains characters outside visible ASCII"));
        }
        Ok(token.to_string())
    }
}

fn is_loopback(host: &Host<String>) -> bool {
    match host {
        Host::Domain(domain) => domain == "localhost",
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Bearer auth with the device token read when the connection auth was loaded.
#[derive(Debug)]
pub(super) struct SiteRelayAuthProvider {
    pub(super) authorization: HeaderValue,
}

impl SiteRelayAuthProvider {
    pub(super) fn new(token: &str) -> io::Result<Self> {
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                io::Error::new(
                    ErrorKind::PermissionDenied,
                    "site relay device token is not a header value",
                )
            })?;
        authorization.set_sensitive(true);
        Ok(Self { authorization })
    }
}

impl AuthProvider for SiteRelayAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(
            axum::http::header::AUTHORIZATION,
            self.authorization.clone(),
        );
    }
}

#[cfg(test)]
#[path = "site_relay_tests.rs"]
mod site_relay_tests;

/// File names under `codex_home` written by `login` (the single definition of the signed-in state).
pub const SITE_ACCOUNT_FILE: &str = "site-account.json";
pub const SITE_ACCOUNT_TOKEN_FILE: &str = "site-account.token";
const MAX_ACCOUNT_FILE_BYTES: u64 = 16 * 1024;

/// The site account this computer is signed in to: the exchange response of the site's
/// `/api/agent-devices/token/`, minus the token, which lives in its own owner-only file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SiteAccount {
    /// Site origin, e.g. `https://onigio.com`; the session link and API calls use it.
    pub site_url: String,
    pub relay_url: String,
    pub account_id: String,
    pub device_id: u64,
    pub name: String,
}

impl SiteAccount {
    pub fn token_path(codex_home: &Path) -> PathBuf {
        codex_home.join(SITE_ACCOUNT_TOKEN_FILE)
    }

    /// `Ok(None)` when this computer has not signed in.
    pub fn load(codex_home: &Path) -> io::Result<Option<Self>> {
        let path = codex_home.join(SITE_ACCOUNT_FILE);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut raw = String::new();
        file.take(MAX_ACCOUNT_FILE_BYTES).read_to_string(&mut raw)?;
        serde_json::from_str(&raw).map(Some).map_err(|err| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("{} is not a valid site account: {err}", path.display()),
            )
        })
    }

    /// Writes the token (owner-only) first, then the account, so a half-written sign-in never
    /// looks signed in.
    pub fn save(&self, codex_home: &Path, token: &str) -> io::Result<()> {
        std::fs::create_dir_all(codex_home)?;
        write_private(&Self::token_path(codex_home), token.as_bytes())?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_private(&codex_home.join(SITE_ACCOUNT_FILE), &json)
    }

    /// Forgets the sign-in; missing files are fine.
    pub fn remove(codex_home: &Path) -> io::Result<()> {
        for path in [
            codex_home.join(SITE_ACCOUNT_FILE),
            Self::token_path(codex_home),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    pub fn relay(&self, codex_home: &Path) -> io::Result<SiteRelay> {
        SiteRelay::new(
            &self.relay_url,
            Self::token_path(codex_home),
            &self.account_id,
        )
    }

    /// Where a phone or browser opens this computer's sessions.
    pub fn session_url(&self) -> String {
        format!(
            "{}/ask?mode=agent&device={}",
            self.site_url.trim_end_matches('/'),
            self.device_id
        )
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
