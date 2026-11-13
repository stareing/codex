use super::*;
use pretty_assertions::assert_eq;

#[test]
fn fallback_title_flattens_markdown_into_one_line() {
    assert_eq!(
        fallback_thread_title("# Fix login\n\n- retry `auth` calls\n- add tests"),
        Some("Fix login retry auth calls add tests".to_string())
    );
}

#[test]
fn fallback_title_keeps_sixty_characters_and_marks_truncation() {
    let exact = "a".repeat(60);
    assert_eq!(fallback_thread_title(&exact), Some(exact.clone()));

    let long = format!("{} tail", "word ".repeat(20));
    let title = fallback_thread_title(&long).expect("title");
    assert_eq!(title.chars().count() <= 60, true);
    assert_eq!(title.ends_with('…'), true);
    assert_eq!(title, format!("{}…", "word ".repeat(12).trim_end()));
}

#[test]
fn fallback_title_never_splits_unicode() {
    let title = fallback_thread_title(&"修".repeat(80)).expect("title");
    assert_eq!(title, format!("{}…", "修".repeat(59)));
}

#[test]
fn fallback_title_is_absent_for_blank_messages() {
    assert_eq!(fallback_thread_title("  \n\t"), None);
    assert_eq!(fallback_thread_title("---"), None);
}

#[test]
fn title_source_uses_the_request_after_model_context() {
    assert_eq!(
        thread_title_source([
            "# Context\n## My request for Codex:\nold\n## My request for Codex:\n  Fix login  ",
            "and tests",
        ]),
        Some("Fix login and tests".to_string())
    );
    assert_eq!(thread_title_source(["  ", ""]), None);
    assert_eq!(thread_title_source([]), None);
}

#[test]
fn output_schema_requires_one_bounded_title() {
    assert_eq!(
        thread_title_output_schema(),
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "minLength": 1, "maxLength": 36},
            },
            "required": ["title"],
            "additionalProperties": false,
        })
    );
}

#[test]
fn title_prompt_is_bounded_and_ends_with_the_request() {
    let prompt = thread_title_prompt("  Fix the login flow  ");
    assert_eq!(
        prompt.ends_with("\n\nUser prompt:\nFix the login flow"),
        true
    );
    assert_eq!(
        prompt.starts_with("Generate a concise, single-line task title"),
        true
    );

    let long = thread_title_prompt(&"é".repeat(2_000));
    assert_eq!(long.len() <= THREAD_TITLE_PROMPT_MAX_BYTES, true);
    assert_eq!(long.is_char_boundary(long.len()), true);
}

#[test]
fn parses_and_normalizes_generated_titles() {
    assert_eq!(
        parse_thread_title(r#"{"title":"  “Fix   login flow.”  "}"#),
        Some("Fix login flow".to_string())
    );
    assert_eq!(parse_thread_title(r#"{"title":"..."}"#), None);
    assert_eq!(parse_thread_title(r#"{"title":"x","extra":1}"#), None);
    assert_eq!(parse_thread_title("Fix login"), None);
    assert_eq!(
        parse_thread_title(&format!(r#"{{"title":"{}"}}"#, "界".repeat(50))),
        Some("界".repeat(THREAD_TITLE_MAX_CHARS))
    );
}

#[test]
fn hidden_thread_overrides_disable_every_side_feature() {
    let overrides = hidden_thread_config_overrides();
    assert_eq!(overrides.len(), 27);
    assert_eq!(overrides.get("web_search"), Some(&Value::from("disabled")));
    assert_eq!(
        overrides
            .iter()
            .filter(|(key, _)| key.as_str() != "web_search")
            .all(|(_, value)| value == &Value::Bool(false)),
        true
    );
}
