#![allow(clippy::expect_used)]
use std::sync::Arc;
use std::time::Duration;

use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::ChatCompletionsClient;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesOptions;
use codex_api::RetryConfig;
use codex_client::ReqwestTransport;
use codex_client::TransportError;
use codex_http_client::HttpClientBuilder;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Default)]
struct BearerAuth;

impl AuthProvider for BearerAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer test-key"),
        );
    }
}

fn provider(base_url: &str) -> Provider {
    Provider {
        name: "nim".to_string(),
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
    }
}

fn client(server: &MockServer) -> ChatCompletionsClient<ReqwestTransport> {
    let transport = ReqwestTransport::from_http_client(
        HttpClientBuilder::new()
            .build_direct()
            .expect("test HTTP client should build"),
    );
    ChatCompletionsClient::new(
        transport,
        provider(&format!("{}/v1", server.uri())),
        Arc::new(BearerAuth),
    )
}

fn request() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "meta/llama-3.3-70b-instruct".to_string(),
        instructions: "Be brief.".to_string(),
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: Some("cache".to_string()),
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

#[tokio::test]
async fn chat_client_posts_chat_body_and_streams_events() {
    let server = MockServer::start().await;
    let sse = [
        json!({"id": "chatcmpl-9", "choices": [{"index": 0, "delta": {"content": "Hello"}}]}),
        json!({"id": "chatcmpl-9", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"id": "chatcmpl-9", "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut stream = client(&server)
        .stream_request(&request(), ResponsesOptions::default())
        .await
        .expect("stream should start");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(format!("{:?}", event.expect("event should parse")));
    }

    let received = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        body,
        json!({
            "model": "meta/llama-3.3-70b-instruct",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "hi"}
            ],
            "stream": true,
            "stream_options": {"include_usage": true}
        })
    );
    let expected_message = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "Hello".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert_eq!(events.len(), 5, "{events:#?}");
    assert_eq!(
        events[3],
        format!("{:?}", ResponseEvent::OutputItemDone(expected_message))
    );
    assert!(events[4].starts_with("Completed { response_id: \"chatcmpl-9\""));
}

#[tokio::test]
async fn chat_client_surfaces_http_errors() {
    for status in [429_u16, 500] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(json!({"error": {"message": "nope", "code": status}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .stream_request(&request(), ResponsesOptions::default())
            .await
            .err()
            .expect("HTTP error should fail the request");

        match error {
            ApiError::Transport(TransportError::Http {
                status: actual,
                body,
                ..
            }) => {
                assert_eq!(actual, StatusCode::from_u16(status).expect("valid status"));
                assert!(body.unwrap_or_default().contains("nope"));
            }
            other => panic!("unexpected error for {status}: {other:?}"),
        }
    }
}
