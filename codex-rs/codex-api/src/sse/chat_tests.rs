use super::*;
use crate::requests::chat_tools::ChatToolTarget;
use codex_client::TransportError;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::io::ReaderStream;

fn sse_body(chunks: &[Value], done: bool) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

async fn run(body: String, tool_index: ChatToolIndex) -> Vec<Result<ResponseEvent, ApiError>> {
    let (tx, mut rx) = mpsc::channel(64);
    let stream = ReaderStream::new(std::io::Cursor::new(body))
        .map_err(|err| TransportError::Network(err.to_string()));
    tokio::spawn(process_chat_sse(
        Box::pin(stream),
        tx,
        Duration::from_secs(1),
        /*telemetry*/ None,
        tool_index,
    ));
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        out.push(event);
    }
    out
}

/// `ResponseEvent` is not `PartialEq`; compare its debug rendering instead.
fn render(events: &[Result<ResponseEvent, ApiError>]) -> Vec<String> {
    events.iter().map(|event| format!("{event:?}")).collect()
}

fn ok(events: Vec<ResponseEvent>) -> Vec<String> {
    events
        .into_iter()
        .map(|event| format!("{:?}", Ok::<_, ApiError>(event)))
        .collect()
}

fn completed(response_id: &str, token_usage: Option<TokenUsage>) -> ResponseEvent {
    ResponseEvent::Completed {
        response_id: response_id.to_string(),
        token_usage,
        usage_metadata: None,
        end_turn: None,
    }
}

fn created(response_id: &str) -> ResponseEvent {
    ResponseEvent::Created {
        response_id: Some(response_id.to_string()),
    }
}

fn content_chunk(content: &str) -> Value {
    json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": content}}]})
}

fn finish_chunk(reason: &str) -> Value {
    json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

#[tokio::test]
async fn parses_plain_text_stream() {
    let body = sse_body(
        &[
            json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            content_chunk("Hel"),
            content_chunk("lo"),
            finish_chunk("stop"),
        ],
        /*done*/ true,
    );

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(
        render(&events),
        ok(vec![
            created("chatcmpl-1"),
            ResponseEvent::OutputItemAdded(message_item(String::new())),
            ResponseEvent::OutputTextDelta("Hel".to_string()),
            ResponseEvent::OutputTextDelta("lo".to_string()),
            ResponseEvent::OutputItemDone(message_item("Hello".to_string())),
            completed("chatcmpl-1", /*token_usage*/ None),
        ])
    );
}

#[tokio::test]
async fn parses_reasoning_then_text() {
    let body = sse_body(
        &[
            json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"reasoning_content": "Think"}}]}),
            json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"reasoning": "ing"}}]}),
            content_chunk("Answer"),
            finish_chunk("stop"),
        ],
        /*done*/ true,
    );

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(
        render(&events),
        ok(vec![
            created("chatcmpl-1"),
            ResponseEvent::OutputItemAdded(reasoning_item(String::new())),
            ResponseEvent::ReasoningContentDelta {
                delta: "Think".to_string(),
                content_index: 0,
            },
            ResponseEvent::ReasoningContentDelta {
                delta: "ing".to_string(),
                content_index: 0,
            },
            ResponseEvent::OutputItemDone(reasoning_item("Thinking".to_string())),
            ResponseEvent::OutputItemAdded(message_item(String::new())),
            ResponseEvent::OutputTextDelta("Answer".to_string()),
            ResponseEvent::OutputItemDone(message_item("Answer".to_string())),
            completed("chatcmpl-1", /*token_usage*/ None),
        ])
    );
}

#[tokio::test]
async fn accumulates_parallel_tool_calls_and_usage_only_chunk() {
    let body = sse_body(
        &[
            json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "type": "function", "function": {"name": "shell", "arguments": "{\"comm"}},
                {"index": 1, "id": "call_b", "type": "function", "function": {"name": "apply_patch", "arguments": ""}}
            ]}}]}),
            json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"input\":\"*** Begin"}}
            ]}}]}),
            json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "and\":[\"ls\"]}"}},
                {"index": 1, "function": {"arguments": " Patch\"}"}}
            ]}}]}),
            json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"id": "chatcmpl-2", "choices": [], "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 5}
            }}),
        ],
        /*done*/ true,
    );
    let tool_index = ChatToolIndex::from_targets([(
        "apply_patch".to_string(),
        ChatToolTarget {
            namespace: None,
            name: "apply_patch".to_string(),
            kind: ChatToolKind::Freeform,
        },
    )]);

    let events = run(body, tool_index).await;

    assert_eq!(
        render(&events),
        ok(vec![
            created("chatcmpl-2"),
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                name: "shell".to_string(),
                namespace: None,
                arguments: "{\"command\":[\"ls\"]}".to_string(),
                encrypted_function_args: None,
                call_id: "call_a".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseEvent::OutputItemDone(ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call_b".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                input: "*** Begin Patch".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            completed(
                "chatcmpl-2",
                Some(TokenUsage {
                    input_tokens: 100,
                    cached_input_tokens: 64,
                    cache_write_input_tokens: 0,
                    output_tokens: 20,
                    reasoning_output_tokens: 5,
                    total_tokens: 120,
                    codex_rollout_budget_units: None,
                }),
            ),
        ])
    );
}

#[tokio::test]
async fn maps_namespaced_tool_call_back_to_namespace() {
    let body = sse_body(
        &[
            json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_ns", "function": {"name": "mcp__docs__search", "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"}]}),
        ],
        /*done*/ false,
    );
    let tool_index = ChatToolIndex::from_targets([(
        "mcp__docs__search".to_string(),
        ChatToolTarget {
            namespace: Some("mcp__docs__".to_string()),
            name: "search".to_string(),
            kind: ChatToolKind::Function,
        },
    )]);

    let events = run(body, tool_index).await;

    assert_eq!(
        render(&events),
        ok(vec![
            created("chatcmpl-3"),
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                name: "search".to_string(),
                namespace: Some("mcp__docs__".to_string()),
                arguments: "{}".to_string(),
                encrypted_function_args: None,
                call_id: "call_ns".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }),
            completed("chatcmpl-3", /*token_usage*/ None),
        ])
    );
}

#[tokio::test]
async fn freeform_input_passes_through_non_json_arguments() {
    assert_eq!(
        vec![
            freeform_input("{\"input\":\"raw\"}".to_string()),
            freeform_input("*** Begin Patch".to_string()),
            freeform_input("{\"other\":1}".to_string()),
        ],
        vec![
            "raw".to_string(),
            "*** Begin Patch".to_string(),
            "{\"other\":1}".to_string(),
        ]
    );
}

#[tokio::test]
async fn mid_stream_error_object_is_classified() {
    let body = sse_body(
        &[
            content_chunk("partial"),
            json!({"error": {"message": "Rate limit reached, try again in 2s", "code": "rate_limit_exceeded"}}),
        ],
        /*done*/ false,
    );

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(events.len(), 4);
    assert!(matches!(
        events.last(),
        Some(Err(ApiError::RateLimitExceeded { message, retry_after: Some(_) }))
            if message == "Rate limit reached, try again in 2s"
    ));
}

#[tokio::test]
async fn numeric_error_code_keeps_message() {
    let body = sse_body(
        &[json!({"error": {"message": "upstream overloaded", "code": 503}})],
        /*done*/ false,
    );

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(
        render(&events),
        vec![format!(
            "{:?}",
            Err::<ResponseEvent, _>(ApiError::Retryable {
                message: "upstream overloaded".to_string(),
                retry_after: None,
            })
        )]
    );
}

#[tokio::test]
async fn stream_closed_without_finish_is_an_error() {
    let body = sse_body(&[content_chunk("partial")], /*done*/ false);

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(
        render(&events[3..]),
        vec![format!(
            "{:?}",
            Err::<ResponseEvent, _>(ApiError::Stream(
                "stream closed before chat completion finished".to_string()
            ))
        )]
    );
}

#[tokio::test]
async fn length_finish_reports_incomplete_response() {
    let body = sse_body(
        &[content_chunk("cut"), finish_chunk("length")],
        /*done*/ true,
    );

    let events = run(body, ChatToolIndex::default()).await;

    assert_eq!(
        render(&events),
        vec![
            format!("{:?}", Ok::<_, ApiError>(created("chatcmpl-1"))),
            format!(
                "{:?}",
                Ok::<_, ApiError>(ResponseEvent::OutputItemAdded(message_item(String::new())))
            ),
            format!(
                "{:?}",
                Ok::<_, ApiError>(ResponseEvent::OutputTextDelta("cut".to_string()))
            ),
            format!(
                "{:?}",
                Ok::<_, ApiError>(ResponseEvent::OutputItemDone(message_item(
                    "cut".to_string()
                )))
            ),
            format!(
                "{:?}",
                Err::<ResponseEvent, _>(incomplete_error("max_output_tokens"))
            ),
        ]
    );
}
