use super::*;
use pretty_assertions::assert_eq;

#[test]
fn whole_file_patches_parse_as_one_update_or_add() {
    let patch = whole_file_patch("src/a.txt", Some("one\ntwo\n"), "one\n2\n");
    assert_eq!(
        patch,
        "*** Begin Patch\n*** Update File: src/a.txt\n@@\n-one\n-two\n+one\n+2\n*** End Patch"
    );
    let parsed = codex_apply_patch::parse_patch(&patch).expect("update parses");
    assert_eq!(parsed.hunks.len(), 1);

    let added = whole_file_patch("new/b.md", None, "# title\n\n*** End Patch\n");
    assert_eq!(
        added,
        "*** Begin Patch\n*** Add File: new/b.md\n+# title\n+\n+*** End Patch\n*** End Patch"
    );
    let parsed = codex_apply_patch::parse_patch(&added)
        .expect("content that looks like the grammar stays content");
    assert_eq!(parsed.hunks.len(), 1);
}

#[test]
fn replacing_needs_one_exact_match_unless_all() {
    assert_eq!(replaced("a b a", "b", "c", false), Ok("a c a".to_string()));
    assert_eq!(replaced("a b a", "a", "x", true), Ok("x b x".to_string()));
    let twice = replaced("a b a", "a", "x", false).unwrap_err();
    assert!(twice.contains("appears 2 times"), "{twice}");
    assert!(
        replaced("abc", "zz", "y", false)
            .unwrap_err()
            .contains("not found")
    );
    assert!(
        replaced("abc", "", "y", false)
            .unwrap_err()
            .contains("Write")
    );
    assert!(
        replaced("abc", "b", "b", false)
            .unwrap_err()
            .contains("same")
    );
}

#[test]
fn replacing_follows_the_files_crlf_endings() {
    assert_eq!(
        replaced("one\r\ntwo\r\nthree\r\n", "one\ntwo", "1\n2", false),
        Ok("1\r\n2\r\nthree\r\n".to_string())
    );
}

#[test]
fn read_numbers_lines_and_pages() {
    assert_eq!(
        numbered("a\nb\r\nc\n", 1, 2000, 10_000),
        "     1\ta\n     2\tb\n     3\tc\n"
    );
    assert_eq!(
        numbered("a\nb\nc", 2, 1, 10_000),
        "     2\tb\n(showing lines 2-2 of 3; pass offset 3 to read on)\n"
    );
    assert_eq!(numbered("", 1, 10, 10_000), "(the file is empty)");
    assert!(numbered("a\n", 5, 10, 10_000).contains("past the end"));
    let long = "x".repeat(MAX_LINE_CHARS + 5);
    assert!(numbered(&long, 1, 1, 10_000).ends_with(" … (line cut)\n"));
}

#[test]
fn read_stops_at_a_whole_line_within_the_size_limit() {
    let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    let out = numbered(&text, 1, 2000, 60);
    assert!(out.starts_with("     1\tline 1\n"), "{out}");
    assert!(
        out.ends_with("(output size limit: showing lines 1-3 of 100; pass offset 4 to read on, or Grep for what you need)\n"),
        "{out}"
    );
    // One line is always shown, however long.
    let one = numbered(&"y".repeat(500), 1, 2000, 10);
    assert!(one.starts_with("     1\tyyy"), "{one}");
}

#[test]
fn replacing_tolerates_quote_style_and_keeps_the_files_quotes() {
    let file = "say \u{201C}hi\u{201D} and don\u{2019}t stop\n";
    assert_eq!(
        replaced(
            file,
            "say \"hi\" and don't",
            "say \"hello\" and can't",
            false
        ),
        Ok("say \u{201C}hello\u{201D} and can\u{2019}t stop\n".to_string())
    );
    // Straight quotes in the file, curly ones from the model: the file's straight style wins.
    assert_eq!(
        replaced(
            "it's \"x\"\n",
            "it\u{2019}s \u{201C}x\u{201D}",
            "it's \"y\"",
            false
        ),
        Ok("it's \"y\"\n".to_string())
    );
    let twice = replaced("\u{2018}a\u{2019} \u{2018}a\u{2019}", "'a'", "'b'", false).unwrap_err();
    assert!(twice.contains("appears 2 times"), "{twice}");
    assert_eq!(
        replaced("\u{2018}a\u{2019} \u{2018}a\u{2019}", "'a'", "'b'", true),
        Ok("\u{2018}b\u{2019} \u{2018}b\u{2019}".to_string())
    );
    assert!(
        replaced("plain", "'q'", "x", false)
            .unwrap_err()
            .contains("not found")
    );
}
