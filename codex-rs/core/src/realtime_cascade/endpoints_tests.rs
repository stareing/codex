use super::*;
use codex_api::AuthProvider;
use codex_api::RetryConfig;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

pub(crate) struct TestAuth;

impl AuthProvider for TestAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), codex_api::AuthError> {
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-token"),
        );
        Ok(())
    }
}

pub(crate) fn test_endpoints(
    base_url: &str,
) -> ProviderAudioEndpoints<codex_api::ReqwestTransport> {
    let provider = ApiProvider {
        name: "test".to_string(),
        base_url: base_url.to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(5),
    };
    // Direct: the mock server is local and the test must not depend on the machine's proxies.
    let client = codex_client::HttpClientBuilder::new()
        .build_direct()
        .expect("direct test client");
    let transport = codex_api::ReqwestTransport::from_http_client(client);
    ProviderAudioEndpoints::new(transport, provider, Arc::new(TestAuth))
}

#[test]
fn transcription_form_has_the_file_and_fields() {
    let wav = b"RIFF----WAVEdata".to_vec();
    let (content_type, body) = transcription_form(&wav, Some("zh"));
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.starts_with(&format!("--{boundary}\r\n")));
    assert!(body.ends_with(&format!("\r\n--{boundary}--\r\n")));
    assert!(body.contains("name=\"response_format\"\r\n\r\njson\r\n"));
    assert!(body.contains("name=\"language\"\r\n\r\nzh\r\n"));
    assert!(body.contains(
        "name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF----WAVEdata\r\n"
    ));
    let (_, without_language) = transcription_form(&wav, None);
    assert!(!String::from_utf8_lossy(&without_language).contains("name=\"language\""));
}

#[test]
fn error_codes_are_copied_only_when_short_and_plain() {
    let cases = [
        (
            r#"{"error":{"code":"voice_unavailable","message":"x"}}"#,
            Some("voice_unavailable"),
        ),
        (r#"{"code":"too_many_streams"}"#, Some("too_many_streams")),
        (r#"{"error":{"code":"has spaces and <html>"}}"#, None),
        (r#"{"error":{"code":12}}"#, None),
        ("<html>bad gateway</html>", None),
    ];
    for (body, expected) in cases {
        assert_eq!(error_code(body).as_deref(), expected, "{body}");
    }
    let long = format!(r#"{{"code":"{}"}}"#, "a".repeat(MAX_ERROR_CODE_CHARS + 1));
    assert_eq!(error_code(&long), None);
}

#[test]
fn transport_errors_never_carry_the_body_or_url() {
    let error = TransportError::Http {
        status: http::StatusCode::BAD_GATEWAY,
        url: Some("https://secret.example/v1/audio/speech".to_string()),
        headers: None,
        body: Some(
            r#"{"error":{"code":"voice_failed","message":"key sk-123 rejected"}}"#.to_string(),
        ),
        retry_after: None,
    };
    assert_eq!(
        describe_transport_error("speech", &error).0,
        "speech failed: HTTP 502 (voice_failed)"
    );
    assert_eq!(
        describe_transport_error("speech", &TransportError::Timeout).0,
        "speech failed: timed out"
    );
}

#[tokio::test]
async fn voices_are_read_from_the_provider() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/audio/voices"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "available": true,
            "default_voice": "af_heart",
            "voices": [
                {"id": "af_heart", "label": "Heart", "lang": "en"},
                {"id": "zf_xiaobei"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let endpoints = test_endpoints(&format!("{}/v1", server.uri()));
    assert_eq!(
        endpoints.voices().await.unwrap(),
        CascadeVoicesList {
            voices: vec![
                CascadeVoice {
                    id: "af_heart".to_string(),
                    label: Some("Heart".to_string()),
                    lang: Some("en".to_string()),
                },
                CascadeVoice {
                    id: "zf_xiaobei".to_string(),
                    label: None,
                    lang: None,
                },
            ],
            default_voice: Some("af_heart".to_string()),
        }
    );
}

#[tokio::test]
async fn speech_posts_json_and_returns_the_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(header("content-type", "application/json"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "input": "Hello.",
            "voice": "af_heart",
            "response_format": "wav"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"RIFFdata".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let endpoints = test_endpoints(&format!("{}/v1", server.uri()));
    assert_eq!(
        endpoints
            .speak("Hello.".to_string(), Some("af_heart".to_string()))
            .await
            .unwrap(),
        b"RIFFdata".to_vec()
    );
    // Nothing listens on the discard port: a connection failure, worded without the URL.
    let unreachable = test_endpoints("http://127.0.0.1:9/v1");
    let error = unreachable.speak("x".to_string(), None).await.unwrap_err();
    assert_eq!(error.0, "speech failed: the provider could not be reached");
}
