//! Output-filter wrappers. A `PreToolUse` hook may rewrite a command `git log` into `rtk git log`
//! (rtk runs the command and shortens its output); safety checks and exec-policy rules must still
//! see `git log`, or a rule forbidding `git push` would not apply to `rtk git push`.

/// Wrapper programs that run the rest of the command line unchanged apart from their output.
const OUTPUT_FILTERS: &[&str] = &["rtk"];
/// A wrapper's own pass-through subcommand (`rtk proxy <cmd>` runs `<cmd>` unfiltered).
const PASS_THROUGH: &[&str] = &["proxy"];

/// The command a known output filter runs, or the command itself.
pub fn unwrap_output_filter(command: &[String]) -> &[String] {
    let Some(first) = command.first() else {
        return command;
    };
    if !OUTPUT_FILTERS.contains(&program_name(first).as_str()) {
        return command;
    }
    let mut rest = &command[1..];
    if rest
        .first()
        .is_some_and(|word| PASS_THROUGH.contains(&word.as_str()))
    {
        rest = &rest[1..];
    }
    if rest.is_empty() { command } else { rest }
}

/// `C:\Tools\RTK.EXE` and `/usr/bin/rtk` are both `rtk`.
fn program_name(raw: &str) -> String {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .to_ascii_lowercase();
    name.strip_suffix(".exe").map(str::to_owned).unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::unwrap_output_filter;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_wrapped_command_is_what_gets_checked() {
        for (line, expected) in [
            ("rtk git push --force", "git push --force"),
            ("rtk proxy git push", "git push"),
            (
                r"C:\Users\me\AppData\Local\Onigio\RTK.exe rm -rf build",
                "rm -rf build",
            ),
            ("/opt/rtk/rtk cargo test", "cargo test"),
            ("git log", "git log"),
            ("rtkx git push", "rtkx git push"),
            ("rtk", "rtk"),
            ("rtk proxy", "rtk proxy"),
        ] {
            let command = words(line);
            assert_eq!(
                unwrap_output_filter(&command),
                words(expected).as_slice(),
                "{line}"
            );
        }
        assert!(unwrap_output_filter(&[]).is_empty());
    }
}
