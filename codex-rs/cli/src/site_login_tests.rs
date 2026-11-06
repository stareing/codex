use super::*;
use pretty_assertions::assert_eq;

#[test]
fn challenge_matches_the_site() {
    // Same S256 rule as the site's `agent_devices.service._challenge_of`.
    assert_eq!(
        challenge_of("dBjftJeZ4CVP-mJ92K23F2ZzW0x2SHOjNcRzaghkI1yWdk"),
        "uxvs_hipFoiFGdrIXu1bxWykIE_j2hZH3rpbV_egnTM"
    );
    let (verifier, challenge) = pkce_pair();
    assert_eq!(verifier.len(), 64);
    assert_eq!(challenge.len(), 43);
    assert_eq!(challenge, challenge_of(&verifier));
}

#[test]
fn site_url_is_an_https_origin() {
    assert_eq!(
        resolve_site_url(Some("https://onigio.com/")).unwrap(),
        "https://onigio.com"
    );
    assert_eq!(
        resolve_site_url(Some("http://127.0.0.1:3000")).unwrap(),
        "http://127.0.0.1:3000"
    );
    for bad in [
        "http://onigio.com",
        "https://onigio.com/ask",
        "https://onigio.com/?x=1",
        "ftp://x",
    ] {
        assert!(
            resolve_site_url(Some(bad)).is_err(),
            "{bad} should be refused"
        );
    }
}

#[test]
fn connect_url_carries_the_challenge_and_loopback_redirect() {
    let url = connect_url(
        "https://onigio.com",
        "ch",
        "http://127.0.0.1:5000/callback",
        "st",
        "my pc",
    )
    .unwrap();
    assert_eq!(url.path(), "/agent/connect");
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["code_challenge"], "ch");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["redirect_uri"], "http://127.0.0.1:5000/callback");
    assert_eq!(params["state"], "st");
    assert_eq!(params["name"], "my pc");
}

#[test]
fn only_our_callback_counts() {
    assert_eq!(
        parse_callback("/callback?code=abc&state=st", "st"),
        Callback::Approved("abc".into())
    );
    assert_eq!(
        parse_callback("/callback?error=access_denied&state=st", "st"),
        Callback::Denied
    );
    assert_eq!(
        parse_callback("/callback?code=abc&state=other", "st"),
        Callback::NotOurs
    );
    assert_eq!(parse_callback("/favicon.ico", "st"), Callback::NotOurs);
    assert_eq!(
        parse_callback("/callback?code=&state=st", "st"),
        Callback::Denied
    );
}

#[test]
fn account_round_trip_and_session_link() {
    let home = tempfile::TempDir::new().unwrap();
    assert_eq!(SiteAccount::load(home.path()).unwrap(), None);
    let account = SiteAccount {
        site_url: "https://onigio.com".into(),
        relay_url: "https://onigio.com/agent-relay/".into(),
        account_id: "u7".into(),
        device_id: 3,
        name: "studio".into(),
        model: None,
    };
    account.save(home.path(), "oad_secret").unwrap();
    assert_eq!(
        SiteAccount::load(home.path()).unwrap(),
        Some(account.clone())
    );
    assert_eq!(
        account.session_url(),
        "https://onigio.com/ask?mode=agent&device=3"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(SiteAccount::token_path(home.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "token must be owner-only");
    }
    // The app-server picks the saved account up without any environment variable.
    let relay = account.relay(home.path()).unwrap();
    assert_eq!(relay.url(), "https://onigio.com/agent-relay/");
    assert!(session_banner(&account).contains("https://onigio.com/ask?mode=agent&device=3"));
    SiteAccount::remove(home.path()).unwrap();
    assert_eq!(SiteAccount::load(home.path()).unwrap(), None);
}

#[tokio::test]
async fn login_completes_through_the_loopback_callback() {
    use std::io::Read as _;
    use std::io::Write as _;
    // A stand-in site: answers the token exchange.
    let site = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let site_url = format!(
        "http://127.0.0.1:{}",
        site.server_addr().to_ip().unwrap().port()
    );
    let site_thread = std::thread::spawn(move || {
        let mut request = site.recv().unwrap();
        assert_eq!(request.url(), "/api/agent-devices/token/");
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "the-code");
        let verifier = body["code_verifier"].as_str().unwrap().to_string();
        let reply = serde_json::json!({
            "device_id": 9, "token": "oad_new", "account_id": "u7",
            "environment_id": "env_d9", "relay_url": "https://onigio.com/agent-relay/", "name": "pc",
        });
        request
            .respond(tiny_http::Response::from_string(reply.to_string()).with_status_code(201))
            .unwrap();
        verifier
    });
    let home = tempfile::TempDir::new().unwrap();
    // Plays the browser: reads the connect URL the login prints is not needed — it intercepts the
    // redirect by scanning for the listener and sending the approved callback with the right state.
    let home_path = home.path().to_path_buf();
    let login_task = tokio::spawn({
        let site_url = site_url.clone();
        async move {
            login_with_opener(&home_path, &site_url, move |url| {
            let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            let redirect = Url::parse(&params["redirect_uri"]).unwrap();
            let mut stream = std::net::TcpStream::connect((
                redirect.host_str().unwrap(), redirect.port().unwrap())).unwrap();
            write!(stream, "GET /callback?code=the-code&state={} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                params["state"]).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            assert!(answer.contains("connected"), "{answer}");
        }).await
        }
    });
    let account = login_task.await.unwrap().unwrap();
    let verifier = site_thread.join().unwrap();
    assert_eq!(account.device_id, 9);
    assert_eq!(
        std::fs::read_to_string(SiteAccount::token_path(home.path())).unwrap(),
        "oad_new"
    );
    assert_eq!(verifier.len(), 64);
}
