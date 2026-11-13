//! The plain JSON tools that do their work without an item of their own (`Read`, `Glob`, `Grep`, file_tools.rs and
//! search_tools.rs) are shown to clients as an MCP tool call of the agent's own server: started before the work,
//! completed with its text or error after it, the way the MCP resource tools show (mcp_resource.rs). Without an item
//! a client saw nothing between the model's sentences ("let me read these files", then the answer). `Edit` and
//! `Write` need none: their patch has its own file-change item.

use std::future::Future;
use std::time::Instant;

use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;

/// The server the items name: the agent itself (clients show it under the product's name).
pub(crate) const SHOWN_TOOL_SERVER: &str = "codex";

/// The item for `tool` as `invocation` called it; the arguments as the model wrote them (`Null` when not JSON).
fn item(invocation: &ToolInvocation, tool: &str) -> McpToolCallItem {
    let arguments = match &invocation.payload {
        ToolPayload::Function { arguments } => {
            serde_json::from_str(arguments).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    McpToolCallItem {
        id: invocation.call_id.clone(),
        server: SHOWN_TOOL_SERVER.to_string(),
        tool: tool.to_string(),
        arguments,
        // Built into the agent: not registered in any environment.
        environment_id: None,
        connector_id: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        link_id: None,
        app_name: None,
        action_name: None,
        plugin_id: None,
        read_only_hint: Some(true),
        status: McpToolCallStatus::InProgress,
        result: None,
        error: None,
        duration: None,
    }
}

/// The finished item: the text the model got back, or the error it was told.
pub(crate) fn finished(
    mut item: McpToolCallItem,
    outcome: &Result<String, FunctionCallError>,
    duration: std::time::Duration,
) -> McpToolCallItem {
    match outcome {
        Ok(text) => {
            item.status = McpToolCallStatus::Completed;
            item.result = Some(CallToolResult {
                content: vec![serde_json::json!({"type": "text", "text": text})],
                structured_content: None,
                is_error: Some(false),
                meta: None,
            });
        }
        Err(err) => {
            item.status = McpToolCallStatus::Failed;
            item.error = Some(McpToolCallError {
                message: err.to_string(),
            });
        }
    }
    item.duration = Some(duration);
    item
}

/// Runs `work` (the tool's text answer) between the item's start and completion.
pub(crate) async fn shown(
    invocation: &ToolInvocation,
    tool: &str,
    work: impl Future<Output = Result<String, FunctionCallError>>,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let started = item(invocation, tool);
    let session = &invocation.session;
    let turn = invocation.turn.as_ref();
    session
        .emit_turn_item_started(turn, &TurnItem::McpToolCall(started.clone()))
        .await;
    let began = Instant::now();
    let outcome = work.await;
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::McpToolCall(finished(started, &outcome, began.elapsed())),
        )
        .await;
    outcome.map(|text| boxed_tool_output(FunctionToolOutput::from_text(text, Some(true))))
}

#[cfg(test)]
#[path = "shown_tool_tests.rs"]
mod tests;
