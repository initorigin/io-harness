//! What a failing command gate tells a watcher (0.86.0).
//!
//! Since 0.17.0 a failing `Verification::Command` has written what it printed
//! into `sandbox_events` as a `"gate_output"` row, and announced only that the
//! row existed: `EventKind::Sandbox { kind: "gate_output", backend }` carries no
//! payload. An observer watching a live run therefore learned that a gate had
//! failed and nothing about why, and a caller with no store had no second place
//! to look. `EventKind::GateOutput` carries both halves — what the command
//! printed and what it exited with.
//!
//! Negative control: `a_passing_gate_announces_no_output` fails if the event is
//! emitted unconditionally, which would make every assertion below vacuous.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Store,
    TaskContract, Verification,
};
use serde_json::json;

/// Records every event, so a test can assert on what a watcher saw.
#[derive(Default)]
struct Recorder(Mutex<Vec<RunEvent>>);

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

impl Recorder {
    /// Every `GateOutput` the run emitted, in order.
    fn gate_outputs(&self) -> Vec<(String, Option<i32>)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::GateOutput { output, exit_code } => Some((output.clone(), *exit_code)),
                _ => None,
            })
            .collect()
    }
}

/// One step that writes the file the run was asked for, then stops calling.
struct WritesOnce {
    at: AtomicUsize,
}

impl Provider for WritesOnce {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let calls = match self.at.fetch_add(1, Ordering::SeqCst) {
            0 => vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "out.txt", "content": "done" }),
            }],
            _ => Vec::new(),
        };
        Ok(CompletionResponse {
            tool_calls: calls,
            ..Default::default()
        })
    }
}

/// Run one step under `verification` and return what the watcher saw.
async fn run_gated(verification: Verification) -> Recorder {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace("Write the file.", dir.path())
        .with_verification(verification)
        .with_max_steps(2);
    let store = Store::memory().unwrap();
    let watcher = Recorder::default();
    let _ = run_with_observed(
        &contract,
        &WritesOnce {
            at: AtomicUsize::new(0),
        },
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();
    watcher
}

/// A gate that prints to stderr and exits non-zero reports both.
#[tokio::test]
async fn a_failing_gate_announces_what_it_printed_and_what_it_exited_with() {
    let watcher = run_gated(Verification::Command {
        argv: vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
        expect_exit: 0,
    })
    .await;

    let seen = watcher.gate_outputs();
    let (output, exit_code) = seen
        .first()
        .expect("a failing gate announced its output; the watcher saw none");
    assert!(
        output.contains("boom"),
        "the gate's stderr reached the event. It carried: {output:?}"
    );
    assert_eq!(
        *exit_code,
        Some(3),
        "the gate's exit status reached the event"
    );
}

/// A gate that fails without printing anything still reports its exit status,
/// which is the only thing there is to report.
#[tokio::test]
async fn a_silent_failing_gate_still_announces_its_exit_status() {
    let watcher = run_gated(Verification::Command {
        argv: vec!["sh".into(), "-c".into(), "exit 7".into()],
        expect_exit: 0,
    })
    .await;

    let seen = watcher.gate_outputs();
    let (output, exit_code) = seen
        .first()
        .expect("a silent failing gate announced its exit status; the watcher saw nothing");
    assert!(
        output.is_empty(),
        "it printed nothing, and the event says so rather than inventing text: {output:?}"
    );
    assert_eq!(*exit_code, Some(7));
}

/// A recording of a failing gate replays to the same event.
///
/// The gate runs a real command on both passes rather than being replayed —
/// `Record` and `Replay` stand between the run and the *provider*, and a gate is
/// the run's own execution. That is the point: the fields have to survive a
/// replayed run, and a run whose provider answers from a recording must report
/// the same gate as the run that recorded it.
#[tokio::test]
async fn a_recorded_run_replays_to_the_same_gate_event() {
    let verification = || Verification::Command {
        argv: vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
        expect_exit: 0,
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");
    let contract = |root: &std::path::Path| {
        TaskContract::workspace("Write the file.", root)
            .with_verification(verification())
            .with_max_steps(2)
    };

    // Recorded under the same policy the replay runs under: the boundary section
    // is in the system prompt and the system prompt is part of the replay key.
    let policy = Policy::permissive();
    let recorder = io_harness::provider::Record::new(WritesOnce {
        at: AtomicUsize::new(0),
    });
    let recorded = Recorder::default();
    let store = Store::memory().unwrap();
    let _ = run_with_observed(
        &contract(dir.path()),
        &recorder,
        &store,
        &policy,
        &ApproveAll,
        &recorded,
    )
    .await
    .unwrap();
    recorder.save(&path).unwrap();

    // The workspace goes back to what it was, or the second run assembles a
    // different observation section and misses on the very first key.
    let _ = std::fs::remove_file(dir.path().join("out.txt"));

    let replayed = Recorder::default();
    let store = Store::memory().unwrap();
    let _ = run_with_observed(
        &contract(dir.path()),
        &io_harness::provider::Replay::load(&path).unwrap(),
        &store,
        &policy,
        &ApproveAll,
        &replayed,
    )
    .await
    .unwrap();

    let first = recorded.gate_outputs();
    assert!(
        !first.is_empty(),
        "the recorded run reported a failing gate, or there is nothing to reproduce"
    );
    assert_eq!(
        first,
        replayed.gate_outputs(),
        "the replayed run reports the same output and the same exit code"
    );
}

/// Negative control. Without this every assertion above would pass on a build
/// that emitted the event for every gate, failing or not.
#[tokio::test]
async fn a_passing_gate_announces_no_output() {
    let watcher = run_gated(Verification::Command {
        argv: vec!["sh".into(), "-c".into(), "exit 0".into()],
        expect_exit: 0,
    })
    .await;

    assert!(
        watcher.gate_outputs().is_empty(),
        "a gate that passed announced nothing: {:?}",
        watcher.gate_outputs()
    );
}
