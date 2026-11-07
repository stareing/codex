//! `app-server proxy --lines`: one JSON-RPC message per line on stdin/stdout, relayed to an app-server control
//! socket (which speaks WebSocket). A launcher that already talks newline-delimited JSON to
//! `app-server --listen stdio://` can then join a running server instead of starting its own — the desktop app's
//! local sessions share the remote-control process this way, so a phone sees the same live session
//! (`docs/agent-codex/REMOTE_CONTROL.md` R9).

use std::path::Path;

use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio_tungstenite::tungstenite::Message;

/// Relays stdin/stdout until either side ends.
pub async fn proxy_lines(socket_path: &Path) -> Result<()> {
    proxy_lines_with(socket_path, tokio::io::stdin(), tokio::io::stdout()).await
}

pub(crate) async fn proxy_lines_with(
    socket_path: &Path,
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> Result<()> {
    let websocket = crate::client::connect(socket_path).await?;
    let (mut sink, mut stream) = websocket.split();
    let to_socket = async {
        let mut lines = BufReader::new(input).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            sink.send(Message::Text(line.into())).await?;
        }
        sink.close().await.ok();
        anyhow::Ok(())
    };
    let from_socket = async {
        while let Some(message) = stream.next().await {
            let bytes = match message? {
                Message::Text(text) => text.as_str().as_bytes().to_vec(),
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            output.write_all(&bytes).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = to_socket => result,
        result = from_socket => result,
    }
}

#[cfg(test)]
mod tests {
    use codex_uds::UnixListener;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio_tungstenite::accept_async;

    use super::*;

    #[tokio::test]
    async fn lines_go_to_the_socket_and_messages_come_back_as_lines() -> Result<()> {
        let dir = TempDir::new()?;
        let socket_path = dir.path().join("app-server.sock");
        let mut listener = UnixListener::bind(&socket_path).await?;
        let server = tokio::spawn(async move {
            let mut websocket = accept_async(listener.accept().await?).await?;
            let mut got = Vec::new();
            while got.len() < 2 {
                match websocket.next().await.transpose()? {
                    Some(Message::Text(text)) => got.push(text.as_str().to_string()),
                    Some(_) => {}
                    None => break,
                }
            }
            websocket
                .send(Message::Text(r#"{"id":1,"result":{}}"#.into()))
                .await?;
            websocket
                .send(Message::Text(r#"{"method":"n"}"#.into()))
                .await?;
            websocket.close(None).await.ok();
            anyhow::Ok(got)
        });

        let (mut stdin_tx, stdin_rx) = tokio::io::duplex(4096);
        let (stdout_tx, mut stdout_rx) = tokio::io::duplex(4096);
        stdin_tx
            .write_all(b"{\"id\":1,\"method\":\"initialize\"}\n\n{\"method\":\"initialized\"}\n")
            .await?;
        let proxy =
            tokio::spawn(async move { proxy_lines_with(&socket_path, stdin_rx, stdout_tx).await });

        let got = server.await??;
        assert_eq!(
            got,
            [
                r#"{"id":1,"method":"initialize"}"#,
                r#"{"method":"initialized"}"#
            ],
            "blank lines skipped"
        );
        proxy.await??;
        let mut printed = String::new();
        stdout_rx.read_to_string(&mut printed).await?;
        assert_eq!(printed, "{\"id\":1,\"result\":{}}\n{\"method\":\"n\"}\n");
        drop(stdin_tx);
        Ok(())
    }
}
