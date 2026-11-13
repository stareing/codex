//! Pure parts of Codex's native thread-title generation, shared by every client.
//!
//! A title request runs one structured turn on a hidden, ephemeral, read-only thread. This crate
//! owns what that request asks and how its answer is read: the prompt, the output schema, the
//! response parser, the single-line summary used before and instead of a generated title, and the
//! limits and config overrides of the hidden thread. Running the hidden thread is the caller's job.

use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use pulldown_cmark::Event;
use pulldown_cmark::Parser;
use pulldown_cmark::TagEnd;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

pub const THREAD_TITLE_MAX_CHARS: usize = 36;
pub const THREAD_TITLE_PROMPT_MAX_BYTES: usize = 960;
/// `ThreadSource::Feature` tag of hidden title threads; core keeps their request-level effort.
pub const THREAD_TITLE_THREAD_SOURCE: &str = "thread_title";
/// Deadline for each step of a hidden structured request: start, turn, interrupt, detach.
pub const STRUCTURED_TURN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);
/// Largest structured answer accepted from a hidden turn.
pub const STRUCTURED_RESPONSE_MAX_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedThreadTitle {
    title: String,
}

/// Config overrides for a hidden structured thread: no tools, extensions, or side features.
///
/// Callers must also disable every MCP server; how depends on how they start the thread.
pub fn hidden_thread_config_overrides() -> HashMap<String, Value> {
    HashMap::from([
        ("features.apps".to_string(), false.into()),
        ("features.code_mode".to_string(), false.into()),
        ("features.code_mode_only".to_string(), false.into()),
        ("features.context_management".to_string(), false.into()),
        ("features.current_time_reminder".to_string(), false.into()),
        ("features.deferred_executor".to_string(), false.into()),
        ("features.enable_fanout".to_string(), false.into()),
        ("features.goals".to_string(), false.into()),
        ("features.hooks".to_string(), false.into()),
        ("features.image_generation".to_string(), false.into()),
        ("features.memories".to_string(), false.into()),
        ("features.multi_agent".to_string(), false.into()),
        ("features.multi_agent_v2".to_string(), false.into()),
        ("features.plugins".to_string(), false.into()),
        (
            "features.request_permissions_tool".to_string(),
            false.into(),
        ),
        ("features.shell_snapshot".to_string(), false.into()),
        ("features.shell_tool".to_string(), false.into()),
        ("features.standalone_web_search".to_string(), false.into()),
        ("features.token_budget".to_string(), false.into()),
        ("features.tool_suggest".to_string(), false.into()),
        ("features.unified_exec".to_string(), false.into()),
        ("features.view_image".to_string(), false.into()),
        ("cloud.skills.enabled".to_string(), false.into()),
        ("skills.include_instructions".to_string(), false.into()),
        (
            "tools.experimental_request_user_input.enabled".to_string(),
            false.into(),
        ),
        ("tools.update_plan.enabled".to_string(), false.into()),
        ("web_search".to_string(), "disabled".into()),
    ])
}

/// Constrain generated metadata to one nonempty title within the display limit.
pub fn thread_title_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": THREAD_TITLE_MAX_CHARS,
            },
        },
        "required": ["title"],
        "additionalProperties": false,
    })
}

fn thread_title_instructions() -> String {
    format!(
        "Generate a concise, single-line task title of at most \
  {THREAD_TITLE_MAX_CHARS} characters and under five words where possible. \
  Start with an imperative verb. Capitalize only the first word unless the \
  user's language, proper nouns, acronyms, or code terms require otherwise. \
  Preserve ticket references exactly. Write in the user's language. \
  Do not use quotes, markdown, or trailing punctuation. \
  Do not answer the request."
    )
}

/// The text a title is generated from: the request part of each text input, joined by spaces.
///
/// Clients may prefix a message with model-only context ending in [`USER_MESSAGE_BEGIN`]; only
/// what follows the last delimiter is the user's request. Returns `None` when nothing remains.
pub fn thread_title_source<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let source = texts
        .into_iter()
        .map(|text| match text.rsplit_once(USER_MESSAGE_BEGIN) {
            Some((_, request)) => request.trim(),
            None => text,
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!source.trim().is_empty()).then_some(source)
}

/// Desktop shows a single line from the first message until a generated title is available.
pub fn fallback_thread_title(message: &str) -> Option<String> {
    const MAX_CHARS: usize = 60;
    const INPUT_CHAR_LIMIT: usize = 2_000;
    let excerpt: String = message.trim().chars().take(INPUT_CHAR_LIMIT).collect();
    let mut text = String::new();
    for event in Parser::new(&excerpt) {
        match event {
            Event::Text(value) | Event::Code(value) => text.push_str(&value),
            Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock,
            ) => text.push(' '),
            _ => {}
        }
    }
    let mut title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = title.char_indices().skip(MAX_CHARS - 1);
    if let Some((end, _)) = chars.next()
        && chars.next().is_some()
    {
        title.truncate(title[..end].trim_end().len());
        title.push('…');
    }
    (!title.is_empty()).then_some(title)
}

/// Build a bounded title request without truncating a Unicode character.
pub fn thread_title_prompt(user_message: &str) -> String {
    let instructions = thread_title_instructions();
    let prefix = format!("{instructions}\n\nUser prompt:\n");
    let remaining_bytes = THREAD_TITLE_PROMPT_MAX_BYTES.saturating_sub(prefix.len());
    let user_message = user_message
        .trim()
        .char_indices()
        .take_while(|(index, character)| index + character.len_utf8() <= remaining_bytes)
        .map(|(_, character)| character)
        .collect::<String>();

    format!("{prefix}{user_message}")
}

/// Bound the entire suggestion prompt while preserving complete Unicode characters.
pub fn recent_conversation_thread_title_prompt(conversation: &str) -> String {
    let instructions = thread_title_instructions();
    let prefix = format!(
        "{instructions}\n\
Prioritize the current task and latest substantive user request.\n\n\
Recent conversation messages:\n"
    );
    let conversation = conversation.trim();
    let remaining_bytes = THREAD_TITLE_PROMPT_MAX_BYTES.saturating_sub(prefix.len());
    let mut start = conversation.len().saturating_sub(remaining_bytes);
    while !conversation.is_char_boundary(start) {
        start += 1;
    }
    let conversation = &conversation[start..];

    format!("{prefix}{conversation}")
}

/// Normalize a generated title and truncate it without splitting Unicode characters.
pub fn parse_thread_title(response: &str) -> Option<String> {
    if !response.trim_start().starts_with('{') {
        return None;
    }

    let title = serde_json::from_str::<GeneratedThreadTitle>(response)
        .ok()?
        .title;

    let normalized = title
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '?', '!'])
        .trim_end()
        .to_string();

    if normalized.is_empty() {
        return None;
    }

    Some(normalized.chars().take(THREAD_TITLE_MAX_CHARS).collect())
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
