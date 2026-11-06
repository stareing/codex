//! `$CODEX_HOME/.env`: where the values of a remote MCP server's secret headers are kept. The
//! agent loads this file into its environment at start-up (`codex-arg0` `load_dotenv`), and
//! `env_http_headers` in config.toml reads each header from there.
//!
//! Lines this CLI did not write are kept as they are; a variable is replaced in place or added at
//! the end. Written atomically, readable by the owner only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

pub(crate) const ENV_FILE: &str = ".env";
const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;

pub(crate) fn env_file_path(codex_home: &Path) -> PathBuf {
    codex_home.join(ENV_FILE)
}

/// The variable a line sets (`NAME=…` or `export NAME=…`), if any.
fn assigned_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (name, _) = line.split_once('=')?;
    let name = name.trim();
    (!name.is_empty() && !name.starts_with('#')).then_some(name)
}

/// `value` as the dotenv parser reads it back unchanged: single quotes (no expansion, no escapes)
/// unless the value has one, then double quotes with `\`, `"` and `$` escaped.
///
/// @throws when the value has a line break or another control character (a header value never
/// does, and it could not be written on one line).
pub(crate) fn quoted(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("the value has a control character or a line break".to_string());
    }
    if !value.contains('\'') {
        return Ok(format!("'{value}'"));
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

fn read(codex_home: &Path) -> std::io::Result<String> {
    let file = match std::fs::File::open(env_file_path(codex_home)) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(err),
    };
    let mut raw = String::new();
    file.take(MAX_ENV_FILE_BYTES).read_to_string(&mut raw)?;
    Ok(raw)
}

fn write(codex_home: &Path, lines: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let mut tmp = tempfile::NamedTempFile::new_in(codex_home)?;
    std::io::Write::write_all(&mut tmp, text.as_bytes())?;
    tmp.as_file().sync_all()?;
    // NamedTempFile is created owner-only (0600 on Unix); the rename keeps that.
    tmp.persist(env_file_path(codex_home))
        .map_err(|err| err.error)?;
    Ok(())
}

/// Sets each variable of `values` (replacing an existing line for it, else appending one).
pub(crate) fn set(codex_home: &Path, values: &BTreeMap<String, String>) -> Result<(), String> {
    if values.is_empty() {
        return Ok(());
    }
    let mut pending: BTreeMap<&str, String> = BTreeMap::new();
    for (name, value) in values {
        let line = quoted(value).map_err(|err| format!("{name}: {err}"))?;
        pending.insert(name.as_str(), format!("{name}={line}"));
    }
    let raw = read(codex_home).map_err(|err| err.to_string())?;
    let mut lines: Vec<String> = raw
        .lines()
        .map(
            |line| match assigned_name(line).and_then(|n| pending.remove(n)) {
                Some(replacement) => replacement,
                None => line.to_string(),
            },
        )
        .collect();
    lines.extend(pending.into_values());
    write(codex_home, &lines).map_err(|err| err.to_string())
}

/// Deletes the lines setting any of `names` (a missing file or name is not an error).
pub(crate) fn unset(codex_home: &Path, names: &[String]) -> Result<(), String> {
    if names.is_empty() || !env_file_path(codex_home).exists() {
        return Ok(());
    }
    let raw = read(codex_home).map_err(|err| err.to_string())?;
    let lines: Vec<String> = raw
        .lines()
        .filter(|line| assigned_name(line).is_none_or(|name| !names.iter().any(|n| n == name)))
        .map(str::to_string)
        .collect();
    write(codex_home, &lines).map_err(|err| err.to_string())
}

#[cfg(test)]
#[path = "env_file_tests.rs"]
mod tests;
