//! The manager against the fake server in `src/bin/test_lsp_server.rs` (never a real language server).

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_lsp::DiagnosticSink;
use codex_lsp::EditorPosition;
use codex_lsp::FileEvent;
use codex_lsp::LspContext;
use codex_lsp::LspError;
use codex_lsp::LspLimits;
use codex_lsp::LspManager;
use codex_lsp::Operation;
use codex_lsp::QueryOutcome;
use codex_lsp::ServerSpec;
use codex_lsp::ServerTable;
use codex_lsp::format::format_result;
use pretty_assertions::assert_eq;
use serde_json::json;

const FAKE: &str = env!("CARGO_BIN_EXE_test_lsp_server");
/// Generous bound for anything that should finish quickly; a hang fails the test instead of blocking it.
const BOUND: Duration = Duration::from_secs(20);

fn spec(command: &str, env: &[(&str, &str)]) -> ServerSpec {
    ServerSpec {
        name: "fake".to_string(),
        command: command.to_string(),
        args: Vec::new(),
        extensions: BTreeMap::from([(".fake".to_string(), "fake".to_string())]),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        initialization_options: None,
        settings: Some(json!({"fake": {"answer": 42}})),
        priority: 100,
        optional: false,
        source: Some("test".to_string()),
        startup_timeout: None,
        workspace_folder: None,
        max_restarts: None,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ctx: LspContext,
    manager: Arc<LspManager>,
}

fn fixture(spec: ServerSpec, limits: LspLimits) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let table = ServerTable::new(&[spec], std::env::var_os("PATH"), &root);
    let ctx = LspContext {
        table: Arc::new(table),
        sink: Arc::new(DiagnosticSink::new(limits.diagnostics.clone())),
        limits,
        cwd: root.clone(),
    };
    Fixture {
        _dir: dir,
        root,
        ctx,
        manager: LspManager::new(),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn query(
    f: &Fixture,
    file: &Path,
    op: Operation,
    line: u32,
    character: u32,
) -> Result<QueryOutcome, LspError> {
    let position = EditorPosition::new(line, character).unwrap();
    tokio::time::timeout(BOUND, f.manager.query(&f.ctx, file, op, position))
        .await
        .expect("query did not finish")
}

fn value(outcome: QueryOutcome) -> serde_json::Value {
    match outcome {
        QueryOutcome::Result { value, .. } => value,
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operations_convert_positions_and_pass_settings() {
    let f = fixture(spec(FAKE, &[]), LspLimits::default());
    let file = f.root.join("src/a.fake");
    write(&file, "one\ntwo\nthree\n");
    let cwd = Some(f.root.as_path());

    let definition = value(
        query(&f, &file, Operation::GoToDefinition, 3, 5)
            .await
            .unwrap(),
    );
    assert_eq!(
        definition["range"]["start"],
        json!({"line": 2, "character": 4})
    );
    assert_eq!(
        format_result(Operation::GoToDefinition, &definition, cwd).text,
        "Defined in src/a.fake:3:5"
    );

    let references = value(
        query(&f, &file, Operation::FindReferences, 1, 1)
            .await
            .unwrap(),
    );
    assert_eq!(
        format_result(Operation::FindReferences, &references, cwd).result_count,
        2
    );

    // The server asked `workspace/configuration` for section `fake` and got the configured settings.
    let hover = value(query(&f, &file, Operation::Hover, 1, 1).await.unwrap());
    assert_eq!(
        hover["contents"]["value"],
        json!({"answer": 42}).to_string()
    );

    let incoming = value(
        query(&f, &file, Operation::IncomingCalls, 2, 2)
            .await
            .unwrap(),
    );
    assert_eq!(
        format_result(Operation::IncomingCalls, &incoming, cwd).text,
        "Found 1 incoming call:\n\nsrc/a.fake:\n  caller (Function) - Line 4 [calls at: 5:3]"
    );

    let other = f.root.join("notes.txt");
    write(&other, "x");
    assert_eq!(
        query(&f, &other, Operation::Hover, 1, 1).await.unwrap(),
        QueryOutcome::NoServer
    );
    assert_eq!(
        f.manager.instance_count(),
        1,
        "one shared instance for every query"
    );
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edits_publish_diagnostics_to_the_conversation() {
    let f = fixture(spec(FAKE, &[]), LspLimits::default());
    let file = f.root.join("a.fake");
    write(&file, "fine\n");
    let edit = |text: &str| FileEvent::Written(text.to_string());

    tokio::time::timeout(
        BOUND,
        f.manager
            .file_changed(&f.ctx, &file, edit("fine\nERROR here\n")),
    )
    .await
    .unwrap();
    tokio::time::timeout(BOUND, f.ctx.sink.wait_for_edits())
        .await
        .unwrap();
    let delivery = f
        .ctx
        .sink
        .take_delivery()
        .expect("diagnostics after the edit");
    assert_eq!(delivery.count(), 1);
    assert_eq!(delivery.files[0].diagnostics[0].line, 1);
    assert_eq!(delivery.servers, ["fake".to_string()]);

    // Editing again re-arms delivery of the same diagnostic; fixing the file clears it.
    f.manager
        .file_changed(&f.ctx, &file, edit("ERROR again\n"))
        .await;
    f.ctx.sink.wait_for_edits().await;
    assert_eq!(f.ctx.sink.take_delivery().map(|d| d.count()), Some(1));
    f.manager.file_changed(&f.ctx, &file, edit("fixed\n")).await;
    f.ctx.sink.wait_for_edits().await;
    assert_eq!(f.ctx.sink.take_delivery(), None);
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_server_binary_is_a_clear_error_not_a_hang() {
    let f = fixture(
        spec("definitely-missing-language-server-xyz", &[]),
        LspLimits::default(),
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    let started = Instant::now();
    let err = query(&f, &file, Operation::Hover, 1, 1).await.unwrap_err();
    assert!(
        matches!(err, LspError::CommandNotFound(ref c) if c.contains("definitely-missing")),
        "{err}"
    );
    assert!(err.to_string().contains("was not found"));
    assert!(started.elapsed() < Duration::from_secs(5));

    // An edit of such a file does not block the next step's diagnostics wait.
    f.manager
        .file_changed(&f.ctx, &file, FileEvent::Written("x".into()))
        .await;
    tokio::time::timeout(Duration::from_millis(500), f.ctx.sink.wait_for_edits())
        .await
        .expect("wait released after the failed sync");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_and_request_timeouts_are_bounded_and_retries_stop() {
    let limits = LspLimits {
        startup_timeout: Duration::from_millis(200),
        ..LspLimits::default()
    };
    let f = fixture(spec(FAKE, &[("FAKE_LSP_INIT_DELAY_MS", "5000")]), limits);
    let file = f.root.join("a.fake");
    write(&file, "x");
    // The first start plus maxRestarts (3, Claude Code's default) more.
    for _ in 0..=codex_lsp::manager::MAX_START_FAILURES {
        let err = query(&f, &file, Operation::Hover, 1, 1).await.unwrap_err();
        assert!(matches!(err, LspError::StartupTimeout { .. }), "{err}");
    }
    let err = query(&f, &file, Operation::Hover, 1, 1).await.unwrap_err();
    assert!(
        err.to_string().contains("will not be started again"),
        "{err}"
    );

    let limits = LspLimits {
        request_timeout: Duration::from_millis(200),
        ..LspLimits::default()
    };
    let f = fixture(
        spec(FAKE, &[("FAKE_LSP_HANG_METHOD", "textDocument/hover")]),
        limits,
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    let err = query(&f, &file, Operation::Hover, 1, 1).await.unwrap_err();
    assert!(
        matches!(err, LspError::Timeout { ref method, .. } if method == "textDocument/hover"),
        "{err}"
    );
    // The timed-out request left the pending table, and the server is still usable.
    assert_eq!(f.manager.pending_requests(), 0);
    assert!(
        query(&f, &file, Operation::GoToDefinition, 1, 1)
            .await
            .is_ok()
    );
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_servers_stop_and_the_server_cap_evicts_the_least_recently_used() {
    let limits = LspLimits {
        idle_shutdown: Duration::from_millis(300),
        max_servers: 1,
        ..LspLimits::default()
    };
    let f = fixture(spec(FAKE, &[]), limits);
    // Two projects outside the working directory: two roots, so two instances of the same server.
    let p1 = tempfile::tempdir().unwrap();
    let p2 = tempfile::tempdir().unwrap();
    let first = p1.path().canonicalize().unwrap().join("a.fake");
    let second = p2.path().canonicalize().unwrap().join("a.fake");
    for file in [&first, &second] {
        write(&file.parent().unwrap().join(".git/HEAD"), "x");
        write(file, "x");
    }
    query(&f, &first, Operation::Hover, 1, 1).await.unwrap();
    query(&f, &second, Operation::Hover, 1, 1).await.unwrap();
    assert_eq!(
        f.manager.instance_count(),
        1,
        "max_servers = 1 keeps one instance"
    );

    let deadline = Instant::now() + BOUND;
    while f.manager.instance_count() > 0 {
        assert!(Instant::now() < deadline, "idle server was not stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Used again, it starts again.
    query(&f, &second, Operation::Hover, 1, 1).await.unwrap();
    assert_eq!(f.manager.instance_count(), 1);
    f.manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_server_kills_its_child_processes() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let f = fixture(
        spec(
            FAKE,
            &[("FAKE_LSP_CHILD_PID_FILE", pid_file.to_str().unwrap())],
        ),
        LspLimits::default(),
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    query(&f, &file, Operation::Hover, 1, 1).await.unwrap();
    let child: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(child));
    f.manager.shutdown_all().await;
    let deadline = Instant::now() + BOUND;
    while alive(child) {
        assert!(
            Instant::now() < deadline,
            "child process {child} survived the server"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawns(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn with_restarts(mut spec: ServerSpec, max_restarts: u32) -> ServerSpec {
    spec.max_restarts = Some(max_restarts);
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_mid_request_fails_fast_and_leaves_nothing_pending() {
    let limits = LspLimits {
        request_timeout: BOUND,
        ..LspLimits::default()
    };
    let f = fixture(
        spec(
            FAKE,
            &[("FAKE_LSP_CRASH_METHOD", "textDocument/definition")],
        ),
        limits,
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    let started = Instant::now();
    let err = query(&f, &file, Operation::GoToDefinition, 1, 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, LspError::Exited { ref detail, .. } if detail.contains("crashing")),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered by the exit, not the request timeout"
    );
    assert_eq!(f.manager.pending_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashes_during_initialize_or_after_start_are_bounded_by_max_restarts() {
    for crash in ["initialize", "textDocument/definition"] {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("spawns");
        let f = fixture(
            with_restarts(
                spec(
                    FAKE,
                    &[
                        ("FAKE_LSP_CRASH_METHOD", crash),
                        ("FAKE_LSP_SPAWN_LOG", log.to_str().unwrap()),
                    ],
                ),
                1,
            ),
            LspLimits::default(),
        );
        let file = f.root.join("a.fake");
        write(&file, "x");
        for _ in 0..2 {
            let err = query(&f, &file, Operation::GoToDefinition, 1, 1)
                .await
                .unwrap_err();
            assert!(matches!(err, LspError::Exited { .. }), "{crash}: {err}");
        }
        for _ in 0..3 {
            let err = query(&f, &file, Operation::GoToDefinition, 1, 1)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("maxRestarts"), "{crash}: {err}");
        }
        assert_eq!(
            spawns(&log),
            2,
            "{crash}: the first start plus one restart, never a loop"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_frame_closes_the_server_with_an_error_instead_of_spinning() {
    let limits = LspLimits {
        request_timeout: BOUND,
        ..LspLimits::default()
    };
    let f = fixture(
        spec(
            FAKE,
            &[("FAKE_LSP_GARBAGE_METHOD", "textDocument/definition")],
        ),
        limits,
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    let started = Instant::now();
    let err = query(&f, &file, Operation::GoToDefinition, 1, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, LspError::Exited { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(f.manager.pending_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_requests_are_always_answered_and_stray_replies_ignored() {
    let f = fixture(
        spec(FAKE, &[("FAKE_LSP_SERVER_REQUESTS", "1")]),
        LspLimits::default(),
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    let replies = value(query(&f, &file, Operation::Hover, 1, 1).await.unwrap());
    let by_id = |id: &str| {
        replies
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(by_id("q1")["result"], serde_json::Value::Null);
    assert_eq!(by_id("q2")["result"], serde_json::Value::Null);
    assert_eq!(by_id("q3")["result"]["applied"], false);
    assert_eq!(by_id("q4")["error"]["code"], -32601);
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_use_starts_the_server_once() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("spawns");
    let f = Arc::new(fixture(
        spec(
            FAKE,
            &[
                ("FAKE_LSP_INIT_DELAY_MS", "300"),
                ("FAKE_LSP_SPAWN_LOG", log.to_str().unwrap()),
            ],
        ),
        LspLimits::default(),
    ));
    let file = f.root.join("a.fake");
    write(&file, "x");
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let (f, file) = (f.clone(), file.clone());
            tokio::spawn(async move {
                query(&f, &file, Operation::GoToDefinition, 1, 1)
                    .await
                    .map(|_| ())
            })
        })
        .collect();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(spawns(&log), 1);
    assert_eq!(f.manager.instance_count(), 1);
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chatty_stderr_never_blocks_the_server() {
    // Far more than a pipe buffer (64 KiB on Linux): without draining, the server would block before reading.
    let f = fixture(
        spec(FAKE, &[("FAKE_LSP_STDERR_FLOOD_BYTES", "4194304")]),
        LspLimits::default(),
    );
    let file = f.root.join("a.fake");
    write(&file, "x");
    assert!(
        query(&f, &file, Operation::GoToDefinition, 1, 1)
            .await
            .is_ok()
    );
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_never_publishes_does_not_hold_the_next_step() {
    let limits = LspLimits {
        diagnostics_wait: Duration::from_millis(300),
        ..LspLimits::default()
    };
    let f = fixture(spec(FAKE, &[("FAKE_LSP_NO_PUBLISH", "1")]), limits);
    let file = f.root.join("a.fake");
    write(&file, "ERROR\n");
    f.manager
        .file_changed(&f.ctx, &file, FileEvent::Written("ERROR\n".into()))
        .await;
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), f.ctx.sink.wait_for_edits())
        .await
        .expect("the wait is bounded");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(f.ctx.sink.take_delivery(), None);
    f.manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_workspace_folder_is_the_servers_root() {
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let mut server = spec(FAKE, &[]);
    server.workspace_folder = Some(folder.clone());
    let f = fixture(server, LspLimits::default());
    // Two files in different projects share the one instance rooted at the workspace folder.
    let p1 = tempfile::tempdir().unwrap();
    let first = p1.path().canonicalize().unwrap().join("a.fake");
    write(&first.parent().unwrap().join(".git/HEAD"), "x");
    write(&first, "x");
    let second = f.root.join("b.fake");
    write(&second, "x");
    query(&f, &first, Operation::GoToDefinition, 1, 1)
        .await
        .unwrap();
    query(&f, &second, Operation::GoToDefinition, 1, 1)
        .await
        .unwrap();
    assert_eq!(f.manager.instance_count(), 1);
    f.manager.shutdown_all().await;
}

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // A zombie waiting for its reaper is already dead.
    let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
    match state {
        Some(stat) => !stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        None => unsafe { libc::kill(pid, 0) == 0 },
    }
}
