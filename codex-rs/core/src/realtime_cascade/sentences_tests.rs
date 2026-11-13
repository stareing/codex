use super::*;
use pretty_assertions::assert_eq;

fn split_all(chunks: &[&str]) -> Vec<String> {
    let mut splitter = SentenceSplitter::default();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(splitter.push(chunk));
    }
    out.extend(splitter.finish());
    out
}

fn split_stream(text: &str) -> Vec<String> {
    // One character at a time: the hardest streaming case.
    let chunks: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
    split_all(&refs)
}

#[test]
fn ascii_terminators_need_following_whitespace() {
    let mut splitter = SentenceSplitter::default();
    assert_eq!(splitter.push("Hello there. How"), vec!["Hello there."]);
    // "3." might be "3.14": only the question is complete.
    assert_eq!(splitter.push(" are you? Pi is 3."), vec!["How are you?"]);
    assert_eq!(
        splitter.push("14 exactly! Done"),
        vec!["Pi is 3.14 exactly!"]
    );
    assert_eq!(splitter.finish(), vec!["Done"]);
}

#[test]
fn a_terminator_at_the_end_of_the_stream_waits_for_the_next_character() {
    let mut splitter = SentenceSplitter::default();
    assert!(splitter.push("Wait.").is_empty());
    assert_eq!(splitter.push(" Next"), vec!["Wait."]);
    assert_eq!(splitter.finish(), vec!["Next"]);
}

#[test]
fn cjk_terminators_end_immediately() {
    let mut splitter = SentenceSplitter::default();
    assert_eq!(splitter.push("你好。今天"), vec!["你好。"]);
    assert_eq!(
        splitter.push("天气不错！真的吗？好；"),
        vec!["今天天气不错！", "真的吗？", "好；"]
    );
    assert_eq!(split_all(&["等一下…然后"]), vec!["等一下…", "然后"]);
}

#[test]
fn line_breaks_end_sentences_and_blank_lines_are_skipped() {
    assert_eq!(
        split_all(&["First line\n\nSecond line\nThird"]),
        vec!["First line", "Second line", "Third"]
    );
}

#[test]
fn every_rule_gives_the_same_result_when_streamed_character_by_character() {
    let text = "# Title\n- one item. two.\n1. first\n> quote here\n```rust\nlet x = 1;\n```\nAfter code. 中文。End";
    let expected = vec![
        "Title",
        "one item.",
        "two.",
        "first",
        "quote here",
        "After code.",
        "中文。",
        "End",
    ];
    assert_eq!(split_all(&[text]), expected);
    assert_eq!(split_stream(text), expected);
}

#[test]
fn code_blocks_are_skipped_including_bare_fences_and_tildes() {
    assert_eq!(
        split_all(&["Before.\n```\nprint('x')\nmore code\n```\nAfter."]),
        vec!["Before.", "After."]
    );
    assert_eq!(
        split_all(&["~~~\ncode. still code.\n~~~\nText."]),
        vec!["Text."]
    );
    // An unterminated block swallows the rest.
    assert_eq!(split_all(&["Intro\n```py\nx = 1\n"]), vec!["Intro"]);
}

#[test]
fn list_and_heading_markers_are_dropped_but_not_text_that_looks_alike() {
    assert_eq!(
        split_all(&[
            "* star\n+ plus\n12) twelve\n## Sub\n#hashtag\n**bold** start\n2024 was a year"
        ]),
        vec![
            "star",
            "plus",
            "twelve",
            "Sub",
            "hashtag",
            "bold start",
            "2024 was a year"
        ]
    );
}

#[test]
fn over_long_sentences_are_cut_at_a_soft_break() {
    let long = format!("{} tail", "word ".repeat(80));
    let sentences = split_all(&[&long]);
    assert!(sentences.len() >= 2, "{sentences:?}");
    assert!(
        sentences
            .iter()
            .all(|s| s.chars().count() <= MAX_SENTENCE_CHARS)
    );
    assert_eq!(
        sentences.join(" "),
        long.split_whitespace().collect::<Vec<_>>().join(" ")
    );

    // No soft break at all: a hard cut at the limit.
    let solid = "字".repeat(MAX_SENTENCE_CHARS + 10);
    let sentences = split_all(&[&solid]);
    assert_eq!(sentences.len(), 2);
    assert_eq!(sentences[0].chars().count(), MAX_SENTENCE_CHARS);
}

#[test]
fn speakable_strips_inline_markdown_and_urls() {
    let cases = [
        ("**Bold** and `code`", Some("Bold and code")),
        (
            "See [the docs](https://example.com/a) now",
            Some("See the docs now"),
        ),
        (
            "An image ![diagram](x.png) here",
            Some("An image diagram here"),
        ),
        (
            "Visit https://example.com/path?q=1 today",
            Some("Visit today"),
        ),
        ("| a | b |", Some("a b")),
        ("~~old~~ new", Some("old new")),
        ("[1] is not a link", Some("[1] is not a link")),
        ("snake_case stays", Some("snake_case stays")),
        ("---", None),
        ("```", None),
        ("   ", None),
        ("…", None),
    ];
    for (input, expected) in cases {
        assert_eq!(speakable(input).as_deref(), expected, "{input}");
    }
}

#[test]
fn finish_resets_for_the_next_message() {
    let mut splitter = SentenceSplitter::default();
    assert!(splitter.push("```\ncode").is_empty());
    assert!(splitter.finish().is_empty());
    assert_eq!(splitter.push("Fresh start. "), vec!["Fresh start."]);
}
