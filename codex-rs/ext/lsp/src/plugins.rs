//! Language servers declared by installed plugins, read the way Claude Code reads them (its plugin `lspServers`):
//!
//! * `.lsp.json` in the plugin root, then the manifest's `lspServers` — a path to such a file inside the plugin, an
//!   inline `{name: server}` record, or an array of either (later declarations replace earlier ones of the same name);
//! * each server is checked strictly (unknown fields, an empty command, a command with spaces that is not an absolute
//!   path, no extensions, extensions without a leading dot): a bad server is skipped with an error, the rest load;
//! * `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PLUGIN_DATA}` (and this agent's `${PLUGIN_ROOT}` / `${PLUGIN_DATA}`) are
//!   replaced, then `${VAR}` / `${VAR:-default}` from the environment (a missing variable stays as written and is
//!   reported) — in `command`, `args`, `env` values and `workspaceFolder`;
//! * the server is named `plugin:<plugin>:<server>`; its command must be on `PATH` (or an absolute path).
//!
//! `restartOnCrash` and `shutdownTimeout` are refused as Claude Code refuses them (not implemented there either);
//! `transport` is accepted and, as in Claude Code, every server runs over stdio.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_lsp::ServerSpec;
use serde::Deserialize;
use serde_json::Value;

/// Below `[lsp.servers]` entries in the agent config (`codex_lsp::config::DEFAULT_PRIORITY`, 100), so a server the
/// user configured wins over a plugin's for the same extension.
pub const PLUGIN_PRIORITY: i64 = 50;
/// Largest `.lsp.json` / manifest read.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const LSP_JSON: &str = ".lsp.json";
const MANIFEST_KEY: &str = "lspServers";
/// Variables replaced with the plugin's folders: (name, is the data folder).
const PLUGIN_VARIABLES: [(&str, bool); 4] = [
    ("CLAUDE_PLUGIN_ROOT", false),
    ("CLAUDE_PLUGIN_DATA", true),
    ("PLUGIN_ROOT", false),
    ("PLUGIN_DATA", true),
];

/// One installed, enabled plugin.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginDir {
    /// The plugin's name (its manifest name).
    pub name: String,
    pub root: PathBuf,
    /// The plugin's persistent data folder.
    pub data_root: PathBuf,
}

/// What the plugins declared.
#[derive(Debug, Default)]
pub struct PluginServers {
    pub specs: Vec<ServerSpec>,
    /// One line per server or file that could not be used.
    pub errors: Vec<String>,
}

/// Claude Code's `LspServerConfigSchema`, field for field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    extension_to_language: BTreeMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    transport: Option<Transport>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    initialization_options: Option<Value>,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    workspace_folder: Option<String>,
    #[serde(default)]
    startup_timeout: Option<u64>,
    #[serde(default)]
    shutdown_timeout: Option<u64>,
    #[serde(default)]
    restart_on_crash: Option<bool>,
    #[serde(default)]
    max_restarts: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Stdio,
    Socket,
}

/// Reads every plugin's servers. `env` looks up environment variables (the process environment in production).
pub fn load(plugins: &[PluginDir], env: &dyn Fn(&str) -> Option<String>) -> PluginServers {
    let mut out = PluginServers::default();
    for plugin in plugins {
        for (name, raw) in declared(plugin, &mut out.errors) {
            match server_spec(plugin, &name, raw, env) {
                Ok((spec, missing)) => {
                    if !missing.is_empty() {
                        out.errors.push(format!(
                            "plugin {}: server `{name}` uses unset environment variables: {}",
                            plugin.name,
                            missing.join(", ")
                        ));
                    }
                    out.specs.push(spec);
                }
                Err(err) => out
                    .errors
                    .push(format!("plugin {}: server `{name}`: {err}", plugin.name)),
            }
        }
    }
    out
}

/// The servers a plugin declares, by name, before checking (later declarations win).
fn declared(plugin: &PluginDir, errors: &mut Vec<String>) -> BTreeMap<String, Value> {
    let mut servers = BTreeMap::new();
    let lsp_json = plugin.root.join(LSP_JSON);
    if lsp_json.is_file() {
        merge_file(plugin, &lsp_json, LSP_JSON, &mut servers, errors);
    }
    let Some(manifest_path) = codex_utils_plugins::find_plugin_manifest_path(&plugin.root) else {
        return servers;
    };
    let declaration = match read_json(&manifest_path) {
        Ok(manifest) => manifest.get(MANIFEST_KEY).cloned(),
        Err(err) => {
            errors.push(format!(
                "plugin {}: {}: {err}",
                plugin.name,
                manifest_path.display()
            ));
            None
        }
    };
    let items = match declaration {
        None => Vec::new(),
        Some(Value::Array(items)) => items,
        Some(item) => vec![item],
    };
    for item in items {
        match item {
            Value::String(relative) => match within(&plugin.root, &relative) {
                Some(path) => merge_file(plugin, &path, &relative, &mut servers, errors),
                None => errors.push(format!(
                    "plugin {}: `{relative}`: an lspServers path must be relative and inside the plugin folder",
                    plugin.name
                )),
            },
            Value::Object(record) => servers.extend(record),
            other => errors.push(format!(
                "plugin {}: lspServers entries must be a path or an object, not {other}",
                plugin.name
            )),
        }
    }
    servers
}

fn merge_file(
    plugin: &PluginDir,
    path: &Path,
    label: &str,
    servers: &mut BTreeMap<String, Value>,
    errors: &mut Vec<String>,
) {
    match read_json(path) {
        Ok(Value::Object(record)) => servers.extend(record),
        Ok(_) => errors.push(format!(
            "plugin {}: {label}: expected an object of servers",
            plugin.name
        )),
        Err(err) => errors.push(format!("plugin {}: {label}: {err}", plugin.name)),
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| err.to_string())?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!("larger than {MAX_CONFIG_BYTES} bytes"));
    }
    serde_json::from_str(&text).map_err(|err| format!("invalid JSON: {err}"))
}

/// `relative` resolved under `root`, or None when it is absolute or climbs out (checked on the path's text, as Claude
/// Code checks it).
fn within(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut depth = 0usize;
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => {
                depth += 1;
                path.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                depth -= 1;
                path.pop();
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (depth > 0).then_some(path)
}

fn server_spec(
    plugin: &PluginDir,
    name: &str,
    raw: Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(ServerSpec, Vec<String>), String> {
    let raw: RawServer = serde_json::from_value(raw).map_err(|err| err.to_string())?;
    check(&raw)?;
    let mut missing = Vec::new();
    let mut resolve = |value: &str| expand(value, plugin, env, &mut missing);
    let command = resolve(&raw.command);
    let args = raw.args.iter().map(|arg| resolve(arg)).collect();
    let mut server_env = BTreeMap::new();
    for (variable, is_data) in PLUGIN_VARIABLES {
        server_env.insert(variable.to_string(), folder(plugin, is_data));
    }
    for (key, value) in &raw.env {
        // As in Claude Code: a plugin's own value for a reserved variable is kept as written.
        let reserved = PLUGIN_VARIABLES.iter().any(|(variable, _)| variable == key);
        let value = if reserved {
            value.clone()
        } else {
            resolve(value)
        };
        server_env.insert(key.clone(), value);
    }
    let workspace_folder = match raw
        .workspace_folder
        .as_deref()
        .map(|folder| resolve(folder))
    {
        Some(folder) if !Path::new(&folder).is_absolute() => {
            return Err(format!(
                "workspaceFolder must be an absolute path, got `{folder}`"
            ));
        }
        folder => folder.map(PathBuf::from),
    };
    missing.sort();
    missing.dedup();
    let spec = ServerSpec {
        name: format!("plugin:{}:{name}", plugin.name),
        command,
        args,
        extensions: raw
            .extension_to_language
            .into_iter()
            .map(|(ext, language)| (ServerSpec::normalized_extension(&ext), language))
            .collect(),
        env: server_env,
        initialization_options: raw.initialization_options,
        settings: raw.settings,
        priority: PLUGIN_PRIORITY,
        // A plugin whose language server is not installed is listed as such instead of failing every use.
        optional: true,
        source: Some(format!("plugin {}", plugin.name)),
        startup_timeout: raw.startup_timeout.map(Duration::from_millis),
        workspace_folder,
        max_restarts: raw.max_restarts,
    };
    Ok((spec, missing))
}

/// Claude Code's schema rules beyond the field types.
fn check(raw: &RawServer) -> Result<(), String> {
    if raw.command.is_empty() {
        return Err("command must not be empty".to_string());
    }
    if raw.command.contains(' ') && !raw.command.starts_with('/') {
        return Err("command must not contain spaces; put arguments in args".to_string());
    }
    if raw.args.iter().any(String::is_empty) {
        return Err("args must not contain empty strings".to_string());
    }
    if raw.extension_to_language.is_empty() {
        return Err("extensionToLanguage must have at least one mapping".to_string());
    }
    for (ext, language) in &raw.extension_to_language {
        if ext.len() < 2 || !ext.starts_with('.') {
            return Err(format!(
                "file extensions must start with a dot (\".ts\", not \"ts\"), got `{ext}`"
            ));
        }
        if language.is_empty() {
            return Err(format!("the language id for `{ext}` must not be empty"));
        }
    }
    if raw.startup_timeout == Some(0) || raw.shutdown_timeout == Some(0) {
        return Err("timeouts must be positive".to_string());
    }
    if raw.restart_on_crash.is_some() {
        return Err(
            "restartOnCrash is not supported; remove it (crashed servers start again on next use)"
                .to_string(),
        );
    }
    if raw.shutdown_timeout.is_some() {
        return Err("shutdownTimeout is not supported; remove it".to_string());
    }
    Ok(())
}

fn folder(plugin: &PluginDir, data: bool) -> String {
    let path = if data {
        &plugin.data_root
    } else {
        &plugin.root
    };
    let text = path.to_string_lossy().into_owned();
    // Claude Code writes plugin folders with forward slashes on Windows.
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

/// Plugin folders first, then `${VAR}` / `${VAR:-default}`; unknown variables stay as written and are recorded.
fn expand(
    value: &str,
    plugin: &PluginDir,
    env: &dyn Fn(&str) -> Option<String>,
    missing: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let content = &after[..end];
        let (variable, default) = match content.split_once(":-") {
            Some((variable, default)) => (variable, Some(default)),
            None => (content, None),
        };
        let plugin_folder = PLUGIN_VARIABLES
            .iter()
            .find(|(name, _)| *name == content)
            .map(|(_, is_data)| folder(plugin, *is_data));
        match plugin_folder
            .or_else(|| env(variable))
            .or_else(|| default.map(str::to_string))
        {
            Some(replacement) => out.push_str(&replacement),
            None => {
                missing.push(variable.to_string());
                out.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
