//! `$CODEX_HOME/onigio-installed.json`: what this computer installed from the site's catalog, so
//! `list`, `equip`, `unequip` and `remove` know where each item lives. Written atomically.

use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::spec::Kind;
use super::spec::TRUST_REVIEWED;

pub(crate) const LEDGER_FILE: &str = "onigio-installed.json";
const LEDGER_FORMAT: u32 = 1;
const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerEntry {
    /// The reference exactly as the site names it, e.g. `store:mcp/context7` or
    /// `tools:mcp/io.github.acme/weather`.
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) kind: Kind,
    pub(crate) slug: String,
    /// Local name: the `[mcp_servers.<name>]` key, or the directory under `skills/`.
    pub(crate) name: String,
    /// The catalog's display name.
    #[serde(default)]
    pub(crate) title: String,
    pub(crate) version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    /// The skill's `name` from its SKILL.md (what `[[skills.config]]` matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) skill_name: Option<String>,
    pub(crate) installed_at: String,
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
    /// The spec's `trust` when installed (empty in ledgers written before it was kept).
    ///
    /// An `update` (not implemented yet) must show the new command and ask again before
    /// reinstalling any entry for which [`LedgerEntry::reconfirm_on_update`] holds; it never
    /// upgrades those silently (PLUGINS.md §10 item 7).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) trust: String,
    /// Variables this install wrote to `$CODEX_HOME/.env` (secret header values), removed with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) env_file: Vec<String>,
    /// Set while the site has withdrawn the item (`sync-installed`): it is off until the site offers
    /// it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) withdrawn: Option<Withdrawal>,
}

/// Why and since when the site withdrew an item, and whether it was on before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Withdrawal {
    /// The site's `withdrawn_reason` (e.g. `version_yanked`).
    pub(crate) reason: String,
    pub(crate) since: String,
    /// What `sync-installed` sets `enabled` back to when the site offers the item again.
    pub(crate) enabled_before: bool,
}

impl LedgerEntry {
    /// Not reviewed by the site (a `tools:` entry, or an unknown trust): any reinstall, update
    /// included, needs the user's confirmation again.
    pub(crate) fn reconfirm_on_update(&self) -> bool {
        self.trust != TRUST_REVIEWED || self.reference.starts_with("tools:")
    }
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ledger {
    #[serde(default)]
    pub(crate) items: Vec<LedgerEntry>,
}

#[derive(Serialize)]
struct LedgerFileOut<'a> {
    format: u32,
    items: &'a [LedgerEntry],
}

/// The result of looking an item up by what the user typed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lookup<'a> {
    Found(&'a LedgerEntry),
    Missing,
    /// A bare name matching more than one item (an MCP server and a skill with the same slug).
    Ambiguous(Vec<&'a LedgerEntry>),
}

pub(crate) fn ledger_path(codex_home: &Path) -> PathBuf {
    codex_home.join(LEDGER_FILE)
}

impl Ledger {
    /// A missing file is an empty ledger; an unreadable one is an error (never silently reset).
    pub(crate) fn load(codex_home: &Path) -> std::io::Result<Self> {
        let path = ledger_path(codex_home);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err),
        };
        let mut raw = String::new();
        file.take(MAX_LEDGER_BYTES).read_to_string(&mut raw)?;
        serde_json::from_str(&raw).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a valid install list: {err}", path.display()),
            )
        })
    }

    /// Writes a temporary file next to the ledger and renames it over the old one.
    pub(crate) fn save(&self, codex_home: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(codex_home)?;
        let json = serde_json::to_vec_pretty(&LedgerFileOut {
            format: LEDGER_FORMAT,
            items: &self.items,
        })
        .map_err(std::io::Error::other)?;
        let mut tmp = tempfile::NamedTempFile::new_in(codex_home)?;
        std::io::Write::write_all(&mut tmp, &json)?;
        tmp.as_file().sync_all()?;
        tmp.persist(ledger_path(codex_home))
            .map_err(|err| err.error)?;
        Ok(())
    }

    pub(crate) fn by_ref(&self, reference: &str) -> Option<&LedgerEntry> {
        self.items.iter().find(|item| item.reference == reference)
    }

    /// By reference, else by local name, else by slug.
    pub(crate) fn lookup(&self, selector: &str) -> Lookup<'_> {
        if let Some(entry) = self.by_ref(selector) {
            return Lookup::Found(entry);
        }
        let by_name: Vec<&LedgerEntry> = self
            .items
            .iter()
            .filter(|item| item.name == selector)
            .collect();
        let matches = if by_name.is_empty() {
            self.items
                .iter()
                .filter(|item| item.slug == selector)
                .collect()
        } else {
            by_name
        };
        match matches.as_slice() {
            [] => Lookup::Missing,
            [entry] => Lookup::Found(entry),
            _ => Lookup::Ambiguous(matches),
        }
    }

    /// The entry owning the local name `name` of `kind`, if any.
    pub(crate) fn owner_of(&self, kind: Kind, name: &str) -> Option<&LedgerEntry> {
        self.items
            .iter()
            .find(|item| item.kind == kind && item.name == name)
    }

    /// Replaces the entry with the same reference, or adds one.
    pub(crate) fn upsert(&mut self, entry: LedgerEntry) {
        match self
            .items
            .iter_mut()
            .find(|item| item.reference == entry.reference)
        {
            Some(existing) => *existing = entry,
            None => self.items.push(entry),
        }
    }

    pub(crate) fn remove(&mut self, reference: &str) -> Option<LedgerEntry> {
        let index = self
            .items
            .iter()
            .position(|item| item.reference == reference)?;
        Some(self.items.remove(index))
    }

    pub(crate) fn set_enabled(&mut self, reference: &str, enabled: bool) {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.reference == reference)
        {
            item.enabled = enabled;
        }
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
