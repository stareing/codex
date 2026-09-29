//! Converts Responses API tool definitions into Chat Completions `tools`.
//!
//! Chat Completions only knows plain function tools, so:
//! - `function` tools pass through as function tools;
//! - `custom` (freeform) tools become function tools with a single required
//!   string parameter named [`FREEFORM_INPUT_ARGUMENT`];
//! - `namespace` tools are flattened to `<namespace><name>` (the same spelling
//!   as [`ToolName`]'s `Display`);
//! - hosted tools (`web_search`, `tool_search`, `image_generation`, ...) are
//!   skipped with a warning because a chat provider cannot execute them.
//!
//! The returned [`ChatToolIndex`] lets the stream parser map a chat function
//! call back onto the `ResponseItem` variant the Responses path would produce.

use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::ToolName;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use tracing::warn;

/// Name of the single string argument that carries a freeform tool's raw input.
pub(crate) const FREEFORM_INPUT_ARGUMENT: &str = "input";

/// How a chat function call maps back onto a Responses item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatToolKind {
    /// Maps to `ResponseItem::FunctionCall`.
    Function,
    /// Maps to `ResponseItem::CustomToolCall`; the raw input is the
    /// [`FREEFORM_INPUT_ARGUMENT`] string argument.
    Freeform,
}

/// Original Responses identity of a flattened chat tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatToolTarget {
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
    pub(crate) kind: ChatToolKind,
}

/// Chat tool names that do not map to a plain top-level function call.
///
/// @invariant: names absent from the index are top-level function tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChatToolIndex {
    targets: HashMap<String, ChatToolTarget>,
}

impl ChatToolIndex {
    pub(crate) fn resolve(&self, chat_name: &str) -> ChatToolTarget {
        self.targets
            .get(chat_name)
            .cloned()
            .unwrap_or_else(|| ChatToolTarget {
                namespace: None,
                name: chat_name.to_string(),
                kind: ChatToolKind::Function,
            })
    }

    #[cfg(test)]
    pub(crate) fn from_targets(
        targets: impl IntoIterator<Item = (String, ChatToolTarget)>,
    ) -> Self {
        Self {
            targets: targets.into_iter().collect(),
        }
    }
}

/// Chat-visible function name for a Responses tool identity.
pub(crate) fn chat_tool_name(namespace: Option<&str>, name: &str) -> String {
    ToolName::new(namespace.map(str::to_string), name).to_string()
}

pub(crate) struct ChatTools {
    pub(crate) tools: Vec<Value>,
    pub(crate) index: ChatToolIndex,
}

/// @input: the Responses `tools` array (already serialized JSON).
/// @output: chat function tools plus the reverse-mapping index.
pub(crate) fn convert_tools(responses_tools: &[Value]) -> ChatTools {
    let mut converted = ChatTools {
        tools: Vec::new(),
        index: ChatToolIndex::default(),
    };
    for tool in responses_tools {
        match tool_type(tool) {
            "function" | "custom" => push_tool(&mut converted, /*namespace*/ None, tool),
            "namespace" => {
                let namespace = tool.get("name").and_then(Value::as_str);
                for nested in tool
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    push_tool(&mut converted, namespace, nested);
                }
            }
            other => {
                warn!("skipping `{other}` tool: not supported by the chat completions wire api")
            }
        }
    }
    converted
}

fn tool_type(tool: &Value) -> &str {
    tool.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn push_tool(converted: &mut ChatTools, namespace: Option<&str>, tool: &Value) {
    let Some(name) = tool.get("name").and_then(Value::as_str) else {
        warn!("skipping tool without a name in chat completions request");
        return;
    };
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (kind, description, parameters) = match tool_type(tool) {
        "function" => (
            ChatToolKind::Function,
            description.to_string(),
            tool.get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        ),
        "custom" => (
            ChatToolKind::Freeform,
            freeform_description(description, tool.get("format")),
            json!({
                "type": "object",
                "properties": {
                    FREEFORM_INPUT_ARGUMENT: {
                        "type": "string",
                        "description": "Raw tool input.",
                    },
                },
                "required": [FREEFORM_INPUT_ARGUMENT],
                "additionalProperties": false,
            }),
        ),
        other => {
            warn!("skipping `{other}` tool: not supported by the chat completions wire api");
            return;
        }
    };
    let namespace = namespace.filter(|namespace| *namespace != DEFAULT_FUNCTION_NAMESPACE);
    let chat_name = chat_tool_name(namespace, name);
    if namespace.is_some() || kind == ChatToolKind::Freeform {
        converted.index.targets.insert(
            chat_name.clone(),
            ChatToolTarget {
                namespace: namespace.map(str::to_string),
                name: name.to_string(),
                kind,
            },
        );
    }
    converted.tools.push(json!({
        "type": "function",
        "function": {
            "name": chat_name,
            "description": description,
            "parameters": parameters,
        },
    }));
}

fn freeform_description(description: &str, format: Option<&Value>) -> String {
    let syntax = format
        .and_then(|format| format.get("syntax"))
        .and_then(Value::as_str);
    let definition = format
        .and_then(|format| format.get("definition"))
        .and_then(Value::as_str);
    let note = format!(
        "Pass the complete raw tool input as the `{FREEFORM_INPUT_ARGUMENT}` string argument."
    );
    match (syntax, definition) {
        (Some(syntax), Some(definition)) => format!(
            "{description}\n\n{note} The input must follow this {syntax} grammar:\n{definition}"
        ),
        (Some(_), None) | (None, Some(_)) | (None, None) => format!("{description}\n\n{note}"),
    }
}
