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
use std::sync::{Mutex, MutexGuard};

use io_harness::observe::{Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, Config, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

static ENV: Mutex<()> = Mutex::new(());

/// Hold the environment and point the user scope at somewhere empty, so a config
/// file on the developer's own machine cannot change what these tests measure.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IO_CONFIG_HOME", user_dir);
    guard
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
    let user = tempfile::tempdir().unwrap();
    let _guard = env(user.path());
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
    let user = tempfile::tempdir().unwrap();
    let _guard = env(user.path());
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
