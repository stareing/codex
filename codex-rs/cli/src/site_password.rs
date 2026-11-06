//! Sign in with the site account's username (or email) and password, without a browser.
//!
//! `POST /api/agent-devices/password/` answers either with the device-token payload (201, saved
//! exactly like the browser flow) or with a two-factor challenge (200), which is completed at
//! `/api/agent-devices/password/2fa/` with a code from `--otp` or a terminal prompt. The password
//! and the token are never printed or logged.

use std::io;
use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;

use codex_app_server_transport::SiteAccount;
use serde_json::Value;

use crate::site_login::Exchanged;
use crate::site_login::Refusal;
use crate::site_login::device_name;
use crate::site_login::http_client;
use crate::site_login::platform;
use crate::site_login::save_exchanged;

const PASSWORD_PATH: &str = "/api/agent-devices/password/";
const TWO_FACTOR_PATH: &str = "/api/agent-devices/password/2fa/";
/// Codes tried per challenge when the user is typing them.
const MAX_CODE_ATTEMPTS: u32 = 3;
const MAX_SECRET_CHARS: usize = 1024;

/// Refusal codes for which a browser sign-in is the way forward.
const BROWSER_CODES: &[&str] = &["captcha_required", "browser_sign_in_required"];

/// A sign-in that did not complete: the site's `code` (or a local one) and a readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignInError {
    pub(crate) code: String,
    pub(crate) detail: String,
}

impl SignInError {
    pub(crate) fn new(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            detail: detail.into(),
        }
    }

    /// What to tell a person: the site's detail plus what to do next for the known codes.
    pub(crate) fn message(&self) -> String {
        let hint = match self.code.as_str() {
            code if BROWSER_CODES.contains(&code) => {
                Some("Sign in through the browser instead: run `login` without --password.")
            }
            "invalid_credentials" => Some("Check the username or email and the password."),
            "email_unverified" => Some("Verify your email address on the site first."),
            "too_many_devices" => {
                Some("Remove a computer under My computers on the site, then try again.")
            }
            "throttled" => Some("Too many attempts; wait a while and try again."),
            "two_factor_expired" => Some("The two-factor step expired; sign in again."),
            "too_many_attempts" => Some("Too many wrong codes; sign in again later."),
            _ => None,
        };
        match hint {
            Some(hint) if self.detail.is_empty() => hint.to_string(),
            Some(hint) => format!("{} {hint}", self.detail),
            None if self.detail.is_empty() => format!("sign-in failed ({})", self.code),
            None => self.detail.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PasswordOutcome {
    SignedIn(SiteAccount),
    /// The site wants a second factor and there was no way to ask for one.
    TwoFactorRequired,
}

/// Asks for a two-factor code; `attempt` counts from 0. `None` means no code can be had.
pub(crate) type CodePrompt<'a> = &'a mut (dyn FnMut(u32) -> Option<String> + Send);

enum Answer {
    Token(Box<Exchanged>),
    Challenge(String),
}

fn refusal_of(status: u16, body: &Value) -> SignInError {
    let refusal: Option<Refusal> = serde_json::from_value(body.clone()).ok();
    let (code, detail) = refusal
        .map(|r| (r.code, r.detail.unwrap_or_default()))
        .unwrap_or_default();
    let code = code.unwrap_or_else(|| match status {
        429 => "throttled".to_string(),
        _ => format!("http_{status}"),
    });
    SignInError::new(&code, detail)
}

async fn post(url: &str, body: &Value) -> Result<Answer, SignInError> {
    let client = http_client().map_err(|err| SignInError::new("local_error", err.to_string()))?;
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|_| SignInError::new("network_error", "could not reach the site"))?;
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(refusal_of(status, &body));
    }
    if body["two_factor_required"] == Value::Bool(true) {
        return match body["challenge"].as_str() {
            Some(challenge) if !challenge.is_empty() => {
                Ok(Answer::Challenge(challenge.to_string()))
            }
            _ => Err(SignInError::new(
                "unexpected_response",
                "the site asked for a second factor without a challenge",
            )),
        };
    }
    serde_json::from_value(body)
        .map(|exchanged| Answer::Token(Box::new(exchanged)))
        .map_err(|_| SignInError::new("unexpected_response", "unexpected sign-in response"))
}

fn save(codex_home: &Path, site: &str, exchanged: Exchanged) -> Result<SiteAccount, SignInError> {
    save_exchanged(codex_home, site, exchanged)
        .map_err(|err| SignInError::new("local_error", format!("{err:#}")))
}

/// The whole exchange; `otp` is tried first, then `ask_code` (up to three codes) when given.
pub(crate) async fn sign_in(
    codex_home: &Path,
    site: &str,
    identifier: &str,
    password: &str,
    mut otp: Option<String>,
    mut ask_code: Option<CodePrompt<'_>>,
) -> Result<PasswordOutcome, SignInError> {
    let first = post(
        &format!("{site}{PASSWORD_PATH}"),
        &serde_json::json!({
            "identifier": identifier,
            "password": password,
            "device_name": device_name(),
            "platform": platform(),
        }),
    )
    .await?;
    let challenge = match first {
        Answer::Token(exchanged) => {
            return save(codex_home, site, *exchanged).map(PasswordOutcome::SignedIn);
        }
        Answer::Challenge(challenge) => challenge,
    };
    let mut attempt = 0;
    loop {
        let code = match otp.take() {
            Some(code) => Some(code),
            None => ask_code.as_mut().and_then(|ask| ask(attempt)),
        };
        let Some(code) = code.map(|code| code.trim().to_string()) else {
            return if attempt == 0 {
                Ok(PasswordOutcome::TwoFactorRequired)
            } else {
                Err(SignInError::new(
                    "invalid_code",
                    "the two-factor code was not accepted",
                ))
            };
        };
        attempt += 1;
        let answer = post(
            &format!("{site}{TWO_FACTOR_PATH}"),
            &serde_json::json!({
                "challenge": challenge,
                "code": code,
                "device_name": device_name(),
                "platform": platform(),
            }),
        )
        .await;
        match answer {
            Ok(Answer::Token(exchanged)) => {
                return save(codex_home, site, *exchanged).map(PasswordOutcome::SignedIn);
            }
            Ok(Answer::Challenge(_)) => {
                return Err(SignInError::new(
                    "unexpected_response",
                    "the site asked for a second factor twice",
                ));
            }
            Err(err)
                if err.code == "invalid_code"
                    && ask_code.is_some()
                    && attempt < MAX_CODE_ATTEMPTS =>
            {
                eprintln!("That code was not accepted; try again.");
            }
            Err(err) => return Err(err),
        }
    }
}

/// Where the credentials come from (`login --password`, `--username`, `--password-stdin`, `--otp`).
#[derive(Debug, Default, Clone)]
pub(crate) struct PasswordArgs {
    pub(crate) username: Option<String>,
    pub(crate) password_stdin: bool,
    pub(crate) otp: Option<String>,
}

fn stdin_is_terminal() -> bool {
    io::stdin().is_terminal()
}

fn read_visible_line(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(first_line(&line))
}

/// The first line without its line ending, capped (a password piped in may end in `\r\n`).
pub(crate) fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n'])
        .chars()
        .take(MAX_SECRET_CHARS)
        .collect()
}

/// Reads a line on the terminal without echoing it.
fn read_hidden_line(prompt: &str) -> io::Result<String> {
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;

    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    eprint!("{prompt}");
    io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let raw = RawMode;
    let mut secret = String::new();
    loop {
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                drop(raw);
                eprintln!();
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            KeyCode::Backspace => {
                secret.pop();
            }
            KeyCode::Char(c) if secret.chars().count() < MAX_SECRET_CHARS => secret.push(c),
            _ => {}
        }
    }
    drop(raw);
    eprintln!();
    Ok(secret)
}

/// Gathers the credentials and signs in; prompts only on a terminal.
pub(crate) async fn run(
    codex_home: &Path,
    site: &str,
    args: PasswordArgs,
) -> Result<PasswordOutcome, SignInError> {
    let interactive = stdin_is_terminal() && !args.password_stdin;
    let local = |code: &str, detail: &str| SignInError::new(code, detail);
    let identifier = match args.username.filter(|u| !u.trim().is_empty()) {
        Some(username) => username.trim().to_string(),
        None if interactive => read_visible_line("Username or email: ")
            .map_err(|_| local("input_error", "could not read the username"))?
            .trim()
            .to_string(),
        None => {
            return Err(local(
                "missing_username",
                "pass --username when the password comes from stdin or there is no terminal",
            ));
        }
    };
    if identifier.is_empty() {
        return Err(local("missing_username", "a username or email is required"));
    }
    let password = if args.password_stdin {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|_| local("input_error", "could not read the password from stdin"))?;
        first_line(&line)
    } else if interactive {
        read_hidden_line("Password: ")
            .map_err(|_| local("input_error", "could not read the password"))?
    } else {
        return Err(local(
            "missing_password",
            "no terminal to ask for the password; use --password-stdin",
        ));
    };
    if password.is_empty() {
        return Err(local("missing_password", "a password is required"));
    }
    let can_prompt_code = stdin_is_terminal() && !args.password_stdin;
    let mut prompt = |attempt: u32| {
        let label = if attempt == 0 {
            "Two-factor code: "
        } else {
            "Two-factor code (try again): "
        };
        read_visible_line(label)
            .ok()
            .filter(|code| !code.trim().is_empty())
    };
    let ask_code: Option<CodePrompt<'_>> = can_prompt_code.then_some(&mut prompt as _);
    sign_in(codex_home, site, &identifier, &password, args.otp, ask_code).await
}

#[cfg(test)]
#[path = "site_password_tests.rs"]
mod tests;
