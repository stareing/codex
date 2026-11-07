//! Read/Edit/Write for models without apply_patch (core/src/tools/handlers/file_tools.rs), end to end: the model's
//! JSON call changes the file on disk through apply_patch's own path.

use anyhow::Result;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

async fn harness() -> Result<TestCodexHarness> {
    let builder = test_codex().with_model_info_override("gpt-5.5", |model| {
        model.apply_patch_tool_type = None;
    });
    Box::pin(TestCodexHarness::with_auto_env_builder(builder)).await
}

async fn call(
    harness: &TestCodexHarness,
    call_id: &str,
    tool: &str,
    args: Value,
) -> Result<String> {
    mount_function_call_agent_response(harness.server(), call_id, &args.to_string(), tool).await;
    harness.submit(&format!("use {tool}")).await?;
    Ok(harness.function_call_stdout(call_id).await)
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replaces_the_exact_string_and_the_model_sees_the_file_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness
        .write_file("notes.txt", "alpha\nbeta\ngamma\n")
        .await?;

    call(
        &harness,
        "edit-1",
        "Edit",
        json!({ "file_path": "notes.txt", "old_string": "beta", "new_string": "BETA" }),
    )
    .await?;

    assert_eq!(
        harness.read_file_text("notes.txt").await?,
        "alpha\nBETA\ngamma\n"
    );
    let names = tool_names(&harness.request_bodies().await[0]);
    for name in ["Read", "Edit", "Write"] {
        assert!(
            names.iter().any(|n| n == name),
            "{name} missing from {names:?}"
        );
    }
    assert!(!names.iter().any(|n| n == "apply_patch"), "{names:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_refuses_an_ambiguous_string_and_leaves_the_file() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness.write_file("twice.txt", "x\nx\n").await?;

    let output = call(
        &harness,
        "edit-2",
        "Edit",
        json!({ "file_path": "twice.txt", "old_string": "x", "new_string": "y" }),
    )
    .await?;

    assert!(output.contains("appears 2 times"), "{output}");
    assert_eq!(harness.read_file_text("twice.txt").await?, "x\nx\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_creates_a_file_with_its_folders() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;

    call(
        &harness,
        "write-1",
        "Write",
        json!({ "file_path": "docs/md-test.md", "content": "# Title\n\n```mermaid\ngraph TD; A-->B\n```\n" }),
    )
    .await?;

    assert_eq!(
        harness.read_file_text("docs/md-test.md").await?,
        "# Title\n\n```mermaid\ngraph TD; A-->B\n```\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_returns_numbered_lines() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness.write_file("read.txt", "one\ntwo\nthree\n").await?;

    let output = call(
        &harness,
        "read-1",
        "Read",
        json!({ "file_path": "read.txt", "offset": 2, "limit": 1 }),
    )
    .await?;

    assert!(output.starts_with("     2\ttwo\n"), "{output}");
    assert!(output.contains("pass offset 3"), "{output}");
    Ok(())
}
