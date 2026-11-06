//! Sign in with the site's own accounts (docs/agent-codex/REMOTE_CONTROL.md R7 in the site repo).
//!
//! `login` opens the site's connect page in the browser with an S256 PKCE challenge and a
//! loopback redirect; the signed-in user approves this computer there, the page sends the browser
//! back to `http://127.0.0.1:<port>/callback?code=…&state=…`, and the code is exchanged at
//! `/api/agent-devices/token/` for a device token. The result is saved as a [`SiteAccount`]; the
//! app-server reads it to reach the site relay, so `remote-control` needs nothing else.

use std::io;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use base64::Engine;
use codex_app_server_transport::SiteAccount;
use codex_utils_cli::CliConfigOverrides;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use rand::Rng;
use sha2::Digest;
use sha2::Sha256;
use url::Url;

use crate::site_password::PasswordArgs;
use crate::site_password::PasswordOutcome;
use crate::site_password::SignInError;

/// The site this build signs in to unless `--site` or [`SITE_URL_ENV_VAR`] says otherwise.
pub const DEFAULT_SITE_URL: &str = "https://onigio.com";
pub const SITE_URL_ENV_VAR: &str = "AGENT_SITE_URL";
const CONNECT_PATH: &str = "/agent/connect";
const TOKEN_PATH: &str = "/api/agent-devices/token/";
const SELF_REVOKE_PATH: &str = "/api/agent-devices/self/revoke/";
const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// The site origin to use: `--site`, then the environment, then [`DEFAULT_SITE_URL`].
pub fn resolve_site_url(flag: Option<&str>) -> anyhow::Result<String> {
    let raw = flag
        .map(str::to_string)
        .or_else(|| {
            std::env::var(SITE_URL_ENV_VAR)
                .ok()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_SITE_URL.to_string());
    let url = Url::parse(&raw).with_context(|| format!("site URL {raw:?} does not parse"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("site URL must be HTTPS (HTTP only on this computer)");
    }
    if url.path() != "/" || url.query().is_some() || !url.username().is_empty() {
        bail!("site URL must be just an origin, e.g. {DEFAULT_SITE_URL}");
    }
    Ok(url.origin().ascii_serialization())
}

/// PKCE verifier (64 unreserved characters) and its S256 challenge.
fn pkce_pair() -> (String, String) {
    let mut rng = rand::rng();
    let verifier: String = (0..64)
        .map(|_| VERIFIER_CHARS[rng.random_range(0..VERIFIER_CHARS.len())] as char)
        .collect();
    (verifier.clone(), challenge_of(&verifier))
}

fn challenge_of(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

pub(crate) fn device_name() -> String {
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    if name.is_empty() {
        "computer".to_string()
    } else {
        name
    }
}

fn connect_url(
    site: &str,
    challenge: &str,
    redirect_uri: &str,
    state: &str,
    name: &str,
) -> anyhow::Result<Url> {
    Url::parse_with_params(
        &format!("{site}{CONNECT_PATH}"),
        &[
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("name", name),
            ("platform", platform()),
        ],
    )
    .context("connect URL")
}

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Approved(String),
    Denied,
    NotOurs,
}

/// Reads the browser's return: only `/callback` with our `state` counts.
fn parse_callback(request_url: &str, state: &str) -> Callback {
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{request_url}")) else {
        return Callback::NotOurs;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::NotOurs;
    }
    let param = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    if param("state").as_deref() != Some(state) {
        return Callback::NotOurs;
    }
    match (param("code"), param("error")) {
        (Some(code), None) if !code.is_empty() => Callback::Approved(code),
        _ => Callback::Denied,
    }
}

fn page(body: &str) -> tiny_http::Response<io::Cursor<Vec<u8>>> {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>Onigio</title><body style=\"font:16px system-ui;margin:20vh auto;max-width:28em;text-align:center\">\
         <p>{body}</p></body>"
    );
    let response = tiny_http::Response::from_string(html);
    match tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        Ok(content_type) => response.with_header(content_type),
        Err(()) => response,
    }
}

/// Waits for the browser to come back; answers every request so the tab never hangs.
fn wait_for_code(server: &tiny_http::Server, state: &str) -> anyhow::Result<String> {
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("timed out waiting for the browser; run login again");
        }
        let Some(request) = server.recv_timeout(left.min(Duration::from_secs(1)))? else {
            continue;
        };
        match parse_callback(request.url(), state) {
            Callback::Approved(code) => {
                let _ =
                    request.respond(page("This computer is connected. You can close this tab."));
                return Ok(code);
            }
            Callback::Denied => {
                let _ = request.respond(page("Sign-in was cancelled. You can close this tab."));
                bail!("sign-in was cancelled in the browser");
            }
            Callback::NotOurs => {
                let _ = request.respond(page("Not found.").with_status_code(404));
            }
        }
    }
}

/// The site's device-token payload (browser exchange and password sign-in alike).
#[derive(serde::Deserialize)]
pub(crate) struct Exchanged {
    device_id: u64,
    token: String,
    account_id: String,
    relay_url: String,
    name: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct Refusal {
    pub(crate) code: Option<String>,
    pub(crate) detail: Option<String>,
}

pub(crate) fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("HTTP client")
}

async fn exchange(site: &str, code: &str, verifier: &str) -> anyhow::Result<Exchanged> {
    let response = http_client()?
        .post(format!("{site}{TOKEN_PATH}"))
        .json(&serde_json::json!({ "code": code, "code_verifier": verifier }))
        .send()
        .await
        .context("could not reach the site")?;
    if !response.status().is_success() {
        let status = response.status();
        let refusal: Option<Refusal> = response.json().await.ok();
        let detail = refusal
            .map(|r| {
                format!(
                    "{} ({})",
                    r.detail.unwrap_or_default(),
                    r.code.unwrap_or_default()
                )
            })
            .unwrap_or_default();
        bail!("the site refused the sign-in: HTTP {status} {detail}");
    }
    response.json().await.context("unexpected sign-in response")
}

/// `login`: browser approval → device token → saved account. Replaces an earlier sign-in.
pub async fn login(codex_home: &Path, site: &str) -> anyhow::Result<SiteAccount> {
    login_with_opener(codex_home, site, |url| {
        let _ = webbrowser::open(url.as_str());
    })
    .await
}

/// [`login`] with the browser step injected (tests play the browser).
async fn login_with_opener(
    codex_home: &Path,
    site: &str,
    open: impl FnOnce(Url) + Send + 'static,
) -> anyhow::Result<SiteAccount> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|err| anyhow::anyhow!("could not open the local sign-in port: {err}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|addr| addr.port())
        .context("local sign-in port")?;
    let (verifier, challenge) = pkce_pair();
    let state: String = format!("{:032x}", rand::rng().random::<u128>());
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let url = connect_url(site, &challenge, &redirect_uri, &state, &device_name())?;

    eprintln!("Opening your browser to connect this computer to {site}.");
    eprintln!("If it does not open, visit:\n\n  {url}\n");
    std::thread::spawn(move || open(url));

    let state_for_wait = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_code(&server, &state_for_wait))
        .await
        .context("sign-in listener stopped")??;
    let exchanged = exchange(site, &code, &verifier).await?;
    save_exchanged(codex_home, site, exchanged)
}

/// Saves a device-token payload as this computer's sign-in (replacing an earlier one).
pub(crate) fn save_exchanged(
    codex_home: &Path,
    site: &str,
    exchanged: Exchanged,
) -> anyhow::Result<SiteAccount> {
    let account = SiteAccount {
        site_url: site.to_string(),
        relay_url: exchanged.relay_url,
        account_id: exchanged.account_id,
        device_id: exchanged.device_id,
        name: exchanged.name,
        model: exchanged.model.filter(|model| !model.trim().is_empty()),
    };
    account
        .relay(codex_home)
        .context("the site returned an unusable relay address")?;
    account
        .save(codex_home, &exchanged.token)
        .context("could not save the sign-in")?;
    Ok(account)
}

/// `logout`: asks the site to revoke this computer, then forgets the sign-in either way.
pub async fn logout(codex_home: &Path) -> anyhow::Result<Option<SiteAccount>> {
    let Some(account) = SiteAccount::load(codex_home)? else {
        return Ok(None);
    };
    let token = std::fs::read_to_string(SiteAccount::token_path(codex_home)).unwrap_or_default();
    let revoked = async {
        http_client()?
            .post(format!("{}{SELF_REVOKE_PATH}", account.site_url))
            .bearer_auth(token.trim())
            .send()
            .await?
            .error_for_status()?;
        anyhow::Ok(())
    }
    .await;
    if let Err(err) = revoked {
        eprintln!(
            "Could not reach {} to revoke this computer ({err}); remove it under My computers on the site.",
            account.site_url
        );
    }
    SiteAccount::remove(codex_home)?;
    Ok(Some(account))
}

/// Terminal QR code (dark modules as blocks) for opening `url` on a phone.
pub fn qr_lines(url: &str) -> Option<String> {
    let code = QrCode::new(url.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

/// What `remote-control` prints once this computer is reachable: the link and its QR code.
pub fn session_banner(account: &SiteAccount) -> String {
    let url = account.session_url();
    match qr_lines(&url) {
        Some(qr) => format!("Open on your phone or browser:\n\n  {url}\n\n{qr}"),
        None => format!("Open on your phone or browser:\n\n  {url}\n"),
    }
}

pub(crate) fn codex_home() -> anyhow::Result<std::path::PathBuf> {
    Ok(codex_core::config::find_codex_home()
        .context("failed to resolve CODEX_HOME")?
        .as_path()
        .to_path_buf())
}

/// `login` as parsed from the command line.
#[derive(Debug, Default)]
pub(crate) struct LoginRequest {
    pub(crate) site: Option<String>,
    /// Print exactly one JSON result line on stdout.
    pub(crate) json: bool,
    pub(crate) no_host_info: bool,
    /// `Some` for the password flow, `None` for the browser.
    pub(crate) password: Option<PasswordArgs>,
    pub(crate) config_overrides: CliConfigOverrides,
}

/// Exit code of `login` when the site wants a second factor and none could be asked for.
pub const EXIT_TWO_FACTOR_REQUIRED: i32 = 3;

/// The single result line of `login --json`.
pub(crate) fn login_json_line(result: &Result<PasswordOutcome, SignInError>) -> serde_json::Value {
    match result {
        Ok(PasswordOutcome::SignedIn(account)) => serde_json::json!({
            "status": "signed_in",
            "site": account.site_url,
            "device_id": account.device_id,
            "name": account.name,
        }),
        Ok(PasswordOutcome::TwoFactorRequired) => {
            serde_json::json!({"status": "two_factor_required"})
        }
        Err(err) => serde_json::json!({
            "status": "error",
            "code": err.code,
            "detail": err.message(),
        }),
    }
}

/// `login [--site URL] [--password …] [--json] [--no-host-info]`.
pub(crate) async fn run_login(request: LoginRequest) -> ! {
    let result = async {
        let site = resolve_site_url(request.site.as_deref())
            .map_err(|err| SignInError::new("invalid_site", format!("{err:#}")))?;
        let home =
            codex_home().map_err(|err| SignInError::new("local_error", format!("{err:#}")))?;
        let outcome = match request.password {
            Some(args) => crate::site_password::run(&home, &site, args).await?,
            None => login(&home, &site)
                .await
                .map(PasswordOutcome::SignedIn)
                .map_err(|err| SignInError::new("login_failed", format!("{err:#}")))?,
        };
        if matches!(outcome, PasswordOutcome::SignedIn(_)) {
            crate::host_info::report_for_saved_account(
                &home,
                &request.config_overrides,
                request.no_host_info,
            )
            .await;
        }
        Ok(outcome)
    }
    .await;
    if request.json {
        println!("{}", login_json_line(&result));
    }
    match result {
        Ok(PasswordOutcome::SignedIn(account)) => {
            if !request.json {
                eprintln!(
                    "Signed in. This computer is connected to {} as \"{}\".\nRun `remote-control` to use it from your phone or browser.",
                    account.site_url, account.name
                );
            }
            std::process::exit(0);
        }
        Ok(PasswordOutcome::TwoFactorRequired) => {
            if !request.json {
                eprintln!("A two-factor code is required; run again with --otp <CODE>.");
            }
            std::process::exit(EXIT_TWO_FACTOR_REQUIRED);
        }
        Err(err) => {
            if !request.json {
                eprintln!("Error: {}", err.message());
            }
            std::process::exit(1);
        }
    }
}

/// The `login status --json` object.
pub(crate) fn status_json(account: Option<&SiteAccount>) -> serde_json::Value {
    match account {
        Some(account) => serde_json::json!({
            "signed_in": true,
            "site": account.site_url,
            "device_id": account.device_id,
            "name": account.name,
            "session_url": account.session_url(),
        }),
        None => serde_json::json!({"signed_in": false}),
    }
}

/// `login status --json`: one JSON line about the site sign-in; always exits 0.
pub fn run_site_status_json() -> ! {
    let account = codex_home()
        .ok()
        .and_then(|home| SiteAccount::load(&home).ok().flatten());
    println!("{}", status_json(account.as_ref()));
    std::process::exit(0);
}

/// `login status` when signed in to a site; `None` lets the caller fall back to upstream.
pub fn site_status() -> Option<String> {
    let account = SiteAccount::load(&codex_home().ok()?).ok()??;
    Some(format!(
        "Signed in to {} as computer \"{}\" (device {})",
        account.site_url, account.name, account.device_id
    ))
}

/// `logout`; `false` when there was no site sign-in (the caller falls back to upstream).
pub async fn run_site_logout() -> anyhow::Result<bool> {
    let account = logout(&codex_home()?).await?;
    if let Some(account) = &account {
        eprintln!(
            "Signed out; \"{}\" is disconnected from {}.",
            account.name, account.site_url
        );
    }
    Ok(account.is_some())
}

/// Before `remote-control`: sign in first when there is neither a saved account nor a launcher
/// configuration, so one command is enough on a new computer.
pub async fn ensure_signed_in() -> anyhow::Result<()> {
    if codex_app_server_transport::SiteRelay::from_env()?.is_some() {
        return Ok(());
    }
    let home = codex_home()?;
    if SiteAccount::load(&home)?.is_some() {
        return Ok(());
    }
    let site = resolve_site_url(None)?;
    let account = login(&home, &site).await?;
    eprintln!(
        "Signed in to {} as computer \"{}\".",
        account.site_url, account.name
    );
    Ok(())
}

/// After remote control is up: the link and QR code for this computer, when signed in to a site.
pub fn print_session_link() {
    let account = codex_home()
        .ok()
        .and_then(|home| SiteAccount::load(&home).ok().flatten());
    if let Some(account) = account {
        println!("\n{}", session_banner(&account));
    }
}

#[cfg(test)]
#[path = "site_login_tests.rs"]
mod tests;
