use super::*;
use http::HeaderValue;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

#[tokio::test]
async fn custom_ca_fallback_preserves_builder_configuration() {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("HTTP listener should bind");
    let address = listener
        .local_addr()
        .expect("HTTP listener should have an address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("HTTP listener should accept");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let bytes_read = stream.read(&mut chunk).expect("HTTP request should read");
            assert!(bytes_read > 0, "HTTP request should include headers");
            request.extend_from_slice(&chunk[..bytes_read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("HTTP listener should write response");
        String::from_utf8(request).expect("HTTP request should be UTF-8")
    });
    let mut headers = HeaderMap::new();
    headers.insert("x-builder-test", HeaderValue::from_static("preserved"));
    let client = HttpClientBuilder::new()
        .default_headers(headers)
        .build_with_custom_ca_fallback_using(ProxyRouting::Direct, |_| {
            Err(BuildCustomCaTransportError::InvalidCaFile {
                source_env: "TEST_CA_ENV",
                path: PathBuf::from("invalid-test-ca.pem"),
                detail: "synthetic invalid CA".to_string(),
            })
        });

    let client = HttpClient {
        backend: HttpClientBackend::Direct(client),
    };

    let response = client
        .get(format!("http://{address}/fallback"))
        .send()
        .await
        .expect("fallback client should send request");
    assert!(response.status().is_success());
    let request = server.join().expect("HTTP listener should finish");
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("x-builder-test: preserved"))
    );
}

/// Accepts one HTTP/1.1 request on `listener`, answers 200, and returns the raw request head.
#[cfg(unix)]
fn serve_one_unix_request(
    listener: std::os::unix::net::UnixListener,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("unix listener should accept");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let bytes_read = stream.read(&mut chunk).expect("HTTP request should read");
            assert!(bytes_read > 0, "HTTP request should include headers");
            request.extend_from_slice(&chunk[..bytes_read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("unix listener should write response");
        let request = String::from_utf8(request).expect("HTTP request should be UTF-8");
        request
            .split("\r\n\r\n")
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    })
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_client_sends_requests_through_the_socket() {
    let socket_dir = tempfile::tempdir().expect("socket dir");
    let socket_path = socket_dir.path().join("llm.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("unix listener should bind");
    let server = serve_one_unix_request(listener);
    let client = HttpClientBuilder::new()
        .build_unix_socket(&socket_path)
        .expect("unix socket client should build");

    let response = client
        .post("http://nvidia.llm.internal/v1/chat/completions")
        .send()
        .await
        .expect("request should reach the unix socket");

    assert!(response.status().is_success());
    let head = server.join().expect("unix listener should finish");
    let mut lines = head.lines();
    assert_eq!(
        lines.next(),
        Some("post /v1/chat/completions http/1.1"),
        "{head}"
    );
    assert!(
        lines.any(|line| line == "host: nvidia.llm.internal"),
        "{head}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_client_ignores_configured_proxies() {
    let socket_dir = tempfile::tempdir().expect("socket dir");
    let socket_path = socket_dir.path().join("llm.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("unix listener should bind");
    let server = serve_one_unix_request(listener);
    // A proxy that would refuse connections: reaching the socket proves it was ignored.
    let proxied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("proxy url should parse"));
    let client = unix_socket_reqwest_builder(proxied, &socket_path)
        .build()
        .expect("unix socket client should build");

    let response = client
        .get("http://nvidia.llm.internal/v1/models")
        .send()
        .await
        .expect("request should bypass the proxy and reach the unix socket");

    assert!(response.status().is_success());
    let head = server.join().expect("unix listener should finish");
    assert_eq!(
        head.lines().next(),
        Some("get /v1/models http/1.1"),
        "{head}"
    );
}

/// A server cannot keep a connection reusable after the caller requests closure.
#[tokio::test]
async fn connection_close_prevents_reuse_when_server_ignores_it()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let url = format!("http://{}/", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await?);
        }
        // Deliberately ignore the request's Connection: close header.
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
        stream.write_all(response).await?;
        let mut byte = [0];
        let reused = tokio::select! {
            read = stream.read(&mut byte) => {
                if read? == 0 {
                    stream = listener.accept().await?.0;
                    false
                } else {
                    true
                }
            }
            accepted = listener.accept() => {
                stream = accepted?.0;
                false
            }
        };
        headers.clear();
        if reused {
            headers.push(byte[0]);
        }
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await?);
        }
        stream.write_all(response).await?;
        Ok::<bool, std::io::Error>(reused)
    });
    let client = HttpClientBuilder::new().build_direct()?;
    for _ in 0..2 {
        client
            .get(&url)
            .header(http::header::CONNECTION, HeaderValue::from_static("close"))
            .send()
            .await?
            .bytes()
            .await?;
    }
    assert!(
        !server.await??,
        "the server reused a connection marked closed"
    );
    Ok(())
}
