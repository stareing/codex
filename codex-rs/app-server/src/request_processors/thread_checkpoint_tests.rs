use std::path::PathBuf;

use codex_checkpoint::Change;
use codex_checkpoint::FileChange;
use codex_checkpoint::SkipReason;
use codex_checkpoint::Skipped;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn reports_map_to_the_wire_shapes() {
    let report = RewindReport {
        files_changed: vec![FileChange {
            path: PathBuf::from("/w/a.txt"),
            change: Change::Recreate,
            insertions: 3,
            deletions: 1,
        }],
        skipped: vec![Skipped {
            path: PathBuf::from("/w/link/b.txt"),
            reason: SkipReason::Symlink,
        }],
    };
    let response = rewind_response(&report);
    assert!(response.can_rewind);
    assert_eq!(response.error, None);
    assert_eq!(
        response.files_changed,
        vec![ThreadCheckpointFileChange {
            path: "/w/a.txt".into(),
            change: ThreadCheckpointChange::Recreate,
            insertions: 3,
            deletions: 1,
        }]
    );
    assert_eq!(
        response.skipped,
        vec![ThreadCheckpointSkipped {
            path: "/w/link/b.txt".into(),
            reason: "symlink".into(),
        }]
    );
}

#[test]
fn refusals_carry_the_stable_code() {
    let response = refused(&CheckpointError::TurnInProgress);
    assert!(!response.can_rewind);
    assert_eq!(response.error.as_deref(), Some("turn_in_progress"));
    let error = checkpoint_error(&CheckpointError::Disabled);
    assert_eq!(error.data, Some(serde_json::json!({ "code": "disabled" })));
}
