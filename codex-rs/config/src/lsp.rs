//! `[lsp]`: language servers for the code-navigation tool and passive diagnostics. Off unless `enabled = true`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LspConfigToml {
    /// Turns on the `lsp` tool and diagnostics after edits. Defaults to false.
    pub enabled: Option<bool>,
    /// Time one request may take, in milliseconds (default 30000).
    pub request_timeout_ms: Option<u64>,
    /// Time from starting a server until it is initialized, in milliseconds (default 60000).
    pub startup_timeout_ms: Option<u64>,
    /// A server unused for this many seconds is stopped (default 600).
    pub idle_shutdown_secs: Option<u64>,
    /// Language servers running at once in this agent process (default 3).
    pub max_servers: Option<usize>,
    /// After an edit, how long the next model step waits for its diagnostics, in milliseconds (default 1500).
    pub diagnostics_wait_ms: Option<u64>,
    /// Diagnostics shown per file (default 10).
    pub max_diagnostics_per_file: Option<usize>,
    /// Diagnostics shown per delivery (default 30).
    pub max_diagnostics_total: Option<usize>,
    /// Servers by name.
    #[serde(default)]
    pub servers: BTreeMap<String, LspServerConfigToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LspServerConfigToml {
    /// Program to start (a name on PATH or a path). It must speak LSP over stdio.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extension → LSP language id, for example `".rs" = "rust"`.
    #[serde(default)]
    pub extensions: BTreeMap<String, String>,
    /// Extra environment for the server process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Sent as `initializationOptions` in `initialize`.
    pub initialization_options: Option<JsonValue>,
    /// Answers to the server's `workspace/configuration` requests, by section.
    pub settings: Option<JsonValue>,
    /// When several servers serve one extension the highest priority wins (default 100).
    pub priority: Option<i64>,
    /// Ignore this server when its command is not installed (instead of reporting an error on use).
    #[serde(default)]
    pub optional: bool,
    /// Where the entry came from, shown to the model.
    pub source: Option<String>,
    /// Overrides `lsp.startup_timeout_ms` for this server.
    pub startup_timeout_ms: Option<u64>,
    /// A fixed folder used as the server's working directory and its only workspace root (by default each file's
    /// project root, found from the nearest version-control marker).
    pub workspace_folder: Option<String>,
    /// After this many failed starts or crashes the server is not started again in this session (default 3).
    pub max_restarts: Option<u32>,
}
