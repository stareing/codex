//! `sync-installed`: brings this computer in line with what the site still offers (PLUGINS.md §9).
//!
//! An item the site withdrew (`installed/` row `status: withdrawn`) is turned off, and the ledger
//! keeps why, since when, and whether it was on. Once the site offers it again it goes back to
//! that. Nothing changes when the site cannot be read (offline keeps things as they are) or does
//! not say (`status` missing: an older site). The site's own `enabled` is never touched: it stays
//! what the user chose.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use super::client::CatalogClient;
use super::client::CatalogError;
use super::client::STATUS_AVAILABLE;
use super::client::STATUS_WITHDRAWN;
use super::client::SiteInstalled;
use super::install::apply_enabled;
use super::install::load_ledger;
use super::install::now_rfc3339;
use super::install::save_ledger;
use super::ledger::Ledger;
use super::ledger::Withdrawal;

/// One change `sync` makes to an installed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Off, recording `reason` and what it was before the site withdrew it.
    Withdraw {
        reference: String,
        reason: String,
        enabled_before: bool,
    },
    /// Back to `enabled`, the withdrawal record dropped.
    Restore { reference: String, enabled: bool },
}

impl Step {
    fn reference(&self) -> &str {
        match self {
            Step::Withdraw { reference, .. } | Step::Restore { reference, .. } => reference,
        }
    }
}

/// The steps that bring `ledger` in line with `site` (see the module doc).
pub(crate) fn plan(ledger: &Ledger, site: &[SiteInstalled]) -> Vec<Step> {
    let rows: HashMap<&str, &SiteInstalled> = site
        .iter()
        .map(|row| (row.reference.as_str(), row))
        .collect();
    ledger
        .items
        .iter()
        .filter_map(|entry| {
            let row = rows.get(entry.reference.as_str())?;
            match (row.status.as_deref(), &entry.withdrawn) {
                (Some(STATUS_WITHDRAWN), held) => {
                    let reason = row.withdrawn_reason()?;
                    // Already off for this reason: nothing to do. Turned back on by hand (config
                    // edited): off again, keeping what it was before the site withdrew it.
                    let settled =
                        !entry.enabled && held.as_ref().is_some_and(|w| w.reason == reason);
                    let enabled_before = held.as_ref().map_or(entry.enabled, |w| w.enabled_before);
                    (!settled).then(|| Step::Withdraw {
                        reference: entry.reference.clone(),
                        reason,
                        enabled_before,
                    })
                }
                (Some(STATUS_AVAILABLE), Some(held)) => Some(Step::Restore {
                    reference: entry.reference.clone(),
                    enabled: held.enabled_before,
                }),
                _ => None,
            }
        })
        .collect()
}

/// What happened to one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Synced {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) name: String,
    /// `withdrawn`, `restored` or `failed`.
    pub(crate) action: &'static str,
    pub(crate) reason: Option<String>,
    /// On after this run.
    pub(crate) enabled: bool,
    pub(crate) error: Option<String>,
}

/// Reads the site's `installed/` and applies [`plan`]; one item failing does not stop the others.
pub(crate) async fn sync(
    codex_home: &Path,
    client: &CatalogClient,
) -> Result<Vec<Synced>, CatalogError> {
    let site = client.installed().await?;
    let mut ledger = load_ledger(codex_home)?;
    let mut done = Vec::new();
    for step in plan(&ledger, &site) {
        let Some(mut entry) = ledger.by_ref(step.reference()).cloned() else {
            continue;
        };
        let (enabled, action, reason) = match &step {
            Step::Withdraw {
                reason,
                enabled_before,
                ..
            } => {
                let since = entry
                    .withdrawn
                    .as_ref()
                    .map_or_else(now_rfc3339, |w| w.since.clone());
                entry.withdrawn = Some(Withdrawal {
                    reason: reason.clone(),
                    since,
                    enabled_before: *enabled_before,
                });
                (false, "withdrawn", Some(reason.clone()))
            }
            Step::Restore { enabled, .. } => {
                entry.withdrawn = None;
                (*enabled, "restored", None)
            }
        };
        let applied = if entry.enabled == enabled && action == "restored" {
            Ok(())
        } else {
            apply_enabled(codex_home, &entry, enabled).await
        };
        if let Err(err) = applied {
            done.push(Synced {
                reference: entry.reference.clone(),
                name: entry.name.clone(),
                action: "failed",
                reason,
                enabled: entry.enabled,
                error: Some(err.message()),
            });
            continue;
        }
        entry.enabled = enabled;
        done.push(Synced {
            reference: entry.reference.clone(),
            name: entry.name.clone(),
            action,
            reason,
            enabled,
            error: None,
        });
        ledger.upsert(entry);
        save_ledger(codex_home, &ledger)?;
    }
    Ok(done)
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
