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

const ABOUT: &str = "Onigio agent: a coding agent for your computer, reachable from the site.";

/// Renames the top-level command for help and usage output.
pub fn rename_command(command: Command, name: &'static str) -> Command {
    rebrand_texts(command)
        .name(name)
        .bin_name(name)
        .about(ABOUT)
        // The derive's doc comment is the long help (`--help`); replace it too.
        .long_about(ABOUT)
        .override_usage(format!(
            "{name} [OPTIONS] [PROMPT]\n       {name} [OPTIONS] <COMMAND> [ARGS]"
        ))
}

/// The upstream product name in help text, replaced recursively (subcommands and arguments).
/// Paths such as `~/.codex` and `$CODEX_HOME` stay as they are: they are real locations.
fn rebrand(text: &clap::builder::StyledStr) -> String {
    text.to_string().replace("Codex", "Onigio agent")
}

fn rebrand_texts(command: Command) -> Command {
    let about = command.get_about().map(rebrand);
    let long_about = command.get_long_about().map(rebrand);
    let mut command = command.mut_args(|arg| {
        let help = arg.get_help().map(rebrand);
        let long_help = arg.get_long_help().map(rebrand);
        let arg = match help {
            Some(help) => arg.help(help),
            None => arg,
        };
        match long_help {
            Some(long_help) => arg.long_help(long_help),
            None => arg,
        }
    });
    if let Some(about) = about {
        command = command.about(about);
    }
    if let Some(long_about) = long_about {
        command = command.long_about(long_about);
    }
    command.mut_subcommands(rebrand_texts)
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
                // The real command's doc comment sets a long help, which `--help` prints.
                .long_about("Codex CLI\n\nIf no subcommand is specified, ...")
                .bin_name("codex")
                .arg(
                    clap::Arg::new("strict")
                        .long("strict")
                        .help("Unknown to this version of Codex"),
                )
                .subcommand(Command::new("exec").about("Run Codex non-interactively")),
            BRANDED_BIN_NAME,
        );
        for help in [
            command.render_help().to_string(),
            command.render_long_help().to_string(),
        ] {
            assert!(help.contains("onigio-agent [OPTIONS] [PROMPT]"), "{help}");
            assert!(!help.contains("codex [OPTIONS]"), "{help}");
            assert!(!help.contains("Codex"), "{help}");
            assert!(help.starts_with("Onigio agent"), "{help}");
        }
        let exec = command.find_subcommand_mut("exec").expect("exec");
        assert_eq!(
            exec.render_help().to_string().lines().next(),
            Some("Run Onigio agent non-interactively")
        );
    }
}
