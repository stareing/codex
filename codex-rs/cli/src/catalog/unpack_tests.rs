use super::*;
use crate::catalog::test_support::SKILL_MD;
use crate::catalog::test_support::zip_of;
use crate::catalog::test_support::zip_with_symlink;
use pretty_assertions::assert_eq;

fn install(bytes: &[u8], home: &Path) -> Result<String, UnpackError> {
    install_skill_bundle(
        bytes,
        &sha256_hex(bytes),
        bytes.len() as u64,
        home,
        &home.join(SKILLS_DIR),
        "pdf",
    )
}

/// Nothing is left behind after a refused bundle: no skill directory, no staging directory.
fn assert_nothing_written(home: &Path) {
    assert!(!home.join(SKILLS_DIR).join("pdf").exists());
    let leftovers: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".onigio-staging-")
        })
        .collect();
    assert!(leftovers.is_empty(), "staging left behind");
}

#[test]
fn a_good_bundle_is_unpacked_and_named_from_its_skill_file() {
    let home = tempfile::tempdir().unwrap();
    let bytes = zip_of(&[("SKILL.md", SKILL_MD), ("scripts/run.py", b"print(1)\n")]);
    assert_eq!(install(&bytes, home.path()).unwrap(), "pdf-helper");
    let dir = home.path().join("skills/pdf");
    assert_eq!(std::fs::read(dir.join("SKILL.md")).unwrap(), SKILL_MD);
    assert_eq!(
        std::fs::read(dir.join("scripts/run.py")).unwrap(),
        b"print(1)\n"
    );
    assert_no_staging(home.path());
}

fn assert_no_staging(home: &Path) {
    let leftovers = std::fs::read_dir(home)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".onigio-staging-")
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[test]
fn reinstalling_replaces_the_previous_copy() {
    let home = tempfile::tempdir().unwrap();
    install(
        &zip_of(&[("SKILL.md", SKILL_MD), ("old.txt", b"x")]),
        home.path(),
    )
    .unwrap();
    install(
        &zip_of(&[("SKILL.md", SKILL_MD), ("new.txt", b"y")]),
        home.path(),
    )
    .unwrap();
    let dir = home.path().join("skills/pdf");
    assert!(dir.join("new.txt").exists());
    assert!(!dir.join("old.txt").exists());
}

#[test]
fn a_failed_reinstall_keeps_the_previous_copy() {
    let home = tempfile::tempdir().unwrap();
    install(
        &zip_of(&[("SKILL.md", SKILL_MD), ("old.txt", b"x")]),
        home.path(),
    )
    .unwrap();
    let bad = zip_of(&[("../evil", b"x")]);
    assert!(install(&bad, home.path()).is_err());
    assert!(home.path().join("skills/pdf/old.txt").exists());
}

#[test]
fn hash_and_size_must_match_the_catalog() {
    let home = tempfile::tempdir().unwrap();
    let bytes = zip_of(&[("SKILL.md", SKILL_MD)]);
    let root = home.path().join(SKILLS_DIR);
    let wrong_sha = "0".repeat(64);
    assert_eq!(
        install_skill_bundle(
            &bytes,
            &wrong_sha,
            bytes.len() as u64,
            home.path(),
            &root,
            "pdf"
        ),
        Err(UnpackError::ShaMismatch)
    );
    assert_eq!(
        install_skill_bundle(&bytes, &sha256_hex(&bytes), 1, home.path(), &root, "pdf"),
        Err(UnpackError::SizeMismatch {
            expected: 1,
            actual: bytes.len() as u64
        })
    );
    assert_nothing_written(home.path());
}

#[test]
fn traversal_and_absolute_paths_are_refused() {
    for name in [
        "../evil.txt",
        "a/../../evil.txt",
        "/etc/evil",
        "\\evil",
        "..\\evil",
        "C:/evil",
        "a/b:stream",
    ] {
        let home = tempfile::tempdir().unwrap();
        let bytes = zip_of(&[("SKILL.md", SKILL_MD), (name, b"x")]);
        assert_eq!(
            install(&bytes, home.path()),
            Err(UnpackError::UnsafePath(name.to_string())),
            "{name}"
        );
        assert_nothing_written(home.path());
        assert!(!home.path().join("evil.txt").exists());
    }
}

#[test]
fn links_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let bytes = zip_with_symlink("link", "/etc/passwd");
    assert_eq!(
        install(&bytes, home.path()),
        Err(UnpackError::Link("link".to_string()))
    );
    assert_nothing_written(home.path());
}

#[test]
fn too_many_files_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..MAX_FILES).map(|i| format!("f{i}.txt")).collect();
    let mut files: Vec<(&str, &[u8])> = vec![("SKILL.md", SKILL_MD)];
    files.extend(names.iter().map(|name| (name.as_str(), &b"x"[..])));
    assert_eq!(
        install(&zip_of(&files), home.path()),
        Err(UnpackError::TooManyFiles)
    );
    assert_nothing_written(home.path());
    // Exactly the limit is fine.
    let at_limit: Vec<(&str, &[u8])> = files[..MAX_FILES].to_vec();
    assert!(install(&zip_of(&at_limit), home.path()).is_ok());
}

#[test]
fn too_many_unpacked_bytes_are_refused() {
    let home = tempfile::tempdir().unwrap();
    // Zeros compress well, so the archive itself stays small.
    let big = vec![0u8; MAX_TOTAL_BYTES as usize + 1];
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    writer.start_file("SKILL.md", options).unwrap();
    std::io::Write::write_all(&mut writer, SKILL_MD).unwrap();
    writer.start_file("big.bin", options).unwrap();
    std::io::Write::write_all(&mut writer, &big).unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    assert!((bytes.len() as u64) < MAX_TOTAL_BYTES);
    assert_eq!(install(&bytes, home.path()), Err(UnpackError::TooLarge));
    assert_nothing_written(home.path());
}

#[test]
fn a_skill_file_must_sit_at_the_top_level() {
    let home = tempfile::tempdir().unwrap();
    let nested = zip_of(&[("pdf/SKILL.md", SKILL_MD)]);
    assert_eq!(
        install(&nested, home.path()),
        Err(UnpackError::MissingSkillFile)
    );
    let none = zip_of(&[("README.md", b"hi")]);
    assert_eq!(
        install(&none, home.path()),
        Err(UnpackError::MissingSkillFile)
    );
    assert_nothing_written(home.path());
}

#[test]
fn garbage_and_bad_skill_files_are_refused() {
    let home = tempfile::tempdir().unwrap();
    assert!(matches!(
        install(b"not a zip", home.path()),
        Err(UnpackError::NotAZip(_))
    ));
    let bad_frontmatter = zip_of(&[("SKILL.md", b"no frontmatter")]);
    assert!(matches!(
        install(&bad_frontmatter, home.path()),
        Err(UnpackError::InvalidSkillFile(_))
    ));
    assert_nothing_written(home.path());
}

#[cfg(unix)]
#[test]
fn removing_a_linked_skill_does_not_follow_the_link() {
    let home = tempfile::tempdir().unwrap();
    let outside = home.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"x").unwrap();
    let link = home.path().join("linked");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    remove_skill_dir(&link).unwrap();
    assert!(!link.exists());
    assert!(outside.join("keep.txt").exists());
    remove_skill_dir(&home.path().join("missing")).unwrap();
}
