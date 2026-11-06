//! The program name users see. The cargo binary stays `codex` (upstream tests run it), while our
//! releases ship it as `onigio-agent`; help, usage and `--version` follow the name it was run as.

use clap::ArgMatches;
use clap::Command;

/// The release name of the executable (`onigio-agent.exe` on Windows).
pub const BRANDED_BIN_NAME: &str = "onigio-agent";

/// `Some(BRANDED_BIN_NAME)` when `argv0` is the branded executable, whatever its directory or
/// extension; `None` keeps upstream's `codex` naming.
pub fn branded_name(argv0: Option<&std::ffi::OsStr>) -> Option<&'static str> {
    // Split on both separators so a Windows path is recognised on any host.
    let file = argv0?.to_str()?.rsplit(['/', '\\']).next()?;
    let stem = match file.len().checked_sub(4) {
        Some(cut) if file.is_char_boundary(cut) && file[cut..].eq_ignore_ascii_case(".exe") => {
            &file[..cut]
        }
        _ => file,
    };
    stem.eq_ignore_ascii_case(BRANDED_BIN_NAME)
        .then_some(BRANDED_BIN_NAME)
}

/// Renames the top-level command for help and usage output.
pub fn rename_command(command: Command, name: &'static str) -> Command {
    command
        .name(name)
        .bin_name(name)
        .about("Onigio agent: a coding agent for your computer, reachable from the site.")
        .override_usage(format!(
            "{name} [OPTIONS] [PROMPT]\n       {name} [OPTIONS] <COMMAND> [ARGS]"
        ))
}

/// Whether this process was started as the branded executable (`onigio-agent`).
pub fn invoked_as_branded() -> bool {
    branded_name(std::env::args_os().next().as_deref()).is_some()
}

/// Parses the process arguments with the branded name applied when it was run under it.
pub fn get_matches(command: Command) -> ArgMatches {
    match branded_name(std::env::args_os().next().as_deref()) {
        Some(name) => rename_command(command, name).get_matches(),
        None => command.get_matches(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsStr;

    #[test]
    fn only_the_branded_executable_is_renamed() {
        for argv0 in [
            "onigio-agent",
            "/usr/local/bin/onigio-agent",
            r"C:\Program Files\Onigio\onigio-agent.exe",
            "ONIGIO-AGENT.EXE",
        ] {
            assert_eq!(
                branded_name(Some(OsStr::new(argv0))),
                Some(BRANDED_BIN_NAME),
                "{argv0}"
            );
        }
        for argv0 in [
            "codex",
            "/usr/bin/codex",
            "codex-x86_64-unknown-linux-gnu",
            "agent",
        ] {
            assert_eq!(branded_name(Some(OsStr::new(argv0))), None, "{argv0}");
        }
        assert_eq!(branded_name(None), None);
    }

    #[test]
    fn renamed_help_uses_the_branded_name() {
        let mut command = rename_command(
            Command::new("codex")
                .about("Codex CLI")
                .bin_name("codex")
                .subcommand(Command::new("login")),
            BRANDED_BIN_NAME,
        );
        let help = command.render_help().to_string();
        assert!(help.contains("onigio-agent [OPTIONS] [PROMPT]"), "{help}");
        assert!(!help.contains("codex [OPTIONS]"), "{help}");
        assert!(help.starts_with("Onigio agent"), "{help}");
    }
}
