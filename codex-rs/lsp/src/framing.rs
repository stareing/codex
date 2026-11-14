//! Base-protocol framing: every JSON-RPC message is preceded by `Content-Length: <n>\r\n\r\n`.

use serde_json::Value;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;

/// Upper bound for one header block. Servers send one or two short header lines.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

const CONTENT_LENGTH: &str = "content-length";

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The header block ended without a `Content-Length` line.
    #[error("message header has no Content-Length")]
    MissingContentLength,
    #[error("invalid message header: {0}")]
    InvalidHeader(String),
    #[error("message header exceeds {MAX_HEADER_BYTES} bytes")]
    HeaderTooLong,
    /// The body is larger than the configured limit; the stream cannot be resynchronised.
    #[error("message body of {length} bytes exceeds the {max} byte limit")]
    TooLarge { length: usize, max: usize },
    #[error("message body is not JSON: {0}")]
    InvalidJson(String),
    /// The stream ended in the middle of a message.
    #[error("stream ended inside a message")]
    UnexpectedEof,
}

/// Serialises one message with its header.
pub fn encode(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Reads one message. `Ok(None)` means the stream ended cleanly between messages.
pub async fn read_message<R>(reader: &mut R, max_body: usize) -> Result<Option<Value>, FramingError>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    let mut saw_any_header = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return if saw_any_header {
                Err(FramingError::UnexpectedEof)
            } else {
                Ok(None)
            };
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(FramingError::HeaderTooLong);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !saw_any_header {
                // Tolerate stray blank lines between messages.
                continue;
            }
            break;
        }
        saw_any_header = true;
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(FramingError::InvalidHeader(trimmed.to_string()));
        };
        if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH) {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| FramingError::InvalidHeader(trimmed.to_string()))?;
            content_length = Some(parsed);
        }
    }
    let length = content_length.ok_or(FramingError::MissingContentLength)?;
    if length > max_body {
        return Err(FramingError::TooLarge {
            length,
            max: max_body,
        });
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            FramingError::UnexpectedEof
        } else {
            FramingError::Io(err)
        }
    })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| FramingError::InvalidJson(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const LIMIT: usize = 1024;

    async fn read_all(bytes: &[u8]) -> Vec<Result<Option<Value>, String>> {
        let mut reader = tokio::io::BufReader::new(bytes);
        let mut out = Vec::new();
        loop {
            let next = read_message(&mut reader, LIMIT).await;
            let stop = !matches!(next, Ok(Some(_)));
            out.push(next.map_err(|err| err.to_string()));
            if stop {
                return out;
            }
        }
    }

    #[tokio::test]
    async fn round_trips_consecutive_messages_with_multibyte_text() {
        let first = json!({"jsonrpc": "2.0", "id": 1, "method": "a", "params": {"text": "函数 ✓"}});
        let second = json!({"jsonrpc": "2.0", "method": "b"});
        let mut bytes = encode(&first);
        // Content-Length counts bytes, not characters: the multibyte body must still parse.
        assert!(String::from_utf8_lossy(&bytes).starts_with("Content-Length: "));
        bytes.extend(encode(&second));
        assert_eq!(
            read_all(&bytes).await,
            vec![Ok(Some(first)), Ok(Some(second)), Ok(None)]
        );
    }

    #[tokio::test]
    async fn accepts_extra_headers_in_any_case() {
        let body = br#"{"id":2}"#;
        let mut bytes = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        assert_eq!(read_all(&bytes).await[0], Ok(Some(json!({"id": 2}))));
    }

    #[tokio::test]
    async fn rejects_missing_length_oversized_body_and_truncated_stream() {
        let missing = read_all(b"Content-Type: x\r\n\r\n{}").await;
        assert_eq!(
            missing[0],
            Err(FramingError::MissingContentLength.to_string())
        );

        let big = format!("Content-Length: {}\r\n\r\n", LIMIT + 1);
        let oversized = read_all(big.as_bytes()).await;
        assert!(matches!(&oversized[0], Err(message) if message.contains("exceeds")));

        let truncated = read_all(b"Content-Length: 10\r\n\r\n{}").await;
        assert_eq!(truncated[0], Err(FramingError::UnexpectedEof.to_string()));

        let header_only = read_all(b"Content-Length: 2\r\n").await;
        assert_eq!(header_only[0], Err(FramingError::UnexpectedEof.to_string()));
    }
}
