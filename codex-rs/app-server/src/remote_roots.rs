//! Folder allow-list for remote-control connections.
//!
//! A phone or browser driving this computer through remote control may only work inside the folders
//! the user allowed (`remote-control --allow <DIR>`, default: the folder it was started in). Every
//! cwd or path a remote request names — and the default cwd it would get when it names none — must
//! resolve inside one of them. Paths are resolved the way the request processors resolve them
//! (relative to the server cwd), then canonicalized so `..` and symlinks cannot step outside; a
//! path that does not exist yet is judged by its deepest existing ancestor and may not contain `..`
//! below it.
//!
//! Policy when no roots were given (neither the runtime option nor
//! `SITE_RELAY_ALLOWED_ROOTS`): unrestricted, as upstream. Our `remote-control` command always passes
//! roots (foreground directly, daemon via the saved roots file), so this only covers launchers that
//! run `app-server --remote-control` themselves and own that decision. A value that is set but names
//! no usable folder refuses everything (fail closed).
//!
//! Local connections (stdio, websocket) are checked the same way only when the launcher names a roots
//! file (`local_roots`, `SITE_LOCAL_ALLOWED_ROOTS_FILE`); in-process connections never are.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GetConversationSummaryParams;
use codex_app_server_protocol::ProjectRoot;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::TurnEnvironmentParams;
use codex_app_server_protocol::UserInput;
use codex_app_server_transport::SITE_RELAY_ALLOWED_ROOTS_ENV_VAR;
use codex_app_server_transport::decode_allowed_roots;

#[derive(Debug, Clone)]
pub(crate) struct RemoteRoots {
    /// Canonical roots; `None` = unrestricted.
    roots: Option<Vec<PathBuf>>,
    /// Where relative paths and omitted cwds resolve (the server cwd).
    base: PathBuf,
    /// Saved threads live here; a remote resume may name a rollout file under it.
    codex_home: PathBuf,
    /// Held to these folders as a local connection (`local_roots`): refusals say so.
    local: bool,
}

impl RemoteRoots {
    /// Roots from the runtime option, else from [`SITE_RELAY_ALLOWED_ROOTS_ENV_VAR`].
    pub(crate) fn resolve(
        configured: Option<Vec<PathBuf>>,
        base: PathBuf,
        codex_home: PathBuf,
    ) -> Self {
        let configured = configured.or_else(|| {
            std::env::var_os(SITE_RELAY_ALLOWED_ROOTS_ENV_VAR)
                .map(|value| decode_allowed_roots(&value))
        });
        Self::new(configured, base, codex_home)
    }

    pub(crate) fn new(
        configured: Option<Vec<PathBuf>>,
        base: PathBuf,
        codex_home: PathBuf,
    ) -> Self {
        let roots = configured.map(|roots| {
            roots
                .into_iter()
                .filter_map(|root| {
                    let absolute = if root.is_absolute() {
                        root
                    } else {
                        base.join(root)
                    };
                    match std::fs::canonicalize(&absolute) {
                        Ok(root) if root.is_dir() => Some(root),
                        _ => {
                            tracing::warn!(
                                root = %absolute.display(),
                                "remote-control allowed folder is not an existing directory; ignored"
                            );
                            None
                        }
                    }
                })
                .collect::<Vec<_>>()
        });
        let codex_home = std::fs::canonicalize(&codex_home).unwrap_or(codex_home);
        Self {
            roots,
            base,
            codex_home,
            local: false,
        }
    }

    pub(crate) fn unrestricted() -> Self {
        Self {
            roots: None,
            base: PathBuf::new(),
            codex_home: PathBuf::new(),
            local: false,
        }
    }

    /// The same roots, for a connection from this computer (refusals name the launcher's folders).
    pub(crate) fn local(mut self) -> Self {
        self.local = true;
        self
    }

    /// Who a refusal is about: `remote control …` or `this session …`.
    pub(crate) fn subject(&self) -> &'static str {
        if self.local {
            "this session"
        } else {
            "remote control"
        }
    }

    /// A categorical refusal from [`request_scope`] (worded for remote control) for this connection.
    pub(crate) fn reword(&self, reason: &str) -> String {
        let rest = reason.strip_prefix("remote control ").unwrap_or(reason);
        format!("{} {rest}", self.subject())
    }

    pub(crate) fn is_restricted(&self) -> bool {
        self.roots.is_some()
    }

    /// `Ok` when `path` (absolute, or relative to the server cwd) is inside an allowed folder.
    pub(crate) fn check(&self, path: &Path) -> Result<(), String> {
        let Some(roots) = &self.roots else {
            return Ok(());
        };
        let resolved = resolve(&self.base, path).ok_or_else(|| self.refusal(path))?;
        if roots.iter().any(|root| resolved.starts_with(root)) {
            Ok(())
        } else {
            Err(self.refusal(path))
        }
    }

    /// [`Self::check`] when present; an omitted path means the server cwd.
    pub(crate) fn check_or_base(&self, path: Option<&Path>) -> Result<(), String> {
        self.check(path.unwrap_or(&self.base))
    }

    /// A saved-thread file named by resume/fork: allowed folders or the Codex home.
    pub(crate) fn check_rollout(&self, path: &Path) -> Result<(), String> {
        if self.roots.is_none() {
            return Ok(());
        }
        match resolve(&self.base, path) {
            Some(resolved) if resolved.starts_with(&self.codex_home) => Ok(()),
            _ => self.check(path),
        }
    }
}

impl RemoteRoots {
    fn refusal(&self, path: &Path) -> String {
        if self.local {
            return format!(
                "this session may only use the folders allowed in the app; `{}` is outside them \
                 (allow the folder in the app first)",
                path.display()
            );
        }
        format!(
            "remote control may only use the folders allowed on this computer; `{}` is outside them \
             (restart remote-control with --allow <DIR> to allow it)",
            path.display()
        )
    }
}

/// Absolute, canonical form of `path`; `None` when it escapes through `..` below the deepest
/// existing ancestor or cannot be resolved at all.
fn resolve(base: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            if tail
                .iter()
                .any(|part: &Component<'_>| !matches!(part, Component::Normal(_)))
            {
                return None;
            }
            let mut resolved = canonical;
            for part in tail.iter().rev() {
                resolved.push(part.as_os_str());
            }
            return Some(resolved);
        }
        let name = existing.components().next_back()?;
        tail.push(name);
        existing = existing.parent()?;
    }
}

/// What a request would touch on disk, as far as the allow-list is concerned.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RequestScope {
    /// Paths named by the request (absolute or relative to the server cwd).
    pub(crate) paths: Vec<PathBuf>,
    /// Saved-thread files named by resume/fork, and skill files named in input: allowed folders
    /// or the Codex home.
    pub(crate) rollouts: Vec<PathBuf>,
    /// The request runs in the server cwd because it named none.
    pub(crate) server_cwd: bool,
    /// The request runs in this thread's cwd because it named none; an unknown cwd is refused.
    pub(crate) thread_cwd: Option<String>,
    /// Refused outright: the request reaches local files the allow-list cannot judge.
    pub(crate) refused: Option<&'static str>,
}

impl RequestScope {
    fn cwd_or_server(cwd: Option<PathBuf>) -> Self {
        match cwd {
            Some(cwd) => Self {
                paths: vec![cwd],
                ..Self::default()
            },
            None => Self {
                server_cwd: true,
                ..Self::default()
            },
        }
    }

    fn cwd_or_thread(cwd: Option<PathBuf>, thread_id: &str) -> Self {
        match cwd {
            Some(cwd) => Self {
                paths: vec![cwd],
                ..Self::default()
            },
            None => Self::thread(thread_id),
        }
    }

    fn thread(thread_id: &str) -> Self {
        Self {
            thread_cwd: Some(thread_id.to_string()),
            ..Self::default()
        }
    }

    fn paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
            ..Self::default()
        }
    }

    fn with_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.paths.extend(paths);
        self
    }

    /// Folders listed by the request; none listed means the server cwd.
    fn folders_or_server(folders: impl IntoIterator<Item = PathBuf>) -> Self {
        let paths: Vec<PathBuf> = folders.into_iter().collect();
        if paths.is_empty() {
            Self::cwd_or_server(None)
        } else {
            Self::paths(paths)
        }
    }

    /// The local files a turn's input reads (images, audio, skills); mentions name tools, not files.
    fn with_input(mut self, input: &[UserInput]) -> Self {
        for item in input {
            match item {
                UserInput::LocalImage { path, .. } | UserInput::LocalAudio { path } => {
                    self.paths.push(path.clone())
                }
                UserInput::Skill { path, .. } => self.rollouts.push(path.clone()),
                UserInput::Text { .. }
                | UserInput::Image { .. }
                | UserInput::Audio { .. }
                | UserInput::Mention { .. } => {}
            }
        }
        self
    }

    fn refuse(reason: &'static str) -> Self {
        Self {
            refused: Some(reason),
            ..Self::default()
        }
    }
}

fn environment_paths(environments: Option<&Vec<TurnEnvironmentParams>>) -> Vec<PathBuf> {
    environments
        .into_iter()
        .flatten()
        .flat_map(|environment| {
            std::iter::once(PathBuf::from(environment.cwd.as_str())).chain(
                environment
                    .runtime_workspace_roots
                    .iter()
                    .flatten()
                    .map(|root| PathBuf::from(root.as_str())),
            )
        })
        .collect()
}

fn project_roots(roots: Option<&Vec<ProjectRoot>>) -> impl Iterator<Item = PathBuf> + '_ {
    roots
        .into_iter()
        .flatten()
        .map(|root| root.path.as_path().to_path_buf())
}

/// The forms `marketplace/add` reads as a local folder (core-plugins `looks_like_local_path`).
fn looks_like_local_source(source: &str) -> bool {
    let source = source.trim();
    let bytes = source.as_bytes();
    Path::new(source).is_absolute()
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || source.starts_with('.')
        || source.starts_with('~')
        || source.starts_with('/')
        || source.starts_with('\\')
}

fn writable_roots(policy: Option<&SandboxPolicy>) -> Vec<PathBuf> {
    match policy {
        Some(SandboxPolicy::WorkspaceWrite { writable_roots, .. }) => writable_roots
            .iter()
            .map(|root| root.as_path().to_path_buf())
            .collect(),
        _ => Vec::new(),
    }
}

fn absolute_paths<'a>(
    paths: Option<&'a Vec<codex_utils_absolute_path::AbsolutePathBuf>>,
) -> impl Iterator<Item = PathBuf> + 'a {
    paths
        .into_iter()
        .flatten()
        .map(|path| path.as_path().to_path_buf())
}

/// The cwd and paths of the requests that run commands, start agent work or touch files. Requests
/// not listed here name no local folder.
pub(crate) fn request_scope(request: &ClientRequest) -> RequestScope {
    match request {
        ClientRequest::ThreadStart { params, .. } => {
            if params
                .selected_capability_roots
                .as_ref()
                .is_some_and(|roots| !roots.is_empty())
            {
                return RequestScope::refuse(
                    "remote control cannot select capability roots on this computer",
                );
            }
            RequestScope::cwd_or_server(params.cwd.as_ref().map(PathBuf::from))
                .with_paths(environment_paths(params.environments.as_ref()))
                .with_paths(absolute_paths(params.runtime_workspace_roots.as_ref()))
        }
        ClientRequest::ThreadResume { params, .. } => {
            let mut scope = RequestScope::cwd_or_thread(
                params.cwd.as_ref().map(PathBuf::from),
                &params.thread_id,
            )
            .with_paths(absolute_paths(params.runtime_workspace_roots.as_ref()));
            scope.rollouts.extend(params.path.clone());
            scope
        }
        ClientRequest::ThreadFork { params, .. } => {
            let mut scope = RequestScope::cwd_or_thread(
                params.cwd.as_ref().map(PathBuf::from),
                &params.thread_id,
            )
            .with_paths(absolute_paths(params.runtime_workspace_roots.as_ref()));
            scope.rollouts.extend(params.path.clone());
            scope
        }
        ClientRequest::ThreadSettingsUpdate { params, .. } => {
            RequestScope::paths(params.cwd.clone())
                .with_paths(writable_roots(params.sandbox_policy.as_ref()))
        }
        ClientRequest::TurnStart { params, .. } => {
            RequestScope::cwd_or_thread(params.cwd.clone(), &params.thread_id)
                .with_paths(environment_paths(params.environments.as_ref()))
                .with_paths(absolute_paths(params.runtime_workspace_roots.as_ref()))
                .with_paths(writable_roots(params.sandbox_policy.as_ref()))
                .with_input(&params.input)
        }
        ClientRequest::TurnSteer { params, .. } => {
            RequestScope::thread(&params.thread_id).with_input(&params.input)
        }
        ClientRequest::ThreadQueueAdd { params, .. } => {
            RequestScope::thread(&params.thread_id).with_input(&params.input)
        }
        ClientRequest::ThreadQueueUpdate { params, .. } => {
            RequestScope::thread(&params.thread_id).with_input(&params.input)
        }
        ClientRequest::ReviewStart { params, .. } => RequestScope::thread(&params.thread_id),
        ClientRequest::ThreadShellCommand { params, .. } => RequestScope::thread(&params.thread_id),
        ClientRequest::ThreadCompactStart { params, .. } => RequestScope::thread(&params.thread_id),
        ClientRequest::ThreadQueueStart { params, .. } => RequestScope::thread(&params.thread_id),
        // A rewind writes in the thread's folders: the thread must be one --allow covers.
        ClientRequest::ThreadCheckpointList { params, .. } => {
            RequestScope::thread(&params.thread_id)
        }
        ClientRequest::ThreadCheckpointRewindFiles { params, .. } => {
            RequestScope::thread(&params.thread_id)
        }
        ClientRequest::ThreadCheckpointUndoRewind { params, .. } => {
            RequestScope::thread(&params.thread_id)
        }
        ClientRequest::ThreadRealtimeStart { params, .. } => {
            RequestScope::thread(&params.thread_id)
        }
        ClientRequest::OneOffCommandExec { params, .. } => {
            RequestScope::cwd_or_server(params.cwd.clone())
                .with_paths(writable_roots(params.sandbox_policy.as_ref()))
        }
        ClientRequest::ProcessSpawn { params, .. } => {
            RequestScope::paths([params.cwd.as_path().to_path_buf()])
        }
        ClientRequest::SkillsExtraRootsSet { params, .. } => {
            RequestScope::paths(absolute_paths(Some(&params.extra_roots)))
        }
        ClientRequest::FsReadFile { params, .. } => {
            RequestScope::paths([params.path.to_path_buf()])
        }
        ClientRequest::FsWriteFile { params, .. } => {
            RequestScope::paths([params.path.to_path_buf()])
        }
        ClientRequest::FsCreateDirectory { params, .. } => {
            RequestScope::paths([params.path.to_path_buf()])
        }
        ClientRequest::FsGetMetadata { params, .. } => {
            RequestScope::paths([params.path.to_path_buf()])
        }
        ClientRequest::FsReadDirectory { params, .. } => {
            RequestScope::paths([params.path.to_path_buf()])
        }
        ClientRequest::FsRemove { params, .. } => RequestScope::paths([params.path.to_path_buf()]),
        ClientRequest::FsCopy { params, .. } => RequestScope::paths([
            params.source_path.to_path_buf(),
            params.destination_path.to_path_buf(),
        ]),
        ClientRequest::FsWatch { params, .. } => RequestScope::paths([params.path.to_path_buf()]),
        ClientRequest::FsSearchText { params, .. } => {
            RequestScope::paths([params.root.to_path_buf()])
        }
        ClientRequest::FuzzyFileSearch { params, .. } => {
            RequestScope::folders_or_server(params.roots.iter().map(PathBuf::from))
        }
        ClientRequest::FuzzyFileSearchSessionStart { params, .. } => {
            RequestScope::folders_or_server(params.roots.iter().map(PathBuf::from))
        }
        ClientRequest::SkillsList { params, .. } => {
            RequestScope::folders_or_server(params.cwds.iter().cloned())
        }
        ClientRequest::HooksList { params, .. } => {
            RequestScope::folders_or_server(params.cwds.iter().cloned())
        }
        ClientRequest::PluginList { params, .. } => {
            RequestScope::paths(absolute_paths(params.cwds.as_ref()))
        }
        ClientRequest::PluginInstalled { params, .. } => {
            RequestScope::paths(absolute_paths(params.cwds.as_ref()))
        }
        ClientRequest::PluginSearch { params, .. } => {
            RequestScope::paths(absolute_paths(params.cwds.as_ref()))
        }
        ClientRequest::PluginRead { params, .. } => RequestScope::paths(
            params
                .marketplace_path
                .iter()
                .map(|path| path.as_path().to_path_buf()),
        ),
        ClientRequest::PluginInstall { params, .. } => RequestScope::paths(
            params
                .marketplace_path
                .iter()
                .map(|path| path.as_path().to_path_buf()),
        ),
        ClientRequest::PluginShareSave { params, .. } => {
            RequestScope::paths([params.plugin_path.as_path().to_path_buf()])
        }
        ClientRequest::SkillsConfigWrite { params, .. } => RequestScope {
            rollouts: params
                .path
                .iter()
                .map(|path| path.as_path().to_path_buf())
                .collect(),
            ..RequestScope::default()
        },
        ClientRequest::MarketplaceAdd { params, .. } => {
            if looks_like_local_source(&params.source) {
                RequestScope::refuse(
                    "remote control cannot add a marketplace from a folder on this computer",
                )
            } else {
                RequestScope::default()
            }
        }
        ClientRequest::ConfigRead { params, .. } => {
            RequestScope::paths(params.cwd.iter().map(PathBuf::from))
        }
        ClientRequest::PermissionProfileList { params, .. } => {
            RequestScope::paths(params.cwd.iter().map(PathBuf::from))
        }
        ClientRequest::ExternalAgentConfigDetect { params, .. } => {
            if params.include_home {
                RequestScope::refuse(
                    "remote control cannot scan the home folder for other agents' settings",
                )
            } else {
                RequestScope::paths(params.cwds.iter().flatten().cloned())
            }
        }
        ClientRequest::ExternalAgentConfigImport { .. } => RequestScope::refuse(
            "remote control cannot import other agents' settings on this computer",
        ),
        ClientRequest::FeedbackUpload { params, .. } => {
            RequestScope::paths(params.extra_log_files.iter().flatten().cloned())
        }
        ClientRequest::WindowsSandboxSetupStart { params, .. } => {
            RequestScope::cwd_or_server(params.cwd.as_ref().map(|cwd| cwd.as_path().to_path_buf()))
        }
        ClientRequest::ProjectCreate { params, .. } => {
            RequestScope::paths(project_roots(Some(&params.roots)))
        }
        ClientRequest::ProjectImport { params, .. } => {
            RequestScope::paths(project_roots(Some(&params.roots)))
        }
        ClientRequest::ProjectUpdate { params, .. } => {
            RequestScope::paths(project_roots(params.roots.as_ref()))
        }
        ClientRequest::GitDiffToRemote { params, .. } => RequestScope::paths([params.cwd.clone()]),
        ClientRequest::GetConversationSummary { params, .. } => match params {
            GetConversationSummaryParams::RolloutPath { rollout_path } => RequestScope {
                rollouts: vec![rollout_path.clone()],
                ..RequestScope::default()
            },
            GetConversationSummaryParams::ThreadId { .. } => RequestScope::default(),
        },
        _ => RequestScope::default(),
    }
}

#[cfg(test)]
#[path = "remote_roots_tests.rs"]
mod tests;
