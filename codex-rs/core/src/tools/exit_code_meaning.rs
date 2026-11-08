//! What a non-zero exit code means for commands that use it for an answer rather than a failure: `grep` exiting 1
//! found nothing, `diff` exiting 1 found differences. Without this the model takes "exited with code 1" as an error
//! and retries or "fixes" a command that worked. The command that sets the exit code is the last one of a
//! pipeline or `&&`/`||`/`;` list.

/// Commands whose exit code 1 is an answer, with that answer.
const ANSWERS: &[(&[&str], &str)] = &[
    (&["grep", "egrep", "fgrep", "rg"], "no matches found"),
    (&["find"], "some paths could not be read"),
    (&["diff", "cmp"], "the inputs differ"),
    (&["test", "["], "the condition is false"),
];

/// The note for `exit_code` of `command`, or `None` when the code means what it usually does.
pub(crate) fn exit_code_note(command: &str, exit_code: i32) -> Option<String> {
    if exit_code != 1 {
        return None;
    }
    let program = last_program(command)?;
    ANSWERS
        .iter()
        .find(|(names, _)| names.contains(&program.as_str()))
        .map(|(_, answer)| format!("{program}: {answer}; exit code 1 is not an error here"))
}

/// The program name of the last simple command in `command` (outside quotes), without a path or leading
/// `VAR=value` assignments.
fn last_program(command: &str) -> Option<String> {
    let mut segments = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                segments.last_mut()?.push(c);
                if let Some(next) = chars.next() {
                    segments.last_mut()?.push(next);
                }
                continue;
            }
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            // `2>&1`, `&>file`: redirections, not a list.
            (None, '&')
                if segments.last().is_some_and(|s| s.ends_with('>'))
                    || chars.peek() == Some(&'>') => {}
            (None, '|' | ';' | '&' | '\n') => {
                while chars.peek().is_some_and(|n| matches!(n, '|' | '&')) {
                    chars.next();
                }
                segments.push(String::new());
                continue;
            }
            (None, _) => {}
        }
        segments.last_mut()?.push(c);
    }
    let last = segments.iter().rev().find(|s| !s.trim().is_empty())?;
    let word = last
        .split_whitespace()
        .find(|w| !is_assignment(w))?
        .trim_matches(|c| c == '\'' || c == '"');
    let name = word.rsplit(['/', '\\']).next()?.trim_end_matches(".exe");
    (!name.is_empty()).then(|| name.to_string())
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn exit_code_one_of_a_search_or_comparison_is_an_answer() {
        assert_eq!(
            exit_code_note("rg -n needle src", 1).as_deref(),
            Some("rg: no matches found; exit code 1 is not an error here")
        );
        assert!(exit_code_note("diff a b", 1).unwrap().contains("differ"));
        assert!(
            exit_code_note("[ -f x ]", 1)
                .unwrap()
                .contains("condition is false")
        );
        assert!(exit_code_note("/usr/bin/grep x f", 1).is_some());
        assert!(exit_code_note("LC_ALL=C grep x f", 1).is_some());
    }

    #[test]
    fn only_the_last_command_of_a_list_counts() {
        assert!(exit_code_note("cat f | grep x", 1).is_some());
        assert!(exit_code_note("grep x f && make", 1).is_none());
        assert!(exit_code_note("make; diff a b", 1).is_some());
        assert!(
            exit_code_note("echo 'a | grep' ", 1).is_none(),
            "a | inside quotes is text"
        );
        assert!(
            exit_code_note("grep -r x . 2>&1", 1).is_some(),
            "2>&1 is a redirection"
        );
        assert!(exit_code_note("grep x f &>/dev/null", 1).is_some());
    }

    #[test]
    fn other_codes_and_commands_keep_their_meaning() {
        assert!(exit_code_note("grep x f", 2).is_none());
        assert!(exit_code_note("grep x f", 0).is_none());
        assert!(exit_code_note("cargo test", 1).is_none());
        assert!(exit_code_note("", 1).is_none());
    }
}
