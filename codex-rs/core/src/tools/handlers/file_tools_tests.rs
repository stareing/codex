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
        numbered("a\nb\r\nc\n", 1, 2000),
        "     1\ta\n     2\tb\n     3\tc\n"
    );
    assert_eq!(
        numbered("a\nb\nc", 2, 1),
        "     2\tb\n(showing lines 2-2 of 3; pass offset 3 to read on)\n"
    );
    assert_eq!(numbered("", 1, 10), "(the file is empty)");
    assert!(numbered("a\n", 5, 10).contains("past the end"));
    let long = "x".repeat(MAX_LINE_CHARS + 5);
    assert!(numbered(&long, 1, 1).ends_with(" … (line cut)\n"));
}
