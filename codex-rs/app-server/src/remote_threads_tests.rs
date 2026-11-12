use super::*;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadState;
use codex_app_server_protocol::ThreadStatus;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use serde_json::json;

fn request(method: &str, params: serde_json::Value) -> ClientRequest {
    serde_json::from_value(json!({ "id": 1, "method": method, "params": params }))
        .unwrap_or_else(|err| panic!("{method}: {err}"))
}

/// Params every thread method in LISTED_METHODS accepts (each reads what it needs, unknown fields are ignored).
fn every_param(thread_id: &str) -> serde_json::Value {
    json!({
        "threadId": thread_id,
        "turnId": "u",
        "expectedTurnId": "u",
        "input": [],
        "name": "n",
        "includeTurns": false,
        "excludeTurns": false,
        "ephemeral": false,
        "deferGoalContinuation": false,
    })
}

fn listed(ids: &[&str]) -> Shared {
    Shared {
        relay: true,
        exposure: Exposure::Listed,
        threads: ids.iter().map(|id| id.to_string()).collect(),
    }
}

#[test]
fn the_sessions_file_is_read_strictly() {
    assert_eq!(
        parse(r#"{"relay":true,"mode":"listed","threadIds":["a"," ","b"]}"#),
        Some(listed(&["a", "b"]))
    );
    assert_eq!(
        parse(r#"{"relay":false,"mode":"all"}"#),
        Some(Shared {
            relay: false,
            exposure: Exposure::All,
            threads: HashSet::new()
        })
    );
    assert_eq!(
        parse(r#"{"relay":true,"mode":"some"}"#),
        None,
        "unknown mode"
    );
    assert_eq!(parse(r#"{"mode":"all"}"#), None, "relay must be said");
    assert_eq!(
        parse(r#"{"relay":true,"mode":"all","extra":1}"#),
        None,
        "unknown field"
    );
    assert_eq!(parse("not json"), None);
}

#[test]
fn a_missing_or_broken_file_shares_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("remote-threads.json");
    let threads = RemoteThreads::new(Some(path.clone()));
    assert!(threads.is_on());
    assert_eq!(*threads.current(), Shared::nothing());
    std::fs::write(&path, "{").expect("write");
    assert_eq!(*threads.current(), Shared::nothing());
    std::fs::write(
        &path,
        r#"{"relay":true,"mode":"listed","threadIds":["t1"]}"#,
    )
    .expect("write");
    assert_eq!(*threads.current(), listed(&["t1"]), "read again every time");
    std::fs::write(&path, " ".repeat(MAX_FILE_BYTES as usize + 1)).expect("write");
    assert_eq!(*threads.current(), Shared::nothing(), "too large");

    let off = RemoteThreads::off();
    assert!(!off.is_on());
    assert!(
        off.check(&request("thread/start", json!({}))).is_ok(),
        "no file: as upstream"
    );
}

#[test]
fn listed_mode_reaches_only_the_listed_sessions() {
    let shared = listed(&["t1"]);
    assert!(
        check_request(
            &shared,
            &request("thread/resume", json!({ "threadId": "t1" }))
        )
        .is_ok()
    );
    assert!(
        check_request(
            &shared,
            &request("turn/interrupt", json!({ "threadId": "t1", "turnId": "u" }))
        )
        .is_ok()
    );
    let other = check_request(
        &shared,
        &request("thread/resume", json!({ "threadId": "t2" })),
    );
    assert!(other.unwrap_err().contains("t2"), "an unlisted session");
    assert!(
        check_request(&shared, &request("thread/list", json!({}))).is_ok(),
        "filtered, not refused"
    );
    assert!(check_request(&shared, &request("model/list", json!({}))).is_ok());
    for (method, params) in [
        ("thread/start", json!({})),
        ("thread/fork", every_param("t1")),
        ("remoteControl/disable", json!({})),
        (
            "command/exec",
            json!({ "command": ["ls"], "streamStdoutStderr": false, "disableTimeout": false }),
        ),
    ] {
        assert!(
            check_request(&shared, &request(method, params)).is_err(),
            "{method} is refused"
        );
    }
    // `all` mode: everything, as before.
    let all = Shared {
        relay: true,
        exposure: Exposure::All,
        threads: HashSet::new(),
    };
    assert!(check_request(&all, &request("thread/start", json!({}))).is_ok());
    assert!(check_request(&all, &request("thread/resume", json!({ "threadId": "t2" }))).is_ok());
}

#[test]
fn every_listed_thread_method_names_its_thread() {
    // A typo in LISTED_METHODS would refuse a method the web uses, or let one through unchecked.
    let shared = listed(&[]);
    for (method, rule) in LISTED_METHODS {
        if *rule == Rule::Thread {
            assert!(
                check_request(&shared, &request(method, every_param("t9")))
                    .unwrap_err()
                    .contains("t9"),
                "{method} reads params.threadId"
            );
        }
    }
}

#[test]
fn thread_list_keeps_only_listed_sessions() {
    let response = ThreadListResponse {
        data: vec![thread("t1"), thread("t2")],
        read_states: Some(
            ["t1", "t2"]
                .into_iter()
                .map(|id| {
                    (
                        id.to_string(),
                        ThreadReadState {
                            first_unread: None,
                            revision: "r1".to_string(),
                        },
                    )
                })
                .collect(),
        ),
        next_cursor: None,
        backwards_cursor: None,
    };
    let ClientResponsePayload::ThreadList(filtered) = filter_response(
        &listed(&["t2"]),
        ClientResponsePayload::ThreadList(response.clone()),
    ) else {
        panic!("still a thread list");
    };
    assert_eq!(
        filtered
            .data
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["t2"]
    );
    assert_eq!(
        filtered
            .read_states
            .as_ref()
            .map(|states| states.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["t2".to_string()]),
        "read receipts of unshared sessions are left out too"
    );
    let all = Shared {
        relay: true,
        exposure: Exposure::All,
        threads: HashSet::new(),
    };
    let ClientResponsePayload::ThreadList(unfiltered) =
        filter_response(&all, ClientResponsePayload::ThreadList(response))
    else {
        panic!("still a thread list");
    };
    assert_eq!(unfiltered.data.len(), 2);
}

#[test]
fn taking_a_session_away_is_a_narrowing() {
    let all = Shared {
        relay: true,
        exposure: Exposure::All,
        threads: HashSet::new(),
    };
    assert!(listed(&["a", "b"]).narrowed_to(&listed(&["a"])));
    assert!(!listed(&["a"]).narrowed_to(&listed(&["a", "b"])));
    assert!(all.narrowed_to(&listed(&["a"])));
    assert!(!listed(&["a"]).narrowed_to(&all));
}

fn thread(id: &str) -> Thread {
    Thread {
        originator: None,
        environments: None,
        id: id.to_string(),
        extra: None,
        session_id: "session-1".to_string(),
        forked_from_id: None,
        parent_thread_id: None,
        attached_to_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        daybreak_enabled: None,
        history_mode: Default::default(),
        model_provider: "mock_provider".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: 0,
        updated_at: 0,
        recency_at: None,
        status: ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp").abs(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::Cli,
        can_accept_direct_input: None,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: Vec::new(),
    }
}
