use super::*;
use pretty_assertions::assert_eq;

#[test]
fn questions_on_the_last_line_count_as_prompts() {
    for prompt in [
        "Install these packages? (y/n)",
        "Proceed [Y/n] ",
        "Delete everything? (yes/no)",
        "Press Enter to continue",
        "press any key...",
        "Overwrite?",
        "Do you want to continue?",
        "Are you sure you want to delete it?\n\n",
        "building...\nWould you like to use TypeScript? ",
    ] {
        assert!(looks_like_prompt(prompt), "{prompt:?}");
    }
}

#[test]
fn ordinary_output_is_not_a_prompt() {
    for output in [
        "",
        "   \n",
        "Listening on http://localhost:3000",
        "Do you know? It compiled.",
        "Are you sure? (y/n)\nyes\nDeleted 3 files",
        "compiling 12 of 40 crates",
    ] {
        assert!(!looks_like_prompt(output), "{output:?}");
    }
}

#[test]
fn the_tail_keeps_the_end_and_no_broken_character() {
    let mut output = "é".repeat(600).into_bytes();
    output.extend_from_slice(b"\nlast line");
    let tail = tail(&output);
    assert!(tail.ends_with("last line"));
    assert!(tail.len() <= TAIL_BYTES);
    assert!(!tail.contains('\u{FFFD}'));
    assert_eq!(super::tail(b"short"), "short");
}

#[test]
fn notices_name_the_session_and_explain_the_exit_code() {
    let notice = exit_notice(7, "rg needle", 1, "");
    assert!(notice.contains("session 7"), "{notice}");
    assert!(
        notice.contains("exit code 1 (rg: no matches found"),
        "{notice}"
    );
    assert!(notice.contains("It printed nothing."), "{notice}");

    let notice = exit_notice(7, "cargo build", 0, "Finished\n");
    assert!(notice.contains("exit code 0."), "{notice}");
    assert!(notice.contains("```\nFinished\n```"), "{notice}");

    let notice = stall_notice(3, "npx create-app", "Continue? (y/n)");
    assert!(notice.contains("write_stdin on session 3"), "{notice}");
    assert!(notice.contains("45 seconds"), "{notice}");
}
