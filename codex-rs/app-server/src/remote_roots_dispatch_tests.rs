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
