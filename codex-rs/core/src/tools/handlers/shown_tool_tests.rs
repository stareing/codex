use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::handlers::file_tools::READ_TOOL_NAME;
use crate::tools::handlers::file_tools::ReadFileHandler;
use crate::tools::handlers::search_tools::GLOB_TOOL_NAME;
use crate::tools::handlers::search_tools::GREP_TOOL_NAME;
use crate::tools::handlers::search_tools::GlobHandler;
use crate::tools::handlers::search_tools::GrepHandler;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use test_case::test_case;
use tokio::sync::Mutex;

async fn invocation(
    tool: &str,
    arguments: Value,
) -> (ToolInvocation, async_channel::Receiver<Event>) {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(tool),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    };
    (invocation, events)
}

/// The MCP tool-call items the session sent: (started?, item).
fn tool_items(events: &async_channel::Receiver<Event>) -> Vec<(bool, McpToolCallItem)> {
    let mut items = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::ItemStarted(e) => {
                if let TurnItem::McpToolCall(item) = e.item {
                    items.push((true, item));
                }
            }
            EventMsg::ItemCompleted(e) => {
                if let TurnItem::McpToolCall(item) = e.item {
                    items.push((false, item));
                }
            }
            _ => {}
        }
    }
    items
}

#[tokio::test]
async fn the_work_is_shown_between_a_started_and_a_completed_item() {
    let (invocation, events) = invocation("Read", json!({"file_path": "a.txt"})).await;
    let output = shown(&invocation, "Read", async { Ok("1\thello".to_string()) }).await;
    assert!(output.is_ok());
    let items = tool_items(&events);
    assert_eq!(items.len(), 2, "{items:?}");
    let (started, first) = &items[0];
    assert!(*started);
    assert_eq!(first.id, "call-1");
    assert_eq!(first.server, SHOWN_TOOL_SERVER);
    assert_eq!(first.tool, "Read");
    assert_eq!(first.arguments, json!({"file_path": "a.txt"}));
    assert_eq!(first.status, McpToolCallStatus::InProgress);
    let (started, last) = &items[1];
    assert!(!*started);
    assert_eq!(last.status, McpToolCallStatus::Completed);
    assert_eq!(
        last.result.as_ref().map(|r| r.content.clone()),
        Some(vec![json!({"type": "text", "text": "1\thello"})])
    );
    assert!(last.duration.is_some());
}

#[tokio::test]
async fn a_refusal_completes_the_item_as_failed_with_the_models_message() {
    let (invocation, events) = invocation("Grep", json!("not an object")).await;
    let output = shown(&invocation, "Grep", async {
        Err(FunctionCallError::RespondToModel(
            "no such directory".to_string(),
        ))
    })
    .await;
    assert!(output.is_err());
    let items = tool_items(&events);
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].1.arguments, json!("not an object"));
    assert_eq!(items[1].1.status, McpToolCallStatus::Failed);
    assert_eq!(
        items[1].1.error.as_ref().map(|e| e.message.as_str()),
        Some("no such directory")
    );
    assert_eq!(items[1].1.result, None);
}

/// Each silent tool goes through it, whatever its outcome (11-10: the workbench showed nothing for them). The
/// searches name a missing folder: refused at once instead of walking the test's working directory.
#[test_case(READ_TOOL_NAME, json!({"file_path": "missing-file.txt"}); "read")]
#[test_case(GLOB_TOOL_NAME, json!({"pattern": "*.nothing", "path": "/nonexistent-onigio-test-dir"}); "glob")]
#[test_case(GREP_TOOL_NAME, json!({"pattern": "zz-no-match-zz", "path": "/nonexistent-onigio-test-dir"}); "grep")]
#[tokio::test]
async fn read_glob_and_grep_show_their_call(tool: &str, arguments: Value) {
    let (invocation, events) = invocation(tool, arguments.clone()).await;
    let _ = match tool {
        READ_TOOL_NAME => ReadFileHandler::new(false).handle(invocation).await,
        GLOB_TOOL_NAME => GlobHandler::new(false).handle(invocation).await,
        _ => GrepHandler::new(false).handle(invocation).await,
    };
    let items = tool_items(&events);
    assert_eq!(items.len(), 2, "{tool}: {items:?}");
    assert!(items[0].0 && !items[1].0);
    assert_eq!(items[0].1.tool, tool);
    assert_eq!(items[0].1.arguments, arguments);
    assert_ne!(items[1].1.status, McpToolCallStatus::InProgress);
}
