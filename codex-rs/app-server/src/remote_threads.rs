//! Which sessions remote control may reach (11-07, `docs/agent-codex/REMOTE_CONTROL.md` R9: like the Claude
//! desktop app's Remote Control switch on each session).
//!
//! Off unless the launcher names a sessions file (`remote-control --sessions-file <PATH>`). The desktop app owns
//! that file and rewrites it whenever the user flips a session's switch:
//!
//! ```json
//! {"relay": true, "mode": "listed", "threadIds": ["019a…"]}
//! ```
//!
//! - `relay`: whether this computer is on the relay at all (the app server connects or disconnects).
//! - `mode: "all"`: every session, and a phone may start new ones (what remote control did before).
//! - `mode: "listed"`: only the sessions in `threadIds`. A remote request that names a thread must name a listed
//!   one; starting, forking or creating anything is refused; `thread/list` returns only listed sessions; terminal
//!   commands are refused (reading files stays, inside `--allow` as before); `remoteControl/*` is refused, so a
//!   phone cannot widen its own access. Methods not in [`LISTED_METHODS`] are refused (fail closed).
//!
//! The file is read again for every remote request and once a second by the server loop, which disconnects every
//! remote client when a session was taken away (they reconnect and only see what is still shared) and applies
//! `relay`. A missing or malformed file shares nothing and keeps the relay off (fail closed).
//!
//! Local connections (the app's own, stdio, in-process) are never checked.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use serde::Deserialize;

/// A larger file is not one the app wrote.
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Exposure {
    All,
    Listed,
}

/// What remote clients may reach right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shared {
    pub(crate) relay: bool,
    pub(crate) exposure: Exposure,
    pub(crate) threads: HashSet<String>,
}

impl Shared {
    fn everything() -> Self {
        Self {
            relay: true,
            exposure: Exposure::All,
            threads: HashSet::new(),
        }
    }

    fn nothing() -> Self {
        Self {
            relay: false,
            exposure: Exposure::Listed,
            threads: HashSet::new(),
        }
    }

    /// Whether a remote client may see `thread_id` (new sessions included).
    pub(crate) fn allows_thread(&self, thread_id: &str) -> bool {
        self.exposure == Exposure::All || self.threads.contains(thread_id)
    }

    /// Whether going from `self` to `next` takes a session (or every session) away from remote clients.
    pub(crate) fn narrowed_to(&self, next: &Shared) -> bool {
        match (self.exposure, next.exposure) {
            (_, Exposure::All) => false,
            (Exposure::All, Exposure::Listed) => true,
            (Exposure::Listed, Exposure::Listed) => !self.threads.is_subset(&next.threads),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionsFile {
    relay: bool,
    mode: Exposure,
    #[serde(default)]
    thread_ids: Vec<String>,
}

/// The file's contents, or `None` when it is not a sessions file.
pub(crate) fn parse(raw: &str) -> Option<Shared> {
    let file: SessionsFile = serde_json::from_str(raw).ok()?;
    Some(Shared {
        relay: file.relay,
        exposure: file.mode,
        threads: file
            .thread_ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .collect(),
    })
}

/// How a method is judged while only listed sessions are shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Names no session (`--allow` still applies to its paths).
    Free,
    /// `params.threadId` must be a listed session.
    Thread,
}

/// Everything a remote client may call in `listed` mode; anything else is refused.
const LISTED_METHODS: &[(&str, Rule)] = &[
    ("model/list", Rule::Free),
    ("thread/list", Rule::Free),
    ("project/list", Rule::Free),
    ("fs/readFile", Rule::Free),
    ("fs/readDirectory", Rule::Free),
    ("fs/getMetadata", Rule::Free),
    ("fs/searchText", Rule::Free),
    ("fuzzyFileSearch", Rule::Free),
    ("thread/resume", Rule::Thread),
    ("thread/read", Rule::Thread),
    ("thread/unsubscribe", Rule::Thread),
    ("thread/name/set", Rule::Thread),
    ("thread/metadata/update", Rule::Thread),
    ("thread/compact/start", Rule::Thread),
    ("turn/start", Rule::Thread),
    ("turn/steer", Rule::Thread),
    ("turn/interrupt", Rule::Thread),
];

/// Whether `request` from a remote client is allowed under `shared`.
pub(crate) fn check_request(shared: &Shared, request: &ClientRequest) -> Result<(), String> {
    if shared.exposure == Exposure::All {
        return Ok(());
    }
    let method = request.method_name();
    match LISTED_METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, rule)| *rule)
    {
        None => Err(format!(
            "{method} is not available: this computer shares only the sessions chosen for remote control"
        )),
        Some(Rule::Free) => Ok(()),
        Some(Rule::Thread) => match thread_id(request) {
            Some(id) if shared.threads.contains(&id) => Ok(()),
            Some(id) => Err(format!(
                "session {id} is not shared for remote control on this computer"
            )),
            None => Err(format!("{method} needs a threadId")),
        },
    }
}

/// `params.threadId` of a request.
fn thread_id(request: &ClientRequest) -> Option<String> {
    let value = serde_json::to_value(request).ok()?;
    value
        .get("params")?
        .get("threadId")?
        .as_str()
        .map(str::to_string)
}

/// Removes the sessions remote clients may not see from a response.
pub(crate) fn filter_response(
    shared: &Shared,
    payload: ClientResponsePayload,
) -> ClientResponsePayload {
    if shared.exposure == Exposure::All {
        return payload;
    }
    match payload {
        ClientResponsePayload::ThreadList(mut response) => {
            response
                .data
                .retain(|thread| shared.threads.contains(&thread.id));
            ClientResponsePayload::ThreadList(response)
        }
        other => other,
    }
}

/// The sessions file named at startup, if any.
#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteThreads {
    file: Option<PathBuf>,
}

impl RemoteThreads {
    pub(crate) fn new(file: Option<PathBuf>) -> Self {
        Self { file }
    }

    /// No sessions file: remote control reaches every session, as upstream.
    pub(crate) fn off() -> Self {
        Self { file: None }
    }

    pub(crate) fn is_on(&self) -> bool {
        self.file.is_some()
    }

    /// What is shared now (read from the file each time; it is small and only the app writes it).
    pub(crate) fn current(&self) -> Arc<Shared> {
        let Some(path) = &self.file else {
            return Arc::new(Shared::everything());
        };
        let read = std::fs::File::open(path).and_then(|file| {
            let mut raw = String::new();
            file.take(MAX_FILE_BYTES + 1).read_to_string(&mut raw)?;
            Ok(raw)
        });
        let shared = match read {
            Ok(raw) if raw.len() as u64 <= MAX_FILE_BYTES => parse(&raw),
            _ => None,
        };
        Arc::new(shared.unwrap_or_else(Shared::nothing))
    }

    pub(crate) fn check(&self, request: &ClientRequest) -> Result<(), String> {
        if !self.is_on() {
            return Ok(());
        }
        check_request(&self.current(), request)
    }

    pub(crate) fn filter(&self, payload: ClientResponsePayload) -> ClientResponsePayload {
        if !self.is_on() {
            return payload;
        }
        filter_response(&self.current(), payload)
    }
}

#[cfg(test)]
#[path = "remote_threads_tests.rs"]
mod remote_threads_tests;
