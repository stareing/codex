//! Onigio fork: remote control against the site relay with only a device token.

use super::*;
use pretty_assertions::assert_eq;

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

async fn start_with_site_relay(
    listener: &TcpListener,
    codex_home: &TempDir,
    token_dir: &TempDir,
) -> (
    tokio::task::JoinHandle<()>,
    RemoteControlHandle,
    CancellationToken,
) {
    let relay_url = remote_control_url_for_listener(listener);
    let site_relay = SiteRelay::new(&relay_url, token_dir.path().join("device-token"), "u42")
        .expect("relay should be valid");
    let (transport_event_tx, _transport_event_rx) =
        mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    let shutdown_token = CancellationToken::new();
    let (remote_task, remote_handle) = start_remote_control(
        RemoteControlStartConfig {
            remote_control_url: relay_url,
            installation_id: TEST_INSTALLATION_ID.to_string(),
            policy: RemoteControlPolicy::Allowed,
            site_relay: Some(site_relay),
        },
        Some(remote_control_state_runtime(codex_home).await),
        signed_out_auth_manager(codex_home).await,
        transport_event_tx,
        shutdown_token.clone(),
        /*app_server_client_name_rx*/ None,
        RemoteControlStartupMode::EnabledEphemeral,
    )
    .await
    .expect("remote control should start");
    (remote_task, remote_handle, shutdown_token)
}

#[tokio::test]
async fn site_relay_enrolls_and_connects_with_the_device_token() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let codex_home = TempDir::new().expect("temp dir should create");
    let token_dir = TempDir::new().expect("temp dir should create");
    std::fs::write(token_dir.path().join("device-token"), "dt_device\n")
        .expect("token should write");
    let (remote_task, remote_handle, shutdown_token) =
        start_with_site_relay(&listener, &codex_home, &token_dir).await;
    let mut status_rx = remote_handle.status_receiver();

    let enroll_request = accept_http_request(&listener).await;
    assert_eq!(
        enroll_request.request_line,
        "POST /backend-api/wham/remote/control/server/enroll HTTP/1.1"
    );
    assert_eq!(
        enroll_request.headers.get("authorization"),
        Some(&"Bearer dt_device".to_string())
    );
    assert_eq!(
        enroll_request
            .headers
            .get_all(crate::transport::remote_control::auth::SITE_RELAY_ACCOUNT_ID_HEADER),
        vec!["u42"]
    );
    assert!(
        enroll_request
            .headers
            .get_all(REMOTE_CONTROL_ACCOUNT_ID_HEADER)
            .is_empty()
    );
    respond_with_json(
        enroll_request.stream,
        remote_control_server_token_response(
            "srv_e_site",
            "env_site",
            TEST_REMOTE_CONTROL_SERVER_TOKEN,
        ),
    )
    .await;

    let (handshake_request, _websocket) = accept_remote_control_backend_connection(&listener).await;
    assert_eq!(
        handshake_request.path,
        "/backend-api/wham/remote/control/server"
    );
    assert_eq!(
        handshake_request.headers.get("authorization"),
        Some(&format!("Bearer {TEST_REMOTE_CONTROL_SERVER_TOKEN}"))
    );
    assert_eq!(
        handshake_request.headers.get("x-codex-server-id"),
        Some(&"srv_e_site".to_string())
    );
    expect_remote_control_status(&mut status_rx, None, Some("env_site")).await;

    shutdown_token.cancel();
    let _ = remote_task.await;
}

#[tokio::test]
async fn site_relay_waits_without_a_device_token() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let codex_home = TempDir::new().expect("temp dir should create");
    let token_dir = TempDir::new().expect("temp dir should create");
    let (remote_task, _remote_handle, shutdown_token) =
        start_with_site_relay(&listener, &codex_home, &token_dir).await;

    timeout(Duration::from_millis(300), listener.accept())
        .await
        .expect_err("no request may reach the relay without a device token");

    shutdown_token.cancel();
    timeout(Duration::from_secs(1), remote_task)
        .await
        .expect("remote control task should stop")
        .expect("remote control task should join");
}
