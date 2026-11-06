use super::*;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

#[test]
fn refusals_become_readable_messages() {
    let err = CatalogError::from_response(
        409,
        &serde_json::json!({"code": "ambiguous_ref", "detail": "Both kinds exist.",
                            "candidates": ["store:mcp/pdf", {"ref": "store:skill/pdf"}]}),
    );
    assert_eq!(err.code, "ambiguous_ref");
    assert_eq!(err.candidates, vec!["store:mcp/pdf", "store:skill/pdf"]);
    assert!(err.message().starts_with("Both kinds exist."));
    // 401 always means "sign in again", whatever the body says.
    let err = CatalogError::from_response(401, &serde_json::json!({"code": "token_revoked"}));
    assert_eq!(err.code, "unauthorized");
    assert!(err.message().contains("onigio-agent login"));
    // No body: the status decides.
    assert_eq!(
        CatalogError::from_response(404, &serde_json::Value::Null).code,
        "not_found"
    );
    assert_eq!(
        CatalogError::from_response(429, &serde_json::Value::Null).code,
        "throttled"
    );
    assert_eq!(
        CatalogError::from_response(500, &serde_json::Value::Null).message(),
        "the request failed (http_500)"
    );
    // Server text is kept on one line and bounded.
    let err = CatalogError::from_response(
        400,
        &serde_json::json!({"code": "x", "detail": format!("a\nb{}", "c".repeat(1000))}),
    );
    assert!(!err.detail.contains('\n'));
    assert!(err.detail.chars().count() <= 300);
}

#[tokio::test]
async fn reads_are_retried_on_gateway_errors_then_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/search/"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/agent-catalog/search/"))
        .and(query_param("q", "pdf tools"))
        .and(query_param("kind", "skill"))
        .and(header("authorization", "Bearer oad_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"ref": "store:skill/pdf", "kind": "skill", "slug": "pdf", "name": "PDF",
                      "summary": "s", "trust": "reviewed", "installs": 3, "supported": true}],
            "pagination": {"page": 1}
        })))
        .mount(&server)
        .await;
    let client = CatalogClient::new(&server.uri(), "oad_test").unwrap();
    let page = client
        .search("pdf tools", Some("skill"), "store", None)
        .await
        .unwrap();
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].reference, "store:skill/pdf");
}

#[tokio::test]
async fn reads_give_up_after_the_bounded_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(502))
        .expect(3)
        .mount(&server)
        .await;
    let client = CatalogClient::new(&server.uri(), "oad_test").unwrap();
    let err = client.installed().await.unwrap_err();
    assert_eq!(err.status, Some(502));
}

#[tokio::test]
async fn writes_are_not_retried_on_server_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agent-catalog/install/"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let client = CatalogClient::new(&server.uri(), "oad_test").unwrap();
    assert_eq!(
        client.install("store:mcp/a").await.unwrap_err().status,
        Some(503)
    );
}

#[tokio::test]
async fn an_unreachable_site_is_a_network_error() {
    // Port 9 on loopback: nothing listens there.
    let client = CatalogClient::new("http://127.0.0.1:9", "oad_test").unwrap();
    let err = client.resolve("store:mcp/a").await.unwrap_err();
    assert_eq!(err.code, "network_error");
}

#[tokio::test]
async fn downloads_stop_at_the_announced_size() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/skills/download/tok/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 100]))
        .mount(&server)
        .await;
    let client = CatalogClient::new(&server.uri(), "oad_test").unwrap();
    let bundle = |size| BundleSpec {
        sha256: "a".repeat(64),
        size,
        download_url: None,
    };
    let bytes = client
        .download("/api/skills/download/tok/", &bundle(100))
        .await
        .unwrap();
    assert_eq!(bytes.len(), 100);
    let err = client
        .download("/api/skills/download/tok/", &bundle(10))
        .await
        .unwrap_err();
    assert_eq!(err.code, "bundle_size_mismatch");
    let err = client
        .download("ftp://example.com/x", &bundle(10))
        .await
        .unwrap_err();
    assert_eq!(err.code, "bad_download");
}
