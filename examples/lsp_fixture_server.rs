//! A language server that answers exactly what a test told it to, and
//! misbehaves exactly when a test told it to.
//!
//! The point of a fixture here is not to be a small real server — it is to be a
//! server that can be *wrong on purpose*. A real `rust-analyzer` cannot be asked
//! to hang its handshake, omit a capability, or exit after `initialize`, and
//! those are the paths 0.52.0 has to prove. It also answers instantly, which is
//! what lets these tests run on every platform without gating on a clock.
//!
//! It is driven by one JSON file, named in `IO_HARNESS_LSP_SCRIPT`:
//!
//! ```json
//! {
//!   "capabilities": {"hoverProvider": true},
//!   "hang_initialize": false,
//!   "exit_after_initialize": false,
//!   "responses": {"textDocument/definition": {"uri": "file:///x", "range": {}}}
//! }
//! ```
//!
//! Two response values are special rather than literal:
//!
//! - `"echo-document"` answers with the text this server was last *sent* for the
//!   document the request names. That is what proves a client re-syncs a file it
//!   edited instead of answering from a buffer it opened once.
//! - `"echo-position"` answers with the `line`/`character` the request carried,
//!   on the wire, which is what proves the 1-based/0-based conversion.
//!
//! Everything else is returned as written.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};

use serde_json::{json, Value};

fn main() {
    // Proof of existence, for the tests that assert a process was NOT started.
    // A refusal is only worth asserting on the absence of the child, and an error
    // message can say "refused" while a server is already running.
    if let Ok(path) = std::env::var("IO_HARNESS_LSP_TOUCH") {
        let _ = std::fs::write(&path, "started");
    }

    let script: Value = match std::env::var("IO_HARNESS_LSP_SCRIPT") {
        Ok(path) => {
            serde_json::from_str(&std::fs::read_to_string(path).expect("script is readable"))
                .expect("script is JSON")
        }
        Err(_) => json!({}),
    };

    let mut stdin = BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout();
    // The text this server was last sent per document uri.
    let mut documents: HashMap<String, String> = HashMap::new();

    while let Some(message) = read_frame(&mut stdin) {
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method.as_str() {
            "initialize" => {
                if script["hang_initialize"].as_bool().unwrap_or(false) {
                    // Read on, answering nothing. A server that never finishes
                    // starting is not the same as one that died.
                    continue;
                }
                let caps = script
                    .get("capabilities")
                    .cloned()
                    .unwrap_or_else(default_capabilities);
                answer(&mut stdout, id, json!({"capabilities": caps}));
                if script["exit_after_initialize"].as_bool().unwrap_or(false) {
                    return;
                }
            }
            "initialized" => {}
            "textDocument/didOpen" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                let text = params["textDocument"]["text"].as_str().unwrap_or_default();
                documents.insert(uri.to_string(), text.to_string());
            }
            "textDocument/didClose" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                documents.remove(uri);
            }
            "shutdown" => answer(&mut stdout, id, Value::Null),
            "exit" => return,
            _ => {
                let scripted = script["responses"].get(&method).cloned();
                let result = match scripted.as_ref().and_then(Value::as_str) {
                    Some("echo-document") => {
                        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                        json!({"contents": documents.get(uri).cloned().unwrap_or_default()})
                    }
                    Some("echo-position") => json!({"contents": format!(
                        "line={} character={}",
                        params["position"]["line"], params["position"]["character"]
                    )}),
                    _ => scripted.unwrap_or(Value::Null),
                };
                answer(&mut stdout, id, result);
            }
        }
    }
}

/// What this server claims to do when the script does not say.
fn default_capabilities() -> Value {
    json!({
        "definitionProvider": true,
        "referencesProvider": true,
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "hoverProvider": true,
        "renameProvider": true,
        "diagnosticProvider": {"interFileDependencies": false, "workspaceDiagnostics": true},
        "textDocumentSync": 1,
    })
}

fn answer(out: &mut impl Write, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("stdout accepts a frame");
    out.flush().expect("stdout flushes");
}

/// Read one `Content-Length`-framed message, or `None` at end of stream.
fn read_frame(input: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = rest.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; len?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}
