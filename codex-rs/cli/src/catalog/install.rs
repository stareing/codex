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
    /// Environment variables an HTTP server reads its header secrets from that are not set now.
    pub(crate) unset_header_env: Vec<String>,
}

fn local(code: &str, detail: impl Into<String>) -> CatalogError {
    CatalogError::local(code, detail)
}

fn io_failure(context: &str, err: impl std::fmt::Display) -> CatalogError {
    local("local_error", format!("{context}: {err}"))
}

pub(crate) fn skills_root(codex_home: &Path) -> PathBuf {
    codex_home.join(unpack::SKILLS_DIR)
}

/// The lines shown before asking to install.
pub(crate) fn summary_lines(spec: &InstallSpec, local_name: &str) -> Vec<String> {
    let trust = if spec.reviewed() {
        "reviewed by the site"
    } else {
        "NOT reviewed by the site"
    };
    let mut lines = vec![
        format!("{} {} ({})", spec.name, spec.version, spec.reference),
        format!("  trust:   {trust}"),
    ];
    if let Some(source) = &spec.source_url {
        lines.push(format!("  page:    {source}"));
    }
    match (&spec.mcp, &spec.bundle) {
        (Some(mcp), _) => {
            lines.push(format!("  installs as MCP server `{local_name}`"));
            match mcp.transport {
                Transport::Stdio => {
                    let command = std::iter::once(mcp.command.clone().unwrap_or_default())
                        .chain(mcp.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    lines.push(format!("  runs:    {command}"));
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
                }
            }
            let secrets = secret_names(mcp);
            if !secrets.is_empty() {
                lines.push(format!(
                    "  secrets: {} (kept on this computer only)",
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
    let wanted: Vec<&String> = mcp.secret_env.iter().chain(mcp.env.keys()).collect();
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

fn load_ledger(codex_home: &Path) -> Result<Ledger, CatalogError> {
    Ledger::load(codex_home).map_err(|err| io_failure("could not read the install list", err))
}

fn save_ledger(codex_home: &Path, ledger: &Ledger) -> Result<(), CatalogError> {
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
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Writes the item on this computer; returns the skill name for skills.
async fn apply_locally(
    codex_home: &Path,
    client: &CatalogClient,
    answer: &super::client::InstallAnswer,
    local_name: &str,
    secrets: &BTreeMap<String, String>,
) -> Result<Option<String>, CatalogError> {
    let spec = &answer.spec;
    match (&spec.mcp, &spec.bundle) {
        (Some(mcp), _) => {
            let entry = mcp_server_config(mcp, secrets, true)?;
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
    let ledger = load_ledger(codex_home)?;
    let local_name = local_name_for(codex_home, &ledger, &spec, options.alias.as_deref()).await?;
    for line in summary_lines(&spec, &local_name) {
        show(&line);
    }
    if !options.yes {
        if !prompter.interactive() {
            return Err(local(
                "confirmation_required",
                "no terminal to confirm on; pass --yes to install without asking",
            ));
        }
        if !prompter.confirm("Install it?")? {
            return Err(local("cancelled", "nothing was installed"));
        }
    }
    let secrets = match &spec.mcp {
        Some(mcp) => collect_secrets(mcp, &options.env, prompter)?,
        None if !options.env.is_empty() => {
            return Err(local("invalid_env", "--env applies to MCP servers only"));
        }
        None => BTreeMap::new(),
    };

    let answer = client.install(&spec.reference).await?;
    answer
        .spec
        .validate()
        .map_err(|detail| local("unexpected_response", detail))?;
    if answer.spec.reference != spec.reference || answer.spec.mcp != spec.mcp {
        // What was confirmed is what gets installed; anything else is withdrawn.
        let _ = client.uninstall(&spec.reference).await;
        return Err(local(
            "spec_changed",
            "the item changed on the site while installing; run install again",
        ));
    }
    let skill_name = match apply_locally(codex_home, client, &answer, &local_name, &secrets).await {
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
    };
    let mut ledger = load_ledger(codex_home)?;
    ledger.upsert(entry.clone());
    save_ledger(codex_home, &ledger)?;
    let unset_header_env = installed
        .mcp
        .as_ref()
        .filter(|mcp| mcp.transport == Transport::Http)
        .map(|mcp| {
            mcp.secret_headers
                .values()
                .filter(|name| std::env::var_os(name).is_none())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Ok(InstallOutcome {
        entry,
        unset_header_env,
    })
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

/// The result of a local change that the site is told about afterwards.
#[derive(Debug)]
pub(crate) struct LocalChange {
    pub(crate) entry: LedgerEntry,
    /// Set when the site could not be updated (the local change stands).
    pub(crate) site_warning: Option<CatalogError>,
}

/// `equip` / `unequip`: flips `enabled` locally, then tells the site.
pub(crate) async fn set_enabled(
    codex_home: &Path,
    client: &CatalogClient,
    selector: &str,
    enabled: bool,
) -> Result<LocalChange, CatalogError> {
    let mut ledger = load_ledger(codex_home)?;
    let mut entry = find_entry(&ledger, selector)?;
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
    }
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
    }
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
