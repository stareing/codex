use super::*;
use pretty_assertions::assert_eq;

pub(crate) fn entry(reference: &str, kind: Kind, slug: &str, name: &str) -> LedgerEntry {
    LedgerEntry {
        reference: reference.to_string(),
        kind,
        slug: slug.to_string(),
        name: name.to_string(),
        title: slug.to_string(),
        version: "1.0.0".to_string(),
        sha256: None,
        skill_name: None,
        installed_at: "2026-11-06T00:00:00Z".to_string(),
        enabled: true,
        trust: "reviewed".to_string(),
        env_file: Vec::new(),
    }
}

#[test]
fn a_missing_file_is_an_empty_ledger_and_saving_round_trips() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(Ledger::load(home.path()).unwrap(), Ledger::default());
    let mut ledger = Ledger::default();
    ledger.upsert(entry("store:mcp/a", Kind::Mcp, "a", "a"));
    ledger.save(home.path()).unwrap();
    assert_eq!(Ledger::load(home.path()).unwrap(), ledger);
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ledger_path(home.path())).unwrap()).unwrap();
    assert_eq!(raw["format"], 1);
    assert_eq!(raw["items"][0]["ref"], "store:mcp/a");
    assert_eq!(raw["items"][0]["kind"], "mcp");
    // No temporary files are left next to it.
    let files: Vec<_> = std::fs::read_dir(home.path()).unwrap().collect();
    assert_eq!(files.len(), 1);
}

#[test]
fn a_corrupt_file_is_an_error_not_a_reset() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(ledger_path(home.path()), b"{not json").unwrap();
    assert!(Ledger::load(home.path()).is_err());
}

#[test]
fn upsert_replaces_by_reference_and_remove_deletes() {
    let mut ledger = Ledger::default();
    ledger.upsert(entry("store:mcp/a", Kind::Mcp, "a", "a"));
    let mut newer = entry("store:mcp/a", Kind::Mcp, "a", "alias");
    newer.version = "2.0.0".to_string();
    ledger.upsert(newer.clone());
    assert_eq!(ledger.items, vec![newer]);
    ledger.set_enabled("store:mcp/a", false);
    assert!(!ledger.items[0].enabled);
    assert!(ledger.remove("store:mcp/a").is_some());
    assert!(ledger.remove("store:mcp/a").is_none());
    assert!(ledger.items.is_empty());
}

#[test]
fn lookup_by_reference_name_or_slug() {
    let mut ledger = Ledger::default();
    ledger.upsert(entry("store:mcp/ctx", Kind::Mcp, "ctx", "my-ctx"));
    ledger.upsert(entry("store:mcp/pdf", Kind::Mcp, "pdf", "pdf"));
    ledger.upsert(entry("store:skill/pdf", Kind::Skill, "pdf", "pdf"));
    let found = |selector: &str| match ledger.lookup(selector) {
        Lookup::Found(entry) => Some(entry.reference.clone()),
        _ => None,
    };
    assert_eq!(found("store:mcp/ctx").as_deref(), Some("store:mcp/ctx"));
    assert_eq!(found("my-ctx").as_deref(), Some("store:mcp/ctx"));
    assert_eq!(found("ctx").as_deref(), Some("store:mcp/ctx"));
    assert_eq!(found("store:skill/pdf").as_deref(), Some("store:skill/pdf"));
    assert!(matches!(ledger.lookup("pdf"), Lookup::Ambiguous(ref e) if e.len() == 2));
    assert_eq!(ledger.lookup("nope"), Lookup::Missing);
    assert_eq!(
        ledger
            .owner_of(Kind::Skill, "pdf")
            .map(|e| e.reference.as_str()),
        Some("store:skill/pdf")
    );
}
