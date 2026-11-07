use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_apply_patch_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadCheckpointChange;
use codex_app_server_protocol::ThreadCheckpointListParams;
use codex_app_server_protocol::ThreadCheckpointListResponse;
use codex_app_server_protocol::ThreadCheckpointRestoredNotification;
use codex_app_server_protocol::ThreadCheckpointRewindFilesParams;
use codex_app_server_protocol::ThreadCheckpointRewindResponse;
use codex_app_server_protocol::ThreadCheckpointUndoRewindParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_features::Feature;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn changes(response: &ThreadCheckpointRewindResponse) -> Vec<(String, ThreadCheckpointChange)> {
    let mut changes: Vec<_> = response
        .files_changed
        .iter()
        .map(|change| {
            let name = std::path::Path::new(&change.path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, change.change)
        })
        .collect();
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

/// A turn edits one file and adds another through apply_patch; rewinding to that turn's checkpoint puts both back,
/// and undoing the rewind restores the turn's result.
#[tokio::test]
async fn rewind_files_to_the_start_of_a_turn_and_undo_it() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "apply-patch workspace fixture is only materialized on the host"
    );
    skip_if_no_network!(Ok(()));

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("notes.txt"), "before\n")?;

    let patch = r#"*** Begin Patch
*** Update File: notes.txt
@@
-before
+after
*** Add File: added.txt
+added
*** End Patch
"#;
    let responses = vec![
        create_apply_patch_sse_response(patch, "patch-call")?,
        create_final_assistant_message_sse_response("patch applied")?,
    ];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("never")
        .with_sandbox_mode("workspace-write")
        .disable_feature(Feature::ShellSnapshot)
        .write(&codex_home)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(&codex_home)
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![V2UserInput::Text {
                    text: "apply patch".into(),
                    text_elements: Vec::new(),
                }],
                cwd: Some(workspace.clone()),
                ..Default::default()
            },
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt"))?, "after\n");
    assert_eq!(std::fs::read_to_string(workspace.join("added.txt"))?, "added\n");

    let listed: ThreadCheckpointListResponse = mcp
        .request(|request_id| ClientRequest::ThreadCheckpointList {
            request_id,
            params: ThreadCheckpointListParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    assert_eq!(listed.checkpoints.len(), 1);
    assert_eq!(listed.checkpoints[0].turn_id, turn.id);
    assert_eq!(listed.checkpoints[0].file_count, 2);
    assert!(!listed.can_undo);

    let rewind = |dry_run| {
        let thread_id = thread.id.clone();
        let turn_id = turn.id.clone();
        move |request_id| ClientRequest::ThreadCheckpointRewindFiles {
            request_id,
            params: ThreadCheckpointRewindFilesParams {
                thread_id,
                turn_id,
                dry_run,
            },
        }
    };
    let preview: ThreadCheckpointRewindResponse = mcp.request(rewind(true)).await?;
    assert!(preview.can_rewind, "{preview:?}");
    assert_eq!(
        changes(&preview),
        vec![
            ("added.txt".to_string(), ThreadCheckpointChange::Delete),
            ("notes.txt".to_string(), ThreadCheckpointChange::Restore),
        ]
    );
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt"))?, "after\n");

    let rewound: ThreadCheckpointRewindResponse = mcp.request(rewind(false)).await?;
    assert_eq!(changes(&rewound), changes(&preview));
    assert!(rewound.skipped.is_empty(), "{rewound:?}");
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt"))?, "before\n");
    assert!(!workspace.join("added.txt").exists());
    let restored: ThreadCheckpointRestoredNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("thread/checkpoint/restored"),
    )
    .await??;
    assert_eq!(restored.thread_id, thread.id);
    assert_eq!(restored.turn_id.as_deref(), Some(turn.id.as_str()));

    let undone: ThreadCheckpointRewindResponse = mcp
        .request(|request_id| ClientRequest::ThreadCheckpointUndoRewind {
            request_id,
            params: ThreadCheckpointUndoRewindParams {
                thread_id: thread.id.clone(),
                dry_run: false,
            },
        })
        .await?;
    assert!(undone.can_rewind, "{undone:?}");
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt"))?, "after\n");
    assert_eq!(std::fs::read_to_string(workspace.join("added.txt"))?, "added\n");

    let again: ThreadCheckpointRewindResponse = mcp
        .request(|request_id| ClientRequest::ThreadCheckpointUndoRewind {
            request_id,
            params: ThreadCheckpointUndoRewindParams {
                thread_id: thread.id.clone(),
                dry_run: false,
            },
        })
        .await?;
    assert!(!again.can_rewind);
    assert_eq!(again.error.as_deref(), Some("nothing_to_undo"));

    let missing: ThreadCheckpointRewindResponse = mcp
        .request(|request_id| ClientRequest::ThreadCheckpointRewindFiles {
            request_id,
            params: ThreadCheckpointRewindFilesParams {
                thread_id: thread.id.clone(),
                turn_id: "no-such-turn".into(),
                dry_run: true,
            },
        })
        .await?;
    assert_eq!(missing.error.as_deref(), Some("checkpoint_not_found"));
    Ok(())
}
