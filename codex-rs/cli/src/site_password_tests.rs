use super::*;
use pretty_assertions::assert_eq;

/// A stand-in site answering each request with the next scripted `(status, body)`; returns the
/// `(path, json body)` of every request it saw.
fn fake_site(script: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<(String, Value)>>) {
    let site = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}",
        site.server_addr().to_ip().unwrap().port()
    );
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, reply) in script {
            let mut request = site.recv().unwrap();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            seen.push((
                request.url().to_string(),
                serde_json::from_str(&body).unwrap_or(Value::Null),
            ));
            request
                .respond(
                    tiny_http::Response::from_string(reply.to_string()).with_status_code(status),
                )
                .unwrap();
        }
        seen
    });
    (url, handle)
}

fn token_payload() -> Value {
    serde_json::json!({
        "device_id": 12, "token": "oad_pw", "account_id": "u7", "environment_id": "env_d12",
        "relay_url": "https://onigio.com/agent-relay/", "name": "laptop", "model": "site-model-1",
    })
}

#[tokio::test]
async fn password_sign_in_saves_the_account() {
    let (site, server) = fake_site(vec![(201, token_payload())]);
    let home = tempfile::TempDir::new().unwrap();
    let outcome = sign_in(home.path(), &site, "ann@example.com", "pw", None, None)
        .await
        .unwrap();
    let PasswordOutcome::SignedIn(account) = outcome else {
        panic!("expected a sign-in");
    };
    assert_eq!(account.device_id, 12);
    assert_eq!(account.model.as_deref(), Some("site-model-1"));
    assert_eq!(SiteAccount::load(home.path()).unwrap(), Some(account));
    assert_eq!(
        std::fs::read_to_string(SiteAccount::token_path(home.path())).unwrap(),
        "oad_pw"
    );
    let seen = server.join().unwrap();
    assert_eq!(seen[0].0, "/api/agent-devices/password/");
    assert_eq!(seen[0].1["identifier"], "ann@example.com");
    assert_eq!(seen[0].1["password"], "pw");
    assert!(["windows", "macos", "linux"].contains(&seen[0].1["platform"].as_str().unwrap()));
    assert!(seen[0].1["device_name"].is_string());
}

#[tokio::test]
async fn two_factor_without_a_code_source_reports_it() {
    let (site, server) = fake_site(vec![(
        200,
        serde_json::json!({"two_factor_required": true, "challenge": "ch1"}),
    )]);
    let home = tempfile::TempDir::new().unwrap();
    let outcome = sign_in(home.path(), &site, "ann", "pw", None, None)
        .await
        .unwrap();
    assert_eq!(outcome, PasswordOutcome::TwoFactorRequired);
    assert_eq!(SiteAccount::load(home.path()).unwrap(), None);
    server.join().unwrap();
}

#[tokio::test]
async fn otp_flag_completes_the_second_factor() {
    let (site, server) = fake_site(vec![
        (
            200,
            serde_json::json!({"two_factor_required": true, "challenge": "ch1"}),
        ),
        (201, token_payload()),
    ]);
    let home = tempfile::TempDir::new().unwrap();
    let outcome = sign_in(
        home.path(),
        &site,
        "ann",
        "pw",
        Some(" 123456 ".into()),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PasswordOutcome::SignedIn(_)));
    let seen = server.join().unwrap();
    assert_eq!(seen[1].0, "/api/agent-devices/password/2fa/");
    assert_eq!(seen[1].1["challenge"], "ch1");
    assert_eq!(seen[1].1["code"], "123456");
    assert!(seen[1].1.get("password").is_none());
}

#[tokio::test]
async fn wrong_codes_are_re_asked_up_to_three_times() {
    let wrong = (
        400,
        serde_json::json!({"code": "invalid_code", "detail": "Wrong code."}),
    );
    let (site, server) = fake_site(vec![
        (
            200,
            serde_json::json!({"two_factor_required": true, "challenge": "ch1"}),
        ),
        wrong.clone(),
        wrong.clone(),
        wrong,
    ]);
    let home = tempfile::TempDir::new().unwrap();
    let mut asked = Vec::new();
    let mut ask = |attempt: u32| {
        asked.push(attempt);
        Some(format!("00000{attempt}"))
    };
    let err = sign_in(home.path(), &site, "ann", "pw", None, Some(&mut ask))
        .await
        .unwrap_err();
    assert_eq!(err.code, "invalid_code");
    assert_eq!(asked, vec![0, 1, 2]);
    assert_eq!(server.join().unwrap().len(), 4);
}

#[tokio::test]
async fn second_try_succeeds_after_a_wrong_code() {
    let (site, server) = fake_site(vec![
        (
            200,
            serde_json::json!({"two_factor_required": true, "challenge": "ch1"}),
        ),
        (400, serde_json::json!({"code": "invalid_code"})),
        (201, token_payload()),
    ]);
    let home = tempfile::TempDir::new().unwrap();
    let mut ask = |_attempt: u32| Some("654321".to_string());
    let outcome = sign_in(
        home.path(),
        &site,
        "ann",
        "pw",
        Some("111111".into()),
        Some(&mut ask),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PasswordOutcome::SignedIn(_)));
    let seen = server.join().unwrap();
    assert_eq!(seen[1].1["code"], "111111");
    assert_eq!(seen[2].1["code"], "654321");
}

#[tokio::test]
async fn refusals_carry_the_site_code() {
    let cases = [
        (
            401,
            serde_json::json!({"code": "invalid_credentials", "detail": "Wrong password."}),
            "invalid_credentials",
        ),
        (
            400,
            serde_json::json!({"code": "captcha_required", "detail": "x"}),
            "captcha_required",
        ),
        (
            403,
            serde_json::json!({"code": "browser_sign_in_required"}),
            "browser_sign_in_required",
        ),
        (
            409,
            serde_json::json!({"code": "too_many_devices"}),
            "too_many_devices",
        ),
        (429, serde_json::json!({}), "throttled"),
        (
            503,
            serde_json::json!({"code": "agent_relay_unconfigured"}),
            "agent_relay_unconfigured",
        ),
        (500, Value::Null, "http_500"),
    ];
    for (status, body, code) in cases {
        let (site, server) = fake_site(vec![(status, body)]);
        let home = tempfile::TempDir::new().unwrap();
        let err = sign_in(home.path(), &site, "ann", "pw", None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code, code);
        assert!(
            !err.message().contains("pw\""),
            "message must not echo input"
        );
        assert_eq!(SiteAccount::load(home.path()).unwrap(), None);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn expired_challenge_and_too_many_attempts_stop() {
    for (status, code) in [(400, "two_factor_expired"), (429, "too_many_attempts")] {
        let (site, server) = fake_site(vec![
            (
                200,
                serde_json::json!({"two_factor_required": true, "challenge": "ch1"}),
            ),
            (status, serde_json::json!({"code": code})),
        ]);
        let home = tempfile::TempDir::new().unwrap();
        let mut ask = |_attempt: u32| Some("123456".to_string());
        let err = sign_in(home.path(), &site, "ann", "pw", None, Some(&mut ask))
            .await
            .unwrap_err();
        assert_eq!(err.code, code);
        assert_eq!(
            server.join().unwrap().len(),
            2,
            "{code} must not be retried"
        );
    }
}

#[test]
fn messages_point_to_the_browser_when_needed() {
    let message = SignInError::new("browser_sign_in_required", "").message();
    assert!(message.contains("without --password"), "{message}");
    let message = SignInError::new("captcha_required", "Captcha needed.").message();
    assert!(message.starts_with("Captcha needed."), "{message}");
    assert_eq!(SignInError::new("x", "").message(), "sign-in failed (x)");
}

#[test]
fn first_line_strips_line_endings() {
    assert_eq!(first_line("secret\r\nmore\n"), "secret");
    assert_eq!(first_line("secret"), "secret");
    assert_eq!(first_line(""), "");
    assert_eq!(first_line(" spaced \n"), " spaced ");
}
