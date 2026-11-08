//! The site's `/api/agent-catalog/` endpoints, called with this computer's device token.
//!
//! Reads are retried a bounded number of times on network errors and 502/503/504; writes are
//! retried only when the connection could not be made (the request never reached the site).
//! Error bodies `{code, detail}` become a [`CatalogError`] whose message says what to do next.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use super::spec::BundleSpec;
use super::spec::InstallSpec;
use super::spec::MAX_BUNDLE_BYTES;
use super::spec::acceptable_url;

const API_PREFIX: &str = "/api/agent-catalog";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Attempts per request, the first included.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const RETRYABLE_STATUSES: &[u16] = &[502, 503, 504];
const MAX_CANDIDATES: usize = 20;
const MAX_DETAIL_CHARS: usize = 300;

/// What to do next, by refusal code (codes are the site's `agent_catalog/refusals.py`, plus the
/// local ones this CLI raises).
const HINTS: &[(&str, &str)] = &[
    ("not_signed_in", "Sign in first: run `onigio-agent login`."),
    (
        "unauthorized",
        "The site no longer accepts this computer's sign-in; run `onigio-agent login` again.",
    ),
    (
        "not_found",
        "Nothing in the catalog matches; try `onigio-agent search <words>`.",
    ),
    (
        "ambiguous_ref",
        "Several items have that name; pick one of the references below.",
    ),
    (
        "unsupported_transport",
        "This server's transport is not supported by the agent.",
    ),
    (
        "throttled",
        "Too many requests; wait a minute and try again.",
    ),
    ("network_error", "Check the connection and try again."),
    (
        "download_token_invalid",
        "The download link expired or was already used; run install again.",
    ),
    (
        "download_unavailable",
        "This version can no longer be downloaded from the site.",
    ),
    (
        "no_version",
        "There is no published version to install yet.",
    ),
    (
        "invalid_template",
        "The item's launch settings are incomplete on the site; it cannot be installed yet.",
    ),
    // Tool directory refusals (`tools:` references); the site's `detail` says why.
    (
        "unsupported_registry",
        "Only npm and PyPI packages from the tool directory can be installed; set it up by hand from its source instead.",
    ),
    (
        "unpinned_version",
        "Only entries with an exact package version can be installed.",
    ),
    (
        "needs_input",
        "It needs settings this command cannot ask for yet; set it up by hand from its source.",
    ),
    (
        "needs_review",
        "It has to be reviewed before it can be installed from the tool directory.",
    ),
    ("no_install_target", "The listing names nothing to install."),
    (
        "pending_sync",
        "Try again after the site's next directory sync.",
    ),
    (
        "not_installable",
        "Follow the official instructions to install it.",
    ),
    (
        "query_too_short",
        "Searching the tool directory needs at least 3 characters.",
    ),
];

/// A failed catalog operation: a refusal code, readable detail, and (for `ambiguous_ref`) the
/// references the user can choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogError {
    pub(crate) status: Option<u16>,
    pub(crate) code: String,
    pub(crate) detail: String,
    pub(crate) candidates: Vec<String>,
    /// For `not_installable`: the official install instructions, when known.
    pub(crate) instructions_url: Option<String>,
}

impl CatalogError {
    pub(crate) fn local(code: &str, detail: impl Into<String>) -> Self {
        Self {
            status: None,
            code: code.to_string(),
            detail: detail.into(),
            candidates: Vec::new(),
            instructions_url: None,
        }
    }

    /// From a non-2xx answer; the code falls back to the status when the body has none.
    pub(crate) fn from_response(status: u16, body: &Value) -> Self {
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(clean);
        let code = match status {
            401 => "unauthorized".to_string(),
            _ => text("code")
                .filter(|code| !code.is_empty())
                .unwrap_or_else(|| match status {
                    404 => "not_found".to_string(),
                    429 => "throttled".to_string(),
                    _ => format!("http_{status}"),
                }),
        };
        let candidates = body
            .get("candidates")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .or_else(|| item.get("ref").and_then(Value::as_str))
                            .map(clean)
                    })
                    .take(MAX_CANDIDATES)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            status: Some(status),
            code,
            detail: text("detail").unwrap_or_default(),
            candidates,
            instructions_url: None,
        }
    }

    pub(crate) fn message(&self) -> String {
        let hint = HINTS
            .iter()
            .find(|(code, _)| *code == self.code)
            .map(|(_, hint)| *hint);
        match (self.detail.is_empty(), hint) {
            (true, Some(hint)) => hint.to_string(),
            (false, Some(hint)) => format!("{} {hint}", self.detail),
            (true, None) => format!("the request failed ({})", self.code),
            (false, None) => self.detail.clone(),
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Server text shown to the user: one line, bounded.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_DETAIL_CHARS)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct SearchHit {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) slug: String,
    #[serde(default)]
    pub(crate) name: String,
    /// `null` for some tool directory rows.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) trust: String,
    /// `null` for tool directory rows (the site does not count those).
    #[serde(default)]
    pub(crate) installs: Option<u64>,
    #[serde(default = "default_true")]
    pub(crate) supported: bool,
    #[serde(default)]
    pub(crate) unsupported_reason: Option<String>,
}

fn default_true() -> bool {
    true
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Option::<String>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct SearchPage {
    pub(crate) data: Vec<SearchHit>,
    #[serde(default)]
    pub(crate) pagination: Value,
}

/// The `install/` answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct InstallAnswer {
    pub(crate) spec: InstallSpec,
    #[serde(default)]
    pub(crate) installed_version: Option<String>,
    #[serde(default)]
    pub(crate) download_url: Option<String>,
}

impl InstallAnswer {
    /// The one-time bundle link, wherever the site put it.
    pub(crate) fn download_url(&self) -> Option<&str> {
        self.download_url.as_deref().or_else(|| {
            self.spec
                .bundle
                .as_ref()
                .and_then(|bundle| bundle.download_url.as_deref())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct SiteInstalled {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) slug: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) installed_version: Option<String>,
    #[serde(default)]
    pub(crate) latest_version: Option<String>,
    #[serde(default)]
    pub(crate) update_available: bool,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    /// `available` or `withdrawn`; `None` from a site that does not say (nothing is changed then).
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default, rename = "withdrawn_reason")]
    pub(crate) reason: Option<String>,
}

pub(crate) const STATUS_AVAILABLE: &str = "available";
pub(crate) const STATUS_WITHDRAWN: &str = "withdrawn";

impl SiteInstalled {
    /// The reason when the site says the item is withdrawn (`withdrawn` when it gives none).
    pub(crate) fn withdrawn_reason(&self) -> Option<String> {
        (self.status.as_deref() == Some(STATUS_WITHDRAWN)).then(|| {
            self.reason
                .clone()
                .unwrap_or_else(|| STATUS_WITHDRAWN.to_string())
        })
    }
}

#[derive(Deserialize)]
struct DataList<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct EnabledAnswer {
    enabled: bool,
}

enum Retry {
    /// Reads: network errors and 502/503/504.
    Idempotent,
    /// Writes: only when the connection was never made.
    ConnectOnly,
}

pub(crate) struct CatalogClient {
    site: String,
    token: String,
    http: reqwest::Client,
}

impl CatalogClient {
    /// Proxies from the environment apply, except to a site on this computer (a loopback address
    /// is never meant to go through a proxy, and `NO_PROXY` patterns such as `127.*` are not
    /// understood by every client).
    pub(crate) fn new(site: &str, token: &str) -> Result<Self, CatalogError> {
        let loopback = Url::parse(site)
            .is_ok_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")));
        let builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT);
        let builder = if loopback {
            builder.no_proxy()
        } else {
            builder
        };
        let http = builder
            .build()
            .map_err(|err| CatalogError::local("local_error", format!("HTTP client: {err}")))?;
        Ok(Self {
            site: site.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http,
        })
    }

    pub(crate) fn site(&self) -> &str {
        &self.site
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.site)
    }

    async fn send(
        &self,
        retry: Retry,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, CatalogError> {
        let mut attempt = 1;
        loop {
            let result = build().send().await;
            let retryable = match &result {
                Ok(response) => {
                    matches!(retry, Retry::Idempotent)
                        && RETRYABLE_STATUSES.contains(&response.status().as_u16())
                }
                Err(err) => match retry {
                    Retry::Idempotent => true,
                    Retry::ConnectOnly => err.is_connect(),
                },
            };
            if !retryable || attempt >= MAX_ATTEMPTS {
                return result.map_err(|_| {
                    CatalogError::local("network_error", format!("could not reach {}", self.site))
                });
            }
            tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1)).await;
            attempt += 1;
        }
    }

    async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, CatalogError> {
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            return Err(CatalogError::from_response(status, &body));
        }
        serde_json::from_value(body).map_err(|err| {
            CatalogError::local(
                "unexpected_response",
                format!("the site's answer was not understood: {err}"),
            )
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, CatalogError> {
        let url = self.url(path);
        let response = self
            .send(Retry::Idempotent, || {
                self.http.get(&url).bearer_auth(&self.token).query(query)
            })
            .await?;
        Self::decode(response).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<reqwest::Response, CatalogError> {
        let url = self.url(path);
        self.send(Retry::ConnectOnly, || {
            self.http.post(&url).bearer_auth(&self.token).json(body)
        })
        .await
    }

    /// `source` is `store`, `tools` or `all` (the site's default is `store`).
    pub(crate) async fn search(
        &self,
        query: &str,
        kind: Option<&str>,
        source: &str,
        page: Option<u32>,
    ) -> Result<SearchPage, CatalogError> {
        let mut params = vec![("q", query.to_string()), ("source", source.to_string())];
        if let Some(kind) = kind {
            params.push(("kind", kind.to_string()));
        }
        if let Some(page) = page {
            params.push(("page", page.to_string()));
        }
        self.get("/search/", &params).await
    }

    pub(crate) async fn resolve(&self, reference: &str) -> Result<InstallSpec, CatalogError> {
        #[derive(Deserialize)]
        struct Answer {
            spec: InstallSpec,
        }
        // The contract returns `{spec}`; a bare spec is accepted as well.
        let body: Value = self
            .get("/resolve/", &[("ref", reference.to_string())])
            .await?;
        match body.get("spec") {
            Some(_) => serde_json::from_value::<Answer>(body).map(|answer| answer.spec),
            None => serde_json::from_value(body),
        }
        .map_err(|err| {
            CatalogError::local(
                "unexpected_response",
                format!("the site's install description was not understood: {err}"),
            )
        })
    }

    pub(crate) async fn install(&self, reference: &str) -> Result<InstallAnswer, CatalogError> {
        let response = self
            .post("/install/", &serde_json::json!({ "ref": reference }))
            .await?;
        Self::decode(response).await
    }

    pub(crate) async fn uninstall(&self, reference: &str) -> Result<(), CatalogError> {
        let response = self
            .post("/uninstall/", &serde_json::json!({ "ref": reference }))
            .await?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let body: Value = response.json().await.unwrap_or(Value::Null);
        Err(CatalogError::from_response(status, &body))
    }

    pub(crate) async fn set_enabled(
        &self,
        reference: &str,
        enabled: bool,
    ) -> Result<bool, CatalogError> {
        let response = self
            .post(
                "/set-enabled/",
                &serde_json::json!({ "ref": reference, "enabled": enabled }),
            )
            .await?;
        Self::decode::<EnabledAnswer>(response)
            .await
            .map(|answer| answer.enabled)
    }

    pub(crate) async fn installed(&self) -> Result<Vec<SiteInstalled>, CatalogError> {
        self.get::<DataList<SiteInstalled>>("/installed/", &[])
            .await
            .map(|list| list.data)
    }

    /// Fetches a bundle from its one-time link (relative links are on the site), refusing more
    /// bytes than the spec announced. The device token goes only to the site itself.
    pub(crate) async fn download(
        &self,
        link: &str,
        bundle: &BundleSpec,
    ) -> Result<Vec<u8>, CatalogError> {
        let unusable =
            || CatalogError::local("bad_download", "the site sent an unusable download link");
        let base = Url::parse(&format!("{}/", self.site)).map_err(|_| unusable())?;
        let url = base.join(link).map_err(|_| unusable())?;
        if !acceptable_url(url.as_str()) {
            return Err(unusable());
        }
        let same_site = url.origin() == base.origin();
        let limit = bundle.size.min(MAX_BUNDLE_BYTES);
        let mut response = self
            .send(Retry::ConnectOnly, || {
                let request = self.http.get(url.clone()).timeout(DOWNLOAD_TIMEOUT);
                if same_site {
                    request.bearer_auth(&self.token)
                } else {
                    request
                }
            })
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            return Err(CatalogError::from_response(status, &body));
        }
        let too_large = || {
            CatalogError::local(
                "bundle_size_mismatch",
                format!("the download is larger than the announced {limit} bytes"),
            )
        };
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| CatalogError::local("network_error", "the download was interrupted"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > limit {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
