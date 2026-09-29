//! Converts a [`ResponsesApiRequest`] into a Chat Completions request body.
//!
//! Only fields every OpenAI-compatible chat provider understands are sent:
//! `model`, `messages`, `tools`, `tool_choice`, `parallel_tool_calls`, `stream`
//! and `stream_options`. Responses-only controls (`reasoning`, `include`,
//! `store`, `prompt_cache_key`, `text`, `service_tier`, client metadata) are
//! dropped.

use crate::common::ResponsesApiRequest;
use crate::error::ApiError;
use crate::requests::chat_tools::ChatToolIndex;
use crate::requests::chat_tools::FREEFORM_INPUT_ARGUMENT;
use crate::requests::chat_tools::chat_tool_name;
use crate::requests::chat_tools::convert_tools;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use tracing::warn;

/// A chat request body plus the tool index needed to parse its stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatCompletionsRequest {
    pub(crate) body: Value,
    pub(crate) tool_index: ChatToolIndex,
}

/// @error: `ApiError::Stream` when the Responses tools JSON cannot be read.
pub(crate) fn build_chat_request(
    request: &ResponsesApiRequest,
) -> Result<ChatCompletionsRequest, ApiError> {
    let responses_tools: Vec<Value> = match &request.tools {
        Some(tools) => serde_json::from_str(tools.as_raw_value().get())
            .map_err(|err| ApiError::Stream(format!("failed to read request tools: {err}")))?,
        None => Vec::new(),
    };
    let tools = convert_tools(&responses_tools);

    let mut builder = MessageBuilder::default();
    if !request.instructions.is_empty() {
        builder.push(json!({"role": "system", "content": request.instructions}));
    }
    for item in &request.input {
        builder.push_item(item);
    }
    builder.flush_tool_images();

    let mut body = json!({
        "model": request.model,
        "messages": builder.messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !tools.tools.is_empty() {
        body["tools"] = Value::Array(tools.tools);
        body["tool_choice"] = json!(request.tool_choice);
        body["parallel_tool_calls"] = json!(request.parallel_tool_calls);
    }
    Ok(ChatCompletionsRequest {
        body,
        tool_index: tools.index,
    })
}

/// Accumulates chat messages while preserving chat ordering rules.
///
/// @invariant: tool results directly follow the assistant message holding
/// their `tool_calls`; images returned by tools are deferred into one user
/// message placed after the contiguous tool-result block.
#[derive(Default)]
struct MessageBuilder {
    messages: Vec<Value>,
    pending_tool_images: Vec<Value>,
}

impl MessageBuilder {
    fn push(&mut self, message: Value) {
        self.flush_tool_images();
        self.messages.push(message);
    }

    fn flush_tool_images(&mut self) {
        if self.pending_tool_images.is_empty() {
            return;
        }
        let content = std::mem::take(&mut self.pending_tool_images);
        self.messages
            .push(json!({"role": "user", "content": content}));
    }

    fn push_item(&mut self, item: &ResponseItem) {
        match item {
            ResponseItem::Message { role, content, .. } => self.push_message(role, content),
            ResponseItem::AgentMessage { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        AgentMessageInputContent::InputText { text } => Some(text.as_str()),
                        AgentMessageInputContent::EncryptedContent { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    self.push(json!({"role": "user", "content": text}));
                }
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => self.push_tool_call(
                call_id,
                &chat_tool_name(namespace.as_deref(), name),
                arguments,
            ),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                input,
                ..
            } => {
                let arguments = json!({FREEFORM_INPUT_ARGUMENT: input}).to_string();
                self.push_tool_call(
                    call_id,
                    &chat_tool_name(namespace.as_deref(), name),
                    &arguments,
                );
            }
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                action,
                ..
            } => {
                let arguments = serde_json::to_string(action).unwrap_or_default();
                self.push_tool_call(call_id, "local_shell", &arguments);
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                arguments,
                ..
            } => self.push_tool_call(call_id, "tool_search", &arguments.to_string()),
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                output,
                ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => self.push_tool_output(call_id, output),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                tools,
                ..
            } => self.push_tool_result(call_id, Value::Array(tools.clone()).to_string()),
            ResponseItem::LocalShellCall { call_id: None, .. }
            | ResponseItem::ToolSearchCall { call_id: None, .. }
            | ResponseItem::FunctionCallOutput { call_id: None, .. }
            | ResponseItem::ToolSearchOutput { call_id: None, .. } => {
                warn!("dropping tool item without call_id from chat completions request");
            }
            // Reasoning and encrypted compaction state are only meaningful to the Responses
            // API; hosted tool calls cannot be replayed to a chat provider.
            ResponseItem::Reasoning { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::CompactionTrigger {}
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::AdditionalTools { .. }
            | ResponseItem::Other => {}
        }
    }

    fn push_message(&mut self, role: &str, content: &[ContentItem]) {
        let role = match role {
            "developer" => "system",
            other => other,
        };
        let has_image = content
            .iter()
            .any(|part| matches!(part, ContentItem::InputImage { .. }));
        if has_image {
            let parts: Vec<Value> = content.iter().filter_map(content_part).collect();
            self.push(json!({"role": role, "content": parts}));
            return;
        }
        let text = content
            .iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.push(json!({"role": role, "content": text}));
    }

    fn push_tool_call(&mut self, call_id: &str, name: &str, arguments: &str) {
        let tool_call = json!({
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments},
        });
        self.flush_tool_images();
        if let Some(last) = self.messages.last_mut()
            && last["role"] == "assistant"
        {
            match last.get_mut("tool_calls").and_then(Value::as_array_mut) {
                Some(tool_calls) => tool_calls.push(tool_call),
                None => last["tool_calls"] = json!([tool_call]),
            }
            return;
        }
        self.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [tool_call],
        }));
    }

    fn push_tool_output(&mut self, call_id: &str, output: &FunctionCallOutputPayload) {
        let content = match &output.body {
            FunctionCallOutputBody::Text(text) => text.clone(),
            FunctionCallOutputBody::ContentItems(items) => {
                let mut texts = Vec::new();
                for item in items {
                    match item {
                        FunctionCallOutputContentItem::InputText { text } => {
                            texts.push(text.as_str());
                        }
                        FunctionCallOutputContentItem::InputImage { image, .. } => {
                            if let Some(part) = image_part(image) {
                                self.pending_tool_images.push(part);
                            }
                        }
                        FunctionCallOutputContentItem::InputAudio { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
                    }
                }
                texts.join("\n")
            }
        };
        self.push_tool_result(call_id, content);
    }

    fn push_tool_result(&mut self, call_id: &str, content: String) {
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        }));
    }
}

fn content_part(part: &ContentItem) -> Option<Value> {
    match part {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
            Some(json!({"type": "text", "text": text}))
        }
        ContentItem::InputImage { image, .. } => image_part(image),
        ContentItem::InputAudio { .. } => {
            warn!("dropping audio content: not supported by the chat completions wire api");
            None
        }
    }
}

fn image_part(image: &ImageReference) -> Option<Value> {
    match image {
        ImageReference::Inline { image_url } => {
            Some(json!({"type": "image_url", "image_url": {"url": image_url}}))
        }
        ImageReference::File { .. } => {
            warn!("dropping file-id image: not supported by the chat completions wire api");
            None
        }
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
