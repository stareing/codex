//! Runtime configuration: which language servers exist, which file extensions they serve, and the resource limits.
//!
//! The host (`[lsp]` in the agent config) builds an [`LspSettings`]; [`ServerTable`] then decides, once per
//! configuration, which server handles each file extension. Servers marked `optional` are dropped when their
//! command cannot be found, so a host can list defaults that only apply where the program is installed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::diagnostics::DiagnosticLimits;

/// Default time one request (definition, references, ...) may take.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default time from spawning a server until it answers `initialize`.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// A server unused for this long is shut down; the next use starts it again.
pub const DEFAULT_IDLE_SHUTDOWN: Duration = Duration::from_secs(10 * 60);
/// Language servers can use gigabytes each (rust-analyzer: 4–6 GB on a large workspace).
pub const DEFAULT_MAX_SERVERS: usize = 3;
/// After an edit, how long the next model step waits for the server to publish diagnostics for it.
pub const DEFAULT_DIAGNOSTICS_WAIT: Duration = Duration::from_millis(1500);
/// Files larger than this are not sent to a server.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10_000_000;
/// Largest single JSON-RPC message accepted from a server.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Priority of a server without an explicit one (a user's own entry).
pub const DEFAULT_PRIORITY: i64 = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct LspLimits {
    pub request_timeout: Duration,
    pub startup_timeout: Duration,
    pub idle_shutdown: Duration,
    pub max_servers: usize,
    pub diagnostics_wait: Duration,
    pub max_file_bytes: u64,
    pub max_message_bytes: usize,
    pub diagnostics: DiagnosticLimits,
}

impl Default for LspLimits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            idle_shutdown: DEFAULT_IDLE_SHUTDOWN,
            max_servers: DEFAULT_MAX_SERVERS,
            diagnostics_wait: DEFAULT_DIAGNOSTICS_WAIT,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            diagnostics: DiagnosticLimits::default(),
        }
    }
}

/// One configured language server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// File extension (with the dot, lower case) → LSP language id.
    pub extensions: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
    /// Answers to `workspace/configuration`, looked up by section (`"rust-analyzer"`, `"python.analysis"`).
    pub settings: Option<Value>,
    /// Higher wins when several servers serve one extension.
    pub priority: i64,
    /// Drop the server when its command is not installed instead of failing on use.
    pub optional: bool,
    /// Where the entry came from, shown to the model (for example the plugin that declared it).
    pub source: Option<String>,
    /// Overrides [`LspLimits::startup_timeout`] (a server installed on first start needs longer).
    pub startup_timeout: Option<Duration>,
    /// Fixed workspace folder: the process's working directory and the only root (Claude Code `workspaceFolder`);
    /// `None` = the conversation's working directory or the file's project root.
    pub workspace_folder: Option<PathBuf>,
    /// Failed starts plus crashes after which the server is not started again in this process (Claude Code
    /// `maxRestarts`); `None` = [`crate::manager::MAX_START_FAILURES`].
    pub max_restarts: Option<u32>,
}

impl ServerSpec {
    /// Normalises extension keys to `.ext` lower case.
    pub fn normalized_extension(ext: &str) -> String {
        let lower = ext.trim().to_ascii_lowercase();
        if lower.starts_with('.') {
            lower
        } else {
            format!(".{lower}")
        }
    }

    pub fn language_id(&self, path: &Path) -> String {
        file_extension(path)
            .and_then(|ext| self.extensions.get(&ext).cloned())
            .unwrap_or_else(|| PLAIN_TEXT_LANGUAGE.to_string())
    }
}

const PLAIN_TEXT_LANGUAGE: &str = "plaintext";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LspSettings {
    pub enabled: bool,
    pub servers: Vec<ServerSpec>,
    pub limits: LspLimits,
}

/// `.rs` for `src/main.rs` (lower case), `None` without an extension.
pub fn file_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
}

/// A server that will be used, with its command resolved when it could be found.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveServer {
    pub spec: ServerSpec,
    /// `None` when the command was not found (only for non-optional servers: they fail on use with a clear error).
    pub program: Option<PathBuf>,
}

/// The servers chosen for one configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerTable {
    servers: Vec<ActiveServer>,
    by_extension: BTreeMap<String, usize>,
    /// Optional servers left out because their command is not installed.
    skipped: Vec<String>,
}

impl ServerTable {
    /// Resolves commands against `path_env` (the agent's `PATH`) and picks one server per extension:
    /// highest priority first, then name.
    pub fn new(specs: &[ServerSpec], path_env: Option<OsString>, cwd: &Path) -> Self {
        let mut candidates: Vec<ActiveServer> = Vec::new();
        let mut skipped = Vec::new();
        for spec in specs {
            if spec.command.trim().is_empty() || spec.extensions.is_empty() {
                skipped.push(spec.name.clone());
                continue;
            }
            let program = resolve_command(&spec.command, path_env.clone(), cwd);
            if program.is_none() && spec.optional {
                skipped.push(spec.name.clone());
                continue;
            }
            candidates.push(ActiveServer {
                spec: spec.clone(),
                program,
            });
        }
        candidates.sort_by(|a, b| {
            b.spec
                .priority
                .cmp(&a.spec.priority)
                .then_with(|| a.spec.name.cmp(&b.spec.name))
        });
        let mut by_extension = BTreeMap::new();
        let mut servers = Vec::new();
        for candidate in candidates {
            let fresh: Vec<String> = candidate
                .spec
                .extensions
                .keys()
                .filter(|ext| !by_extension.contains_key(*ext))
                .cloned()
                .collect();
            if fresh.is_empty() {
                skipped.push(candidate.spec.name.clone());
                continue;
            }
            let index = servers.len();
            for ext in fresh {
                by_extension.insert(ext, index);
            }
            servers.push(candidate);
        }
        skipped.sort();
        Self {
            servers,
            by_extension,
            skipped,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn servers(&self) -> &[ActiveServer] {
        &self.servers
    }

    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    pub fn server_for(&self, path: &Path) -> Option<&ActiveServer> {
        let ext = file_extension(path)?;
        self.by_extension
            .get(&ext)
            .and_then(|index| self.servers.get(*index))
    }

    /// Extensions served by `server` (as chosen, which may be fewer than it declares).
    pub fn extensions_of(&self, server: &str) -> Vec<String> {
        self.by_extension
            .iter()
            .filter(|(_, index)| {
                self.servers
                    .get(**index)
                    .is_some_and(|s| s.spec.name == server)
            })
            .map(|(ext, _)| ext.clone())
            .collect()
    }

    /// One line per server: name, extensions and source, for the tool description and error messages.
    pub fn describe(&self) -> Vec<String> {
        self.servers
            .iter()
            .map(|server| {
                let extensions = self.extensions_of(&server.spec.name).join(", ");
                let source = server
                    .spec
                    .source
                    .as_deref()
                    .map(|source| format!(" — {source}"))
                    .unwrap_or_default();
                let missing = if server.program.is_none() {
                    " (command not found)"
                } else {
                    ""
                };
                format!("{} ({extensions}){source}{missing}", server.spec.name)
            })
            .collect()
    }
}

/// An absolute or relative path that exists, or a program name found on `path_env`.
pub fn resolve_command(command: &str, path_env: Option<OsString>, cwd: &Path) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if candidate.is_absolute() || command.contains('/') || command.contains('\\') {
        let full = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.join(candidate)
        };
        return full.is_file().then_some(full);
    }
    which::which_in(command, path_env, cwd).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn spec(name: &str, command: &str, exts: &[&str], priority: i64, optional: bool) -> ServerSpec {
        ServerSpec {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            extensions: exts
                .iter()
                .map(|ext| (ServerSpec::normalized_extension(ext), "lang".to_string()))
                .collect(),
            env: BTreeMap::new(),
            initialization_options: None,
            settings: None,
            priority,
            optional,
            source: None,
            ..ServerSpec::default()
        }
    }

    fn fake_program(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn normalizes_extensions() {
        assert_eq!(ServerSpec::normalized_extension("RS"), ".rs");
        assert_eq!(ServerSpec::normalized_extension(".Ts"), ".ts");
        assert_eq!(
            file_extension(Path::new("/a/B.PY")),
            Some(".py".to_string())
        );
        assert_eq!(file_extension(Path::new("/a/Makefile")), None);
    }

    #[test]
    fn optional_servers_without_a_command_are_skipped_and_priority_picks_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let installed = fake_program(dir.path(), "installed-ls");
        let specs = vec![
            spec("default-rust", "missing-ls-xyz", &[".rs"], 20, true),
            spec(
                "vscode-rust",
                installed.to_str().unwrap(),
                &[".rs", ".toml"],
                50,
                false,
            ),
            spec(
                "user-rust",
                "installed-ls",
                &[".rs"],
                DEFAULT_PRIORITY,
                false,
            ),
            spec("broken", "missing-ls-xyz", &[".go"], 10, false),
        ];
        let table = ServerTable::new(&specs, Some(dir.path().as_os_str().to_owned()), dir.path());
        let pick = |file: &str| {
            table
                .server_for(Path::new(file))
                .map(|s| s.spec.name.clone())
        };
        assert_eq!(pick("/w/main.rs"), Some("user-rust".to_string()));
        assert_eq!(pick("/w/Cargo.TOML"), Some("vscode-rust".to_string()));
        // A required server keeps its extensions even without its command, so using it reports the problem.
        assert_eq!(pick("/w/x.go"), Some("broken".to_string()));
        assert!(
            table
                .server_for(Path::new("/w/x.go"))
                .unwrap()
                .program
                .is_none()
        );
        assert_eq!(pick("/w/x.py"), None);
        assert_eq!(table.skipped(), ["default-rust".to_string()]);
        assert_eq!(
            table.extensions_of("vscode-rust"),
            vec![".toml".to_string()]
        );
    }
}
