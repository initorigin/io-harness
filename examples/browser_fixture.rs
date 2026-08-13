//! A fake browser that speaks the DevTools protocol over descriptors 3 and 4.
//!
//! The client is written over `AsyncRead + AsyncWrite`, so framing and
//! correlation are proven in-process over `tokio::io::duplex`. This program
//! proves the half that a duplex pair cannot: that a real child, spawned with the
//! real descriptor plumbing, is reachable — and it does it without depending on a
//! browser being installed on the machine running the suite, without a cold start
//! measured in seconds, and without a version this repository does not control.
//!
//! It is also deliberately controllable in ways a real browser is not. A test
//! points `IO_FIXTURE_RECORD` at a file and reads back exactly which requests
//! were continued and which were failed, which is how "the navigation was stopped
//! at the request" is asserted on the fixture's own record rather than on an
//! error message.
//!
//! Environment:
//!
//! - `IO_FIXTURE_RECORD` — append a line per interesting event to this path.
//! - `IO_FIXTURE_LINKS` — comma-separated URLs a click navigates to, in order.
//! - `IO_FIXTURE_TEXT` — what the page's text read returns.
//! - `IO_FIXTURE_NO_SELECTOR` — every selector matches nothing.
//! - `IO_FIXTURE_SILENT` — start, and never answer anything.

// The transport is two inherited descriptors, which is a unix arrangement. On
// Windows the browser feature refuses, so its fixture has nothing to do.
#[cfg(windows)]
fn main() {}

#[cfg(unix)]
use std::io::Write;

#[cfg(unix)]
use serde_json::{json, Value};

/// A 1×1 transparent PNG, so a screenshot returns real image bytes without this
/// file carrying a picture.
#[cfg(unix)]
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

#[cfg(unix)]
fn record(line: &str) {
    if let Ok(path) = std::env::var("IO_FIXTURE_RECORD") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(unix)]
fn main() {
    // The descriptors the parent installed. Reading 3 and writing 4 is the whole
    // transport; nothing here opens a port.
    let mut input = {
        use std::os::fd::FromRawFd;
        // SAFETY: descriptor 3 is the read end the parent installed before exec.
        unsafe { std::fs::File::from_raw_fd(3) }
    };
    let mut output = {
        use std::os::fd::FromRawFd;
        // SAFETY: descriptor 4 is the write end the parent installed before exec.
        unsafe { std::fs::File::from_raw_fd(4) }
    };

    record("started");
    if std::env::var("IO_FIXTURE_SILENT").is_ok() {
        // Started and useless: the parent must bound its wait rather than hang.
        std::thread::sleep(std::time::Duration::from_secs(600));
        return;
    }

    let mut say = |value: Value, out: &mut std::fs::File| {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(0);
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    };

    let links: Vec<String> = std::env::var("IO_FIXTURE_LINKS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut clicks = 0usize;
    let mut paused = 0u32;

    let mut buf = Vec::new();
    loop {
        let message = match read_message(&mut input, &mut buf) {
            Some(m) => m,
            None => return,
        };
        let id = message.get("id").and_then(Value::as_i64).unwrap_or(0);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let session = message
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let answer = |result: Value| json!({"id": id, "result": result});

        match method.as_str() {
            "Target.createTarget" => say(answer(json!({"targetId": "T1"})), &mut output),
            "Target.attachToTarget" => say(answer(json!({"sessionId": "S1"})), &mut output),
            "Page.enable" | "Runtime.enable" | "Emulation.setDeviceMetricsOverride" => {
                say(answer(json!({})), &mut output)
            }
            "Fetch.enable" => {
                record("fetch-enabled");
                say(answer(json!({})), &mut output);
            }
            // A navigation pauses first. The parent's answer to that pause is the
            // whole boundary, and it is what this fixture records.
            "Page.navigate" => {
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                paused += 1;
                let request_id = format!("R{paused}");
                record(&format!("navigate {url}"));
                let mut event = json!({
                    "method": "Fetch.requestPaused",
                    "params": {"requestId": request_id, "request": {"url": url},
                               "resourceType": "Document"}
                });
                if let Some(s) = &session {
                    event["sessionId"] = json!(s);
                }
                say(event, &mut output);
                // The answer to the navigate command itself comes after the pause
                // is resolved, which is the order a real browser uses.
                say(answer(json!({"frameId": "F1"})), &mut output);
            }
            // The parent's decision, recorded so a test asserts on what the
            // browser was told rather than on what the parent reported.
            "Fetch.continueRequest" => {
                let r = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                record(&format!("continue {r}"));
                say(answer(json!({})), &mut output);
            }
            "Fetch.failRequest" => {
                let r = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let reason = params
                    .get("errorReason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                record(&format!("fail {r} {reason}"));
                say(answer(json!({})), &mut output);
            }
            "Runtime.evaluate" => {
                let text = std::env::var("IO_FIXTURE_TEXT")
                    .unwrap_or_else(|_| "fixture page text".to_string());
                say(
                    answer(json!({"result": {"type": "string", "value": text}})),
                    &mut output,
                );
            }
            "Page.captureScreenshot" => say(answer(json!({"data": PIXEL})), &mut output),
            // Locating an element. A selector that matches nothing answers with a
            // node id of 0, which is what a real browser does and what the client
            // must turn into a named failure rather than a silent success.
            "DOM.getDocument" => say(answer(json!({"root": {"nodeId": 1}})), &mut output),
            "DOM.querySelector" => {
                let found = std::env::var("IO_FIXTURE_NO_SELECTOR").is_err();
                say(
                    answer(json!({"nodeId": if found { 2 } else { 0 }})),
                    &mut output,
                );
            }
            "DOM.getBoxModel" => say(
                answer(
                    json!({"model": {"content": [10, 10, 60, 10, 60, 30, 10, 30],
                                        "width": 50, "height": 20}}),
                ),
                &mut output,
            ),
            "DOM.focus" => say(answer(json!({})), &mut output),
            // A click may navigate — that is the case the whole gate exists for,
            // and the one no `Page.navigate` call appears in.
            "Input.dispatchMouseEvent" => {
                let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "mousePressed" {
                    record("click");
                    if let Some(url) = links.get(clicks) {
                        clicks += 1;
                        paused += 1;
                        let request_id = format!("R{paused}");
                        record(&format!("click-navigates {url}"));
                        let mut event = json!({
                            "method": "Fetch.requestPaused",
                            "params": {"requestId": request_id, "request": {"url": url},
                                       "resourceType": "Document"}
                        });
                        if let Some(s) = &session {
                            event["sessionId"] = json!(s);
                        }
                        say(event, &mut output);
                    }
                }
                say(answer(json!({})), &mut output);
            }
            "Input.insertText" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                record(&format!("type {text}"));
                say(answer(json!({})), &mut output);
            }
            "Input.dispatchKeyEvent" => say(answer(json!({})), &mut output),
            "Browser.close" => {
                record("closed");
                say(answer(json!({})), &mut output);
                return;
            }
            "Browser.getVersion" => say(
                answer(json!({"product": "io-harness-fixture/1"})),
                &mut output,
            ),
            _ => say(answer(json!({})), &mut output),
        }
    }
}

/// Read one NUL-terminated message.
#[cfg(unix)]
fn read_message(input: &mut std::fs::File, buf: &mut Vec<u8>) -> Option<Value> {
    use std::io::Read;
    loop {
        if let Some(i) = buf.iter().position(|b| *b == 0) {
            let raw: Vec<u8> = buf.drain(..=i).take(i).collect();
            return serde_json::from_slice(&raw).ok();
        }
        let mut chunk = [0u8; 4096];
        match input.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}
