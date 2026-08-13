//! Drive the browser installed on this machine, through the crate's own API.
//!
//! A fixture proves the client and cannot prove the protocol was understood. The
//! previous release learned that expensively: three defects were found only by
//! running against a real language server, every one invisible to a suite that
//! was green. So this is a gate of the release rather than an optional extra, and
//! what it finds is written into the record.
//!
//! It also produces the release's own measurement: the same page as text and as a
//! screenshot, so an operator can see what a picture costs against what it buys.
//!
//! Run it with:
//!
//! ```text
//! cargo run --features browser --example browser_live
//! ```
//!
//! It needs a browser installed and nothing else — the page it drives is served
//! by this file, on loopback, so there is no network dependency and the host gate
//! is exercised against a real `host:port` rather than a `data:` URL that reaches
//! no host at all.

#[cfg(not(all(feature = "browser", unix)))]
fn main() {
    eprintln!("the browser feature is unix-only in 0.53.0; nothing to run here");
}

#[cfg(all(feature = "browser", unix))]
use std::io::{Read, Write};
#[cfg(all(feature = "browser", unix))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(all(feature = "browser", unix))]
use std::sync::Mutex;

#[cfg(all(feature = "browser", unix))]
use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
#[cfg(all(feature = "browser", unix))]
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
#[cfg(all(feature = "browser", unix))]
use io_harness::{ApproveAll, BrowserConfig, Policy, Provider, Store, TaskContract, Verification};
#[cfg(all(feature = "browser", unix))]
use serde_json::{json, Value};

/// The page the run drives: something to read, something to click, something to
/// type into, and a script that logs and throws.
#[cfg(all(feature = "browser", unix))]
const PAGE: &str = r#"<!doctype html><html><body>
<h1 id="title">Live page</h1>
<p>The quick brown fox jumps over the lazy dog.</p>
<button id="go" onclick="document.getElementById('title').textContent='clicked'">Go</button>
<input id="field" />
<div style="height:3000px"></div>
<script>
  console.log('hello from the live page');
  undefined.property;
</script>
</body></html>"#;

/// A provider that plays a fixed list of browser calls and keeps what it was shown.
#[cfg(all(feature = "browser", unix))]
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<String>>,
    images: Mutex<usize>,
    image_bytes: Mutex<usize>,
}

#[cfg(all(feature = "browser", unix))]
impl Provider for Script {
    fn name(&self) -> &str {
        "live-script"
    }

    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(request.user.clone());
        *self.images.lock().unwrap() += request.media.len();
        *self.image_bytes.lock().unwrap() +=
            request.media.iter().map(|m| m.byte_len()).sum::<usize>();
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

#[cfg(all(feature = "browser", unix))]
#[derive(Default)]
struct Events(Mutex<Vec<RunEvent>>);

#[cfg(all(feature = "browser", unix))]
impl Observer for Events {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

#[cfg(all(feature = "browser", unix))]
fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Serve `PAGE` on loopback until the process ends.
///
/// Written by hand over `std::net` rather than pulled from a dependency: it is
/// twenty lines, it runs in this example only, and the whole point of the release
/// is that the crate adds no dependency to talk to a browser.
#[cfg(all(feature = "browser", unix))]
fn serve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = PAGE.as_bytes();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    addr
}

#[cfg(all(feature = "browser", unix))]
#[tokio::main]
async fn main() {
    let addr = serve();
    let url = format!("http://{addr}/");
    println!("serving the live page at {url}");

    let dir = tempfile::tempdir().expect("a workspace");
    let script = Script {
        steps: vec![
            vec![call("browser_navigate", json!({ "url": url }))],
            vec![call("browser_read", json!({}))],
            vec![call("browser_screenshot", json!({}))],
            vec![call("browser_click", json!({"selector": "#go"}))],
            vec![call("browser_read", json!({"selector": "#title"}))],
            vec![call(
                "browser_type",
                json!({"selector": "#field", "text": "typed by the run"}),
            )],
            vec![call("browser_scroll", json!({"dy": 500}))],
            // A host the policy does not name. The browser must be stopped at the
            // request, and this is the arm a `data:` URL could never exercise.
            vec![call(
                "browser_navigate",
                json!({"url": "http://example.invalid/"}),
            )],
            vec![call(
                "write_file",
                json!({"path": "done.txt", "content": "ok"}),
            )],
        ],
        at: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        images: Mutex::new(0),
        image_bytes: Mutex::new(0),
    };

    let contract = TaskContract::workspace("look at the live page", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(12)
        .with_browser(BrowserConfig::default());

    // Only the loopback host this example serves is permitted. Everything else,
    // including the redirect-shaped second navigation, is refused.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
        .allow_net(&format!("{}:{}", addr.ip(), addr.port()));

    let store = Store::memory().expect("a store");
    let events = Events::default();
    let started = std::time::Instant::now();

    let outcome =
        io_harness::run_with_observed(&contract, &script, &store, &policy, &ApproveAll, &events)
            .await;

    let elapsed = started.elapsed();
    // The LAST prompt, not every prompt joined: each step's prompt already
    // carries every observation before it, so joining them prints the run
    // several times over and reads like the browser did each action twice.
    let seen = script.seen.lock().unwrap();
    let transcript = seen.last().cloned().unwrap_or_default();
    drop(seen);

    println!("\n================ what the model was shown ================\n");
    println!("{transcript}");

    println!("\n================ the boundary ================");
    for event in events.0.lock().unwrap().iter() {
        match &event.kind {
            EventKind::BrowserStarted {
                binary,
                headless,
                ready_ms,
            } => println!("started {binary} (headless {headless}) in {ready_ms} ms"),
            EventKind::BrowserNavigated { host, permitted } => {
                println!(
                    "navigation to {host}: {}",
                    if *permitted { "permitted" } else { "REFUSED" }
                );
            }
            _ => {}
        }
    }

    println!("\n================ the measurement ================");
    let page_text: usize = transcript
        .lines()
        .find(|l| l.contains("quick brown fox"))
        .map(str::len)
        .unwrap_or(0);
    println!("the page as text:       {page_text} bytes");
    println!(
        "the page as a screenshot: {} bytes across {} image(s)",
        script.image_bytes.lock().unwrap(),
        script.images.lock().unwrap()
    );
    println!("whole run wall clock:   {elapsed:?} (recorded, never asserted)");

    // The questions only a real browser can answer, checked rather than eyeballed.
    println!("\n================ what the live run proves ================");
    let mut wrong = 0;
    let mut check = |claim: &str, ok: bool| {
        println!("{} {claim}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            wrong += 1;
        }
    };
    check(
        "the rendered text reached the model",
        transcript.contains("quick brown fox"),
    );
    check(
        "the page's console output reached the model",
        transcript.contains("hello from the live page"),
    );
    check(
        "an uncaught error reached the model as its description, not as `Uncaught`",
        transcript.contains("page error: TypeError"),
    );
    // The click is the one that needs a real browser: a synthetic event a page
    // can ignore would leave the title untouched, and the fixture cannot tell the
    // two apart.
    check(
        "a dispatched click actually changed the page",
        transcript.contains("clicked"),
    );
    check(
        "a screenshot reached the model as an image",
        *script.images.lock().unwrap() >= 1,
    );
    check(
        "a navigation to an unnamed host was refused",
        transcript.contains("refused"),
    );

    match outcome {
        Ok(_) => println!("\nrun finished"),
        Err(e) => println!("\nrun ended with: {e}"),
    }
    if wrong > 0 {
        eprintln!("\n{wrong} live claim(s) did not hold");
        std::process::exit(1);
    }
}
