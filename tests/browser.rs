//! Driving a browser through the full loop, against a real child process.
//!
//! The child is `examples/browser_fixture.rs`, spawned with the real descriptor
//! plumbing and speaking the real protocol — real NUL framing, a real attach, real
//! paused requests. Nothing is mocked at the transport level, which is the only
//! way these tests can fail for the reasons they exist to catch.
//!
//! It is a fixture rather than an installed browser because the paths that matter
//! here are the ones a real browser will not perform on request: a request that
//! must be refused, a selector that matches nothing, a page that reports an
//! uncaught error at a known moment. The one live run against a real browser is
//! `examples/browser_live.rs`, which is outside the default gate.
//!
//! Every assertion about the boundary is made on **what the browser was told** —
//! the fixture's own record of `continue` and `fail` — rather than on the error
//! text the run produced. A build that reported a refusal it never enforced would
//! satisfy the second and fail the first.

#![cfg(all(feature = "browser", unix))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, BrowserConfig, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::{json, Value};

/// Where `cargo test` left the fixture binary. See `tests/lsp.rs`, whose
/// reasoning this follows exactly.
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

/// A provider that plays a fixed script and records what it was offered, what it
/// was shown, and what images it was sent.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<String>>,
    seen: Mutex<Vec<String>>,
    images: Mutex<usize>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            images: Mutex::new(0),
        }
    }

    fn tools_offered(&self) -> Vec<String> {
        self.offered.lock().unwrap().clone()
    }

    fn transcript(&self) -> String {
        self.seen.lock().unwrap().join("\n")
    }

    /// How many images the loop actually put on an outbound request.
    ///
    /// Asserted on the request rather than on a stored row: staging an image and
    /// sending one are different bugs, and only the first leaves a row.
    fn images_sent(&self) -> usize {
        *self.images.lock().unwrap()
    }
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    /// This script stands in for a vision-capable model. Without it the loop
    /// refuses the request before sending it — which is 0.15.0's guard doing its
    /// job, and the reason the screenshot test asserts on the outbound request
    /// rather than on the staging.
    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = request.tools.iter().map(|t| t.name.clone()).collect();
        self.seen.lock().unwrap().push(request.user.clone());
        *self.images.lock().unwrap() += request.media.len();
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

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
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

    fn has(&self, needle: &str) -> bool {
        self.lines().iter().any(|l| l.contains(needle))
    }
}

/// The browser config pointing at the fixture, with its settings as arguments.
///
/// Arguments rather than environment variables, and this was a real defect
/// before it was a style: `std::env::set_var` is process-global, so with the
/// suite running in parallel one test's settings reached another test's child
/// and a click test saw an empty record. An argument belongs to exactly one
/// spawn, so no amount of parallelism can cross them.
fn fixture_config(record: &Record, extra: &[&str]) -> BrowserConfig {
    let mut args = vec![format!("--io-fixture-record={}", record.0.display())];
    args.extend(extra.iter().map(|a| (*a).to_string()));
    BrowserConfig::default()
        .with_binary(fixture_browser().display().to_string())
        .with_args(args)
        .with_timeout(std::time::Duration::from_secs(5))
}

fn permitted() -> Policy {
    Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn contract(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace("look at the page", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(steps)
}

fn finish() -> Vec<ToolCall> {
    vec![call(
        "write_file",
        json!({"path": "done.txt", "content": "ok"}),
    )]
}

/// Collect every event a run emitted.
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

    fn started(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e.kind, EventKind::BrowserStarted { .. }))
            .count()
    }
}

/// F3 — a navigation to a denied host is stopped at the request, and the one to a
/// permitted host is not.
///
/// Both halves in one test, because the claim is the distinction. The assertion
/// that matters is on the fixture's record: `fail` means the browser was told to
/// block the request, and a build that merely reported a refusal without enforcing
/// it would show `continue` here.
#[tokio::test]
async fn a_navigation_to_a_denied_host_is_failed_at_the_request_and_a_permitted_one_continues() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![
        vec![call(
            "browser_navigate",
            json!({"url": "https://allowed.example.com/page"}),
        )],
        vec![call(
            "browser_navigate",
            json!({"url": "https://blocked.example.com/page"}),
        )],
        finish(),
    ]);
    let events = Events::default();
    let store = Store::memory().unwrap();
    let policy = permitted().allow_net("allowed.example.com");

    io_harness::run_with_observed(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &policy,
        &ApproveAll,
        &events,
    )
    .await
    .unwrap();

    let lines = record.lines();
    assert!(
        record.has("continue R1"),
        "the permitted host was not continued: {lines:?}"
    );
    assert!(
        record.has("fail R2 BlockedByClient"),
        "the denied host was not failed at the request: {lines:?}"
    );

    // The refusal reaches the model in the boundary's own words, naming the host
    // and the rule rather than the browser's opaque network error.
    let transcript = script.transcript();
    assert!(
        transcript.contains("blocked.example.com:443"),
        "the refusal did not name the host: {transcript}"
    );
    assert!(
        transcript.contains("not permitted by this run's policy"),
        "the refusal did not read as a policy refusal: {transcript}"
    );

    assert_eq!(
        events.navigations(),
        vec![
            ("allowed.example.com:443".to_string(), true),
            ("blocked.example.com:443".to_string(), false),
        ]
    );
    assert_eq!(events.started(), 1, "the browser should start exactly once");
}

/// F4 — a navigation the model did not type is gated by the same mechanism.
///
/// The run makes **no** `browser_navigate` call at all: it clicks, and the click
/// navigates. This is the criterion that fails an implementation which checks the
/// tool's argument instead of the paused request, and it is why F3 and F4 are two
/// tests rather than one.
#[tokio::test]
async fn a_navigation_a_click_causes_is_refused_by_the_same_gate() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(
        &record,
        &["--io-fixture-links=https://elsewhere.example.com/landing"],
    );

    let script = Script::new(vec![
        vec![call("browser_click", json!({"selector": "a.next"}))],
        finish(),
    ]);
    let events = Events::default();
    let store = Store::memory().unwrap();
    // Nothing is allowed anywhere: the point is that the click's own navigation
    // is decided, not that some other host was permitted.
    let policy = permitted();

    io_harness::run_with_observed(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &policy,
        &ApproveAll,
        &events,
    )
    .await
    .unwrap();

    let lines = record.lines();
    assert!(
        record.has("click"),
        "the click never reached the browser: {lines:?}"
    );
    assert!(
        record.has("fail R1 BlockedByClient"),
        "a navigation the model did not type was not gated: {lines:?}"
    );
    assert!(
        !record.has("continue"),
        "a denied navigation was continued: {lines:?}"
    );
    assert_eq!(
        events.navigations(),
        vec![("elsewhere.example.com:443".to_string(), false)]
    );
}

/// F5 — a selector that matches nothing fails and names itself.
///
/// The absence assertion is the load-bearing half: a build that treated a missing
/// element as a successful no-op would satisfy every assertion about the run
/// continuing, and the model would reason forward from a click that never landed.
#[tokio::test]
async fn a_selector_that_matches_nothing_fails_and_dispatches_no_input() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &["--io-fixture-no-selector"]);

    let script = Script::new(vec![
        vec![call("browser_click", json!({"selector": "button.missing"}))],
        vec![call(
            "browser_type",
            json!({"selector": "input.missing", "text": "hello"}),
        )],
        finish(),
    ]);
    let events = Events::default();
    let store = Store::memory().unwrap();

    io_harness::run_with_observed(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
        &events,
    )
    .await
    .unwrap();

    let transcript = script.transcript();
    assert!(
        transcript.contains("button.missing"),
        "the click failure did not name the selector: {transcript}"
    );
    assert!(
        transcript.contains("input.missing"),
        "the type failure did not name the selector: {transcript}"
    );
    let lines = record.lines();
    assert!(
        !record.has("click"),
        "input was dispatched for an element that does not exist: {lines:?}"
    );
    assert!(
        !record.has("type "),
        "text was typed into an element that does not exist: {lines:?}"
    );
}

/// F6 — console output and uncaught page errors reach the observation of the
/// action that produced them.
///
/// The two arms are asserted separately on purpose: a build that routed console
/// messages and dropped exceptions would pass a single combined assertion that
/// only looked for a `[console]` section.
#[tokio::test]
async fn console_output_and_page_errors_ride_the_action_that_produced_them() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &["--io-fixture-console"]);

    let script = Script::new(vec![
        vec![call(
            "browser_navigate",
            json!({"url": "https://allowed.example.com/"}),
        )],
        finish(),
    ]);
    let store = Store::memory().unwrap();
    let policy = permitted().allow_net("allowed.example.com");

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    let transcript = script.transcript();
    assert!(
        transcript.contains("log: page said hello"),
        "the console output did not reach the model: {transcript}"
    );
    // The readable message, not the bare word the obvious field carries.
    assert!(
        transcript.contains("page error: TypeError: undefined is not a function"),
        "the uncaught error did not reach the model as its description: {transcript}"
    );
}

/// F6, the other half — an action that produced nothing says so.
#[tokio::test]
async fn an_action_that_produced_no_console_output_says_so() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![vec![call("browser_read", json!({}))], finish()]);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let transcript = script.transcript();
    assert!(
        transcript.contains("[console] nothing"),
        "an empty console was omitted rather than stated: {transcript}"
    );
    assert!(
        transcript.contains("fixture page text"),
        "the page's text did not reach the model: {transcript}"
    );
}

/// F7 — a screenshot reaches the model as an image on the outbound request.
#[tokio::test]
async fn a_screenshot_is_sent_to_the_model_as_an_image() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![vec![call("browser_screenshot", json!({}))], finish()]);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        script.images_sent() >= 1,
        "the screenshot never reached an outbound request"
    );
    let transcript = script.transcript();
    assert!(
        transcript.contains("1280x800"),
        "the observation did not name the viewport: {transcript}"
    );
}

/// F8 — no browser process survives the run.
///
/// Asserted on the process rather than on a shutdown call having been issued: a
/// build that called shutdown and left the child alive would satisfy the second
/// and fail this.
#[tokio::test]
async fn no_browser_process_survives_the_run() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![vec![call("browser_read", json!({}))], finish()]);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let started = record
        .lines()
        .into_iter()
        .find_map(|l| l.strip_prefix("started ").map(str::to_string))
        .expect("the fixture recorded its pid");
    let pid: i32 = started.trim().parse().expect("the pid is a number");

    // The child is reaped by the parent, so it may linger as a zombie for a
    // moment; what must be true is that it is no longer a live process able to
    // act. `kill -0` answers exactly that.
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(!alive, "the browser process {pid} outlived its run");
}

/// F9 — a browser that is not installed is a refusal naming it, never a download.
#[tokio::test]
async fn a_missing_browser_is_a_refusal_naming_it() {
    let dir = workspace();
    let browser = BrowserConfig::default()
        .with_binary(dir.path().join("no-such-browser").display().to_string())
        .with_timeout(std::time::Duration::from_secs(5));

    let script = Script::new(vec![vec![call("browser_read", json!({}))], finish()]);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let transcript = script.transcript();
    assert!(
        transcript.contains("no-such-browser"),
        "the failure did not name the binary that was missing: {transcript}"
    );
    assert!(
        transcript.contains("Nothing is downloaded"),
        "the failure did not say that nothing is fetched: {transcript}"
    );
}

/// F10 — a run that configures no browser is offered none of the six schemas.
///
/// The negative control the whole release rests on.
#[tokio::test]
async fn a_run_with_no_browser_configured_is_offered_no_browser_tools() {
    let dir = workspace();
    let script = Script::new(vec![finish()]);
    let store = Store::memory().unwrap();
    let events = Events::default();

    io_harness::run_with_observed(
        &contract(dir.path(), 4),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
        &events,
    )
    .await
    .unwrap();

    let offered = script.tools_offered();
    assert!(
        !offered.iter().any(|t| t.starts_with("browser_")),
        "an unconfigured run was offered browser tools: {offered:?}"
    );
    assert_eq!(events.started(), 0, "an unconfigured run started a browser");
    assert!(events.navigations().is_empty());
}

/// The six schemas appear for a run that did configure one — the other half of
/// F10, so "absent" is shown to be a decision rather than the tools never
/// existing.
#[tokio::test]
async fn a_configured_run_is_offered_exactly_the_six() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![finish()]);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), 4).with_browser(browser),
        &script,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let mut offered: Vec<String> = script
        .tools_offered()
        .into_iter()
        .filter(|t| t.starts_with("browser_"))
        .collect();
    offered.sort();
    assert_eq!(
        offered,
        vec![
            "browser_click",
            "browser_navigate",
            "browser_read",
            "browser_screenshot",
            "browser_scroll",
            "browser_type",
        ]
    );
}

/// The spawn is gated: a policy denying `Act::Exec` on the browser starts no
/// process at all.
#[tokio::test]
async fn a_denied_browser_binary_starts_no_process() {
    let dir = workspace();
    let record = Record::new(dir.path());
    let browser = fixture_config(&record, &[]);

    let script = Script::new(vec![vec![call("browser_read", json!({}))], finish()]);
    let store = Store::memory().unwrap();
    // Reads and writes allowed, exec denied — so the run can still finish, and
    // the only thing that cannot happen is the spawn.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .deny_exec("*");

    run_with(
        &contract(dir.path(), 6).with_browser(browser),
        &script,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    // Asserted on the absence of the process, not on the error text: the fixture
    // records its own start, so an empty record is proof no child ran.
    assert!(
        !record.has("started"),
        "a denied browser was spawned anyway: {:?}",
        record.lines()
    );
}
