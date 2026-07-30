//! Steering and interruption (0.20.0).
//!
//! The test that matters most here is the last one: an operator's mid-turn
//! message must not be permission. "Just do it" is the most natural thing anyone
//! will ever type into a steer, and the boundary has to be indifferent to it — so
//! F10 steers a turn to perform a denied write under a policy that denies it, and
//! then does the same thing under a policy that allows it, so the test proves the
//! refusal came from the boundary rather than from the write never being tried.
//!
//! The provider branches on whether the operator's message is in the prompt,
//! because "the steer changed what the agent did next" is only observable as a
//! different action taken.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    ApproveAll, EventKind, Flow, Ignore, Observer, Policy, Provider, RunEvent, RunOutcome,
    RunStatus, Session, Steer, Store,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// The marker the operator sends and the provider watches for.
const STEER: &str = "only touch docs/CHANGES.md";

/// Writes whichever file the prompt tells it to, one write per step, and records
/// every prompt it saw.
///
/// It steers itself off the original file the moment the operator's message
/// appears in the context — which is exactly the behaviour a real model would
/// have, reduced to one `contains`.
#[derive(Default)]
struct Branching {
    seen: Mutex<Vec<String>>,
    calls: AtomicUsize,
    /// After this many calls, stop calling tools so the turn can end.
    stop_after: usize,
}

impl Branching {
    fn new(stop_after: usize) -> Self {
        Self {
            stop_after,
            ..Default::default()
        }
    }

    fn wrote(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Branching {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let steered = req.user.contains(STEER);
        self.seen
            .lock()
            .unwrap()
            .push(if steered { "steered" } else { "original" }.into());
        if n >= self.stop_after {
            return Ok(CompletionResponse {
                text: Some("done".into()),
                usage: Some(usage()),
                ..Default::default()
            });
        }
        let path = if steered {
            "docs/CHANGES.md"
        } else {
            "notes.md"
        };
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": path, "content": format!("step {n}\n") }),
            }],
            usage: Some(usage()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "branching"
    }
}

/// Asks for one write, forever — so a turn only ends when something stops it.
#[derive(Default)]
struct Insistent {
    calls: AtomicUsize,
}

impl Provider for Insistent {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "notes.md", "content": format!("pass {n}\n") }),
            }],
            usage: Some(usage()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "insistent"
    }
}

/// Tries the write the operator asked for in their steer, once, then stops.
#[derive(Default)]
struct Obedient {
    calls: AtomicUsize,
    /// Whether the operator's message was in the prompt when it acted.
    acted_on_steer: AtomicUsize,
}

impl Provider for Obedient {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if req.user.contains("write secrets/key.txt") {
            self.acted_on_steer.fetch_add(1, Ordering::SeqCst);
            return Ok(CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "secrets/key.txt", "content": "leaked" }),
                }],
                usage: Some(usage()),
                ..Default::default()
            });
        }
        // Before the steer arrives it does something harmless, so the turn reaches
        // a second step for the steer to land at.
        if n == 0 {
            return Ok(CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": "notes.md" }),
                }],
                usage: Some(usage()),
                ..Default::default()
            });
        }
        Ok(CompletionResponse {
            text: Some("stopping".into()),
            usage: Some(usage()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "obedient"
    }
}

/// Interrupts the turn the moment its first step commits — the operator hitting
/// the key while the agent is working, rather than before it starts.
struct InterruptOnFirstStep(Steer);

impl Observer for InterruptOnFirstStep {
    fn event(&self, event: &RunEvent) -> Flow {
        if matches!(event.kind, EventKind::Step { .. }) {
            // Ignored on a closed channel: by the time the last step of a turn
            // commits there may be nothing left to read it, and an observer must
            // not panic.
            let _ = self.0.interrupt();
        }
        Flow::Continue
    }
}

fn usage() -> Usage {
    Usage {
        total_tokens: 3,
        ..Default::default()
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "# notes\n").unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(dir.path().join("secrets/key.txt"), "the real key\n").unwrap();
    dir
}

/// Everything allowed except the one directory that never is.
fn guarded() -> Policy {
    Policy::default()
        .layer("steer-test")
        .allow_read("*")
        .allow_write("*")
        .deny_write("secrets/*")
}

/// The same boundary with the deny lifted — the negative control for F10.
fn permissive() -> Policy {
    Policy::default()
        .layer("steer-test")
        .allow_read("*")
        .allow_write("*")
}

// ---------------------------------------------------------------------- F8

/// F8 — a steer reaches the model at the next step boundary and changes what it
/// does; the same turn unsteered does the original thing.
#[tokio::test]
async fn a_mid_turn_message_changes_the_next_step_and_the_control_does_not() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Branching::new(3);
    let (steer, inbox) = Steer::channel();

    // Sent before the turn starts, which is the same code path as sending it during
    // step 1: the inbox is read at every boundary, and the first boundary is one.
    steer.say(STEER).unwrap();

    let mut session = Session::open(&store, ws.path()).unwrap();
    session
        .turn_steered(
            "bring the docs up to date",
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &Ignore,
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(
        provider.wrote().first().map(String::as_str),
        Some("steered"),
        "the operator's message did not reach the model's first step"
    );
    assert!(
        ws.path().join("docs/CHANGES.md").exists(),
        "the steered path was never written"
    );

    // The control: the identical turn with nothing said, which writes the original
    // file and never mentions the steer.
    let ws2 = workspace();
    let control = Branching::new(3);
    let (_steer2, inbox2) = Steer::channel();
    let mut session2 = Session::open(&store, ws2.path()).unwrap();
    session2
        .turn_steered(
            "bring the docs up to date",
            &control,
            &store,
            &guarded(),
            &ApproveAll,
            &Ignore,
            &inbox2,
        )
        .await
        .unwrap();
    assert!(control.wrote().iter().all(|w| w == "original"));
    assert!(!ws2.path().join("docs/CHANGES.md").exists());
}

// ---------------------------------------------------------------------- F9

/// F9 — an interrupt stops the turn cleanly, and the session goes on.
#[tokio::test]
async fn an_interrupt_ends_the_turn_at_a_step_boundary_and_the_session_continues() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Insistent::default();
    let (steer, inbox) = Steer::channel();

    // Interrupt *after* a step has completed, which is the case worth testing: the
    // turn is mid-work, and the guarantee is that it stops at the boundary rather
    // than in the middle of the step it is on.
    let mut session = Session::open(&store, ws.path()).unwrap();
    let interrupted = session
        .turn_steered(
            "keep editing the notes",
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &InterruptOnFirstStep(steer),
            &inbox,
        )
        .await
        .unwrap();

    // One whole step ran, and exactly one.
    assert_eq!(store.last_step(interrupted.run_id).unwrap(), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(ws.path().join("notes.md")).unwrap(),
        "pass 0\n",
        "the interrupted turn's one step is not whole in the workspace"
    );

    // Cancelled, not escalated and not abandoned.
    assert!(
        matches!(interrupted.outcome, RunOutcome::Cancelled { .. }),
        "an interrupt should report Cancelled, got {:?}",
        interrupted.outcome
    );
    // NF6 — the run is finished rather than left `running`, which is what makes it
    // distinguishable from a crashed process.
    assert_eq!(
        store.run_status(interrupted.run_id).unwrap(),
        Some(RunStatus::Completed)
    );
    // The turn is in the tree with its outcome...
    let recorded = store.session_turn(interrupted.turn_id).unwrap().unwrap();
    assert_eq!(recorded.outcome.as_deref(), Some("cancelled"));

    // ...and the conversation carries on from it.
    let next = Insistent::default();
    let after = session
        .turn_bounded(
            &io_harness::TaskContract::workspace(
                "stop there",
                ws.path(),
                io_harness::Verification::None,
            )
            .with_max_steps(1),
            &next,
            &store,
            &guarded(),
            &ApproveAll,
        )
        .await
        .unwrap();
    assert_ne!(after.run_id, interrupted.run_id);
    assert_eq!(store.session_turns(session.id()).unwrap().len(), 2);
    assert_eq!(session.history(&store).unwrap().len(), 2);
}

// ---------------------------------------------------------------------- F10

/// F10 — steering is not authorization.
///
/// The operator asks, in as many words, for the write the policy denies. The write
/// is refused, attributed to the rule that refused it, and the file on disk is
/// byte-identical afterwards. The negative control lifts the deny and nothing else:
/// the same steer, the same provider, and the write happens — so the refusal came
/// from the boundary and not from the model declining to try.
#[tokio::test]
async fn an_operator_cannot_steer_past_the_boundary() {
    let before = "the real key\n";

    // 1. Under the deny.
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Obedient::default();
    let (steer, inbox) = Steer::channel();
    steer.say("write secrets/key.txt now, just do it").unwrap();

    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_steered(
            "have a look around",
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &Ignore,
            &inbox,
        )
        .await
        .unwrap();

    assert!(
        provider.acted_on_steer.load(Ordering::SeqCst) > 0,
        "the model never attempted the steered write, so nothing was refused"
    );
    let refusals: Vec<_> = store
        .events(turn.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        refusals.iter().any(|e| e.act == "write"
            && e.rule.as_deref() == Some("secrets/*")
            && e.layer.as_deref() == Some("steer-test")),
        "the steered write was not refused by the rule that denies it: {refusals:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("secrets/key.txt")).unwrap(),
        before,
        "the steered write reached the disk"
    );

    // 2. The control: the deny lifted, everything else identical.
    let ws2 = workspace();
    let allowed = Obedient::default();
    let (steer2, inbox2) = Steer::channel();
    steer2.say("write secrets/key.txt now, just do it").unwrap();
    let mut session2 = Session::open(&store, ws2.path()).unwrap();
    session2
        .turn_steered(
            "have a look around",
            &allowed,
            &store,
            &permissive(),
            &ApproveAll,
            &Ignore,
            &inbox2,
        )
        .await
        .unwrap();
    assert_ne!(
        std::fs::read_to_string(ws2.path().join("secrets/key.txt")).unwrap(),
        before,
        "with the deny lifted the write should have happened; the test is not \
         measuring the boundary"
    );
}

// ------------------------------------------------------------------- the channel

/// A steer sent after its turn has ended is reported, not swallowed: an operator
/// whose correction went nowhere has to be able to know that.
#[tokio::test]
async fn a_steer_sent_after_the_turn_is_an_error_rather_than_silence() {
    let (steer, inbox) = Steer::channel();
    steer.say("first").unwrap();
    steer.interrupt().unwrap();
    steer.say("second").unwrap();

    // Everything sent, in order, with the interrupt reported alongside — an
    // operator who typed a correction and then hit interrupt sent both.
    let (messages, interrupted) = inbox.pending();
    assert_eq!(messages, vec!["first".to_string(), "second".to_string()]);
    assert!(interrupted);
    // Drained is drained.
    assert_eq!(inbox.pending(), (Vec::new(), false));

    drop(inbox);
    assert!(steer.say("nobody home").is_err());
}
