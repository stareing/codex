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
        "read-0",
        "Read",
        json!({ "file_path": "notes.txt" }),
    )
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
    call(
        &harness,
        "read-0",
        "Read",
        json!({ "file_path": "twice.txt" }),
    )
    .await?;

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
async fn changing_a_file_needs_a_read_of_what_is_on_disk_now() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness.write_file("cfg.txt", "port = 1\n").await?;
    let edit = json!({ "file_path": "cfg.txt", "old_string": "1", "new_string": "2" });

    let unread = call(&harness, "edit-unread", "Edit", edit.clone()).await?;
    assert!(unread.contains("Read it first"), "{unread}");
    let overwrite = call(
        &harness,
        "write-unread",
        "Write",
        json!({ "file_path": "cfg.txt", "content": "gone\n" }),
    )
    .await?;
    assert!(overwrite.contains("Read it first"), "{overwrite}");

    call(
        &harness,
        "read-1",
        "Read",
        json!({ "file_path": "cfg.txt" }),
    )
    .await?;
    harness
        .write_file("cfg.txt", "port = 1 # by the user\n")
        .await?;
    let stale = call(&harness, "edit-stale", "Edit", edit.clone()).await?;
    assert!(stale.contains("changed since it was last read"), "{stale}");
    assert_eq!(
        harness.read_file_text("cfg.txt").await?,
        "port = 1 # by the user\n"
    );

    call(
        &harness,
        "read-2",
        "Read",
        json!({ "file_path": "cfg.txt" }),
    )
    .await?;
    call(&harness, "edit-ok", "Edit", edit).await?;
    // Its own change counts as read: a second edit needs no new Read.
    call(
        &harness,
        "edit-again",
        "Edit",
        json!({ "file_path": "cfg.txt", "old_string": "user", "new_string": "admin" }),
    )
    .await?;
    assert_eq!(
        harness.read_file_text("cfg.txt").await?,
        "port = 2 # by the admin\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_the_same_unchanged_lines_again_returns_a_note() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness.write_file("same.txt", "one\ntwo\n").await?;
    let args = json!({ "file_path": "same.txt" });

    let first = call(&harness, "read-a", "Read", args.clone()).await?;
    assert!(first.starts_with("     1\tone\n"), "{first}");
    let again = call(&harness, "read-b", "Read", args.clone()).await?;
    assert!(
        again.contains("is unchanged since the last Read"),
        "{again}"
    );
    harness.write_file("same.txt", "one\nTWO\n").await?;
    let changed = call(&harness, "read-c", "Read", args).await?;
    assert!(changed.contains("     2\tTWO"), "{changed}");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_and_glob_search_like_claude_code() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = harness().await?;
    harness
        .write_file("src/app.rs", "fn main() {\n    let needle = 1;\n}\n")
        .await?;
    harness
        .write_file("src/other.rs", "fn other() {}\n")
        .await?;

    let output = call(
        &harness,
        "grep-1",
        "Grep",
        json!({ "pattern": "needle", "output_mode": "content", "-n": "true" }),
    )
    .await?;
    assert_eq!(output, "src/app.rs:2:    let needle = 1;");

    let output = call(
        &harness,
        "grep-2",
        "Grep",
        json!({ "pattern": "fn ", "glob": "*.rs" }),
    )
    .await?;
    assert!(output.starts_with("Found 2 files\n"), "{output}");

    let output = call(
        &harness,
        "glob-1",
        "Glob",
        json!({ "pattern": "src/**/*.rs" }),
    )
    .await?;
    let mut files: Vec<&str> = output.lines().collect();
    files.sort();
    assert_eq!(files, ["src/app.rs", "src/other.rs"]);

    let output = call(
        &harness,
        "glob-2",
        "Glob",
        json!({ "pattern": "*.rs", "path": "nope" }),
    )
    .await?;
    assert!(
        output.starts_with("Directory does not exist: nope."),
        "{output}"
    );

    let names = tool_names(&harness.request_bodies().await[0]);
    for name in ["Glob", "Grep"] {
        assert!(
            names.iter().any(|n| n == name),
            "{name} missing from {names:?}"
        );
    }
    Ok(())
}
