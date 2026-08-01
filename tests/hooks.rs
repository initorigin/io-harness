//! Shaping a run from `io.toml` — F1 through F6 of 0.28.0.
//!
//! Everything here is driven through the real loader and the real run loop with
//! the scripted mock provider the rest of the suite uses. A hook is only worth
//! anything if it fires on the events the loop actually emits, so nothing in this
//! file constructs a `RunEvent` by hand except where it is asserting a property of
//! the filter rather than of the run.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use io_harness::observe::{Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, Config, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

/// One empty directory for the whole binary, so every test in it points the user
/// scope at the same place.
///
/// `tests/config.rs` guards the same variable with a mutex because its tests set it
/// to *different* directories and the process has one environment. Here every test
/// wants the same answer — an empty user scope, so a config file on the developer's
/// own machine cannot change what these tests measure — so one shared directory
/// removes the race rather than serializing around it. Which matters: half of these
/// tests are `async`, and a lock held across an `.await` is a lint and a deadlock
/// waiting for a reason.
static USER: OnceLock<tempfile::TempDir> = OnceLock::new();

fn empty_user_scope() {
    let dir = USER.get_or_init(|| tempfile::tempdir().unwrap());
    std::env::set_var("IO_CONFIG_HOME", dir.path());
}

// ---------------------------------------------------------------- scaffolding

/// Answers with one scripted tool call per turn and nothing afterwards.
struct Mock {
    script: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.script.get(i).cloned().unwrap_or_default(),
            usage: Some(Usage {
                total_tokens: 7,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn mock(script: Vec<Vec<ToolCall>>) -> Mock {
    Mock {
        script,
        at: AtomicUsize::new(0),
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Forwards every event to two observers, so the same run can be watched by the
/// hooks under test and by a recorder that says what the run actually emitted.
/// F1's first control is an equality between those two sets, and it needs both
/// halves of one run rather than two runs that were probably the same.
struct Tee<'a>(&'a dyn Observer, &'a dyn Observer);

impl Observer for Tee<'_> {
    fn event(&self, event: &RunEvent) -> Flow {
        let a = self.0.event(event);
        let b = self.1.event(event);
        if a.is_cancel() || b.is_cancel() {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }
}

#[derive(Default)]
struct Tags(Mutex<Vec<String>>);

impl Observer for Tags {
    fn event(&self, event: &RunEvent) -> Flow {
        let v = serde_json::to_value(event).unwrap();
        self.0
            .lock()
            .unwrap()
            .push(v["event"].as_str().unwrap().to_string());
        Flow::Continue
    }
}

impl Tags {
    fn set(&self) -> BTreeSet<String> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

/// A run that tries one write it is not allowed to make and then stops, so both a
/// `refused` and a `finished` are reached without a socket.
fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("exercise the hooks", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(2)
}

/// Reads anything and writes nothing, and *denies* rather than asks: an `Ask`
/// default reaches `ApproveAll` and the write happens, which is an approval and not
/// a refusal. The distinction is the point of the test.
fn read_only() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .deny_write("*")
}

/// The tags in an audit file, in the order they were written.
fn logged(at: &Path) -> Vec<String> {
    std::fs::read_to_string(at)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// F1 — a hook fires on the events it names and on no others
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f1_a_hook_fires_on_the_events_it_names_and_on_no_others() {
    empty_user_scope();
    let ws = tempfile::tempdir().unwrap();

    std::fs::write(
        ws.path().join("io.local.toml"),
        r#"
        [[hook]]
        on = ["refused", "finished"]
        append = "named.jsonl"

        [[hook]]
        append = "everything.jsonl"

        [[hook]]
        on = ["question_asked"]
        append = "never.jsonl"
        "#,
    )
    .unwrap();

    let hooks = Config::discover(ws.path()).unwrap().hooks();
    let tags = Tags::default();
    let store = Store::open(ws.path().join("s.db")).unwrap();

    run_with_observed(
        &contract(ws.path()),
        &mock(vec![vec![call(
            "write_file",
            json!({"path": "a.txt", "content": "hi"}),
        )]]),
        &store,
        &read_only(),
        &ApproveAll,
        &Tee(&hooks, &tags),
    )
    .await
    .unwrap();

    // The hook that named two events got those two and nothing else.
    let named: BTreeSet<String> = logged(&ws.path().join("named.jsonl")).into_iter().collect();
    assert_eq!(
        named,
        BTreeSet::from(["refused".to_string(), "finished".to_string()]),
        "a filtered hook received something it did not ask for, or missed something it did"
    );

    // Control one: no `on` is every event, asserted as an equality against what the
    // run emitted rather than as "the file is not empty".
    let everything: BTreeSet<String> = logged(&ws.path().join("everything.jsonl"))
        .into_iter()
        .collect();
    assert_eq!(
        everything,
        tags.set(),
        "an unfiltered hook must see exactly the run's own event stream"
    );

    // Control two: a filter that matched nothing leaves an empty file rather than no
    // file, so "matched nothing" stays distinguishable from "never installed".
    let never = ws.path().join("never.jsonl");
    assert!(never.exists(), "an installed hook creates its log");
    assert_eq!(std::fs::read_to_string(&never).unwrap(), "");
}

// ---------------------------------------------------------------------------
// F2 — an event the crate does not emit is refused at load
// ---------------------------------------------------------------------------

#[test]
fn f2_an_event_this_crate_does_not_emit_is_refused_naming_it() {
    empty_user_scope();
    let ws = tempfile::tempdir().unwrap();

    std::fs::write(
        ws.path().join("io.local.toml"),
        "[[hook]]\non = [\"finshed\"]\nappend = \"a.jsonl\"\n",
    )
    .unwrap();

    let err = Config::discover(ws.path()).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("finshed"), "{text}");
    assert!(text.contains("hook[0]"), "{text}");
    assert!(text.contains("io.local.toml"), "{text}");

    // The control is in `src/hooks.rs`: every name the crate emits is accepted, so a
    // rule written against a hand-typed subset fails there rather than here.
}

// ---------------------------------------------------------------------------
// F4 — an executing hook is a fixed argv, is handed the event, and is bounded
// ---------------------------------------------------------------------------

/// A program that reads its stdin into a file whose name carries `;` and `&&`.
///
/// Both halves of F4 in one child. The file name proves the argv element arrived
/// **whole**: this crate never hands a string to a shell, so what an operator wrote
/// as one array element reaches the process as one argument, metacharacters and
/// spaces and all. The file's contents prove the event reached stdin and that what
/// arrived is the JSON of the event that fired.
///
/// Each platform's own programs are named rather than the test being skipped on
/// Windows, which is the lesson 0.27.0's F4 paid for. The shell here is the
/// *operator's* choice of program, which is a different thing from this crate
/// inserting one.
#[cfg(unix)]
const CAPTURE: [&str; 3] = ["sh", "-c", "cat > 'a;b && c.jsonl'"];
#[cfg(windows)]
const CAPTURE: [&str; 3] = ["cmd", "/c", "findstr /r .* > \"a;b c.jsonl\""];

/// The file the capture hook writes, which is also the proof.
///
/// Windows drops the `&&`: `cmd` owns the parsing of the string an operator handed
/// it, and `&` is a separator there in ways that vary with quoting — which is a fact
/// about `cmd` and not about this crate. The space and the semicolon prove what this
/// test is for either way, because an argv split on whitespace or on `;` would
/// produce some other file, or several.
#[cfg(unix)]
const CAPTURE_FILE: &str = "a;b && c.jsonl";
#[cfg(windows)]
const CAPTURE_FILE: &str = "a;b c.jsonl";

/// Sleeps well past any timeout this test sets.
#[cfg(unix)]
const SLOW: [&str; 2] = ["sleep", "30"];
#[cfg(windows)]
const SLOW: [&str; 3] = ["cmd", "/c", "ping -n 31 127.0.0.1 > NUL"];

/// Returns at once, and successfully.
#[cfg(unix)]
const FAST: [&str; 2] = ["true", ""];
#[cfg(windows)]
const FAST: [&str; 3] = ["cmd", "/c", "exit 0"];

/// Returns at once, and unsuccessfully.
#[cfg(unix)]
const FAILS: [&str; 2] = ["false", ""];
#[cfg(windows)]
const FAILS: [&str; 3] = ["cmd", "/c", "exit 1"];

/// A TOML array literal for one of the tables above, dropping the padding entry the
/// unix forms carry so the two platforms can share a fixed-size constant.
fn argv(parts: &[&str]) -> String {
    let items: Vec<String> = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p:?}"))
        .collect();
    format!("[{}]", items.join(", "))
}

/// Run one turn under one hook table and report the outcome.
async fn run_under(ws: &Path, hook: &str) -> io_harness::RunOutcome {
    std::fs::write(ws.join("io.local.toml"), hook).unwrap();
    let hooks = Config::discover(ws).unwrap().hooks();
    let store = Store::open(ws.join("s.db")).unwrap();
    run_with_observed(
        &contract(ws),
        &mock(vec![vec![call(
            "read_file",
            json!({"path": "io.local.toml"}),
        )]]),
        &store,
        &read_only(),
        &ApproveAll,
        &hooks,
    )
    .await
    .unwrap()
    .outcome
}

#[tokio::test]
async fn f4_an_executing_hook_gets_its_argv_whole_and_the_event_on_stdin() {
    empty_user_scope();
    let ws = tempfile::tempdir().unwrap();

    run_under(
        ws.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\ntimeout_ms = 20000\n",
            argv(&CAPTURE)
        ),
    )
    .await;

    // The argument arrived whole. A harness that split on whitespace, or handed the
    // string to a shell, would have produced some other file — or several.
    let at = ws.path().join(CAPTURE_FILE);
    assert!(
        at.is_file(),
        "the argv element did not reach the child intact: {:?}",
        std::fs::read_dir(ws.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect::<Vec<_>>()
    );

    // And what reached its stdin is the event that fired, as JSON rather than as a
    // rendering of one.
    let text = std::fs::read_to_string(&at).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(v["event"], "started");
    assert_eq!(v["run_id"], 1);
}

/// The bound, and the control that says "bounded" is not being satisfied by an
/// implementation that kills everything. A killed hook is a failed hook, and the
/// only way a failure is observable from out here is `on_failure = "cancel"` — so
/// the two criteria are asserted through one another rather than through a log line.
#[tokio::test]
async fn f4_a_hook_that_outlives_its_timeout_is_killed_and_reported_as_a_failure() {
    empty_user_scope();

    let slow = tempfile::tempdir().unwrap();
    let outcome = run_under(
        slow.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\ntimeout_ms = 50\non_failure = \"cancel\"\n",
            argv(&SLOW)
        ),
    )
    .await;
    assert!(
        matches!(outcome, io_harness::RunOutcome::Cancelled { .. }),
        "a hook past its deadline is a failure: {outcome:?}"
    );

    // The negative control: the same shape inside its timeout completes, is not a
    // failure, and does not stop the run.
    let fast = tempfile::tempdir().unwrap();
    let outcome = run_under(
        fast.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\ntimeout_ms = 30000\non_failure = \"cancel\"\n",
            argv(&FAST)
        ),
    )
    .await;
    assert!(
        !matches!(outcome, io_harness::RunOutcome::Cancelled { .. }),
        "a hook that succeeded must not stop the run: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// F5 — a hook can stop a run, and by default cannot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f5_a_failing_hook_stops_the_run_only_when_the_operator_asked_it_to() {
    empty_user_scope();

    let asked = tempfile::tempdir().unwrap();
    let outcome = run_under(
        asked.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\non_failure = \"cancel\"\n",
            argv(&FAILS)
        ),
    )
    .await;
    assert!(
        matches!(outcome, io_harness::RunOutcome::Cancelled { .. }),
        "a local policy check that says no must end the run: {outcome:?}"
    );

    // The negative control, and the whole reason the key exists: the byte-identical
    // hook without `on_failure` leaves the run to reach its own ending. A
    // notification that happens to fail is not a kill switch.
    let unasked = tempfile::tempdir().unwrap();
    let outcome = run_under(
        unasked.path(),
        &format!("[[hook]]\non = [\"started\"]\nrun = {}\n", argv(&FAILS)),
    )
    .await;
    assert!(
        matches!(outcome, io_harness::RunOutcome::StepCapReached { .. }),
        "the default is continue: {outcome:?}"
    );
}
