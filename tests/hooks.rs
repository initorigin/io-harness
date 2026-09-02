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
use std::sync::Mutex;

use io_harness::hooks::OnFailure;
use io_harness::observe::{Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, Config, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

/// Guards `IO_CONFIG_HOME`, which the process has exactly one of.
///
/// Until 0.74.0 every test here wanted the same answer — an empty user scope, so a
/// config file on the developer's own machine could not change what they measure —
/// and one shared directory removed the race without serializing around it. That is
/// no longer available: a `[[hook]]` may now be declared only in the user scope, so
/// each test needs a *different* one, which is the situation `tests/config.rs` has
/// always been in.
///
/// The lock is therefore held across the two lines that touch the environment and
/// the `Config::discover` that reads them, and released before the caller reaches an
/// `.await` — a lock held across one is a lint and a deadlock waiting for a reason,
/// and half of these tests are `async`.
static ENV: Mutex<()> = Mutex::new(());

/// Discover a configuration whose `[[hook]]` tables live in the user scope — since
/// 0.74.0 the only scope that may declare one, because `io.toml` arrives with a
/// clone and `io.local.toml` sits in the workspace root a run's own agent writes to.
///
/// The tempdir is dropped on return, which is safe because discovery has already
/// read the file into the returned value: nothing here reads configuration from disk
/// again once the caller holds it, which is `tests/config.rs`'s NF3.
///
/// `IO_CONFIG` is removed rather than left alone: it names the user-scope *file*
/// outright and wins over `IO_CONFIG_HOME`, so a developer who has one exported
/// would otherwise be running a different test.
fn discover_with(ws: &Path, user_toml: &str) -> io_harness::Result<Config> {
    let user = tempfile::tempdir().unwrap();
    std::fs::write(user.path().join("io.toml"), user_toml).unwrap();
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("IO_CONFIG");
    std::env::set_var("IO_CONFIG_HOME", user.path());
    Config::discover(ws)
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
    let ws = tempfile::tempdir().unwrap();

    let hooks = discover_with(
        ws.path(),
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
    .unwrap()
    .hooks();
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
    let ws = tempfile::tempdir().unwrap();

    let err = discover_with(
        ws.path(),
        "[[hook]]\non = [\"finshed\"]\nappend = \"a.jsonl\"\n",
    )
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("finshed"), "{text}");
    assert!(text.contains("hook[0]"), "{text}");
    assert!(text.contains("io.toml"), "{text}");

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
/// PowerShell rather than `cmd` on Windows, and the reason is a property of `cmd`
/// rather than of this crate: `cmd /c` strips the quotes a spawn adds around a
/// single argument only when that argument contains no other quotes, and keeps them
/// otherwise — so a `/c` string carrying a quoted redirect target is read as the
/// name of a program. PowerShell parses its own command line and does not have that
/// rule, which lets the same script text carry `;`, `&&` and a space on both
/// platforms and keeps this test asserting the same thing everywhere.
///
/// The shell here is the *operator's* choice of program, which is a different thing
/// from this crate inserting one.
#[cfg(unix)]
const CAPTURE: &[&str] = &["sh", "-c", "cat > 'a;b && c.jsonl'"];
#[cfg(windows)]
const CAPTURE: &[&str] = &[
    "powershell",
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "[Console]::In.ReadToEnd() | Set-Content -LiteralPath 'a;b && c.jsonl'",
];

/// The file the capture hook writes, which is also the proof: an argv split on
/// whitespace, on `;` or on `&&` would produce some other file, or several.
const CAPTURE_FILE: &str = "a;b && c.jsonl";

/// Sleeps well past any timeout this test sets.
#[cfg(unix)]
const SLOW: &[&str] = &["sleep", "30"];
#[cfg(windows)]
const SLOW: &[&str] = &["ping", "-n", "31", "127.0.0.1"];

/// Returns at once, and successfully.
#[cfg(unix)]
const FAST: &[&str] = &["true"];
#[cfg(windows)]
const FAST: &[&str] = &["cmd", "/c", "exit 0"];

/// Returns at once, and unsuccessfully.
#[cfg(unix)]
const FAILS: &[&str] = &["false"];
#[cfg(windows)]
const FAILS: &[&str] = &["cmd", "/c", "exit 1"];

/// A TOML array literal for one of the tables above.
fn argv(parts: &[&str]) -> String {
    let items: Vec<String> = parts.iter().map(|p| format!("{p:?}")).collect();
    format!("[{}]", items.join(", "))
}

/// Run one turn under one hook table and report the outcome.
///
/// The table is loaded from the user scope and the turn reads a file of its own, so
/// the run still reaches a `refused` and a `finished` without the configuration
/// having to sit inside the workspace it is watching.
async fn run_under(ws: &Path, hook: &str) -> io_harness::RunOutcome {
    std::fs::write(ws.join("read-me.txt"), "something to read\n").unwrap();
    let hooks = discover_with(ws, hook).unwrap().hooks();
    let store = Store::open(ws.join("s.db")).unwrap();
    run_with_observed(
        &contract(ws),
        &mock(vec![vec![call(
            "read_file",
            json!({"path": "read-me.txt"}),
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
    let ws = tempfile::tempdir().unwrap();

    // A generous bound, because nothing here is asserting one. This test asks what
    // reached the child, not how fast it got there, and the deadline exists only so
    // a wedged child cannot hang the suite. 20_000 was not generous enough: a
    // Windows runner under load took a PowerShell that normally starts in ~2s past
    // it, the hook was killed before it wrote, and the assertion below read that as
    // a split argv.
    run_under(
        ws.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\ntimeout_ms = 120000\n",
            argv(CAPTURE)
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
    // A byte-order mark first: PowerShell's `Set-Content` may write one, and a BOM
    // is whitespace to nobody, least of all to a JSON parser.
    let v: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}').trim()).unwrap();
    assert_eq!(v["event"], "started");
    assert_eq!(v["run_id"], 1);
}

/// The bound, and the control that says "bounded" is not being satisfied by an
/// implementation that kills everything. A killed hook is a failed hook, and the
/// only way a failure is observable from out here is `on_failure = "cancel"` — so
/// the two criteria are asserted through one another rather than through a log line.
#[tokio::test]
async fn f4_a_hook_that_outlives_its_timeout_is_killed_and_reported_as_a_failure() {
    let slow = tempfile::tempdir().unwrap();
    let outcome = run_under(
        slow.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\ntimeout_ms = 50\non_failure = \"cancel\"\n",
            argv(SLOW)
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
            argv(FAST)
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
    let asked = tempfile::tempdir().unwrap();
    let outcome = run_under(
        asked.path(),
        &format!(
            "[[hook]]\non = [\"started\"]\nrun = {}\non_failure = \"cancel\"\n",
            argv(FAILS)
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
        &format!("[[hook]]\non = [\"started\"]\nrun = {}\n", argv(FAILS)),
    )
    .await;
    assert!(
        matches!(outcome, io_harness::RunOutcome::VerificationFailed { .. }),
        "the default is continue: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// 0.71.0 — a configured hook is readable, not merely countable (#223)
// ---------------------------------------------------------------------------

/// Two tables that differ in **every one** of the seven keys, so an accessor
/// wired to the wrong field cannot pass by reading a neighbour that happens to
/// hold the same value.
///
/// The first writes `on_failure` and the second omits it, which is the pair that
/// separates a copied answer from a computed one: `cancel` is the key the table
/// wrote, and `refuse` is a value that appears in no file anywhere — a lifecycle
/// hook's own default, and not the enum's, which is `continue`.
const SEVEN_KEYS: &str = "\
[[hook]]
on = [\"finished\", \"refused\"]
append = \"audit.jsonl\"
on_failure = \"cancel\"

[[hook]]
at = \"before_tool\"
tools = [\"read_file\"]
run = [\"gate\", \"--strict\"]
timeout_ms = 1234
";

/// **#223**, the configuration half. `Hooks::declarations` hands back every table
/// an operator configured with all seven keys readable.
///
/// The plugin half of the identical assertion lives in `tests/plugin.rs`: the two
/// are different holders of the same fact — a configuration's `[[hook]]` array
/// and a manifest's — and a test against one proves nothing about the other.
#[test]
fn a_configured_hook_is_readable_key_by_key_and_not_merely_counted() {
    let ws = tempfile::tempdir().unwrap();
    let hooks = discover_with(ws.path(), SEVEN_KEYS).unwrap().hooks();
    assert!(
        !hooks.is_empty(),
        "the countable answer, which is what 0.70.0 had"
    );

    let tables = hooks.declarations();
    assert_eq!(tables.len(), 2, "both tables, in declaration order");

    let event = &tables[0];
    assert_eq!(event.on().to_vec(), ["finished", "refused"]);
    assert_eq!(event.at(), None);
    assert!(event.tools().is_empty());
    assert_eq!(event.append(), Some(Path::new("audit.jsonl")));
    assert_eq!(event.run(), None);
    assert_eq!(
        event.on_failure(),
        OnFailure::Cancel,
        "the key this table wrote, carried through unchanged"
    );
    assert_eq!(
        event.timeout_ms(),
        None,
        "absent, and reported absent rather than as the module's own 5000"
    );

    let gate = &tables[1];
    assert!(gate.on().is_empty());
    assert_eq!(gate.at(), Some("before_tool"));
    assert_eq!(gate.tools().to_vec(), ["read_file"]);
    assert_eq!(gate.append(), None);
    assert_eq!(
        gate.run(),
        Some(&["gate".to_string(), "--strict".to_string()][..]),
        "the argv whole, program first"
    );
    assert_eq!(
        gate.on_failure(),
        OnFailure::Refuse,
        "computed, never copied: no file wrote `refuse`, and the enum's own default \
         is `continue` — a reader that returned either would be lying about what \
         this hook does to a call"
    );
    assert_eq!(
        gate.timeout_ms(),
        Some(1234),
        "present, and the value written"
    );
}
