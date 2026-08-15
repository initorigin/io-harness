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
//! Settings arrive as command-line arguments rather than as environment
//! variables, and that is not a style choice: `std::env::set_var` is
//! process-global, so a suite running these tests in parallel would have one
//! test's settings decide another test's child. An argument belongs to one spawn.
//!
//! - `--io-fixture-record=<path>` — append a line per interesting event.
//! - `--io-fixture-links=<url,url>` — URLs a click navigates to, in order.
//! - `--io-fixture-text=<text>` — what the page's text read returns.
//! - `--io-fixture-no-selector` — every selector matches nothing.
//! - `--io-fixture-console` — the page logs and throws while loading.
//! - `--io-fixture-silent` — start, and never answer anything.

use std::io::Write;

use serde_json::{json, Value};

/// The two descriptors the parent installed, opened the way each platform
/// installs them.
///
/// **Both are descriptor numbers, and on Windows that is not the same as a
/// handle.** A real browser asks its C runtime for `_get_osfhandle(3)`, so the
/// parent writes the runtime's own descriptor table rather than passing handles,
/// and this fixture reads the table back the same way a real browser does — which
/// is the point of it being a child at all.
fn transport() -> (std::fs::File, std::fs::File) {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // SAFETY: 3 and 4 are the ends the parent installed before exec.
        unsafe {
            (
                std::fs::File::from_raw_fd(3),
                std::fs::File::from_raw_fd(4),
            )
        }
    }
    #[cfg(windows)]
    {
        // Declared rather than depended on: `libc` is a unix-only dependency of
        // this crate, and this symbol is in the UCRT every Windows binary links.
        extern "C" {
            fn _get_osfhandle(fd: i32) -> isize;
        }
        use std::os::windows::io::FromRawHandle;
        // SAFETY: the runtime's table holds 3 and 4 because the parent wrote
        // them into `lpReserved2`; a descriptor the parent did not install
        // answers -1, which fails loudly on first use rather than silently.
        unsafe {
            (
                std::fs::File::from_raw_handle(_get_osfhandle(3) as _),
                std::fs::File::from_raw_handle(_get_osfhandle(4) as _),
            )
        }
    }
}

/// A 1×1 transparent PNG, so a screenshot returns real image bytes without this
/// file carrying a picture.
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// One `--io-fixture-<name>=<value>` argument, if it was given.
fn arg(name: &str) -> Option<String> {
    let prefix = format!("--io-fixture-{name}=");
    std::env::args().find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
}

/// Whether a bare `--io-fixture-<name>` flag was given.
fn flag(name: &str) -> bool {
    let want = format!("--io-fixture-{name}");
    std::env::args().any(|a| a == want)
}

fn record(line: &str) {
    if let Some(path) = arg("record") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn main() {
    // Reading 3 and writing 4 is the whole transport; nothing here opens a port.
    let (mut input, mut output) = transport();

    // The pid, so a test can assert the process is gone after the run rather
    // than assert that a shutdown call was made.
    record(&format!("started {}", std::process::id()));
    // And the argv it was actually started with, so a test can assert what the
    // launch passed rather than what a copy of the argument list says. This is
    // the only place the two could disagree, and on Windows the spawn builds one
    // command line by hand rather than handing a vector to `Command`.
    record(&format!(
        "argv {}",
        std::env::args().collect::<Vec<_>>().join(" ")
    ));
    if flag("silent") {
        // Started and useless: the parent must bound its wait rather than hang.
        std::thread::sleep(std::time::Duration::from_secs(600));
        return;
    }

    let say = |value: Value, out: &mut std::fs::File| {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(0);
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    };

    let links: Vec<String> = arg("links")
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
                // What the page said while loading. A console line and an
                // uncaught error, in the exact shape the real browser sends them
                // — `text` is the useless word `Uncaught` and the readable
                // message is in the exception's description.
                if flag("console") {
                    let mut log = json!({"method": "Runtime.consoleAPICalled",
                                         "params": {"type": "log",
                                                    "args": [{"value": "page said hello"}]}});
                    let mut boom = json!({"method": "Runtime.exceptionThrown",
                                          "params": {"exceptionDetails": {
                                              "text": "Uncaught",
                                              "exception": {"description":
                                                  "TypeError: undefined is not a function"}}}});
                    if let Some(s) = &session {
                        log["sessionId"] = json!(s);
                        boom["sessionId"] = json!(s);
                    }
                    say(log, &mut output);
                    say(boom, &mut output);
                }
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
                let expression = params
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // A page that reports its load state is what lets the parent
                // settle without waiting out its bound on every navigation.
                let text = if expression.contains("readyState") {
                    "complete".to_string()
                } else {
                    arg("text").unwrap_or_else(|| "fixture page text".to_string())
                };
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
                let found = !flag("no-selector");
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
