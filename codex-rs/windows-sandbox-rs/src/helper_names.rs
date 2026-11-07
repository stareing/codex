// File names of the two sandbox helpers that ship next to the agent. This is the one definition:
// `Cargo.toml`'s `[[bin]]` names, `build.rs` (which `include!`s this file) and every lookup use it.

/// Bin name of the command runner the sandbox user launches.
pub const COMMAND_RUNNER_BIN: &str = "onigio-command-runner";
/// File name of [`COMMAND_RUNNER_BIN`] on Windows.
pub const COMMAND_RUNNER_EXE: &str = "onigio-command-runner.exe";
/// Bin name of the elevated setup helper.
pub const SETUP_BIN: &str = "onigio-windows-sandbox-setup";
/// File name of [`SETUP_BIN`] on Windows.
pub const SETUP_EXE: &str = "onigio-windows-sandbox-setup.exe";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_names_are_the_cargo_bins() {
        let manifest = include_str!("../Cargo.toml");
        for bin in [COMMAND_RUNNER_BIN, SETUP_BIN] {
            assert!(
                manifest.contains(&format!("name = \"{bin}\"")),
                "{bin} is not a [[bin]]"
            );
        }
        assert_eq!(COMMAND_RUNNER_EXE, format!("{COMMAND_RUNNER_BIN}.exe"));
        assert_eq!(SETUP_EXE, format!("{SETUP_BIN}.exe"));
    }
}
