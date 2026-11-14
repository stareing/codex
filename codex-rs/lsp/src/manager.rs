//! Language-server instances shared by every conversation in the agent process.
//!
//! An instance is one server for one workspace root. It starts on first use (a query, or an edit of a file it
//! serves), is shared by all conversations that use the same server and root, and stops after
//! [`LspLimits::idle_shutdown`] without use. At most [`LspLimits::max_servers`] run at once: starting another stops
//! the least recently used idle one, and when all are busy the request fails instead of waiting. A server is restarted
//! only on its next use, never in a loop: once its failed starts plus crashes exceed `maxRestarts` (default
//! [`MAX_START_FAILURES`], as in Claude Code) it is not started again in this process.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;

use crate::client::JSONRPC_METHOD_NOT_FOUND;
use crate::client::LSP_CONTENT_MODIFIED;
use crate::client::LspClient;
use crate::client::LspError;
use crate::client::RpcError;
use crate::client::ServerHandler;
use crate::client::SpawnSpec;
use crate::config::ActiveServer;
use crate::config::LspLimits;
use crate::config::ServerTable;
use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticSink;
use crate::operation::EditorPosition;
use crate::operation::Operation;
use crate::operation::request_for;
use crate::uri::path_to_uri;

/// Default `maxRestarts`: failed starts plus crashes tolerated before a server is given up (Claude Code's default).
pub const MAX_START_FAILURES: u32 = 3;
/// Retries of a request the server rejected with ContentModified (it was still processing an edit).
pub const CONTENT_MODIFIED_RETRIES: u32 = 3;
pub const CONTENT_MODIFIED_BASE_DELAY: Duration = Duration::from_millis(500);
/// Time a stopping server gets for `shutdown`/`exit` before its process tree is killed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Upper bound for the idle check interval.
pub const MAX_REAPER_TICK: Duration = Duration::from_secs(30);
/// Files that mark a project root when a file lies outside the working directory.
pub const ROOT_MARKERS: [&str; 7] = [
    ".git",
    "Cargo.toml",
    "package.json",
    "tsconfig.json",
    "pyproject.toml",
    "go.mod",
    "setup.py",
];

/// Everything a conversation brings to a call.
#[derive(Clone)]
pub struct LspContext {
    pub table: Arc<ServerTable>,
    pub limits: LspLimits,
    pub cwd: PathBuf,
    pub sink: Arc<DiagnosticSink>,
}

/// What a query produced.
#[derive(Debug, PartialEq)]
pub enum QueryOutcome {
    /// No configured server handles this file type.
    NoServer,
    /// The file exceeds [`LspLimits::max_file_bytes`].
    TooLarge { bytes: u64 },
    /// The server's answer (for call operations, the answer to the follow-up request; `Null` when the position has
    /// no call hierarchy item).
    Result { server: String, value: Value },
}

/// A change the agent made to a file.
pub enum FileEvent {
    Written(String),
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InstanceKey {
    server: String,
    fingerprint: u64,
    root: PathBuf,
}

struct DocumentState {
    version: i64,
    content_hash: u64,
}

struct Instance {
    key: InstanceKey,
    server: ActiveServer,
    root: PathBuf,
    idle_shutdown: Duration,
    client: OnceCell<Result<Arc<LspClient>, LspError>>,
    last_used: StdMutex<Instant>,
    in_flight: AtomicUsize,
    documents: Mutex<HashMap<String, DocumentState>>,
    sinks: Arc<StdMutex<Vec<Weak<DiagnosticSink>>>>,
}

impl Instance {
    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }

    fn subscribe(&self, sink: &Arc<DiagnosticSink>) {
        let mut sinks = self
            .sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sinks.retain(|existing| existing.strong_count() > 0);
        if !sinks
            .iter()
            .any(|existing| std::ptr::eq(existing.as_ptr(), Arc::as_ptr(sink)))
        {
            sinks.push(Arc::downgrade(sink));
        }
    }

    /// The started client, or `None` while it never started.
    fn running_client(&self) -> Option<Arc<LspClient>> {
        match self.client.get() {
            Some(Ok(client)) => Some(client.clone()),
            _ => None,
        }
    }

    /// Started successfully, then exited (a crash, or the server quit on its own).
    fn crashed(&self) -> bool {
        matches!(self.client.get(), Some(Ok(client)) if !client.is_alive())
    }

    fn is_dead(&self) -> bool {
        match self.client.get() {
            Some(Ok(client)) => !client.is_alive(),
            Some(Err(_)) => true,
            None => false,
        }
    }

    async fn stop(&self) {
        if let Some(client) = self.running_client() {
            client.shutdown(SHUTDOWN_GRACE).await;
        }
    }
}

/// Counts a call as in flight (the instance is not stopped for idleness or eviction meanwhile). Taken while the
/// instance table is locked, so the reaper never stops an instance between lookup and use.
struct InFlight(Arc<Instance>);

impl InFlight {
    fn new(instance: Arc<Instance>) -> Self {
        instance.in_flight.fetch_add(1, Ordering::SeqCst);
        instance.touch();
        Self(instance)
    }
}

impl std::ops::Deref for InFlight {
    type Target = Instance;
    fn deref(&self) -> &Instance {
        &self.0
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.touch();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

struct InstanceHandler {
    server: String,
    settings: Option<Value>,
    root_uri: Option<String>,
    root_name: String,
    sinks: Arc<StdMutex<Vec<Weak<DiagnosticSink>>>>,
}

impl ServerHandler for InstanceHandler {
    fn notification(&self, method: &str, params: Value) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            tracing::debug!("lsp {}: publishDiagnostics without uri", self.server);
            return;
        };
        let diagnostics: Vec<Diagnostic> = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Diagnostic::from_lsp).collect())
            .unwrap_or_default();
        let sinks: Vec<Arc<DiagnosticSink>> = self
            .sinks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for sink in sinks {
            sink.publish(&self.server, uri, diagnostics.clone());
        }
    }

    fn request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "workspace/configuration" => {
                let items = params
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(Value::Array(
                    items
                        .iter()
                        .map(|item| {
                            configuration_section(
                                self.settings.as_ref(),
                                item.get("section").and_then(Value::as_str),
                            )
                        })
                        .collect(),
                ))
            }
            "workspace/workspaceFolders" => Ok(match &self.root_uri {
                Some(uri) => json!([{"uri": uri, "name": self.root_name}]),
                None => Value::Null,
            }),
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "window/showMessageRequest"
            | "window/showDocument"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/diagnostic/refresh" => Ok(Value::Null),
            // The agent edits files itself; servers do not get to.
            "workspace/applyEdit" => Ok(
                json!({"applied": false, "failureReason": "edits are not applied by this client"}),
            ),
            _ => Err(RpcError {
                code: JSONRPC_METHOD_NOT_FOUND,
                message: format!("unsupported request {method}"),
            }),
        }
    }
}

/// The value at `section` (dotted: `"python.analysis"`) in `settings`; the whole settings without a section.
pub fn configuration_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(settings) = settings else {
        return Value::Null;
    };
    let Some(section) = section.filter(|s| !s.is_empty()) else {
        return settings.clone();
    };
    if let Some(direct) = settings.get(section) {
        return direct.clone();
    }
    section
        .split('.')
        .try_fold(settings, |value, part| value.get(part))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The working directory when `file` is inside it; else the nearest ancestor with a project marker; else its folder.
pub fn workspace_root(file: &Path, cwd: &Path) -> PathBuf {
    if file.starts_with(cwd) {
        return cwd.to_path_buf();
    }
    let parent = file.parent().unwrap_or(file);
    parent
        .ancestors()
        .find(|dir| ROOT_MARKERS.iter().any(|marker| dir.join(marker).exists()))
        .unwrap_or(parent)
        .to_path_buf()
}

/// The server's fixed `workspace_folder`, else [`workspace_root`].
fn server_root(server: &ActiveServer, file: &Path, cwd: &Path) -> PathBuf {
    server
        .spec
        .workspace_folder
        .clone()
        .unwrap_or_else(|| workspace_root(file, cwd))
}

fn gave_up(server: &str, max_restarts: u32) -> LspError {
    LspError::Other(format!(
        "language server `{server}` failed or crashed more than {max_restarts} times (maxRestarts) and will not be \
         started again in this session"
    ))
}

fn fingerprint(server: &ActiveServer) -> u64 {
    let mut hasher = DefaultHasher::new();
    server.program.hash(&mut hasher);
    server.spec.args.hash(&mut hasher);
    server.spec.env.hash(&mut hasher);
    server
        .spec
        .initialization_options
        .as_ref()
        .map(Value::to_string)
        .hash(&mut hasher);
    server
        .spec
        .settings
        .as_ref()
        .map(Value::to_string)
        .hash(&mut hasher);
    hasher.finish()
}

fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn initialize_params(server: &ActiveServer, root: &Path) -> Value {
    let root_uri = path_to_uri(root);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "codex-lsp"},
        "rootPath": root.to_string_lossy(),
        "rootUri": root_uri,
        "workspaceFolders": root_uri.as_ref().map(|uri| json!([{"uri": uri, "name": name}])),
        "initializationOptions": server.spec.initialization_options.clone().unwrap_or_else(|| json!({})),
        "capabilities": {
            "general": {"positionEncodings": ["utf-16"]},
            "workspace": {
                "configuration": true,
                "workspaceFolders": false,
                "symbol": {"dynamicRegistration": false},
            },
            "textDocument": {
                "synchronization": {"dynamicRegistration": false, "didSave": true, "willSave": false, "willSaveWaitUntil": false},
                "publishDiagnostics": {"relatedInformation": false, "versionSupport": false},
                "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
                "definition": {"dynamicRegistration": false, "linkSupport": true},
                "implementation": {"dynamicRegistration": false, "linkSupport": true},
                "references": {"dynamicRegistration": false},
                "documentSymbol": {"dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true},
                "callHierarchy": {"dynamicRegistration": false},
            },
        },
    })
}

#[derive(Default)]
pub struct LspManager {
    instances: StdMutex<HashMap<InstanceKey, Arc<Instance>>>,
    failures: StdMutex<HashMap<InstanceKey, u32>>,
    reaper_started: OnceLock<()>,
}

static GLOBAL: OnceLock<Arc<LspManager>> = OnceLock::new();

impl LspManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The manager shared by every conversation in this process.
    pub fn global() -> Arc<Self> {
        GLOBAL.get_or_init(Self::new).clone()
    }

    fn lock_instances(&self) -> std::sync::MutexGuard<'_, HashMap<InstanceKey, Arc<Instance>>> {
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Instances currently known (started or starting).
    pub fn instance_count(&self) -> usize {
        self.lock_instances().len()
    }

    /// Process ids of running servers (tests and diagnostics).
    pub fn running_pids(&self) -> Vec<u32> {
        self.lock_instances()
            .values()
            .filter_map(|instance| instance.running_client().and_then(|client| client.pid()))
            .collect()
    }

    fn ensure_reaper(self: &Arc<Self>, idle: Duration) {
        if self.reaper_started.set(()).is_err() {
            return;
        }
        let tick = (idle / 2).clamp(Duration::from_millis(10), MAX_REAPER_TICK);
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                manager.reap();
            }
        });
    }

    /// Requests waiting for an answer across running servers (tests: the pending tables never grow).
    pub fn pending_requests(&self) -> usize {
        self.lock_instances()
            .values()
            .filter_map(|instance| instance.running_client())
            .map(|client| client.pending_requests())
            .sum()
    }

    fn record_failure(&self, key: &InstanceKey) {
        *self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_default() += 1;
    }

    /// Stops idle and dead instances.
    fn reap(&self) {
        let stopped: Vec<Arc<Instance>> = {
            let mut instances = self.lock_instances();
            let expired: Vec<InstanceKey> = instances
                .iter()
                .filter(|(_, instance)| {
                    instance.in_flight.load(Ordering::SeqCst) == 0
                        && (instance.is_dead() || instance.idle_for() >= instance.idle_shutdown)
                })
                .map(|(key, _)| key.clone())
                .collect();
            expired
                .iter()
                .filter_map(|key| instances.remove(key))
                .collect()
        };
        for instance in stopped.iter().filter(|instance| instance.crashed()) {
            self.record_failure(&instance.key);
        }
        for instance in stopped {
            tracing::debug!("lsp {}: stopping (idle or exited)", instance.key.server);
            tokio::spawn(async move { instance.stop().await });
        }
    }

    /// Stops every server (agent shutdown).
    pub async fn shutdown_all(&self) {
        let all: Vec<Arc<Instance>> = self.lock_instances().drain().map(|(_, i)| i).collect();
        for instance in all {
            instance.stop().await;
        }
    }

    fn instance_for(
        self: &Arc<Self>,
        ctx: &LspContext,
        server: &ActiveServer,
        file: &Path,
    ) -> Result<InFlight, LspError> {
        let Some(program) = server.program.clone() else {
            return Err(LspError::CommandNotFound(server.spec.command.clone()));
        };
        let root = server_root(server, file, &ctx.cwd);
        let key = InstanceKey {
            server: server.spec.name.clone(),
            fingerprint: fingerprint(server),
            root: root.clone(),
        };
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied()
            .unwrap_or_default();
        let max_restarts = server.spec.max_restarts.unwrap_or(MAX_START_FAILURES);
        if failures > max_restarts {
            return Err(gave_up(&server.spec.name, max_restarts));
        }
        self.ensure_reaper(ctx.limits.idle_shutdown);
        let (instance, evicted) = {
            let mut instances = self.lock_instances();
            // A server that exited is replaced on this use (never in a loop), and a crash counts toward maxRestarts.
            if let Some(dead) = instances.get(&key).filter(|i| i.is_dead()).cloned() {
                instances.remove(&key);
                if dead.crashed() {
                    self.record_failure(&key);
                    if failures + 1 > max_restarts {
                        return Err(gave_up(&server.spec.name, max_restarts));
                    }
                }
            }
            if let Some(existing) = instances.get(&key) {
                (InFlight::new(existing.clone()), None)
            } else {
                let mut evicted = None;
                if instances.len() >= ctx.limits.max_servers.max(1) {
                    let victim = instances
                        .iter()
                        .filter(|(_, i)| i.in_flight.load(Ordering::SeqCst) == 0)
                        .max_by_key(|(_, i)| i.idle_for())
                        .map(|(k, _)| k.clone());
                    let Some(victim) = victim else {
                        return Err(LspError::Other(format!(
                            "{} language servers are already busy (limit: lsp.max_servers = {}); try again shortly",
                            instances.len(),
                            ctx.limits.max_servers
                        )));
                    };
                    evicted = instances.remove(&victim);
                }
                let instance = Arc::new(Instance {
                    key: key.clone(),
                    server: ActiveServer {
                        spec: server.spec.clone(),
                        program: Some(program),
                    },
                    root,
                    idle_shutdown: ctx.limits.idle_shutdown,
                    client: OnceCell::new(),
                    last_used: StdMutex::new(Instant::now()),
                    in_flight: AtomicUsize::new(0),
                    documents: Mutex::new(HashMap::new()),
                    sinks: Arc::new(StdMutex::new(Vec::new())),
                });
                instances.insert(key, instance.clone());
                (InFlight::new(instance), evicted)
            }
        };
        if let Some(evicted) = evicted {
            tracing::debug!(
                "lsp {}: stopping to stay within max_servers",
                evicted.key.server
            );
            tokio::spawn(async move { evicted.stop().await });
        }
        instance.subscribe(&ctx.sink);
        Ok(instance)
    }

    async fn started(
        &self,
        instance: &Instance,
        limits: &LspLimits,
    ) -> Result<Arc<LspClient>, LspError> {
        let result = instance
            .client
            .get_or_init(|| async {
                let started = start_client(instance, limits).await;
                if started.is_err() {
                    *self
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(instance.key.clone())
                        .or_default() += 1;
                }
                started
            })
            .await
            .clone();
        if result.is_err() {
            // Forget the failed instance so the next use tries again (until MAX_START_FAILURES).
            let mut instances = self.lock_instances();
            if instances
                .get(&instance.key)
                .is_some_and(|current| std::ptr::eq(current.as_ref(), instance))
            {
                instances.remove(&instance.key);
            }
        }
        let client = result?;
        if !client.is_alive() {
            // Counted as a crash when the instance is replaced or reaped.
            return Err(LspError::Exited {
                server: instance.key.server.clone(),
                detail: String::new(),
            });
        }
        Ok(client)
    }

    /// Sends the current text of `file` (opening it or sending a change when it differs from what the server has).
    async fn sync_document(
        &self,
        instance: &Instance,
        client: &LspClient,
        file: &Path,
        uri: &str,
        text: &str,
    ) -> Result<bool, LspError> {
        let hash = content_hash(text);
        let mut documents = instance.documents.lock().await;
        match documents.get_mut(uri) {
            Some(state) if state.content_hash == hash => Ok(false),
            Some(state) => {
                state.version += 1;
                state.content_hash = hash;
                client
                    .notify(
                        "textDocument/didChange",
                        json!({"textDocument": {"uri": uri, "version": state.version}, "contentChanges": [{"text": text}]}),
                    )
                    .await?;
                Ok(true)
            }
            None => {
                client
                    .notify(
                        "textDocument/didOpen",
                        json!({"textDocument": {
                            "uri": uri,
                            "languageId": instance.server.spec.language_id(file),
                            "version": 1,
                            "text": text,
                        }}),
                    )
                    .await?;
                documents.insert(
                    uri.to_string(),
                    DocumentState {
                        version: 1,
                        content_hash: hash,
                    },
                );
                Ok(true)
            }
        }
    }

    async fn request_with_retry(
        client: &LspClient,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let mut attempt = 0;
        loop {
            match client.request(method, params.clone(), timeout).await {
                Err(LspError::Rpc { code, .. })
                    if code == LSP_CONTENT_MODIFIED && attempt < CONTENT_MODIFIED_RETRIES =>
                {
                    tokio::time::sleep(CONTENT_MODIFIED_BASE_DELAY * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Runs one operation on `file` (absolute path).
    pub async fn query(
        self: &Arc<Self>,
        ctx: &LspContext,
        file: &Path,
        operation: Operation,
        position: EditorPosition,
    ) -> Result<QueryOutcome, LspError> {
        let Some(server) = ctx.table.server_for(file).cloned() else {
            return Ok(QueryOutcome::NoServer);
        };
        let bytes = tokio::fs::metadata(file)
            .await
            .map_err(|err| LspError::Other(format!("cannot read {}: {err}", file.display())))?
            .len();
        if bytes > ctx.limits.max_file_bytes {
            return Ok(QueryOutcome::TooLarge { bytes });
        }
        let text = tokio::fs::read_to_string(file)
            .await
            .map_err(|err| LspError::Other(format!("cannot read {}: {err}", file.display())))?;
        let uri = path_to_uri(file)
            .ok_or_else(|| LspError::Other(format!("not an absolute path: {}", file.display())))?;
        let instance = self.instance_for(ctx, &server, file)?;
        let client = self.started(&instance, &ctx.limits).await?;
        self.sync_document(&instance, &client, file, &uri, &text)
            .await?;
        let timeout = ctx.limits.request_timeout;
        let (method, params) = request_for(operation, &uri, position);
        let mut value = Self::request_with_retry(&client, method, params, timeout).await?;
        if let Some(follow_up) = operation.follow_up_method() {
            let first = value.as_array().and_then(|items| items.first()).cloned();
            value = match first {
                Some(item) => {
                    Self::request_with_retry(&client, follow_up, json!({"item": item}), timeout)
                        .await?
                }
                None => Value::Null,
            };
        }
        Ok(QueryOutcome::Result {
            server: server.spec.name.clone(),
            value,
        })
    }

    /// Keeps servers in step with a file the agent wrote or deleted, starting the server if needed (so its
    /// diagnostics reach the model). Never fails: problems are logged and the diagnostics wait is released.
    pub async fn file_changed(self: &Arc<Self>, ctx: &LspContext, file: &Path, event: FileEvent) {
        let Some(server) = ctx.table.server_for(file).cloned() else {
            return;
        };
        let Some(uri) = path_to_uri(file) else {
            return;
        };
        match event {
            FileEvent::Written(text) => {
                if text.len() as u64 > ctx.limits.max_file_bytes {
                    return;
                }
                ctx.sink.file_edited(&uri, ctx.limits.diagnostics_wait);
                let synced = async {
                    let instance = self.instance_for(ctx, &server, file)?;
                    let client = self.started(&instance, &ctx.limits).await?;
                    let changed = self
                        .sync_document(&instance, &client, file, &uri, &text)
                        .await?;
                    client
                        .notify(
                            "textDocument/didSave",
                            json!({"textDocument": {"uri": uri}}),
                        )
                        .await?;
                    Ok::<bool, LspError>(changed)
                }
                .await;
                if let Err(err) = synced {
                    tracing::debug!(
                        "lsp {}: could not sync {}: {err}",
                        server.spec.name,
                        file.display()
                    );
                    ctx.sink.stop_waiting(&uri);
                }
            }
            FileEvent::Deleted => {
                let instance = {
                    let root = server_root(&server, file, &ctx.cwd);
                    self.lock_instances()
                        .iter()
                        .find(|(key, _)| key.server == server.spec.name && key.root == root)
                        .map(|(_, instance)| instance.clone())
                };
                let Some(instance) = instance else {
                    return;
                };
                let Some(client) = instance.running_client() else {
                    return;
                };
                if instance.documents.lock().await.remove(&uri).is_some() {
                    let _ = client
                        .notify(
                            "textDocument/didClose",
                            json!({"textDocument": {"uri": uri}}),
                        )
                        .await;
                }
            }
        }
    }
}

async fn start_client(instance: &Instance, limits: &LspLimits) -> Result<Arc<LspClient>, LspError> {
    let server = &instance.server;
    let program = server
        .program
        .clone()
        .ok_or_else(|| LspError::CommandNotFound(server.spec.command.clone()))?;
    let env: Vec<(String, String)> = server
        .spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let root_uri = path_to_uri(&instance.root);
    let handler = Arc::new(InstanceHandler {
        server: server.spec.name.clone(),
        settings: server.spec.settings.clone(),
        root_uri,
        root_name: instance
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        sinks: instance.sinks.clone(),
    });
    let client = LspClient::spawn(
        SpawnSpec {
            server: &server.spec.name,
            program: &program,
            args: &server.spec.args,
            env: &env,
            cwd: &instance.root,
            max_message_bytes: limits.max_message_bytes,
        },
        handler,
    )?;
    let startup = server
        .spec
        .startup_timeout
        .unwrap_or(limits.startup_timeout);
    let initialized = client
        .request(
            "initialize",
            initialize_params(server, &instance.root),
            startup,
        )
        .await;
    match initialized {
        Ok(_) => {}
        Err(LspError::Timeout { .. }) => {
            client.kill();
            return Err(LspError::StartupTimeout {
                server: server.spec.name.clone(),
                seconds: startup.as_secs(),
            });
        }
        Err(err) => {
            client.kill();
            return Err(err);
        }
    }
    client.notify("initialized", json!({})).await?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn configuration_sections_resolve_direct_and_dotted_keys() {
        let settings =
            json!({"rust-analyzer": {"check": {"command": "clippy"}}, "python.analysis": {"x": 1}});
        assert_eq!(
            configuration_section(Some(&settings), Some("rust-analyzer.check")),
            json!({"command": "clippy"})
        );
        assert_eq!(
            configuration_section(Some(&settings), Some("python.analysis")),
            json!({"x": 1})
        );
        assert_eq!(
            configuration_section(Some(&settings), Some("missing")),
            Value::Null
        );
        assert_eq!(
            configuration_section(None, Some("rust-analyzer")),
            Value::Null
        );
        assert_eq!(configuration_section(Some(&settings), None), settings);
    }

    #[test]
    fn workspace_root_prefers_cwd_then_markers() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        let other = dir.path().join("other/crate");
        std::fs::create_dir_all(other.join("src")).unwrap();
        std::fs::write(other.join("Cargo.toml"), "").unwrap();
        assert_eq!(workspace_root(&cwd.join("src/a.rs"), &cwd), cwd);
        assert_eq!(workspace_root(&other.join("src/lib.rs"), &cwd), other);
    }
}
