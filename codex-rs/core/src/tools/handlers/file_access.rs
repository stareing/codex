//! Which files `Read`/`Glob`/`Grep` may read and which `Edit`/`Write` changes need the user first: Claude Code's
//! file permission ladder (`checkReadPermissionForTool`, `checkWritePermissionForTool` in its
//! `utils/permissions/filesystem.ts`) on top of this session's sandbox, so the agent sees the same scope there.
//!
//! Reads, first match wins: a UNC path or a suspicious Windows path asks; a path the sandbox may not read is
//! refused; a path the sandbox may write is allowed (editing implies reading); a path in a working directory (the
//! working directory, the workspace roots, folders the user granted for this turn or session) is allowed; the
//! agent's own folders (skills, memories, plugins) are allowed; anything else asks.
//!
//! Writes go on to `apply_patch`'s approvals and sandbox exactly as before; first this asks for UNC and
//! suspicious Windows paths and for Claude Code's sensitive files (`.git`, `.vscode`, `.idea`, the agent's own
//! config folders, shell start-up files), which it never edits on its own.
//!
//! Asking goes through `request_permissions` (the client's permission prompt); what the user grants is recorded
//! for the turn or the session and counts as a working directory from then on. When approval prompts are off
//! (`never`), a question cannot be put: with full disk access that allows the call (Claude Code's
//! `bypassPermissions`), otherwise it is refused (its `dontAsk`). Remote environments are left to their own
//! executor's sandbox.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::file_tools::refuse;

/// Folders Claude Code never edits without asking (`DANGEROUS_DIRECTORIES`), with this agent's own config folders
/// beside its `.claude`.
const SENSITIVE_DIRECTORIES: [&str; 6] =
    [".git", ".vscode", ".idea", ".claude", ".codex", ".agents"];
/// Files Claude Code never edits without asking (`DANGEROUS_FILES`).
const SENSITIVE_FILES: [&str; 10] = [
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
    ".claude.json",
];
/// Folders under the agent's home it reads its own instructions and notes from.
const AGENT_HOME_FOLDERS: [&str; 4] = ["skills", "memories", "plugins", "prompts"];
/// Windows device names that make a trailing `.NAME` suspicious.
const DOS_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    Deny(String),
    Ask(String),
}

/// What the rules know about the session: everything except the path in question.
pub(crate) struct Facts {
    pub(crate) working_dirs: Vec<PathBuf>,
    pub(crate) agent_dirs: Vec<PathBuf>,
    pub(crate) read_denied: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    pub(crate) writable: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    /// The ADS colon check, which only the Windows kernel (and WSL's Windows mounts) interprets.
    pub(crate) windows_paths: bool,
}

/// The path in question: as the model wrote it, made absolute, and with symlinks in its existing part resolved.
pub(crate) struct Probe<'a> {
    pub(crate) raw: &'a str,
    pub(crate) path: PathBuf,
    pub(crate) resolved: PathBuf,
}

impl Probe<'_> {
    fn both(&self) -> [&Path; 2] {
        [&self.path, &self.resolved]
    }

    fn shown(&self) -> String {
        self.path.display().to_string()
    }
}

type Rule = fn(&Facts, &Probe<'_>) -> Option<Verdict>;

const READ_RULES: [Rule; 6] = [
    read_unc,
    read_suspicious,
    read_denied,
    read_writable,
    read_in_working_dir,
    read_agent_dir,
];

const WRITE_RULES: [Rule; 3] = [write_unc, write_suspicious, write_sensitive];

/// The first rule that decides; a read nothing allows asks, a write nothing stops goes on to `apply_patch`.
pub(crate) fn decide(access: Access, facts: &Facts, probe: &Probe<'_>) -> Verdict {
    let (rules, fallback): (&[Rule], fn(&Probe<'_>) -> Verdict) = match access {
        Access::Read => (&READ_RULES, |probe| {
            Verdict::Ask(format!(
                "The agent requested permissions to read from {}, but you haven't granted it yet.",
                probe.shown()
            ))
        }),
        Access::Write => (&WRITE_RULES, |_| Verdict::Allow),
    };
    rules
        .iter()
        .find_map(|rule| rule(facts, probe))
        .unwrap_or_else(|| fallback(probe))
}

fn read_unc(_: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    is_unc(probe.raw).then(|| {
        Verdict::Ask(format!(
            "The agent requested permissions to read from {}, which appears to be a UNC path that could access network resources.",
            probe.raw
        ))
    })
}

fn read_suspicious(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    is_suspicious_windows_path(probe.raw, facts.windows_paths).then(|| {
        Verdict::Ask(format!(
            "The agent requested permissions to read from {}, which contains a suspicious Windows path pattern that requires manual approval.",
            probe.raw
        ))
    })
}

fn read_denied(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    probe
        .both()
        .iter()
        .any(|path| (facts.read_denied)(path))
        .then(|| {
            Verdict::Deny(format!(
                "Permission to read {} has been denied.",
                probe.shown()
            ))
        })
}

fn read_writable(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    probe
        .both()
        .iter()
        .all(|path| (facts.writable)(path))
        .then_some(Verdict::Allow)
}

fn read_in_working_dir(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    within_all(&probe.both(), &facts.working_dirs).then_some(Verdict::Allow)
}

fn read_agent_dir(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    within_all(&probe.both(), &facts.agent_dirs).then_some(Verdict::Allow)
}

fn write_unc(_: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    is_unc(probe.raw).then(|| {
        Verdict::Ask(format!(
            "The agent requested permissions to write to {}, which appears to be a UNC path that could access network resources.",
            probe.raw
        ))
    })
}

fn write_suspicious(facts: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    is_suspicious_windows_path(probe.raw, facts.windows_paths).then(|| {
        Verdict::Ask(format!(
            "The agent requested permissions to write to {}, which contains a suspicious Windows path pattern that requires manual approval.",
            probe.raw
        ))
    })
}

fn write_sensitive(_: &Facts, probe: &Probe<'_>) -> Option<Verdict> {
    probe.both().iter().any(|path| is_sensitive(path)).then(|| {
        Verdict::Ask(format!(
            "The agent requested permissions to edit {} which is a sensitive file.",
            probe.shown()
        ))
    })
}

/// Every form of the path lies inside one of `roots` (compared without case, as Claude Code does).
fn within_all(paths: &[&Path], roots: &[PathBuf]) -> bool {
    paths
        .iter()
        .all(|path| roots.iter().any(|root| within(path, root)))
}

pub(crate) fn within(path: &Path, root: &Path) -> bool {
    let mut path = path.components();
    root.components()
        .all(|part| path.next().is_some_and(|other| same_component(part, other)))
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

/// Claude Code's `isDangerousFilePathToAutoEdit` (without its UNC check, a rule of its own here).
pub(crate) fn is_sensitive(path: &Path) -> bool {
    let names: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let in_sensitive_dir = names.iter().enumerate().any(|(index, name)| {
        SENSITIVE_DIRECTORIES.contains(&name.as_str())
            // `.claude/worktrees/` (and this agent's `.codex/worktrees/`) is where worktrees live, not config.
            && !((name == ".claude" || name == ".codex")
                && names.get(index + 1).is_some_and(|next| next == "worktrees"))
    });
    in_sensitive_dir
        || names
            .last()
            .is_some_and(|file| SENSITIVE_FILES.contains(&file.as_str()))
}

/// Claude Code's `containsVulnerableUncPath`, reduced to its prefixes: `\\host\…` or `//host/…`.
pub(crate) fn is_unc(raw: &str) -> bool {
    raw.starts_with("\\\\") || raw.starts_with("//")
}

/// Claude Code's `hasSuspiciousWindowsPathPattern`: paths Windows rewrites before opening them, which string
/// rules cannot judge (alternate data streams, 8.3 names, `\\?\` prefixes, trailing dots and spaces, device
/// names, `...` components).
pub(crate) fn is_suspicious_windows_path(raw: &str, windows_paths: bool) -> bool {
    let ads = windows_paths && raw.get(2..).is_some_and(|rest| rest.contains(':'));
    let short_name = raw
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit());
    let long_prefix = ["\\\\?\\", "\\\\.\\", "//?/", "//./"]
        .iter()
        .any(|prefix| raw.starts_with(prefix));
    let trailing = raw.ends_with(|c: char| c == '.' || c.is_whitespace());
    let device = raw.rsplit_once('.').is_some_and(|(_, suffix)| {
        DOS_DEVICES
            .iter()
            .any(|name| suffix.eq_ignore_ascii_case(name))
    });
    let dots = raw
        .split(['/', '\\'])
        .any(|part| part.len() >= 3 && part.bytes().all(|byte| byte == b'.'));
    ads || short_name || long_prefix || trailing || device || dots
}

/// `path` with symlinks in its longest existing prefix resolved (Claude Code checks both forms).
pub(crate) fn resolved(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Each root, and its symlink-resolved form when that differs.
fn with_resolved(roots: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let real = resolved(&root);
        if real != root {
            out.push(real);
        }
        out.push(root);
    }
    out
}

/// Folders granted through earlier answers to these questions (or `request_permissions`).
fn granted_dirs(granted: Option<&AdditionalPermissionProfile>, access: Access) -> Vec<PathBuf> {
    granted
        .and_then(|profile| profile.file_system.as_ref())
        .map(|file_system| {
            file_system
                .entries
                .iter()
                .filter(|entry| match access {
                    Access::Read => entry.access.can_read(),
                    Access::Write => entry.access.can_write(),
                })
                .filter_map(|entry| match &entry.path {
                    FileSystemPath::Path { path } => Some(path.to_path_buf()),
                    FileSystemPath::GlobPattern { .. } | FileSystemPath::Special { .. } => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The agent's own folders: `$CODEX_HOME/{skills,memories,plugins,prompts}`, `~/.agents/skills` and the system
/// skills folder.
fn agent_dirs(codex_home: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = AGENT_HOME_FOLDERS
        .iter()
        .map(|folder| codex_home.join(folder))
        .collect();
    if let Some(home) = home {
        dirs.push(home.join(".agents").join("skills"));
    }
    if cfg!(unix) {
        dirs.push(PathBuf::from("/etc/codex/skills"));
    }
    dirs
}

fn read_denied_check(
    policy: &FileSystemSandboxPolicy,
    cwd: &Path,
) -> Result<Box<dyn Fn(&Path) -> bool + Send + Sync>, String> {
    if policy.has_full_disk_read_access() {
        return Ok(
            match ReadDenyMatcher::try_new_for_local_paths(policy, cwd)? {
                Some(denied) => Box::new(move |path: &Path| denied.is_local_path_read_denied(path)),
                None => Box::new(|_: &Path| false),
            },
        );
    }
    let policy = policy.clone();
    let cwd = cwd.to_path_buf();
    Ok(Box::new(move |path: &Path| {
        !policy.can_read_local_path_with_cwd(path, &cwd)
    }))
}

/// Whether a question can be put to the user under this approval policy (as `request_permissions` decides).
fn can_ask(policy: AskForApproval) -> bool {
    match policy {
        AskForApproval::Never => false,
        AskForApproval::Granular(config) => config.allows_request_permissions(),
        AskForApproval::OnRequest | AskForApproval::UnlessTrusted => true,
    }
}

/// Checks `path` (`raw` as the model wrote it) before `tool` reads or writes it, asking the user when the rules
/// say so; `Ok` means go on.
pub(crate) async fn ensure_access(
    invocation: &ToolInvocation,
    environment: &TurnEnvironment,
    tool: &str,
    raw: &str,
    path: &Path,
    access: Access,
) -> Result<(), FunctionCallError> {
    if environment.environment.is_remote() {
        return Ok(());
    }
    let environment_id = environment.selection.environment_id.as_str();
    let session = &invocation.session;
    let granted = merge_permission_profiles(
        session
            .granted_session_permissions(environment_id)
            .await
            .as_ref(),
        session
            .granted_turn_permissions(environment_id)
            .await
            .as_ref(),
    );
    let cwd = environment.cwd().to_path_buf();
    let policy = environment
        .sandbox_context(granted.clone())
        .permissions
        .file_system_sandbox_policy();
    let full_write = policy.has_full_disk_write_access();
    let read_denied =
        read_denied_check(&policy, &cwd).map_err(|err| refuse(format!("{tool}: {err}")))?;
    let writable_cwd = cwd.clone();
    let writable_policy = policy.clone();
    let mut working_dirs = vec![cwd.clone()];
    working_dirs.extend(
        environment
            .workspace_roots()
            .iter()
            .map(PathUri::to_path_buf),
    );
    working_dirs.extend(granted_dirs(granted.as_ref(), access));
    if access == Access::Read {
        working_dirs.extend(granted_dirs(granted.as_ref(), Access::Write));
    }
    let home = environment.user_home_dir.as_ref().map(PathUri::to_path_buf);
    let facts = Facts {
        working_dirs: with_resolved(working_dirs),
        agent_dirs: with_resolved(agent_dirs(
            invocation.turn.config.codex_home.as_path(),
            home.as_deref(),
        )),
        read_denied,
        writable: Box::new(move |path: &Path| {
            writable_policy.can_write_local_path_with_cwd(path, &writable_cwd)
        }),
        windows_paths: cfg!(windows) || is_wsl_windows_mount(path),
    };
    let probe = Probe {
        raw,
        path: path.to_path_buf(),
        resolved: resolved(path),
    };
    let question = match decide(access, &facts, &probe) {
        Verdict::Allow => return Ok(()),
        Verdict::Deny(message) => return Err(refuse(message)),
        Verdict::Ask(question) => question,
    };
    if !can_ask(invocation.step_context.settings.approval_policy()) {
        if full_write {
            return Ok(());
        }
        return Err(refuse(format!(
            "{question} Approval prompts are off for this session, so {tool} was not allowed."
        )));
    }
    let asked_for = match access {
        // As Claude Code suggests, a read grant covers the folder (the file's own, or the searched one).
        Access::Read if !path.is_dir() => path.parent().unwrap_or(path),
        Access::Read | Access::Write => path,
    };
    let entry_path = AbsolutePathBuf::from_absolute_path(asked_for)
        .map(|absolute| PathUri::from_abs_path(&absolute))
        .map_err(|err| {
            refuse(format!(
                "{tool}: unable to ask about `{}`: {err}",
                asked_for.display()
            ))
        })?;
    let args = RequestPermissionsArgs {
        environment_id: Some(environment_id.to_string()),
        reason: Some(question),
        permissions: RequestPermissionProfile {
            network: None,
            file_system: Some(FileSystemPermissions {
                entries: vec![FileSystemSandboxEntry::new(
                    FileSystemPath::Path { path: entry_path },
                    match access {
                        Access::Read => FileSystemAccessMode::Read,
                        Access::Write => FileSystemAccessMode::Write,
                    },
                )],
                glob_scan_max_depth: None,
            }),
        },
    };
    let response = session
        .request_permissions_for_environment(
            &invocation.step_context,
            invocation.call_id.clone(),
            args,
            environment.selection(),
            invocation.cancellation_token.clone(),
        )
        .await
        .ok_or_else(|| refuse(format!("{tool} was cancelled before the user answered")))?;
    if response.permissions.is_empty() {
        let verb = match access {
            Access::Read => "read",
            Access::Write => "change",
        };
        return Err(refuse(format!(
            "The user did not allow {tool} to {verb} {}.",
            probe.shown()
        )));
    }
    Ok(())
}

/// WSL's Windows drives (`/mnt/<letter>/…`) go through the Windows kernel, which reads `:` as a stream separator.
fn is_wsl_windows_mount(path: &Path) -> bool {
    let mut parts = path.components().skip(1);
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(mnt)), Some(Component::Normal(drive)))
            if mnt == "mnt" && drive.len() == 1
    ) && std::env::var_os("WSL_DISTRO_NAME").is_some()
}

#[cfg(test)]
#[path = "file_access_tests.rs"]
mod tests;
