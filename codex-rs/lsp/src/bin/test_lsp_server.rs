//! A fake language server for this crate's integration tests. Not shipped.
//!
//! Behaviour (all positions echo what the client sent, so tests can check the conversions):
//! - `definition` → one location at the requested position; `references` → two locations;
//!   `hover` → the `workspace/configuration` answer it got for section `fake` after `initialized`;
//!   `prepareCallHierarchy` → one item; `callHierarchy/incomingCalls` → one caller.
//! - `didOpen`/`didChange` publish one error per line containing `ERROR` (empty list otherwise).
//! - Environment: `FAKE_LSP_INIT_DELAY_MS` delays the `initialize` answer; `FAKE_LSP_HANG_METHOD` never answers that
//!   method; `FAKE_LSP_CHILD_PID_FILE` starts a long-running child and writes its pid there;
//!   `FAKE_LSP_CRASH_METHOD` exits with status 1 on that method; `FAKE_LSP_GARBAGE_METHOD` answers that method with
//!   an unreadable frame (and keeps running); `FAKE_LSP_SPAWN_LOG` appends one line per start;
//!   `FAKE_LSP_STDERR_FLOOD_BYTES` writes that much to stderr before reading anything; `FAKE_LSP_NO_PUBLISH` never
//!   publishes diagnostics; `FAKE_LSP_SERVER_REQUESTS` makes `hover` first send four requests of its own (and a reply
//!   to an id the client never used), then answer with the client's four replies.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;

fn read(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn send(message: &Value) {
    let body = message.to_string();
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

fn location(uri: &Value, position: &Value) -> Value {
    json!({"uri": uri, "range": {"start": position, "end": position}})
}

fn publish(uri: &Value, text: &str) {
    let diagnostics: Vec<Value> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("ERROR"))
        .map(|(index, _)| {
            json!({"message": "fake error", "severity": 1, "source": "fake",
                "range": {"start": {"line": index, "character": 0}, "end": {"line": index, "character": 5}}})
        })
        .collect();
    send(
        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
        "params": {"uri": uri, "diagnostics": diagnostics}}),
    );
}

/// Requests `hover` sends when `FAKE_LSP_SERVER_REQUESTS` is set.
const SERVER_REQUESTS: [(&str, &str); 4] = [
    ("q1", "window/workDoneProgress/create"),
    ("q2", "client/registerCapability"),
    ("q3", "workspace/applyEdit"),
    ("q4", "x/somethingUnknown"),
];

fn main() {
    let init_delay = std::env::var("FAKE_LSP_INIT_DELAY_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok());
    let hang_method = std::env::var("FAKE_LSP_HANG_METHOD").ok();
    let _child = std::env::var("FAKE_LSP_CHILD_PID_FILE")
        .ok()
        .and_then(|file| {
            let child = std::process::Command::new("sleep")
                .arg("1000")
                .spawn()
                .ok()?;
            std::fs::write(file, child.id().to_string()).ok()?;
            Some(child)
        });
    let crash_method = std::env::var("FAKE_LSP_CRASH_METHOD").ok();
    let garbage_method = std::env::var("FAKE_LSP_GARBAGE_METHOD").ok();
    let no_publish = std::env::var_os("FAKE_LSP_NO_PUBLISH").is_some();
    let server_requests = std::env::var_os("FAKE_LSP_SERVER_REQUESTS").is_some();
    if let Ok(log) = std::env::var("FAKE_LSP_SPAWN_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("spawn log");
        let _ = writeln!(file, "{}", std::process::id());
    }
    if let Some(bytes) = std::env::var("FAKE_LSP_STDERR_FLOOD_BYTES")
        .ok()
        .and_then(|b| b.parse::<usize>().ok())
    {
        let line = "x".repeat(1023) + "\n";
        let mut err = std::io::stderr().lock();
        for _ in 0..bytes / 1024 {
            let _ = err.write_all(line.as_bytes());
        }
    }
    // `hover` waiting for the client's replies to its own requests: (hover id, replies so far).
    let mut awaiting: Option<(Value, Vec<Value>)> = None;
    let mut configuration = Value::Null;
    let mut reader = BufReader::new(std::io::stdin().lock());
    while let Some(message) = read(&mut reader) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if method.is_empty() {
            // Answer to our own `workspace/configuration` request.
            if message.get("id") == Some(&json!("config")) {
                configuration = message["result"][0].clone();
            } else if let Some((hover_id, replies)) = awaiting.as_mut() {
                replies.push(message.clone());
                if replies.len() == SERVER_REQUESTS.len() {
                    send(&json!({"jsonrpc": "2.0", "id": hover_id, "result": replies}));
                    awaiting = None;
                }
            }
            continue;
        }
        if crash_method.as_deref() == Some(method.as_str()) {
            eprintln!("fake: crashing on {method}");
            std::process::exit(1);
        }
        if garbage_method.as_deref() == Some(method.as_str()) {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "Content-Length: not-a-number\r\n\r\n{{}}");
            let _ = out.flush();
            continue;
        }
        if server_requests && method == "textDocument/hover" {
            // A reply nobody asked for comes first; the client must ignore it.
            send(&json!({"jsonrpc": "2.0", "id": 999_999, "result": "stray"}));
            for (request_id, request_method) in SERVER_REQUESTS {
                send(
                    &json!({"jsonrpc": "2.0", "id": request_id, "method": request_method, "params": {}}),
                );
            }
            awaiting = Some((id.clone().unwrap_or(Value::Null), Vec::new()));
            continue;
        }
        if hang_method.as_deref() == Some(method.as_str()) {
            continue;
        }
        let uri = &params["textDocument"]["uri"];
        let position = &params["position"];
        let result = match method.as_str() {
            "initialize" => {
                if let Some(ms) = init_delay {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                json!({"capabilities": {"textDocumentSync": 1, "definitionProvider": true}})
            }
            "initialized" => {
                send(
                    &json!({"jsonrpc": "2.0", "id": "config", "method": "workspace/configuration",
                    "params": {"items": [{"section": "fake"}]}}),
                );
                continue;
            }
            "textDocument/didOpen" | "textDocument/didChange" if no_publish => continue,
            "textDocument/didOpen" => {
                publish(
                    uri,
                    params["textDocument"]["text"].as_str().unwrap_or_default(),
                );
                continue;
            }
            "textDocument/didChange" => {
                publish(
                    uri,
                    params["contentChanges"][0]["text"]
                        .as_str()
                        .unwrap_or_default(),
                );
                continue;
            }
            "textDocument/definition" => location(uri, position),
            "textDocument/references" => json!([
                location(uri, position),
                location(uri, &json!({"line": 0, "character": 0}))
            ]),
            "textDocument/hover" => {
                json!({"contents": {"kind": "plaintext", "value": configuration.to_string()}})
            }
            "textDocument/prepareCallHierarchy" => {
                json!([{"name": "callee", "kind": 12, "uri": uri,
                "range": {"start": position, "end": position}, "selectionRange": {"start": position, "end": position}}])
            }
            "callHierarchy/incomingCalls" => {
                json!([{"from": {"name": "caller", "kind": 12, "uri": params["item"]["uri"],
                "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 1}}},
                "fromRanges": [{"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 3}}]}])
            }
            "shutdown" => Value::Null,
            // The child is left running on purpose: the client must kill the whole process group.
            "exit" => return,
            _ if id.is_none() => continue,
            _ => {
                send(
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unsupported"}}),
                );
                continue;
            }
        };
        if id.is_some() {
            send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
    }
}
