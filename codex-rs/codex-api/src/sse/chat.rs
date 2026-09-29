//! Parses a Chat Completions SSE stream into [`ResponseEvent`]s.
//!
//! Chat chunks carry deltas without item boundaries, so the parser opens and
//! closes Responses-style items itself:
//! - `delta.reasoning_content` / `delta.reasoning` stream into a `Reasoning` item;
//! - `delta.content` streams into an assistant `Message` item;
//! - `delta.tool_calls` fragments are accumulated by `index` and emitted as
//!   `FunctionCall` / `CustomToolCall` items when the stream finishes.
//!
//! @invariant: every delta is preceded by `OutputItemAdded` for its item and
//! at most one text or reasoning item is open at a time.

use super::responses_error::parse_failed_response;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::requests::chat_tools::ChatToolIndex;
use crate::requests::chat_tools::ChatToolKind;
use crate::requests::chat_tools::FREEFORM_INPUT_ARGUMENT;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;
use tracing::warn;

const DONE_SENTINEL: &str = "[DONE]";
const REQUEST_ID_HEADER: &str = "x-request-id";
const CHANNEL_CAPACITY: usize = 1600;

pub fn spawn_chat_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    tool_index: ChatToolIndex,
) -> ResponseStream {
    let upstream_request_id = stream_response
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let (tx_event, rx_event) = mpsc::channel(CHANNEL_CAPACITY);
    // @why: Codex rate-limit headers are OpenAI-specific; chat providers never send them.
    tokio::spawn(process_chat_sse(
        stream_response.bytes,
        tx_event,
        idle_timeout,
        telemetry,
        tool_index,
    ));
    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Option<ChatDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    /// OpenRouter / some vLLM builds spell the reasoning field `reasoning`.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: Option<ChatPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<ChatCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct ChatPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<i64>,
}

impl From<ChatUsage> for TokenUsage {
    fn from(usage: ChatUsage) -> Self {
        TokenUsage {
            input_tokens: usage.prompt_tokens,
            cached_input_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0),
            cache_write_input_tokens: 0,
            output_tokens: usage.completion_tokens,
            reasoning_output_tokens: usage
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or(0),
            total_tokens: usage.total_tokens,
            codex_rollout_budget_units: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenItem {
    Reasoning,
    Text,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Mutable parse state for one chat completion stream.
#[derive(Default)]
struct ChatStreamState {
    tool_index: ChatToolIndex,
    response_id: Option<String>,
    created_sent: bool,
    open_item: Option<OpenItem>,
    reasoning_text: String,
    output_text: String,
    tool_calls: BTreeMap<u32, PendingToolCall>,
    finish_reason: Option<String>,
    token_usage: Option<TokenUsage>,
}

impl ChatStreamState {
    /// Folds one chunk into the state and returns the events it produced.
    fn apply_chunk(&mut self, chunk: ChatChunk) -> Result<Vec<ResponseEvent>, ApiError> {
        if let Some(error) = chunk.error {
            return Err(chat_error(error));
        }
        let mut events = Vec::new();
        if self.response_id.is_none() {
            self.response_id = chunk.id.filter(|id| !id.is_empty());
        }
        if !self.created_sent {
            self.created_sent = true;
            events.push(ResponseEvent::Created {
                response_id: self.response_id.clone(),
            });
        }
        if let Some(usage) = chunk.usage {
            self.token_usage = Some(usage.into());
        }
        for choice in chunk.choices {
            let delta = choice.delta.unwrap_or_default();
            if let Some(reasoning) = delta.reasoning_content.or(delta.reasoning)
                && !reasoning.is_empty()
            {
                self.open(OpenItem::Reasoning, &mut events);
                self.reasoning_text.push_str(&reasoning);
                events.push(ResponseEvent::ReasoningContentDelta {
                    delta: reasoning,
                    content_index: 0,
                });
            }
            if let Some(content) = delta.content
                && !content.is_empty()
            {
                self.open(OpenItem::Text, &mut events);
                self.output_text.push_str(&content);
                events.push(ResponseEvent::OutputTextDelta(content));
            }
            for (position, call) in delta.tool_calls.into_iter().enumerate() {
                self.apply_tool_call_delta(position, call);
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        Ok(events)
    }

    fn apply_tool_call_delta(&mut self, position: usize, call: ChatToolCallDelta) {
        let index = call
            .index
            .unwrap_or_else(|| u32::try_from(position).unwrap_or(u32::MAX));
        let pending = self.tool_calls.entry(index).or_default();
        if let Some(id) = call.id.filter(|id| !id.is_empty()) {
            pending.id = Some(id);
        }
        if let Some(function) = call.function {
            if let Some(name) = function.name {
                pending.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                pending.arguments.push_str(&arguments);
            }
        }
    }

    fn open(&mut self, item: OpenItem, events: &mut Vec<ResponseEvent>) {
        if self.open_item == Some(item) {
            return;
        }
        self.close_open_item(events);
        self.open_item = Some(item);
        events.push(ResponseEvent::OutputItemAdded(match item {
            OpenItem::Reasoning => reasoning_item(String::new()),
            OpenItem::Text => message_item(String::new()),
        }));
    }

    fn close_open_item(&mut self, events: &mut Vec<ResponseEvent>) {
        let Some(item) = self.open_item.take() else {
            return;
        };
        events.push(ResponseEvent::OutputItemDone(match item {
            OpenItem::Reasoning => reasoning_item(std::mem::take(&mut self.reasoning_text)),
            OpenItem::Text => message_item(std::mem::take(&mut self.output_text)),
        }));
    }

    /// Emits the closing items and the terminal event (or error).
    fn finish(mut self) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut events = Vec::new();
        self.close_open_item(&mut events);
        let mut results: Vec<Result<ResponseEvent, ApiError>> =
            events.into_iter().map(Ok).collect();
        match self.finish_reason.as_deref() {
            Some("length") => {
                results.push(Err(incomplete_error("max_output_tokens")));
                return results;
            }
            Some("content_filter") => {
                results.push(Err(incomplete_error("content_filter")));
                return results;
            }
            Some(_) | None => {}
        }
        let tool_calls = std::mem::take(&mut self.tool_calls);
        for (index, call) in tool_calls {
            results.push(Ok(ResponseEvent::OutputItemDone(
                self.tool_call_item(index, call),
            )));
        }
        results.push(Ok(ResponseEvent::Completed {
            response_id: self.response_id.unwrap_or_default(),
            token_usage: self.token_usage,
            usage_metadata: None,
            end_turn: None,
        }));
        results
    }

    fn tool_call_item(&self, index: u32, call: PendingToolCall) -> ResponseItem {
        let target = self.tool_index.resolve(&call.name);
        let call_id = call.id.unwrap_or_else(|| {
            let response_id = self.response_id.as_deref().unwrap_or("chat");
            format!("call_{response_id}_{index}")
        });
        match target.kind {
            ChatToolKind::Function => ResponseItem::FunctionCall {
                id: None,
                name: target.name,
                namespace: target.namespace,
                arguments: call.arguments,
                encrypted_function_args: None,
                call_id,
                internal_chat_message_metadata_passthrough: None,
            },
            ChatToolKind::Freeform => ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id,
                name: target.name,
                namespace: target.namespace,
                input: freeform_input(call.arguments),
                internal_chat_message_metadata_passthrough: None,
            },
        }
    }
}

/// Extracts the raw freeform input from `{"input": "..."}`.
///
/// @compat: models that ignore the schema and send the raw input directly
/// get their arguments passed through unchanged.
fn freeform_input(arguments: String) -> String {
    match serde_json::from_str::<Value>(&arguments) {
        Ok(Value::Object(mut object)) => match object.remove(FREEFORM_INPUT_ARGUMENT) {
            Some(Value::String(input)) => input,
            Some(_) | None => arguments,
        },
        Ok(_) | Err(_) => arguments,
    }
}

fn message_item(text: String) -> ResponseItem {
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentItem::OutputText { text }]
    };
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning_item(text: String) -> ResponseItem {
    let content = if text.is_empty() {
        None
    } else {
        Some(vec![ReasoningItemContent::ReasoningText { text }])
    };
    ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn incomplete_error(reason: &str) -> ApiError {
    ApiError::Stream(format!("Incomplete response returned, reason: {reason}"))
}

/// Maps a chat `error` object onto the Responses error classification.
///
/// @compat: providers send `code` as a string, a number or not at all.
fn chat_error(mut error: Value) -> ApiError {
    if let Some(code) = error.get("code").filter(|code| !code.is_string()).cloned() {
        error["code"] = match code {
            Value::Null => Value::Null,
            other => Value::String(other.to_string()),
        };
    }
    if let Some(error_type) = error
        .get("type")
        .filter(|value| !value.is_string())
        .cloned()
    {
        error["type"] = Value::String(error_type.to_string());
    }
    match parse_failed_response(Some(serde_json::json!({ "error": error }))) {
        ApiError::Retryable {
            message,
            retry_after,
        } if message.is_empty() => ApiError::Retryable {
            message: "chat completions stream returned an error".to_string(),
            retry_after,
        },
        mapped => mapped,
    }
}

pub(crate) async fn process_chat_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    tool_index: ChatToolIndex,
) {
    let mut stream = stream.eventsource();
    let mut state = ChatStreamState {
        tool_index,
        ..ChatStreamState::default()
    };

    loop {
        let start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return,
            response = timeout(idle_timeout, stream.next()) => response,
        };
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("chat SSE error: {e:#}");
                let error = match e {
                    eventsource_stream::EventStreamError::Transport(
                        error @ codex_client::TransportError::Policy(_),
                    ) => ApiError::Transport(error),
                    error => ApiError::Stream(error.to_string()),
                };
                let _ = tx_event.send(Err(error)).await;
                return;
            }
            Ok(None) => {
                // @compat: some providers close the stream without `[DONE]`.
                if state.finish_reason.is_some() {
                    send_all(&tx_event, state.finish()).await;
                } else {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "stream closed before chat completion finished".into(),
                        )))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("chat SSE event: {}", &sse.data);
        let data = sse.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == DONE_SENTINEL {
            send_all(&tx_event, state.finish()).await;
            return;
        }
        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(err) => {
                warn!(
                    error_category = ?err.classify(),
                    payload_bytes = data.len(),
                    "failed to parse chat completions chunk"
                );
                continue;
            }
        };
        match state.apply_chunk(chunk) {
            Ok(events) => {
                for event in events {
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = tx_event.send(Err(error)).await;
                return;
            }
        }
    }
}

async fn send_all(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    results: Vec<Result<ResponseEvent, ApiError>>,
) {
    for result in results {
        if tx_event.send(result).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
