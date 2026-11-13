//! Model requests through the site the user signed in to.
//!
//! When `login` saved a site account under `CODEX_HOME` (or the launcher named a relay with the
//! `SITE_RELAY_*` variables), the relay also serves the Responses API at `<relay>v1`. This module
//! turns that sign-in into a model provider with id [`SITE_PROVIDER_ID`] and makes it the default,
//! so a signed-in user never sees the upstream login screen (`requires_openai_auth = false`).
//!
//! Precedence: an explicitly chosen `model_provider` (config, profile, CLI override or managed
//! requirement) always wins, and a user-defined `[model_providers.onigio]` is never replaced.
//! The account's suggested `model` applies only when the site provider is in use and nothing
//! chose a model.
//!
//! The file names and variable names mirror `codex_app_server_transport` (`SITE_ACCOUNT_FILE`,
//! `SITE_ACCOUNT_TOKEN_FILE`, `SITE_RELAY_*`), which `core` cannot depend on; the CLI test
//! `site_account_names_match_core` pins them together.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_config::config_toml::ConfigToml;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_model_provider_info::WireApi;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use url::Url;

pub const SITE_PROVIDER_ID: &str = "onigio";
pub const SITE_PROVIDER_NAME: &str = "Onigio";
pub const SITE_ACCOUNT_FILE: &str = "site-account.json";
pub const SITE_ACCOUNT_TOKEN_FILE: &str = "site-account.token";
pub const SITE_RELAY_URL_ENV_VAR: &str = "SITE_RELAY_URL";
pub const SITE_RELAY_TOKEN_FILE_ENV_VAR: &str = "SITE_RELAY_TOKEN_FILE";
pub const SITE_RELAY_ACCOUNT_ID_ENV_VAR: &str = "SITE_RELAY_ACCOUNT_ID";
/// Header the relay requires next to the bearer device token.
pub const RELAY_ACCOUNT_HEADER: &str = "x-relay-account-id";

const MAX_TOKEN_BYTES: u64 = 4096;
const MAX_ACCOUNT_FILE_BYTES: u64 = 16 * 1024;
const MAX_ACCOUNT_ID_CHARS: usize = 64;

/// What the model provider needs from a site sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SiteModelAccess {
    relay_url: String,
    account_id: String,
    /// `None` when the token file is missing or unusable: the provider is still installed (so the
    /// upstream login screen stays away) and the relay answers 401, telling the user to sign in.
    token: Option<String>,
    model: Option<String>,
}

#[derive(serde::Deserialize)]
struct SavedAccount {
    relay_url: String,
    account_id: String,
    #[serde(default)]
    model: Option<String>,
}

impl SiteModelAccess {
    /// The launcher's variables win over the saved account, as in the transport's
    /// `SiteRelay::from_env_or_account`. Invalid or partial settings are ignored with a warning.
    pub(crate) fn load(codex_home: &Path) -> Option<Self> {
        Self::load_with(codex_home, |name| std::env::var_os(name))
    }

    fn load_with(codex_home: &Path, lookup: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        match Self::from_env(lookup) {
            Ok(Some(access)) => return Some(access),
            Ok(None) => {}
            Err(reason) => {
                tracing::warn!("ignoring the SITE_RELAY_* settings for model requests: {reason}");
            }
        }
        match Self::from_saved_account(codex_home) {
            Ok(access) => access,
            Err(reason) => {
                tracing::warn!("ignoring the saved site account for model requests: {reason}");
                None
            }
        }
    }

    fn from_env(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Option<Self>, String> {
        let names = [
            SITE_RELAY_URL_ENV_VAR,
            SITE_RELAY_TOKEN_FILE_ENV_VAR,
            SITE_RELAY_ACCOUNT_ID_ENV_VAR,
        ];
        let values = names.map(|name| lookup(name).filter(|value| !value.is_empty()));
        let [url, token_file, account_id] = match values {
            [None, None, None] => return Ok(None),
            [Some(url), Some(token_file), Some(account_id)] => [url, token_file, account_id],
            _ => return Err(format!("set all of {} or none of them", names.join(", "))),
        };
        let text = |value: OsString| {
            value
                .into_string()
                .map_err(|_| "a value is not valid UTF-8".to_string())
        };
        Self::new(
            &text(url)?,
            &text(account_id)?,
            read_token(&PathBuf::from(token_file)),
            None,
        )
        .map(Some)
    }

    fn from_saved_account(codex_home: &Path) -> Result<Option<Self>, String> {
        let path = codex_home.join(SITE_ACCOUNT_FILE);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let mut raw = String::new();
        file.take(MAX_ACCOUNT_FILE_BYTES)
            .read_to_string(&mut raw)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let saved: SavedAccount = serde_json::from_str(&raw)
            .map_err(|err| format!("{} is not a valid site account: {err}", path.display()))?;
        let token = read_token(&codex_home.join(SITE_ACCOUNT_TOKEN_FILE));
        if token.is_none() {
            tracing::warn!(
                "the site sign-in token is missing or unreadable; model requests will be refused until you sign in again"
            );
        }
        Self::new(&saved.relay_url, &saved.account_id, token, saved.model).map(Some)
    }

    fn new(
        relay_url: &str,
        account_id: &str,
        token: Option<String>,
        model: Option<String>,
    ) -> Result<Self, String> {
        let parsed = Url::parse(relay_url).map_err(|_| "relay URL does not parse".to_string())?;
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
            return Err("relay URL must be HTTPS (HTTP only on loopback)".to_string());
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err("relay URL must not carry credentials, a query or a fragment".to_string());
        }
        if account_id.is_empty()
            || account_id.chars().count() > MAX_ACCOUNT_ID_CHARS
            || !account_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("account id must be 1-64 characters of [A-Za-z0-9_-]".to_string());
        }
        Ok(Self {
            relay_url: relay_url.to_string(),
            account_id: account_id.to_string(),
            token,
            model: model
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty()),
        })
    }

    /// `<relay>/v1`, whether or not the relay URL ends with a slash.
    fn base_url(&self) -> String {
        format!("{}/v1", self.relay_url.trim_end_matches('/'))
    }

    pub(crate) fn provider(&self) -> ModelProviderInfo {
        ModelProviderInfo {
            name: SITE_PROVIDER_NAME.to_string(),
            base_url: Some(self.base_url()),
            wire_api: WireApi::Responses,
            experimental_bearer_token: self.token.clone().map(Into::into),
            http_headers: Some(HashMap::from([(
                RELAY_ACCOUNT_HEADER.to_string(),
                self.account_id.clone().into(),
            )])),
            // Global retry and stream-idle defaults, like the built-in providers.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            // The relay serves plain HTTP streaming only.
            supports_websockets: false,
            supports_standalone_web_search: false,
            model_catalog_url: Some(self.catalog_url().into()),
            ..ModelProviderInfo::default()
        }
    }

    /// `<relay>/v1/models`: the relay's catalog in the Codex-native `ModelsResponse` format.
    fn catalog_url(&self) -> String {
        format!("{}/models", self.base_url())
    }
}

/// Where the active model provider came from; decided by [`PROVIDER_SOURCE_RULES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    /// A managed requirement names the provider.
    Required,
    /// A command-line override (`--oss`, app-server request) names it.
    Override,
    /// `model_provider` in config.toml, a profile or `-c`.
    Configured,
    /// Nothing chose one and this computer is signed in to the site.
    SiteAccount,
    /// Nothing chose one and there is no sign-in: upstream would silently use its own default.
    UpstreamDefault,
}

#[derive(Debug, Clone, Copy)]
struct ProviderInputs {
    required: bool,
    overridden: bool,
    configured: bool,
    signed_in: bool,
}

type ProviderSourceRule = (fn(ProviderInputs) -> bool, ProviderSource);

/// First matching rule wins; [`ProviderSource::UpstreamDefault`] when none matches.
const PROVIDER_SOURCE_RULES: &[ProviderSourceRule] = &[
    (|inputs| inputs.required, ProviderSource::Required),
    (|inputs| inputs.overridden, ProviderSource::Override),
    (|inputs| inputs.configured, ProviderSource::Configured),
    (|inputs| inputs.signed_in, ProviderSource::SiteAccount),
];

fn provider_source(inputs: ProviderInputs) -> ProviderSource {
    PROVIDER_SOURCE_RULES
        .iter()
        .find(|(applies, _)| applies(inputs))
        .map_or(ProviderSource::UpstreamDefault, |(_, source)| *source)
}

/// Installs the site provider when signed in (a user-defined `onigio` provider is kept), makes it
/// the default when nothing else chose one, and fills in the site's suggested model when the site
/// provider is in use and no model is configured. `required` and `overridden` are the provider
/// ids picked outside `cfg`.
pub(crate) fn resolve_provider(
    cfg: &mut ConfigToml,
    site: Option<&SiteModelAccess>,
    required: Option<&str>,
    overridden: Option<&str>,
) -> ProviderSource {
    let source = provider_source(ProviderInputs {
        required: required.is_some(),
        // Clients (exec, the TUI) echo the provider id they loaded back in `thread/start`; an
        // override naming the built-in default is that echo, not a choice.
        overridden: overridden.is_some_and(|id| id != OPENAI_PROVIDER_ID),
        configured: cfg.model_provider.is_some(),
        signed_in: site.is_some(),
    });
    let Some(site) = site else {
        return source;
    };
    cfg.model_providers
        .entry(SITE_PROVIDER_ID.to_string())
        .or_insert_with(|| site.provider());
    if source == ProviderSource::SiteAccount {
        cfg.model_provider = Some(SITE_PROVIDER_ID.to_string());
    }
    let effective = required.or(overridden).or(cfg.model_provider.as_deref());
    if effective == Some(SITE_PROVIDER_ID) && cfg.model.is_none() {
        cfg.model = site.model.clone();
    }
    source
}

/// Whether [`SITE_PROVIDER_ID`] will be in the loaded config (a site sign-in under `codex_home` or
/// the launcher's `SITE_RELAY_*` variables). Checks that read the raw config layers, before
/// [`resolve_provider`] adds the site provider, use this to accept it.
pub fn site_provider_available(codex_home: &Path) -> bool {
    SiteModelAccess::load(codex_home).is_some()
}

/// Shown when a branded binary has neither a sign-in nor a configured provider.
pub const NO_MODEL_CONFIGURED: &str = "No model configured. Sign in to use Onigio's models: \
`onigio login` (or `login --password`), or set `model_provider` and `model` in \
config.toml to use your own.";

static REQUIRE_CHOSEN_PROVIDER: AtomicBool = AtomicBool::new(false);

/// Called once by the shipped binary: sessions then refuse to start on the upstream default
/// provider (see [`ensure_model_configured`]). Libraries and tests keep upstream behaviour.
pub fn require_chosen_model_provider() {
    REQUIRE_CHOSEN_PROVIDER.store(true, Ordering::SeqCst);
}

/// The gate at thread start; `Err` carries [`NO_MODEL_CONFIGURED`].
pub fn ensure_model_configured(source: ProviderSource) -> Result<(), String> {
    check_model_configured(REQUIRE_CHOSEN_PROVIDER.load(Ordering::SeqCst), source)
}

fn check_model_configured(required: bool, source: ProviderSource) -> Result<(), String> {
    if required && source == ProviderSource::UpstreamDefault {
        Err(NO_MODEL_CONFIGURED.to_string())
    } else {
        Ok(())
    }
}

/// For a provider that names its own `model_catalog_url` (the site provider, or the site's model
/// gateway inside a cloud sandbox) and has no `model_catalog_json`: the catalog to show until (or
/// instead of) the provider's own list — just the suggested model, never the bundled upstream
/// presets, which that provider does not serve.
pub fn own_catalog_fallback(
    _provider_id: &str,
    provider: &ModelProviderInfo,
    model: Option<&str>,
    configured_catalog: Option<&ModelsResponse>,
) -> Option<ModelsResponse> {
    if provider.model_catalog_url.is_none() || configured_catalog.is_some() {
        return None;
    }
    let models = model
        .map(|slug| ModelInfo {
            visibility: ModelVisibility::List,
            ..model_info_from_slug(slug)
        })
        .into_iter()
        .collect();
    Some(ModelsResponse { models })
}

/// The device token, or `None` when missing, empty, too long or not visible ASCII.
fn read_token(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut raw = String::new();
    file.take(MAX_TOKEN_BYTES + 1)
        .read_to_string(&mut raw)
        .ok()?;
    if raw.len() as u64 > MAX_TOKEN_BYTES {
        return None;
    }
    let token = raw.trim();
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| token.to_string())
}

#[cfg(test)]
#[path = "site_account_provider_tests.rs"]
mod tests;
