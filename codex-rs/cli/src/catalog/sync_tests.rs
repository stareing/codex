use super::*;
use crate::catalog::ledger::LedgerEntry;
use crate::catalog::spec::Kind;
use pretty_assertions::assert_eq;

fn entry(reference: &str, enabled: bool, withdrawn: Option<(&str, bool)>) -> LedgerEntry {
    LedgerEntry {
        reference: reference.to_string(),
        kind: Kind::Mcp,
        slug: reference.to_string(),
        name: reference.to_string(),
        title: reference.to_string(),
        version: "1.0.0".to_string(),
        sha256: None,
        skill_name: None,
        installed_at: "2026-11-08T00:00:00Z".to_string(),
        enabled,
        trust: "reviewed".to_string(),
        env_file: Vec::new(),
        withdrawn: withdrawn.map(|(reason, enabled_before)| Withdrawal {
            reason: reason.to_string(),
            since: "2026-11-08T01:00:00Z".to_string(),
            enabled_before,
        }),
    }
}

fn row(reference: &str, status: Option<&str>, reason: Option<&str>) -> SiteInstalled {
    serde_json::from_value(serde_json::json!({
        "ref": reference, "kind": "mcp", "status": status, "withdrawn_reason": reason,
    }))
    .unwrap()
}

fn withdraw(reference: &str, reason: &str, enabled_before: bool) -> Step {
    Step::Withdraw {
        reference: reference.to_string(),
        reason: reason.to_string(),
        enabled_before,
    }
}

#[test]
fn each_state_and_answer_gives_its_step() {
    let y = "version_yanked";
    let cases: Vec<(&str, LedgerEntry, SiteInstalled, Option<Step>)> = vec![
        (
            "on, withdrawn",
            entry("a", true, None),
            row("a", Some("withdrawn"), Some(y)),
            Some(withdraw("a", y, true)),
        ),
        (
            "off, withdrawn",
            entry("a", false, None),
            row("a", Some("withdrawn"), Some(y)),
            Some(withdraw("a", y, false)),
        ),
        (
            "already off for it",
            entry("a", false, Some((y, true))),
            row("a", Some("withdrawn"), Some(y)),
            None,
        ),
        (
            "turned on by hand",
            entry("a", true, Some((y, false))),
            row("a", Some("withdrawn"), Some(y)),
            Some(withdraw("a", y, false)),
        ),
        (
            "new reason",
            entry("a", false, Some((y, true))),
            row("a", Some("withdrawn"), Some("package_unavailable")),
            Some(withdraw("a", "package_unavailable", true)),
        ),
        (
            "no reason given",
            entry("a", true, None),
            row("a", Some("withdrawn"), None),
            Some(withdraw("a", "withdrawn", true)),
        ),
        (
            "offered again, was on",
            entry("a", false, Some((y, true))),
            row("a", Some("available"), None),
            Some(Step::Restore {
                reference: "a".to_string(),
                enabled: true,
            }),
        ),
        (
            "offered again, was off",
            entry("a", false, Some((y, false))),
            row("a", Some("available"), None),
            Some(Step::Restore {
                reference: "a".to_string(),
                enabled: false,
            }),
        ),
        (
            "available, never withdrawn",
            entry("a", true, None),
            row("a", Some("available"), None),
            None,
        ),
        (
            "older site",
            entry("a", false, Some((y, true))),
            row("a", None, None),
            None,
        ),
        (
            "unknown status",
            entry("a", true, None),
            row("a", Some("paused"), None),
            None,
        ),
        (
            "not on the site",
            entry("a", true, None),
            row("b", Some("withdrawn"), Some(y)),
            None,
        ),
    ];
    for (name, local, site, want) in cases {
        let ledger = Ledger { items: vec![local] };
        assert_eq!(
            plan(&ledger, &[site]),
            want.into_iter().collect::<Vec<_>>(),
            "{name}"
        );
    }
}
