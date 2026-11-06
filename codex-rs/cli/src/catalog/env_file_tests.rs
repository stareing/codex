use super::*;
use pretty_assertions::assert_eq;

fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// What the agent's start-up loader (`dotenvy`, as `codex-arg0` uses it) reads back.
fn loaded(home: &Path) -> BTreeMap<String, String> {
    dotenvy::from_path_iter(env_file_path(home))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn values_round_trip_through_the_agents_dotenv_loader() {
    let home = tempfile::tempdir().unwrap();
    let written = values(&[
        ("PLAIN", "Bearer abc.def"),
        ("QUOTE", "it's \"$HOME\" \\ here"),
        ("DOLLAR", "x$HOME${Y}"),
    ]);
    set(home.path(), &written).unwrap();
    assert_eq!(loaded(home.path()), written);
}

#[test]
fn other_lines_are_kept_and_a_variable_is_replaced_in_place() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        env_file_path(home.path()),
        "# mine\nOTHER=1\nexport KEY='old'\n",
    )
    .unwrap();
    set(home.path(), &values(&[("KEY", "new"), ("ADDED", "a")])).unwrap();
    assert_eq!(
        std::fs::read_to_string(env_file_path(home.path())).unwrap(),
        "# mine\nOTHER=1\nKEY='new'\nADDED='a'\n"
    );
    unset(home.path(), &["KEY".to_string(), "MISSING".to_string()]).unwrap();
    assert_eq!(
        loaded(home.path()),
        values(&[("OTHER", "1"), ("ADDED", "a")])
    );
}

#[test]
fn line_breaks_are_refused_and_nothing_is_written() {
    let home = tempfile::tempdir().unwrap();
    assert!(set(home.path(), &values(&[("KEY", "a\nEVIL=1")])).is_err());
    assert!(!env_file_path(home.path()).exists());
}

#[cfg(unix)]
#[test]
fn the_file_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    set(home.path(), &values(&[("KEY", "v")])).unwrap();
    let mode = std::fs::metadata(env_file_path(home.path()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}
