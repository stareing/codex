//! The remote-control folder allow-list, through the request dispatcher (reuses the user
//! verification dispatcher harness).

use super::test_support::Harness;
use super::*;
use crate::outgoing_message::OutgoingMessage;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;

struct Folders {
    _dir: tempfile::TempDir,
    allowed: std::path::PathBuf,
    outside: std::path::PathBuf,
}

fn folders() -> Folders {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let allowed = base.join("allowed");
    let outside = base.join("outside");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(allowed.join("in.txt"), "in").unwrap();
    std::fs::write(outside.join("out.txt"), "out").unwrap();
    Folders {
        _dir: dir,
        allowed,
        outside,
    }
}

/// The fs dispatch future overflows the default 2 MiB test stack in debug builds (as in
/// `message_processor_tracing_tests`).
fn with_stack(
    future: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(Box::pin(future))
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("test thread panicked"))?
}

fn refusal(message: OutgoingMessage, id: i64) -> rpc::JSONRPCErrorError {
    let OutgoingMessage::Error(error) = message else {
        panic!("request {id} should have been refused, got {message:?}")
    };
    assert_eq!(error.id, rpc::RequestId::Integer(id));
    error.error
}

#[test]
fn remote_requests_outside_the_allowed_folders_are_refused() -> Result<()> {
    with_stack(remote_requests_outside_the_allowed_folders_are_refused_body())
}

async fn remote_requests_outside_the_allowed_folders_are_refused_body() -> Result<()> {
    let folders = folders();
    let mut h = Harness::new_with_remote_roots(
        ConnectionOrigin::RemoteControl,
        || true,
        Some(vec![folders.allowed.clone()]),
    )
    .await?;
    h.initialize("remote", /*opt_in*/ true).await;

    h.send(
        1,
        "fs/readFile",
        json!({"path": folders.outside.join("out.txt")}),
    )
    .await;
    let error = refusal(h.response().await, 1);
    assert_eq!(error.code, -32600);
    assert!(error.message.contains("outside"), "{}", error.message);

    // `..` cannot climb out of an allowed folder.
    h.send(
        2,
        "fs/readFile",
        json!({"path": folders.allowed.join("..").join("outside").join("out.txt")}),
    )
    .await;
    assert_eq!(refusal(h.response().await, 2).code, -32600);

    // No cwd: the server cwd (this crate, outside the allowed folder) is refused too.
    h.send(3, "command/exec", json!({"command": ["true"]}))
        .await;
    assert_eq!(refusal(h.response().await, 3).code, -32600);

    h.send(
        4,
        "fs/readFile",
        json!({"path": folders.allowed.join("in.txt")}),
    )
    .await;
    let OutgoingMessage::Response(response) = h.response().await else {
        panic!("reading inside the allowed folder should succeed")
    };
    assert_eq!(
        serde_json::to_value(response.result)?["dataBase64"],
        json!("aW4=")
    );

    // File-name search may not list another folder's names.
    h.send(
        5,
        "fuzzyFileSearch",
        json!({"query": "out", "roots": [folders.outside]}),
    )
    .await;
    assert_eq!(refusal(h.response().await, 5).code, -32600);
    h.send(
        6,
        "fuzzyFileSearch",
        json!({"query": "in", "roots": [folders.allowed]}),
    )
    .await;
    let OutgoingMessage::Response(response) = h.response().await else {
        panic!("searching inside the allowed folder should succeed")
    };
    let files = serde_json::to_value(response.result)?["files"].clone();
    assert_eq!(files[0]["path"], json!("in.txt"), "{files}");

    // Neither may a text search.
    h.send(
        8,
        "fs/searchText",
        json!({"root": folders.outside, "query": "out"}),
    )
    .await;
    assert_eq!(refusal(h.response().await, 8).code, -32600);

    // Requests the allow-list cannot judge are refused outright.
    h.send(
        7,
        "externalAgentConfig/import",
        json!({"migrationItems": []}),
    )
    .await;
    let error = refusal(h.response().await, 7);
    assert!(error.message.contains("cannot import"), "{}", error.message);
    h.shutdown().await;
    Ok(())
}

#[test]
fn local_connections_ignore_the_remote_allow_list() -> Result<()> {
    with_stack(local_connections_ignore_the_remote_allow_list_body())
}

async fn local_connections_ignore_the_remote_allow_list_body() -> Result<()> {
    let folders = folders();
    let mut h = Harness::new_with_remote_roots(
        ConnectionOrigin::Stdio,
        || true,
        Some(vec![folders.allowed.clone()]),
    )
    .await?;
    h.initialize("local", /*opt_in*/ true).await;
    h.send(
        1,
        "fs/readFile",
        json!({"path": folders.outside.join("out.txt")}),
    )
    .await;
    let OutgoingMessage::Response(response) = h.response().await else {
        panic!("a local client is not limited by the remote allow-list")
    };
    assert_eq!(
        serde_json::to_value(response.result)?["dataBase64"],
        json!("b3V0")
    );
    h.shutdown().await;
    Ok(())
}

#[test]
fn remote_clients_reach_only_the_shared_sessions() -> Result<()> {
    with_stack(remote_clients_reach_only_the_shared_sessions_body())
}

async fn remote_clients_reach_only_the_shared_sessions_body() -> Result<()> {
    let folders = folders();
    let sessions = folders.allowed.join("remote-threads.json");
    std::fs::write(
        &sessions,
        r#"{"relay":true,"mode":"listed","threadIds":["t1"]}"#,
    )?;
    let mut h = Harness::new_with_remote_policy(
        ConnectionOrigin::RemoteControl,
        || true,
        Some(vec![folders.allowed.clone()]),
        Some(sessions.clone()),
    )
    .await?;
    h.initialize("remote", /*opt_in*/ true).await;

    h.send(1, "thread/resume", json!({"threadId": "t2"})).await;
    let error = refusal(h.response().await, 1);
    assert_eq!(error.code, -32600);
    assert!(error.message.contains("not shared"), "{}", error.message);

    h.send(2, "thread/start", json!({"cwd": folders.allowed}))
        .await;
    assert!(
        refusal(h.response().await, 2)
            .message
            .contains("only the sessions")
    );

    h.send(3, "remoteControl/disable", json!({})).await;
    assert_eq!(refusal(h.response().await, 3).code, -32600);

    // Reading a file in an allowed folder needs no session.
    h.send(
        4,
        "fs/readFile",
        json!({"path": folders.allowed.join("in.txt")}),
    )
    .await;
    let OutgoingMessage::Response(_) = h.response().await else {
        panic!("reading inside the allowed folder should still work")
    };

    // The file is read again: sharing everything lets the start through the session check (it then fails or
    // succeeds on its own, but not with the sharing refusal).
    std::fs::write(&sessions, r#"{"relay":true,"mode":"all"}"#)?;
    h.send(5, "thread/resume", json!({"threadId": "t2"})).await;
    if let OutgoingMessage::Error(error) = h.response().await {
        assert!(
            !error.error.message.contains("not shared"),
            "{}",
            error.error.message
        );
    }
    h.shutdown().await;
    Ok(())
}

#[test]
fn local_clients_are_not_limited_by_the_shared_sessions() -> Result<()> {
    with_stack(local_clients_are_not_limited_by_the_shared_sessions_body())
}

async fn local_clients_are_not_limited_by_the_shared_sessions_body() -> Result<()> {
    let folders = folders();
    let sessions = folders.allowed.join("remote-threads.json");
    std::fs::write(
        &sessions,
        r#"{"relay":false,"mode":"listed","threadIds":[]}"#,
    )?;
    let mut h = Harness::new_with_remote_policy(
        ConnectionOrigin::Stdio,
        || true,
        /*remote_roots*/ None,
        Some(sessions),
    )
    .await?;
    h.initialize("local", /*opt_in*/ true).await;
    h.send(1, "thread/resume", json!({"threadId": "t2"})).await;
    if let OutgoingMessage::Error(error) = h.response().await {
        assert!(
            !error.error.message.contains("not shared"),
            "{}",
            error.error.message
        );
    }
    h.shutdown().await;
    Ok(())
}

#[test]
fn local_requests_follow_the_launchers_roots_file() -> Result<()> {
    with_stack(local_requests_follow_the_launchers_roots_file_body())
}

async fn local_requests_follow_the_launchers_roots_file_body() -> Result<()> {
    let folders = folders();
    let file = folders.allowed.parent().unwrap().join("roots.json");
    std::fs::write(&file, serde_json::to_vec(&[&folders.allowed])?)?;
    for origin in [ConnectionOrigin::Stdio, ConnectionOrigin::WebSocket] {
        let mut h = Harness::new_with_local_roots(origin, file.clone()).await?;
        h.initialize("local", /*opt_in*/ true).await;
        h.send(
            1,
            "fs/readFile",
            json!({"path": folders.outside.join("out.txt")}),
        )
        .await;
        let error = refusal(h.response().await, 1);
        assert_eq!(error.code, -32600);
        assert!(
            error
                .message
                .starts_with("this session may only use the folders allowed in the app"),
            "{origin:?}: {}",
            error.message
        );
        h.send(
            2,
            "fs/readFile",
            json!({"path": folders.allowed.join("in.txt")}),
        )
        .await;
        assert!(
            matches!(h.response().await, OutgoingMessage::Response(_)),
            "{origin:?}: inside the allowed folder"
        );
        h.shutdown().await;
    }

    // A folder the launcher allows later counts without a restart.
    let mut h = Harness::new_with_local_roots(ConnectionOrigin::Stdio, file.clone()).await?;
    h.initialize("local", /*opt_in*/ true).await;
    std::fs::write(
        &file,
        serde_json::to_vec(&[&folders.allowed, &folders.outside])?,
    )?;
    h.send(
        3,
        "fs/readFile",
        json!({"path": folders.outside.join("out.txt")}),
    )
    .await;
    assert!(matches!(h.response().await, OutgoingMessage::Response(_)));
    h.shutdown().await;

    // In-process callers are the server's own code: never checked.
    std::fs::write(&file, "[]")?;
    let mut h = Harness::new_with_local_roots(ConnectionOrigin::InProcess, file.clone()).await?;
    h.initialize("local", /*opt_in*/ true).await;
    h.send(
        4,
        "fs/readFile",
        json!({"path": folders.outside.join("out.txt")}),
    )
    .await;
    assert!(matches!(h.response().await, OutgoingMessage::Response(_)));
    h.shutdown().await;
    Ok(())
}
