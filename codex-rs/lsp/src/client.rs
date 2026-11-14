//! One language-server process over stdio: JSON-RPC requests with timeouts, notifications, answers to the server's
//! own requests, and a process tree that dies with the client.
//!
//! The process runs in its own process group (Unix; on Linux it also gets SIGTERM when the agent dies) or job object
//! (Windows), and the whole group/job is killed when the client is dropped, so helpers the server starts (tsserver,
//! cargo check) do not outlive it.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::framing;

/// Lines of the server's stderr kept for error messages.
const STDERR_TAIL_LINES: usize = 20;
/// Longest stderr line kept.
const STDERR_LINE_CHARS: usize = 300;

pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const LSP_CONTENT_MODIFIED: i64 = -32801;
pub const LSP_REQUEST_CANCELLED: i64 = -32800;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LspError {
    #[error(
        "language server command `{0}` was not found on this computer; install it or change `command` for this server in [lsp.servers]"
    )]
    CommandNotFound(String),
    #[error("could not start language server `{server}`: {message}")]
    Spawn { server: String, message: String },
    #[error("language server `{server}` did not finish starting within {seconds} s")]
    StartupTimeout { server: String, seconds: u64 },
    #[error("language server `{server}` did not answer `{method}` within {seconds} s")]
    Timeout {
        server: String,
        method: String,
        seconds: u64,
    },
    #[error("language server `{server}` exited{detail}")]
    Exited { server: String, detail: String },
    #[error("language server `{server}` returned error {code}: {message}")]
    Rpc {
        server: String,
        code: i64,
        message: String,
    },
    #[error("{0}")]
    Other(String),
}

/// An error answer to a server-initiated request.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// What the client does with traffic the server starts.
pub trait ServerHandler: Send + Sync + 'static {
    fn notification(&self, method: &str, params: Value);
    fn request(&self, method: &str, params: &Value) -> Result<Value, RpcError>;
}

pub struct SpawnSpec<'a> {
    pub server: &'a str,
    pub program: &'a Path,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    pub max_message_bytes: usize,
}

type Pending = Arc<StdMutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

struct ProcessGuard {
    child: Child,
    #[cfg(windows)]
    _job: Option<codex_utils_pty::JobObject>,
}

impl ProcessGuard {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The server leads its own group (`process_group(0)`), so this also stops its helpers.
            let _ = codex_utils_pty::process_group::kill_process_group(pid);
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // Windows: dropping the job (kill-on-close) ends the whole tree.
        self.kill();
    }
}

pub struct LspClient {
    server: String,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
    exited: Arc<Notify>,
    stderr_tail: Arc<StdMutex<VecDeque<String>>>,
    process: StdMutex<Option<ProcessGuard>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        // The guard's own drop kills the process tree.
        self.process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// Removes a request from the pending table when its caller stops waiting (timeout or cancellation).
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl LspClient {
    pub fn spawn(
        spec: SpawnSpec<'_>,
        handler: Arc<dyn ServerHandler>,
    ) -> Result<Arc<Self>, LspError> {
        let mut command =
            tokio::process::Command::from(codex_utils_process::background_command(spec.program));
        command
            .args(spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(target_os = "linux")]
        {
            let parent = std::process::id() as libc::pid_t;
            // SAFETY: the closure only makes async-signal-safe calls (prctl, getppid, raise).
            unsafe {
                command.pre_exec(move || {
                    codex_utils_pty::process_group::set_parent_death_signal(parent)
                });
            }
        }
        let spawn_error = |err: std::io::Error| {
            if err.kind() == std::io::ErrorKind::NotFound {
                LspError::CommandNotFound(spec.program.display().to_string())
            } else {
                LspError::Spawn {
                    server: spec.server.to_string(),
                    message: err.to_string(),
                }
            }
        };
        #[cfg(windows)]
        let (mut child, job) =
            codex_utils_pty::JobObject::spawn_background(&mut command).map_err(spawn_error)?;
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(spawn_error)?;

        let missing = |what: &str| LspError::Spawn {
            server: spec.server.to_string(),
            message: format!("no {what} pipe"),
        };
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(Notify::new());
        let stderr_tail = Arc::new(StdMutex::new(VecDeque::new()));

        let reader = tokio::spawn(read_loop(ReadLoop {
            server: spec.server.to_string(),
            stdout,
            stdin: stdin.clone(),
            pending: pending.clone(),
            alive: alive.clone(),
            exited: exited.clone(),
            stderr_tail: stderr_tail.clone(),
            handler,
            max_message_bytes: spec.max_message_bytes,
        }));
        let stderr_reader = tokio::spawn(drain_stderr(
            spec.server.to_string(),
            stderr,
            stderr_tail.clone(),
        ));

        Ok(Arc::new(Self {
            server: spec.server.to_string(),
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            alive,
            exited,
            stderr_tail,
            process: StdMutex::new(Some(ProcessGuard {
                child,
                #[cfg(windows)]
                _job: job,
            })),
            tasks: vec![reader, stderr_reader],
        }))
    }

    /// Requests waiting for an answer (each leaves on answer, timeout, cancellation or exit).
    pub fn pending_requests(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// The process id (for tests and logs).
    pub fn pid(&self) -> Option<u32> {
        self.process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|guard| guard.child.id())
    }

    fn exited_error(&self) -> LspError {
        exited_error(&self.server, &self.stderr_tail)
    }

    async fn write(&self, message: &Value) -> Result<(), LspError> {
        let bytes = framing::encode(message);
        let mut stdin = self.stdin.lock().await;
        let written = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        }
        .await;
        written.map_err(|_| self.exited_error())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        if !self.is_alive() {
            return Err(self.exited_error());
        }
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        if !self.is_alive() {
            return Err(self.exited_error());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let _guard = PendingGuard {
            pending: &self.pending,
            id,
        };
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.exited_error()),
            Err(_) => {
                let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
                Err(LspError::Timeout {
                    server: self.server.clone(),
                    method: method.to_string(),
                    seconds: timeout.as_secs(),
                })
            }
        }
    }

    /// `shutdown` then `exit`, each bounded by `grace`; the process tree is killed afterwards in any case.
    pub async fn shutdown(&self, grace: Duration) {
        if self.is_alive() {
            let _ = self.request("shutdown", Value::Null, grace).await;
            let _ = self.notify("exit", Value::Null).await;
            let exited = self.exited.notified();
            if self.is_alive() {
                let _ = tokio::time::timeout(grace, exited).await;
            }
        }
        self.kill();
    }

    pub fn kill(&self) {
        if let Some(mut guard) = self
            .process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            guard.kill();
        }
    }
}

fn exited_error(server: &str, stderr_tail: &StdMutex<VecDeque<String>>) -> LspError {
    let tail = stderr_tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let detail = if tail.is_empty() {
        String::new()
    } else {
        format!(
            "; last output: {}",
            tail.iter().cloned().collect::<Vec<_>>().join(" | ")
        )
    };
    LspError::Exited {
        server: server.to_string(),
        detail,
    }
}

struct ReadLoop {
    server: String,
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    exited: Arc<Notify>,
    stderr_tail: Arc<StdMutex<VecDeque<String>>>,
    handler: Arc<dyn ServerHandler>,
    max_message_bytes: usize,
}

async fn read_loop(state: ReadLoop) {
    let ReadLoop {
        server,
        stdout,
        stdin,
        pending,
        alive,
        exited,
        stderr_tail,
        handler,
        max_message_bytes,
    } = state;
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match framing::read_message(&mut reader, max_message_bytes).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(err) => {
                tracing::warn!("lsp {server}: unreadable message, closing: {err}");
                break;
            }
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let reply = match handler.request(&method, &params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(err) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": err.code, "message": err.message}})
                    }
                };
                let bytes = framing::encode(&reply);
                let mut stdin = stdin.lock().await;
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            (Some(method), None) => {
                handler.notification(
                    &method,
                    message.get("params").cloned().unwrap_or(Value::Null),
                );
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else {
                    continue;
                };
                let sender = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                let Some(sender) = sender else {
                    continue;
                };
                let result = match message.get("error") {
                    Some(error) => Err(LspError::Rpc {
                        server: server.clone(),
                        code: error
                            .get("code")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(result);
            }
            (None, None) => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    let senders: Vec<_> = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .map(|(_, sender)| sender)
        .collect();
    for sender in senders {
        let _ = sender.send(Err(exited_error(&server, &stderr_tail)));
    }
    exited.notify_waiters();
}

async fn drain_stderr(
    server: String,
    stderr: tokio::process::ChildStderr,
    tail: Arc<StdMutex<VecDeque<String>>>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("lsp {server} stderr: {line}");
        let line: String = line.chars().take(STDERR_LINE_CHARS).collect();
        let mut tail = tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}
