//! `search`, `install`, `list`, `equip`, `unequip` and `remove`: MCP servers and skills from the
//! site's catalog (`/store`), installed on this computer with the site account `login` saved
//! (docs/agent-codex/PLUGINS.md §9 in the site repo).
//!
//! Layout: [`spec`] (references and the install description), [`client`] (the site's
//! `/api/agent-catalog/`), [`install`] (the flows and config writes), [`ledger`]
//! (`onigio-installed.json`), [`unpack`] (skill bundles). This file holds the command line and
//! the terminal.

mod client;
mod install;
mod ledger;
mod spec;
#[cfg(test)]
mod test_support;
mod unpack;

use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;

use codex_app_server_transport::SiteAccount;

use self::client::CatalogClient;
use self::client::CatalogError;
use self::install::InstallOptions;
use self::install::Prompter;

const MAX_TOKEN_BYTES: usize = 4096;
/// Exit status when a bare name matches several items.
const EXIT_AMBIGUOUS: i32 = 2;

#[derive(Debug, clap::Subcommand)]
pub(crate) enum CatalogCommand {
    /// Search the site's catalog of MCP servers and skills.
    Search(SearchArgs),
    /// Install an MCP server or skill from the site's catalog.
    Install(InstallArgs),
    /// List what is installed from the site's catalog.
    List(ListArgs),
    /// Turn on an installed MCP server or skill.
    Equip(SelectorArgs),
    /// Turn off an installed MCP server or skill without removing it.
    Unequip(SelectorArgs),
    /// Uninstall an MCP server or skill installed from the site's catalog.
    Remove(SelectorArgs),
}

#[derive(Debug, clap::ValueEnum, Clone, Copy)]
pub(crate) enum KindArg {
    Mcp,
    Skill,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SearchArgs {
    /// Words to search for.
    #[arg(required = true, num_args = 1..)]
    pub(crate) query: Vec<String>,
    /// Only MCP servers or only skills.
    #[arg(long, value_enum)]
    pub(crate) kind: Option<KindArg>,
    /// Result page (from 1).
    #[arg(long)]
    pub(crate) page: Option<u32>,
    /// Print the site's answer as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct InstallArgs {
    /// store:mcp/<slug>, store:skill/<slug>, or a slug.
    #[arg(value_name = "REF")]
    pub(crate) reference: String,
    /// Install without asking for confirmation.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// Local name for an MCP server (default: its slug).
    #[arg(long, value_name = "NAME")]
    pub(crate) name: Option<String>,
    /// Value for a secret the server needs, kept on this computer only. Repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env_pair)]
    pub(crate) env: Vec<(String, String)>,
    /// Print one JSON result line.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ListArgs {
    /// Print JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SelectorArgs {
    /// The reference, local name or slug of an installed item.
    #[arg(value_name = "REF")]
    pub(crate) reference: String,
    /// Print one JSON result line.
    #[arg(long)]
    pub(crate) json: bool,
}

fn parse_env_pair(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| "use KEY=VALUE".to_string())?;
    let key = key.trim();
    if key.is_empty() {
        return Err("use KEY=VALUE".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

impl CatalogCommand {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Search(_) => "search",
            Self::Install(_) => "install",
            Self::List(_) => "list",
            Self::Equip(_) => "equip",
            Self::Unequip(_) => "unequip",
            Self::Remove(_) => "remove",
        }
    }

    fn json(&self) -> bool {
        match self {
            Self::Search(args) => args.json,
            Self::Install(args) => args.json,
            Self::List(args) => args.json,
            Self::Equip(args) | Self::Unequip(args) | Self::Remove(args) => args.json,
        }
    }
}

/// The signed-in account's site and device token, or `not_signed_in`.
fn signed_in_client(codex_home: &Path) -> Result<CatalogClient, CatalogError> {
    let not_signed_in = || {
        CatalogError::local(
            "not_signed_in",
            "This computer is not signed in to the site.",
        )
    };
    let account = SiteAccount::load(codex_home)
        .map_err(|err| CatalogError::local("local_error", err.to_string()))?
        .ok_or_else(not_signed_in)?;
    let token = std::fs::read(SiteAccount::token_path(codex_home)).map_err(|_| not_signed_in())?;
    if token.len() > MAX_TOKEN_BYTES {
        return Err(not_signed_in());
    }
    let token = String::from_utf8(token).map_err(|_| not_signed_in())?;
    if token.trim().is_empty() {
        return Err(not_signed_in());
    }
    let site = crate::site_login::resolve_site_url(Some(&account.site_url))
        .map_err(|err| CatalogError::local("local_error", format!("{err:#}")))?;
    CatalogClient::new(&site, &token)
}

struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn confirm(&mut self, question: &str) -> Result<bool, CatalogError> {
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|_| CatalogError::local("input_error", "could not read the answer"))?;
        Ok(matches!(
            line.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    fn secret(&mut self, name: &str) -> Result<Option<String>, CatalogError> {
        crate::site_password::read_hidden_line(&format!("{name}: "))
            .map(|value| Some(value).filter(|value| !value.is_empty()))
            .map_err(|_| CatalogError::local("input_error", format!("could not read {name}")))
    }
}

fn error_json(err: &CatalogError) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "code": err.code,
        "detail": err.message(),
        "candidates": err.candidates,
    })
}

fn print_error(err: &CatalogError, json: bool) {
    if json {
        println!("{}", error_json(err));
        return;
    }
    eprintln!("Error: {}", err.message());
    for candidate in &err.candidates {
        eprintln!("  {candidate}");
    }
}

fn warn_site(err: &CatalogError, site: &str) {
    eprintln!(
        "Warning: done on this computer, but {site} was not updated: {}",
        err.message()
    );
}

async fn run_search(client: &CatalogClient, args: &SearchArgs) -> Result<(), CatalogError> {
    let kind = args.kind.map(|kind| match kind {
        KindArg::Mcp => "mcp",
        KindArg::Skill => "skill",
    });
    let page = client
        .search(&args.query.join(" "), kind, args.page)
        .await?;
    if args.json {
        let hits: Vec<serde_json::Value> = page
            .data
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "ref": hit.reference, "kind": hit.kind, "name": hit.name,
                    "summary": hit.summary, "trust": hit.trust, "installs": hit.installs,
                    "supported": hit.supported, "unsupported_reason": hit.unsupported_reason,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "data": hits, "pagination": page.pagination })
        );
        return Ok(());
    }
    if page.data.is_empty() {
        eprintln!("Nothing found.");
        return Ok(());
    }
    for hit in &page.data {
        let note = match (&hit.supported, &hit.unsupported_reason) {
            (true, _) => String::new(),
            (false, Some(reason)) => format!("  [not supported: {reason}]"),
            (false, None) => "  [not supported]".to_string(),
        };
        println!(
            "{}  {}  ({} installs){note}",
            hit.reference, hit.name, hit.installs
        );
        if !hit.summary.is_empty() {
            println!("    {}", hit.summary);
        }
    }
    Ok(())
}

async fn run_install(
    codex_home: &Path,
    client: &CatalogClient,
    args: &InstallArgs,
) -> Result<(), CatalogError> {
    let options = InstallOptions {
        yes: args.yes,
        alias: args.name.clone(),
        env: args.env.iter().cloned().collect::<BTreeMap<_, _>>(),
    };
    let mut prompter = TerminalPrompter;
    let mut show = |line: &str| eprintln!("{line}");
    let outcome = install::install(
        codex_home,
        client,
        &mut prompter,
        &args.reference,
        &options,
        &mut show,
    )
    .await?;
    let entry = &outcome.entry;
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "installed", "ref": entry.reference, "kind": entry.kind,
                "name": entry.name, "version": entry.version, "enabled": entry.enabled,
                "unset_env": outcome.unset_header_env,
            })
        );
    } else {
        let what = match entry.kind {
            spec::Kind::Mcp => format!("MCP server `{}`", entry.name),
            spec::Kind::Skill => format!(
                "skill `{}`",
                entry.skill_name.as_deref().unwrap_or(&entry.name)
            ),
        };
        println!(
            "Installed {} {} as {what}. Start a new session to use it.",
            entry.title, entry.version
        );
        if !outcome.unset_header_env.is_empty() {
            println!(
                "Set these environment variables before starting the agent: {}",
                outcome.unset_header_env.join(", ")
            );
        }
    }
    Ok(())
}

async fn run_list(
    codex_home: &Path,
    client: Option<&CatalogClient>,
    args: &ListArgs,
) -> Result<(), CatalogError> {
    let (items, warning) = install::list(codex_home, client).await?;
    if args.json {
        println!(
            "{}",
            serde_json::json!({ "data": items, "online": client.is_some() && warning.is_none() })
        );
        return Ok(());
    }
    if let Some(warning) = &warning {
        eprintln!("Showing this computer only: {}", warning.message());
    }
    if items.is_empty() {
        eprintln!("Nothing installed from the site yet; try `onigio-agent search <words>`.");
        return Ok(());
    }
    for item in &items {
        let state = match (item.local, item.enabled) {
            (false, _) => "not on this computer",
            (true, Some(false)) => "off",
            (true, _) => "on",
        };
        let update = match (&item.update_available, &item.latest_version) {
            (true, Some(latest)) => format!("  (update: {latest})"),
            _ => String::new(),
        };
        println!(
            "{}  {}  {}  {state}{update}",
            item.reference,
            item.name,
            item.version.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

async fn run_toggle(
    codex_home: &Path,
    client: &CatalogClient,
    args: &SelectorArgs,
    enabled: bool,
) -> Result<(), CatalogError> {
    let change = install::set_enabled(codex_home, client, &args.reference, enabled).await?;
    report_change(
        &change,
        if enabled { "equipped" } else { "unequipped" },
        args.json,
        client,
    );
    Ok(())
}

async fn run_remove(
    codex_home: &Path,
    client: &CatalogClient,
    args: &SelectorArgs,
) -> Result<(), CatalogError> {
    let change = install::remove(codex_home, client, &args.reference).await?;
    report_change(&change, "removed", args.json, client);
    Ok(())
}

fn report_change(change: &install::LocalChange, status: &str, json: bool, client: &CatalogClient) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": status, "ref": change.entry.reference, "name": change.entry.name,
                "site_updated": change.site_warning.is_none(),
            })
        );
    } else {
        println!(
            "{} `{}` ({}).",
            capitalised(status),
            change.entry.name,
            change.entry.reference
        );
    }
    if let Some(err) = &change.site_warning {
        warn_site(err, client.site());
    }
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

async fn dispatch(command: &CatalogCommand) -> Result<(), CatalogError> {
    let codex_home = crate::site_login::codex_home()
        .map_err(|err| CatalogError::local("local_error", format!("{err:#}")))?;
    if let CatalogCommand::List(args) = command {
        // `list` works offline and before signing in: it then shows this computer only.
        let client = signed_in_client(&codex_home).ok();
        return run_list(&codex_home, client.as_ref(), args).await;
    }
    let client = signed_in_client(&codex_home)?;
    match command {
        CatalogCommand::Search(args) => run_search(&client, args).await,
        CatalogCommand::Install(args) => run_install(&codex_home, &client, args).await,
        CatalogCommand::List(args) => run_list(&codex_home, Some(&client), args).await,
        CatalogCommand::Equip(args) => run_toggle(&codex_home, &client, args, true).await,
        CatalogCommand::Unequip(args) => run_toggle(&codex_home, &client, args, false).await,
        CatalogCommand::Remove(args) => run_remove(&codex_home, &client, args).await,
    }
}

/// Runs a catalog command and exits: 0 on success, 2 for an ambiguous name, 1 otherwise.
pub(crate) async fn run(command: CatalogCommand) -> ! {
    let json = command.json();
    match dispatch(&command).await {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            print_error(&err, json);
            std::process::exit(if err.code == "ambiguous_ref" {
                EXIT_AMBIGUOUS
            } else {
                1
            });
        }
    }
}

#[cfg(test)]
#[path = "e2e_tests.rs"]
mod e2e_tests;
