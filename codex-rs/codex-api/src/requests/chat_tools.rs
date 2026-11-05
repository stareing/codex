//! Converts Responses API tool definitions into Chat Completions `tools`.
//!
//! Chat Completions only knows plain function tools, so:
//! - `function` tools pass through as function tools;
//! - `custom` (freeform) tools become function tools with a single required
//!   string parameter named [`FREEFORM_INPUT_ARGUMENT`];
//! - `namespace` tools are flattened to `<namespace>__<name>` (the delimiter is
//!   not repeated when the namespace already ends with it, e.g. `mcp__docs__`);
//! - names that chat providers would reject (over 64 bytes, or characters
//!   outside `[a-zA-Z0-9_-]`) are rewritten; see [`chat_tool_name`];
//! - hosted tools (`web_search`, `tool_search`, `image_generation`, ...) are
//!   skipped with a warning because a chat provider cannot execute them.
//!
//! The returned [`ChatToolIndex`] lets the stream parser map a chat function
//! call back onto the `ResponseItem` variant the Responses path would produce.

use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::ToolName;
use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use sha1::Sha1;
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

/// Joins a namespace and a tool name in a flattened chat function name.
const NAMESPACE_DELIMITER: &str = "__";

/// Longest function name OpenAI-compatible chat providers accept.
const MAX_CHAT_TOOL_NAME_LEN: usize = 64;
/// Hex digits of the SHA-1 suffix that keeps rewritten names distinct; same
/// width as the MCP callable-name suffix in `codex-mcp`.
const CHAT_TOOL_NAME_HASH_LEN: usize = 12;

/// Chat-visible function name for a Responses tool identity.
///
/// Namespaces arrive both with (`mcp__docs__`) and without (`mcp__docs`) a
/// trailing delimiter; plain concatenation (`ToolName`'s `Display`) would fuse
/// the latter into `mcp__docssearch`, a name no model can be expected to use.
///
/// @invariant: the result matches `^[a-zA-Z0-9_-]{1,64}$` and is a pure
/// function of its inputs, so request tools, request history and
/// [`ChatToolIndex`] agree on it.
/// @why: MCP names are only capped at 128 bytes (Responses API limit) and
/// dynamic tool names are unchecked; chat providers reject anything above 64.
pub(crate) fn chat_tool_name(namespace: Option<&str>, name: &str) -> String {
    provider_safe_tool_name(joined_tool_name(namespace, name))
}

fn joined_tool_name(namespace: Option<&str>, name: &str) -> String {
    let tool = ToolName::new(namespace.map(str::to_string), name);
    match tool.namespace.as_deref() {
        Some(namespace) if !tool.is_default_namespace() => {
            if namespace.ends_with(NAMESPACE_DELIMITER) {
                format!("{namespace}{name}")
            } else {
                format!("{namespace}{NAMESPACE_DELIMITER}{name}")
            }
        }
        _ => name.to_string(),
    }
}

fn is_chat_tool_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Keeps provider-safe names as-is; otherwise replaces rejected characters
/// with `_`, truncates, and appends `_<sha1(original)[..12]>` so distinct
/// originals stay distinct.
fn provider_safe_tool_name(joined: String) -> String {
    if !joined.is_empty()
        && joined.len() <= MAX_CHAT_TOOL_NAME_LEN
        && joined.chars().all(is_chat_tool_name_char)
    {
        return joined;
    }
    let digest = format!("{:x}", Sha1::digest(joined.as_bytes()));
    let suffix = format!("_{}", &digest[..CHAT_TOOL_NAME_HASH_LEN]);
    let prefix: String = joined
        .chars()
        .map(|c| if is_chat_tool_name_char(c) { c } else { '_' })
        .take(MAX_CHAT_TOOL_NAME_LEN - suffix.len())
        .collect();
    format!("{prefix}{suffix}")
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
    if namespace.is_some() || kind == ChatToolKind::Freeform || chat_name != name {
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
