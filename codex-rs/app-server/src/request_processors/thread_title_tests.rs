use super::*;
use codex_app_server_protocol::ThreadItem;
use pretty_assertions::assert_eq;

fn turn(items: Vec<ThreadItemPayload>) -> Turn {
    Turn {
        id: "turn".to_string(),
        root_turn_id: None,
        items: items.into_iter().map(ThreadItem::untimed).collect(),
        items_view: Default::default(),
        error: None,
        status: TurnStatus::Completed,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

fn user_message(inputs: Vec<V2UserInput>) -> ThreadItemPayload {
    ThreadItemPayload::UserMessage {
        id: "user".to_string(),
        client_id: None,
        content: inputs,
    }
}

fn text(text: &str) -> V2UserInput {
    V2UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }
}

#[test]
fn title_source_is_the_first_user_message_only() {
    let turns = vec![
        turn(vec![user_message(vec![
            text("# Context\n## My request for Codex:\nFix login"),
            text("and retries"),
        ])]),
        turn(vec![user_message(vec![text("Later request")])]),
    ];
    assert_eq!(
        first_user_message_title_source(&turns),
        Some(Some("Fix login and retries".to_string()))
    );
}

#[test]
fn title_source_distinguishes_a_missing_message_from_one_without_text() {
    assert_eq!(first_user_message_title_source(&[]), None);
    assert_eq!(
        first_user_message_title_source(&[turn(vec![user_message(vec![
            V2UserInput::LocalImage {
                path: "a.png".into(),
                detail: None,
            }
        ])])]),
        Some(None)
    );
}

#[tokio::test]
async fn one_worker_per_thread_and_joiners_share_its_result() {
    let workers = ThreadTitleWorkers::default();
    let thread_id = ThreadId::new();
    let TitleWorkerSlot::Start {
        sender,
        cancellation,
        ..
    } = workers.join_or_start(thread_id)
    else {
        panic!("first caller must start the worker");
    };
    let TitleWorkerSlot::Join(mut joined) = workers.join_or_start(thread_id) else {
        panic!("second caller must join");
    };

    workers.cancel(thread_id);
    assert_eq!(cancellation.is_cancelled(), true);

    sender.send_replace(Some(Ok(ThreadTitleGenerateResponse {
        name: Some("Fix login".to_string()),
        generated: true,
    })));
    workers.finish(thread_id);
    let joined = joined
        .wait_for(Option::is_some)
        .await
        .expect("worker result")
        .clone();
    assert_eq!(
        joined,
        Some(Ok(ThreadTitleGenerateResponse {
            name: Some("Fix login".to_string()),
            generated: true,
        }))
    );
    assert_eq!(
        matches!(
            workers.join_or_start(thread_id),
            TitleWorkerSlot::Start { .. }
        ),
        true
    );
}
