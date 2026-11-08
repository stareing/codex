//! The flows behind `install`, `equip`, `unequip`, `remove` and `list`: the site is asked first
//! for what to install, then this computer's config, skill directory and ledger are written.
//! Secret values are asked for here and written only to the local config; they are never sent to
//! the site.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_config::types::McpServerConfig;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::load_global_mcp_servers;
use serde::Serialize;

use super::client::CatalogClient;
use super::client::CatalogError;
use super::client::SiteInstalled;
use super::env_file;
use super::ledger::Ledger;
use super::ledger::LedgerEntry;
use super::ledger::Lookup;
use super::spec::InstallSpec;
use super::spec::Kind;
use super::spec::McpSpec;
use super::spec::Transport;
use super::spec::acceptable_url;
use super::spec::parse_ref;
use super::spec::substitute;
use super::spec::valid_alias;
use super::spec::valid_env_name;
use super::unpack;

/// Questions the install flow may need answered; the terminal implementation lives in `mod.rs`,
/// tests script the answers.
pub(crate) trait Prompter {
    /// Whether a person can answer (stdin and stderr are terminals).
    fn interactive(&self) -> bool;
    fn confirm(&mut self, question: &str) -> Result<bool, CatalogError>;
    /// A secret value, read without echo; `None` when nothing was entered.
    fn secret(&mut self, name: &str) -> Result<Option<String>, CatalogError>;
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InstallOptions {
    pub(crate) yes: bool,
    pub(crate) alias: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct InstallOutcome {
    pub(crate) entry: LedgerEntry,
}

/// The question asked before installing an entry the site has not reviewed.
pub(crate) const UNREVIEWED_QUESTION: &str = "This entry is NOT reviewed by the site and will run or connect exactly as shown above. Install it anyway?";
const REVIEWED_QUESTION: &str = "Install it?";

fn local(code: &str, detail: impl Into<String>) -> CatalogError {
    CatalogError::local(code, detail)
}

fn io_failure(context: &str, err: impl std::fmt::Display) -> CatalogError {
    local("local_error", format!("{context}: {err}"))
}

pub(crate) fn skills_root(codex_home: &Path) -> PathBuf {
    codex_home.join(unpack::SKILLS_DIR)
}

/// One argument as a shell would need it typed: plain words as they are, anything else in single
/// quotes, so the command shown is exactly the command run.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The full command line of a local server.
pub(crate) fn command_line(mcp: &McpSpec) -> String {
    std::iter::once(mcp.command.as_deref().unwrap_or_default())
        .chain(mcp.args.iter().map(String::as_str))
        .map(shell_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The lines shown before asking to install: what it is, where it comes from, exactly what runs
/// or is connected to, and the names (never the values) of everything asked for.
pub(crate) fn summary_lines(spec: &InstallSpec, local_name: &str) -> Vec<String> {
    let reviewed = spec.reviewed();
    let trust = if reviewed {
        "reviewed by the site"
    } else {
        "NOT reviewed by the site (third-party listing)"
    };
    let mut lines = vec![
        format!("{} {} ({})", spec.name, spec.version, spec.reference),
        format!("  trust:   {trust}"),
    ];
    match (&spec.source_url, reviewed) {
        (Some(source), true) => lines.push(format!("  page:    {source}")),
        (Some(source), false) => lines.push(format!("  source:  {source}")),
        (None, false) => lines.push("  source:  (none given)".to_string()),
        (None, true) => {}
    }
    match (&spec.mcp, &spec.bundle) {
        (Some(mcp), _) => {
            lines.push(format!("  installs as MCP server `{local_name}`"));
            match mcp.transport {
                Transport::Stdio => {
                    lines.push(format!("  runs:    {}", command_line(mcp)));
                    if !mcp.env.is_empty() {
                        let names: Vec<&str> = mcp.env.keys().map(String::as_str).collect();
                        lines.push(format!("  env:     {}", names.join(", ")));
                    }
                }
                Transport::Http => {
                    lines.push(format!(
                        "  address: {}",
                        mcp.url.clone().unwrap_or_default()
                    ));
                    let headers: Vec<String> = mcp
                        .headers
                        .keys()
                        .cloned()
                        .chain(
                            mcp.secret_headers
                                .iter()
                                .map(|(header, variable)| format!("{header} (from ${variable})")),
                        )
                        .collect();
                    if !headers.is_empty() {
                        lines.push(format!("  headers: {}", headers.join(", ")));
                    }
                }
            }
            let secrets = secret_names(mcp);
            if !secrets.is_empty() {
                lines.push(format!(
                    "  asks for: {} (kept on this computer only)",
                    secrets.join(", ")
                ));
            }
        }
        (None, Some(bundle)) => {
            lines.push(format!(
                "  installs as skill in skills/{local_name}/ ({} bytes, sha256 {})",
                bundle.size, bundle.sha256
            ));
        }
        (None, None) => {
            let size = spec
                .skill
                .as_ref()
                .and_then(|skill| skill.skill_md.as_ref())
                .map_or(0, String::len);
            lines.push(format!(
                "  installs as skill in skills/{local_name}/ (SKILL.md, {size} bytes)"
            ));
        }
    }
    lines
}

/// Names of the secrets an MCP server needs: the values asked for here, then the variables HTTP
/// headers read from the environment.
fn secret_names(mcp: &McpSpec) -> Vec<String> {
    mcp.secret_env
        .iter()
        .chain(mcp.secret_headers.values())
        .cloned()
        .collect()
}

/// Secret values for a stdio server: `--env` first, then the terminal; anything still missing
/// fails with the list of names. `--env` keys the server does not ask for are refused.
fn collect_secrets(
    mcp: &McpSpec,
    given: &BTreeMap<String, String>,
    prompter: &mut dyn Prompter,
) -> Result<BTreeMap<String, String>, CatalogError> {
    if let Some(name) = given.keys().find(|name| !valid_env_name(name)) {
        return Err(local(
            "invalid_env",
            format!("`{name}` is not a valid environment variable name"),
        ));
    }
    let wanted: Vec<&String> = mcp
        .secret_env
        .iter()
        .chain(mcp.env.keys())
        .chain(mcp.secret_headers.values())
        .collect();
    if let Some(name) = given.keys().find(|name| !wanted.contains(name)) {
        return Err(local(
            "invalid_env",
            format!("this server does not use `{name}`"),
        ));
    }
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for name in &mcp.secret_env {
        let value = match given.get(name) {
            Some(value) => Some(value.clone()),
            None if prompter.interactive() => prompter.secret(name)?,
            None => None,
        };
        match value.filter(|value| !value.is_empty()) {
            Some(value) => {
                values.insert(name.clone(), value);
            }
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(local(
            "missing_secrets",
            format!(
                "values are needed for {}; pass --env NAME=VALUE for each",
                missing.join(", ")
            ),
        ));
    }
    // Plain `--env` overrides of the server's own variables.
    for (name, value) in given {
        if mcp.env.contains_key(name) {
            values.insert(name.clone(), value.clone());
        }
    }
    Ok(values)
}

/// Values for the variables a remote server's secret headers read (`secret_headers`): `--env`
/// first; a variable already set in the environment (`set`, e.g. from `$CODEX_HOME/.env` of an
/// earlier install) is left as it is; otherwise the terminal is asked for the whole header value
/// (e.g. `Bearer …`). Anything still missing fails with the list of names.
pub(crate) fn collect_header_values(
    mcp: &McpSpec,
    given: &BTreeMap<String, String>,
    prompter: &mut dyn Prompter,
    set: &dyn Fn(&str) -> bool,
) -> Result<BTreeMap<String, String>, CatalogError> {
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for (header, name) in &mcp.secret_headers {
        let value = match given.get(name) {
            Some(value) => Some(value.clone()),
            None if set(name) => continue,
            None if prompter.interactive() => {
                prompter.secret(&format!("{name} ({header} header)"))?
            }
            None => None,
        };
        match value.filter(|value| !value.is_empty()) {
            Some(value) if env_file::quoted(&value).is_err() => {
                return Err(local(
                    "invalid_env",
                    format!("the value for {name} cannot be a header value (line break)"),
                ));
            }
            Some(value) => {
                values.insert(name.clone(), value);
            }
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(local(
            "missing_secrets",
            format!(
                "values are needed for {}; pass --env NAME=VALUE for each",
                missing.join(", ")
            ),
        ));
    }
    Ok(values)
}

/// The `[mcp_servers.<alias>]` entry, built through the config's own parser so it has exactly the
/// shape (and defaults) the agent reads.
pub(crate) fn mcp_server_config(
    mcp: &McpSpec,
    secrets: &BTreeMap<String, String>,
    enabled: bool,
) -> Result<McpServerConfig, CatalogError> {
    let raw = match mcp.transport {
        Transport::Stdio => {
            // A secret is both set in the environment (an env value that was `${KEY}`) and
            // substituted where `${KEY}` appears in the arguments: the spec does not say which
            // one a name came from, and the agent does not expand placeholders itself.
            let mut env = mcp.env.clone();
            env.extend(secrets.clone());
            let args: Vec<String> = mcp
                .args
                .iter()
                .map(|arg| substitute(arg, secrets))
                .collect();
            let mut raw = serde_json::json!({
                "command": mcp.command,
                "args": args,
                "enabled": enabled,
            });
            if !env.is_empty() {
                raw["env"] = serde_json::json!(env);
            }
            raw
        }
        Transport::Http => {
            let url = substitute(mcp.url.as_deref().unwrap_or_default(), secrets);
            if !acceptable_url(&url) {
                return Err(local(
                    "invalid_env",
                    "the server address is not HTTPS once the secrets are filled in",
                ));
            }
            let mut raw = serde_json::json!({ "url": url, "enabled": enabled });
            if !mcp.headers.is_empty() {
                raw["http_headers"] = serde_json::json!(mcp.headers);
            }
            if !mcp.secret_headers.is_empty() {
                raw["env_http_headers"] = serde_json::json!(mcp.secret_headers);
            }
            raw
        }
    };
    serde_json::from_value(raw).map_err(|err| io_failure("MCP server entry", err))
}

async fn load_servers(
    codex_home: &Path,
) -> Result<BTreeMap<String, McpServerConfig>, CatalogError> {
    load_global_mcp_servers(codex_home)
        .await
        .map_err(|err| io_failure("could not read the MCP servers in config.toml", err))
}

async fn save_servers(
    codex_home: &Path,
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<(), CatalogError> {
    ConfigEditsBuilder::new(codex_home)
        .replace_mcp_servers(servers)
        .apply()
        .await
        .map_err(|err| io_failure("could not write config.toml", format!("{err:#}")))
}

async fn set_skill_enabled(
    codex_home: &Path,
    skill_name: &str,
    enabled: bool,
) -> Result<(), CatalogError> {
    ConfigEditsBuilder::new(codex_home)
        .with_edits([ConfigEdit::SetSkillConfigByName {
            name: skill_name.to_string(),
            enabled,
        }])
        .apply()
        .await
        .map_err(|err| io_failure("could not write config.toml", format!("{err:#}")))
}

pub(crate) fn load_ledger(codex_home: &Path) -> Result<Ledger, CatalogError> {
    Ledger::load(codex_home).map_err(|err| io_failure("could not read the install list", err))
}

pub(crate) fn save_ledger(codex_home: &Path, ledger: &Ledger) -> Result<(), CatalogError> {
    ledger
        .save(codex_home)
        .map_err(|err| io_failure("could not write the install list", err))
}

/// The local name an item gets, and a refusal when something not installed from this reference
/// already uses it.
async fn local_name_for(
    codex_home: &Path,
    ledger: &Ledger,
    spec: &InstallSpec,
    alias: Option<&str>,
) -> Result<String, CatalogError> {
    let reference = spec.reference.as_str();
    let owned_by_us = |name: &str| {
        ledger
            .owner_of(spec.kind, name)
            .map(|e| e.reference.as_str())
            == Some(reference)
    };
    match spec.kind {
        Kind::Mcp => {
            let name = alias.unwrap_or(&spec.slug).to_string();
            if !valid_alias(&name) {
                return Err(local(
                    "invalid_name",
                    format!("`{name}` is not a usable server name (letters, digits, '-', '_')"),
                ));
            }
            let servers = load_servers(codex_home).await?;
            if servers.contains_key(&name) && !owned_by_us(&name) {
                return Err(local(
                    "name_taken",
                    format!(
                        "an MCP server named `{name}` is already configured; choose another with --name"
                    ),
                ));
            }
            Ok(name)
        }
        Kind::Skill => {
            if alias.is_some() {
                return Err(local("invalid_name", "--name applies to MCP servers only"));
            }
            let name = spec.slug.clone();
            let exists = std::fs::symlink_metadata(skills_root(codex_home).join(&name)).is_ok();
            if exists && !owned_by_us(&name) {
                return Err(local(
                    "name_taken",
                    format!("skills/{name}/ already exists and was not installed from the site"),
                ));
            }
            Ok(name)
        }
        Kind::Cli => Err(not_installable(spec)),
    }
}

/// A command-line tool: never installed here, only its official instructions are pointed to.
fn not_installable(spec: &InstallSpec) -> CatalogError {
    let url = spec
        .install_instructions
        .as_ref()
        .and_then(|instructions| instructions.url.clone());
    let detail = match &url {
        Some(url) => format!(
            "{} is a command-line tool; My Agent does not install those. Official instructions: {url}",
            spec.name
        ),
        None => format!(
            "{} is a command-line tool; My Agent does not install those, and the site has no instructions link for it.",
            spec.name
        ),
    };
    let mut err = local("not_installable", detail);
    err.instructions_url = url;
    err
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Writes the item on this computer; returns the skill name for skills.
async fn apply_locally(
    codex_home: &Path,
    client: &CatalogClient,
    answer: &super::client::InstallAnswer,
    local_name: &str,
    secrets: &BTreeMap<String, String>,
    header_values: &BTreeMap<String, String>,
) -> Result<Option<String>, CatalogError> {
    let spec = &answer.spec;
    match (&spec.mcp, &spec.bundle) {
        (Some(mcp), _) => {
            let entry = mcp_server_config(mcp, secrets, true)?;
            // Before config.toml: a server entry is never left pointing at values not written.
            env_file::set(codex_home, header_values)
                .map_err(|err| io_failure("could not write $CODEX_HOME/.env", err))?;
            let mut servers = load_servers(codex_home).await?;
            servers.insert(local_name.to_string(), entry);
            save_servers(codex_home, &servers).await?;
            Ok(None)
        }
        (None, None) => {
            let skill_md = spec
                .skill
                .as_ref()
                .and_then(|skill| skill.skill_md.as_deref())
                .ok_or_else(|| local("unexpected_response", "nothing to install"))?;
            let skill_name = unpack::install_skill_text(
                skill_md,
                codex_home,
                &skills_root(codex_home),
                local_name,
            )
            .map_err(|err| local("bad_bundle", err.to_string()))?;
            set_skill_enabled(codex_home, &skill_name, true).await?;
            Ok(Some(skill_name))
        }
        (None, Some(bundle)) => {
            let link = answer.download_url().ok_or_else(|| {
                local(
                    "bad_download",
                    "the site did not send a download link for the skill",
                )
            })?;
            let bytes = client.download(link, bundle).await?;
            let skill_name = unpack::install_skill_bundle(
                &bytes,
                &bundle.sha256,
                bundle.size,
                codex_home,
                &skills_root(codex_home),
                local_name,
            )
            .map_err(|err| local("bad_bundle", err.to_string()))?;
            set_skill_enabled(codex_home, &skill_name, true).await?;
            Ok(Some(skill_name))
        }
    }
}

/// resolve → summary → confirm → secrets → `install/` → local files → ledger. When the local step
/// fails after the site recorded the install, the site record is withdrawn again (best effort).
///
/// A command-line tool (`kind: cli`) stops after resolve with `not_installable` and its
/// instructions link; `install/` is never called for it.
pub(crate) async fn install(
    codex_home: &Path,
    client: &CatalogClient,
    prompter: &mut dyn Prompter,
    raw_ref: &str,
    options: &InstallOptions,
    show: &mut dyn FnMut(&str),
) -> Result<InstallOutcome, CatalogError> {
    let reference = parse_ref(raw_ref).map_err(|detail| local("invalid_ref", detail))?;
    let spec = client.resolve(&reference.to_string()).await?;
    spec.validate()
        .map_err(|detail| local("unexpected_response", detail))?;
    if !spec.answers(&reference) {
        return Err(local(
            "unexpected_response",
            format!(
                "asked for {reference} but the site described {}",
                spec.reference
            ),
        ));
    }
    if spec.kind == Kind::Cli {
        return Err(not_installable(&spec));
    }
    let ledger = load_ledger(codex_home)?;
    let local_name = local_name_for(codex_home, &ledger, &spec, options.alias.as_deref()).await?;
    for line in summary_lines(&spec, &local_name) {
        show(&line);
    }
    confirm(&spec, options, prompter)?;
    let (secrets, header_values) = match &spec.mcp {
        Some(mcp) => (
            collect_secrets(mcp, &options.env, prompter)?,
            collect_header_values(mcp, &options.env, prompter, &|name| {
                std::env::var_os(name).is_some()
            })?,
        ),
        None if !options.env.is_empty() => {
            return Err(local("invalid_env", "--env applies to MCP servers only"));
        }
        None => (BTreeMap::new(), BTreeMap::new()),
    };

    let answer = client.install(&spec.reference).await?;
    answer
        .spec
        .validate()
        .map_err(|detail| local("unexpected_response", detail))?;
    if answer.spec.reference != spec.reference
        || answer.spec.mcp != spec.mcp
        || answer.spec.trust != spec.trust
    {
        // What was confirmed is what gets installed; anything else is withdrawn.
        let _ = client.uninstall(&spec.reference).await;
        return Err(local(
            "spec_changed",
            "the item changed on the site while installing; run install again",
        ));
    }
    let skill_name = match apply_locally(
        codex_home,
        client,
        &answer,
        &local_name,
        &secrets,
        &header_values,
    )
    .await
    {
        Ok(skill_name) => skill_name,
        Err(err) => {
            let _ = client.uninstall(&spec.reference).await;
            return Err(err);
        }
    };

    let installed = &answer.spec;
    let entry = LedgerEntry {
        reference: installed.reference.clone(),
        kind: installed.kind,
        slug: installed.slug.clone(),
        name: local_name,
        title: installed.name.clone(),
        version: answer
            .installed_version
            .clone()
            .unwrap_or_else(|| installed.version.clone()),
        sha256: installed
            .bundle
            .as_ref()
            .map(|bundle| bundle.sha256.clone())
            .or_else(|| {
                installed
                    .skill
                    .as_ref()
                    .and_then(|skill| skill.skill_md.as_deref())
                    .map(|text| unpack::sha256_hex(text.as_bytes()))
            }),
        skill_name,
        installed_at: now_rfc3339(),
        enabled: true,
        trust: installed.trust.clone(),
        env_file: header_values.keys().cloned().collect(),
        withdrawn: None,
    };
    let mut ledger = load_ledger(codex_home)?;
    ledger.upsert(entry.clone());
    save_ledger(codex_home, &ledger)?;
    Ok(InstallOutcome { entry })
}

/// Asks before installing (`--yes` skips the question); an entry the site has not reviewed gets a
/// question saying so. Without a terminal and without `--yes` nothing is installed.
fn confirm(
    spec: &InstallSpec,
    options: &InstallOptions,
    prompter: &mut dyn Prompter,
) -> Result<(), CatalogError> {
    if options.yes {
        return Ok(());
    }
    if !prompter.interactive() {
        return Err(local(
            "confirmation_required",
            "no terminal to confirm on; pass --yes to install without asking",
        ));
    }
    let question = if spec.reviewed() {
        REVIEWED_QUESTION
    } else {
        UNREVIEWED_QUESTION
    };
    if prompter.confirm(question)? {
        Ok(())
    } else {
        Err(local("cancelled", "nothing was installed"))
    }
}

fn find_entry(ledger: &Ledger, selector: &str) -> Result<LedgerEntry, CatalogError> {
    let selector = selector.trim();
    match ledger.lookup(selector) {
        Lookup::Found(entry) => Ok(entry.clone()),
        Lookup::Missing => Err(local(
            "not_installed",
            format!("`{selector}` is not installed on this computer; see `onigio-agent list`"),
        )),
        Lookup::Ambiguous(entries) => {
            let mut err = local(
                "ambiguous_ref",
                format!("`{selector}` matches more than one installed item"),
            );
            err.candidates = entries.iter().map(|e| e.reference.clone()).collect();
            Err(err)
        }
    }
}

/// Command-line tools are never installed, so a ledger row for one was not written by this CLI.
fn cli_in_ledger(entry: &LedgerEntry) -> CatalogError {
    local(
        "not_installable",
        format!(
            "`{}` is a command-line tool; there is nothing to turn on or off",
            entry.name
        ),
    )
}

/// The result of a local change that the site is told about afterwards.
#[derive(Debug)]
pub(crate) struct LocalChange {
    pub(crate) entry: LedgerEntry,
    /// Set when the site could not be updated (the local change stands).
    pub(crate) site_warning: Option<CatalogError>,
}

/// Turns `entry` on or off in config.toml (the ledger is the caller's).
pub(crate) async fn apply_enabled(
    codex_home: &Path,
    entry: &LedgerEntry,
    enabled: bool,
) -> Result<(), CatalogError> {
    match entry.kind {
        Kind::Mcp => {
            let mut servers = load_servers(codex_home).await?;
            let server = servers.get_mut(&entry.name).ok_or_else(|| {
                local(
                    "not_installed",
                    format!(
                        "MCP server `{}` is no longer in config.toml; install it again",
                        entry.name
                    ),
                )
            })?;
            server.enabled = enabled;
            save_servers(codex_home, &servers).await?;
        }
        Kind::Skill => {
            let skill_name = entry
                .skill_name
                .clone()
                .unwrap_or_else(|| entry.slug.clone());
            set_skill_enabled(codex_home, &skill_name, enabled).await?;
        }
        Kind::Cli => return Err(cli_in_ledger(entry)),
    }
    Ok(())
}

/// `equip` / `unequip`: flips `enabled` locally, then tells the site. An item the site withdrew
/// cannot be turned on (`sync-installed` does that once the site offers it again).
pub(crate) async fn set_enabled(
    codex_home: &Path,
    client: &CatalogClient,
    selector: &str,
    enabled: bool,
) -> Result<LocalChange, CatalogError> {
    let mut ledger = load_ledger(codex_home)?;
    let mut entry = find_entry(&ledger, selector)?;
    if let (true, Some(withdrawal)) = (enabled, &entry.withdrawn) {
        return Err(local(
            "withdrawn",
            format!(
                "the site withdrew `{}` ({}); it stays off until the site offers it again \
                 (`onigio-agent sync-installed`), or remove it",
                entry.name, withdrawal.reason
            ),
        ));
    }
    apply_enabled(codex_home, &entry, enabled).await?;
    ledger.set_enabled(&entry.reference, enabled);
    save_ledger(codex_home, &ledger)?;
    entry.enabled = enabled;
    let site_warning = client.set_enabled(&entry.reference, enabled).await.err();
    Ok(LocalChange {
        entry,
        site_warning,
    })
}

/// `remove`: deletes the config entry or skill directory and the ledger row, then tells the site.
pub(crate) async fn remove(
    codex_home: &Path,
    client: &CatalogClient,
    selector: &str,
) -> Result<LocalChange, CatalogError> {
    let mut ledger = load_ledger(codex_home)?;
    let entry = find_entry(&ledger, selector)?;
    match entry.kind {
        Kind::Mcp => {
            let mut servers = load_servers(codex_home).await?;
            if servers.remove(&entry.name).is_some() {
                save_servers(codex_home, &servers).await?;
            }
        }
        Kind::Skill => {
            unpack::remove_skill_dir(&skills_root(codex_home).join(&entry.name))
                .map_err(|err| io_failure("could not delete the skill", err))?;
            let skill_name = entry
                .skill_name
                .clone()
                .unwrap_or_else(|| entry.slug.clone());
            // `enabled = true` drops any `[[skills.config]]` override for the name.
            set_skill_enabled(codex_home, &skill_name, true).await?;
        }
        Kind::Cli => {}
    }
    env_file::unset(codex_home, &entry.env_file)
        .map_err(|err| io_failure("could not update $CODEX_HOME/.env", err))?;
    ledger.remove(&entry.reference);
    save_ledger(codex_home, &ledger)?;
    let site_warning = match client.uninstall(&entry.reference).await {
        Ok(()) => None,
        Err(err) if err.status == Some(404) => None,
        Err(err) => Some(err),
    };
    Ok(LocalChange {
        entry,
        site_warning,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ListedItem {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) title: String,
    /// Installed on this computer.
    pub(crate) local: bool,
    pub(crate) version: Option<String>,
    pub(crate) enabled: Option<bool>,
    pub(crate) latest_version: Option<String>,
    pub(crate) update_available: bool,
    /// Recorded on the site for this account.
    pub(crate) on_site: bool,
    /// Not reviewed by the site: reinstalling (an update included) asks again.
    pub(crate) reconfirm_on_update: bool,
    /// Why the site withdrew it (recorded by `sync-installed`, else as the site says now).
    pub(crate) withdrawn_reason: Option<String>,
}

/// The ledger, merged with the site's `installed/` when it could be read (`site = None` when
/// offline).
pub(crate) fn merge_listing(ledger: &Ledger, site: Option<&[SiteInstalled]>) -> Vec<ListedItem> {
    let site_rows: HashMap<&str, &SiteInstalled> = site
        .unwrap_or_default()
        .iter()
        .map(|row| (row.reference.as_str(), row))
        .collect();
    let mut items: Vec<ListedItem> = ledger
        .items
        .iter()
        .map(|entry| {
            let remote = site_rows.get(entry.reference.as_str());
            ListedItem {
                reference: entry.reference.clone(),
                kind: entry.kind.to_string(),
                name: entry.name.clone(),
                title: entry.title.clone(),
                local: true,
                version: Some(entry.version.clone()),
                enabled: Some(entry.enabled),
                latest_version: remote.and_then(|row| row.latest_version.clone()),
                update_available: remote.is_some_and(|row| row.update_available),
                on_site: remote.is_some(),
                reconfirm_on_update: entry.reconfirm_on_update(),
                withdrawn_reason: entry
                    .withdrawn
                    .as_ref()
                    .map(|w| w.reason.clone())
                    .or_else(|| remote.and_then(|row| row.withdrawn_reason())),
            }
        })
        .collect();
    for row in site.unwrap_or_default() {
        if ledger.by_ref(&row.reference).is_none() {
            items.push(ListedItem {
                reference: row.reference.clone(),
                kind: row.kind.clone(),
                name: row.slug.clone(),
                title: row.name.clone(),
                local: false,
                version: row.installed_version.clone(),
                enabled: None,
                latest_version: row.latest_version.clone(),
                update_available: row.update_available,
                on_site: true,
                reconfirm_on_update: !row.reference.starts_with("store:"),
                withdrawn_reason: row.withdrawn_reason(),
            });
        }
    }
    items
}

pub(crate) async fn list(
    codex_home: &Path,
    client: Option<&CatalogClient>,
) -> Result<(Vec<ListedItem>, Option<CatalogError>), CatalogError> {
    let ledger = load_ledger(codex_home)?;
    let (site, warning) = match client {
        Some(client) => match client.installed().await {
            Ok(rows) => (Some(rows), None),
            Err(err) => (None, Some(err)),
        },
        None => (None, None),
    };
    Ok((merge_listing(&ledger, site.as_deref()), warning))
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
