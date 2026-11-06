//! Skill bundles: check the download against the catalog's sha256 and size, then unzip it into
//! `$CODEX_HOME/skills/<slug>/` without trusting the archive (PLUGINS.md §5: no path traversal,
//! no absolute paths or links, at most [`MAX_FILES`] files and [`MAX_TOTAL_BYTES`] unpacked).
//! The archive is unpacked into a staging directory first and renamed into place, so a failure
//! never leaves a half-written skill behind.

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use super::spec::MAX_BUNDLE_BYTES;

pub(crate) const MAX_FILES: usize = 500;
pub(crate) const MAX_TOTAL_BYTES: u64 = MAX_BUNDLE_BYTES;
pub(crate) const SKILL_FILE: &str = "SKILL.md";
pub(crate) const SKILLS_DIR: &str = "skills";
const STAGING_PREFIX: &str = ".onigio-staging-";
/// Unix file-type bits for a symbolic link.
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnpackError {
    ShaMismatch,
    SizeMismatch { expected: u64, actual: u64 },
    NotAZip(String),
    UnsafePath(String),
    Link(String),
    Duplicate(String),
    TooManyFiles,
    TooLarge,
    MissingSkillFile,
    InvalidSkillFile(String),
    Io(String),
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShaMismatch => f.write_str("the download does not match the catalog's sha256"),
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "the download is {actual} bytes; the catalog announced {expected}"
            ),
            Self::NotAZip(err) => write!(f, "the bundle is not a valid zip archive: {err}"),
            Self::UnsafePath(name) => write!(f, "the bundle contains an unsafe path {name:?}"),
            Self::Link(name) => write!(f, "the bundle contains a link {name:?}"),
            Self::Duplicate(name) => write!(f, "the bundle contains {name:?} twice"),
            Self::TooManyFiles => write!(f, "the bundle has more than {MAX_FILES} files"),
            Self::TooLarge => write!(f, "the bundle unpacks to more than {MAX_TOTAL_BYTES} bytes"),
            Self::MissingSkillFile => {
                write!(f, "the bundle has no {SKILL_FILE} at its top level")
            }
            Self::InvalidSkillFile(err) => write!(f, "the bundle's {SKILL_FILE} is invalid: {err}"),
            Self::Io(err) => write!(f, "could not write the skill: {err}"),
        }
    }
}

fn io_error(err: std::io::Error) -> UnpackError {
    UnpackError::Io(err.to_string())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn verify_bundle(bytes: &[u8], sha256: &str, size: u64) -> Result<(), UnpackError> {
    let actual = bytes.len() as u64;
    if actual != size {
        return Err(UnpackError::SizeMismatch {
            expected: size,
            actual,
        });
    }
    if !sha256_hex(bytes).eq_ignore_ascii_case(sha256) {
        return Err(UnpackError::ShaMismatch);
    }
    Ok(())
}

/// The entry's path as plain relative components, or why it is refused. Backslashes count as
/// separators and `:` is refused so a Windows drive or stream name cannot sneak in.
fn safe_relative_path(name: &str) -> Result<PathBuf, UnpackError> {
    let unsafe_path = || UnpackError::UnsafePath(name.to_string());
    if name.starts_with(['/', '\\']) || name.contains(['\0', ':']) {
        return Err(unsafe_path());
    }
    let mut path = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path()),
            part => path.push(part),
        }
    }
    if path.as_os_str().is_empty() {
        return Err(unsafe_path());
    }
    Ok(path)
}

/// Unpacks `bytes` into `dest`, which must not exist yet.
pub(crate) fn unpack_zip(bytes: &[u8], dest: &Path) -> Result<(), UnpackError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|err| UnpackError::NotAZip(err.to_string()))?;
    std::fs::create_dir(dest).map_err(io_error)?;
    let mut files = 0usize;
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| UnpackError::NotAZip(err.to_string()))?;
        let name = entry.name().to_string();
        let relative = safe_relative_path(&name)?;
        if entry.enclosed_name().is_none() {
            return Err(UnpackError::UnsafePath(name));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & S_IFMT == S_IFLNK)
        {
            return Err(UnpackError::Link(name));
        }
        let target = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(io_error)?;
            continue;
        }
        files += 1;
        if files > MAX_FILES {
            return Err(UnpackError::TooManyFiles);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::AlreadyExists => UnpackError::Duplicate(name.clone()),
                _ => io_error(err),
            })?;
        // Count what is actually written: the archive's declared sizes are not trusted.
        let budget = MAX_TOTAL_BYTES - total;
        let mut limited = (&mut entry).take(budget + 1);
        let written = std::io::copy(&mut limited, &mut output).map_err(io_error)?;
        if written > budget {
            return Err(UnpackError::TooLarge);
        }
        total += written;
        output.flush().map_err(io_error)?;
    }
    let skill_file = dest.join(SKILL_FILE);
    match std::fs::symlink_metadata(&skill_file) {
        Ok(meta) if meta.is_file() => Ok(()),
        _ => Err(UnpackError::MissingSkillFile),
    }
}

/// The skill's name as the agent will read it from SKILL.md (the directory name when the
/// frontmatter has none).
pub(crate) fn skill_name_of(dir: &Path, fallback: &str) -> Result<String, UnpackError> {
    let contents = std::fs::read_to_string(dir.join(SKILL_FILE)).map_err(io_error)?;
    codex_skills::parse_skill_frontmatter_metadata(&contents, || fallback.to_string())
        .map(|parsed| parsed.name)
        .map_err(|err| UnpackError::InvalidSkillFile(err.to_string()))
}

/// Verifies, unpacks and moves a bundle to `skills_root/<dir_name>`. Returns the skill's name.
pub(crate) fn install_skill_bundle(
    bytes: &[u8],
    sha256: &str,
    size: u64,
    staging_root: &Path,
    skills_root: &Path,
    dir_name: &str,
) -> Result<String, UnpackError> {
    verify_bundle(bytes, sha256, size)?;
    install_skill_dir(staging_root, skills_root, dir_name, |dir| {
        unpack_zip(bytes, dir)
    })
}

/// Writes a skill the catalog sent as text (its whole SKILL.md) to `skills_root/<dir_name>`.
pub(crate) fn install_skill_text(
    skill_md: &str,
    staging_root: &Path,
    skills_root: &Path,
    dir_name: &str,
) -> Result<String, UnpackError> {
    install_skill_dir(staging_root, skills_root, dir_name, |dir| {
        std::fs::create_dir(dir).map_err(io_error)?;
        std::fs::write(dir.join(SKILL_FILE), skill_md).map_err(io_error)
    })
}

/// Fills a fresh directory with `fill`, checks its SKILL.md, then moves it to
/// `skills_root/<dir_name>`, replacing what was there only once the new copy is complete. Staging
/// happens under `staging_root` (same file system, outside the skill roots, so a crash leaves
/// nothing the agent would load). Returns the skill's name.
fn install_skill_dir(
    staging_root: &Path,
    skills_root: &Path,
    dir_name: &str,
    fill: impl FnOnce(&Path) -> Result<(), UnpackError>,
) -> Result<String, UnpackError> {
    std::fs::create_dir_all(skills_root).map_err(io_error)?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(staging_root)
        .map_err(io_error)?;
    let unpacked = staging.path().join("new");
    fill(&unpacked)?;
    let name = skill_name_of(&unpacked, dir_name)?;
    let final_dir = skills_root.join(dir_name);
    let previous = staging.path().join("old");
    let had_previous = match std::fs::symlink_metadata(&final_dir) {
        Ok(_) => {
            std::fs::rename(&final_dir, &previous).map_err(io_error)?;
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(io_error(err)),
    };
    if let Err(err) = std::fs::rename(&unpacked, &final_dir) {
        if had_previous {
            let _ = std::fs::rename(&previous, &final_dir);
        }
        return Err(io_error(err));
    }
    // `staging` (with the previous copy) is removed when it goes out of scope.
    Ok(name)
}

/// Removes an installed skill directory; a link is removed as a link, never followed.
pub(crate) fn remove_skill_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "unpack_tests.rs"]
mod tests;
