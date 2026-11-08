use super::*;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use pretty_assertions::assert_eq;

fn plan_call(arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: PLAN_TOOL.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        call_id: "plan-1".to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn budgets_follow_the_reference_limits_and_shrink_on_a_small_window() {
    assert_eq!(budgets(None), (FILES_TOKEN_BUDGET, SKILLS_TOKEN_BUDGET));
    assert_eq!(
        budgets(Some(1_000_000)),
        (FILES_TOKEN_BUDGET, SKILLS_TOKEN_BUDGET)
    );
    assert_eq!(budgets(Some(128_000)), (16_000, 8_000));
}

#[test]
fn only_an_unfinished_plan_is_put_back_and_the_last_one_wins() {
    let old = plan_call(r#"{"plan":[{"step":"old","status":"pending"}]}"#);
    let new = plan_call(
        r#"{"plan":[{"step":"read the code","status":"completed"},{"step":"fix it","status":"in_progress"}]}"#,
    );
    assert_eq!(
        unfinished_plan([old.clone(), new].iter()).as_deref(),
        Some("- [completed] read the code\n- [in_progress] fix it")
    );
    let done = plan_call(r#"{"plan":[{"step":"all","status":"completed"}]}"#);
    assert_eq!(unfinished_plan([old, done].iter()), None);
    assert_eq!(unfinished_plan([plan_call("not json")].iter()), None);
    assert_eq!(unfinished_plan([user("hi")].iter()), None);
}

#[test]
fn skills_come_back_latest_first_once_each_within_the_budget() {
    let a = "<skill>\n<name>a</name>\nalpha\n</skill>";
    let b = "<skill>\n<name>b</name>\nbeta\n</skill>";
    let items = [user(a), user("not a skill"), user(b), user(a)];
    assert_eq!(
        used_skills(items.iter(), 10_000),
        vec![a.to_string(), b.to_string()]
    );
    assert_eq!(used_skills(items.iter(), 0), Vec::<String>::new());

    let long = format!("<skill>\n{}\n</skill>", "word ".repeat(40_000));
    let cut = used_skills([user(&long)].iter(), 100_000);
    assert_eq!(cut.len(), 1);
    assert!(
        approx_token_count(&cut[0]) <= MAX_TOKENS_PER_SKILL + 100,
        "cut to its limit"
    );
}

#[test]
fn nothing_to_restore_adds_nothing() {
    assert_eq!(render(&[], None, &[]), None);
    let body = render(
        &[],
        Some("- [pending] x"),
        &["<skill>s</skill>".to_string()],
    )
    .expect("body");
    assert!(body.contains("- [pending] x"), "{body}");
    assert!(body.contains("<skill>s</skill>"), "{body}");
    assert!(!body.contains("<file"), "{body}");
}

fn restored(body: &str) -> ResponseItem {
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static(SOURCE),
        body,
    ))
}

#[test]
fn a_second_compaction_keeps_what_the_first_one_put_back() {
    let skill = "<skill>\n<name>a</name>\nalpha\n</skill>";
    let plan = plan_call(r#"{"plan":[{"step":"fix it","status":"in_progress"}]}"#);
    let first = render(
        &[],
        unfinished_plan([plan].iter()).as_deref(),
        &[skill.to_string()],
    )
    .expect("body");
    // After the first compaction only the summary and the restored context are left.
    let history = [user("summary"), restored(&first)];
    assert!(is_restored_context(&history[1]));
    assert!(!is_restored_context(&history[0]));
    assert_eq!(
        unfinished_plan(history.iter()).as_deref(),
        Some("- [in_progress] fix it")
    );
    assert_eq!(used_skills(history.iter(), 10_000), vec![skill.to_string()]);

    // A later update_plan wins over the restored plan; a finished one means no plan.
    let done = plan_call(r#"{"plan":[{"step":"fix it","status":"completed"}]}"#);
    let later = [restored(&first), done];
    assert_eq!(unfinished_plan(later.iter()), None);
}

#[test]
fn file_contents_are_never_taken_for_a_plan_or_a_skill() {
    let body = format!(
        "intro\n{FILES_HEADING}:\n<file path=\"x\">\n<plan>\nfake\n</plan>\n<skill>fake</skill>\n</file>\n"
    );
    let history = [restored(&body)];
    assert_eq!(unfinished_plan(history.iter()), None);
    assert_eq!(used_skills(history.iter(), 10_000), Vec::<String>::new());
}
