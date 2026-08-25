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
    ApproveAll, Compaction, Containment, EventKind, Flow, Ignore, Observer, Policy, Provider,
    RunEvent, RunOutcome, RunStatus, Session, Steer, Store, SystemPrompt, TaskContract,
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

/// Says the operator's correction the moment the first step commits — the
/// correction typed while the agent is already working, rather than one queued
/// before it started. Once, so the assertion about *which* step read it means
/// something.
struct SayOnFirstStep(Steer, AtomicUsize);

impl Observer for SayOnFirstStep {
    fn event(&self, event: &RunEvent) -> Flow {
        if matches!(event.kind, EventKind::Step { .. })
            && self.1.fetch_add(1, Ordering::SeqCst) == 0
        {
            // Ignored on a closed channel, for the reason `InterruptOnFirstStep`
            // ignores it: an observer must not panic.
            let _ = self.0.say(STEER);
        }
        Flow::Continue
    }
}

/// Records every request whole, and never stops asking for a write — so the turn
/// ends on the contract's bound rather than on the model's say-so.
///
/// A claim about what the contract composed and a claim about what the operator's
/// message reached are both claims about the requests this kept.
#[derive(Default)]
struct Recording {
    seen: Mutex<Vec<(String, String)>>,
    calls: AtomicUsize,
}

impl Recording {
    /// The system prompt composed for the nth completion.
    fn system(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].0.clone()
    }

    /// The assembled context sent on the nth completion.
    fn user(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].1.clone()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for Recording {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen
            .lock()
            .unwrap()
            .push((req.system.clone(), req.user.clone()));
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
        "recording"
    }
}

/// The sentence the summarising model writes (0.69.0). Distinctive, so a request
/// can be asked whether it carries *this* summary rather than something
/// summary-shaped.
const FOLD_SUMMARY: &str = "ZZ-FOLDED-ZZ the earlier steps looked at notes.md.";

/// How a summarising request is told apart from a working one, without the test
/// re-implementing the prompt: the system block the crate sends for a fold says
/// this and no working request does.
const SUMMARISER: &str = "compacting an agent's own working notes";

/// A string that appears in the first step's observation and nowhere else, so
/// "the fold replaced it" is distinguishable from "the request happens not to
/// mention it".
///
/// Carried by a *read* rather than by a write: a write's observation names the
/// path and the size, so content sent through one never reaches the ledger and an
/// assertion about the ledger losing it would pass whatever the fold did.
const FIRST_STEP: &str = "QQ-ONLY-IN-THE-FIRST-STEP-QQ";

/// Writes a file every step and answers the summariser, keeping every request.
///
/// The working half never stops asking, so a turn ends on its contract's bound
/// rather than on the model's say-so and the operator has boundaries to send at.
#[derive(Default)]
struct Folding {
    seen: Mutex<Vec<(String, String)>>,
    working: AtomicUsize,
    summarised: AtomicUsize,
}

impl Folding {
    /// The requests that were not the summariser's, in order.
    fn working(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(system, _)| !system.contains(SUMMARISER))
            .map(|(_, user)| user.clone())
            .collect()
    }

    fn summarised(&self) -> usize {
        self.summarised.load(Ordering::SeqCst)
    }
}

impl Provider for Folding {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let summarising = req.system.contains(SUMMARISER);
        self.seen
            .lock()
            .unwrap()
            .push((req.system.clone(), req.user.clone()));
        if summarising {
            self.summarised.fetch_add(1, Ordering::SeqCst);
            return Ok(CompletionResponse {
                text: Some(FOLD_SUMMARY.into()),
                usage: Some(usage()),
                ..Default::default()
            });
        }
        let n = self.working.fetch_add(1, Ordering::SeqCst);
        // The first step reads the marked file, so the marker is an observation
        // the fold can be asked to have replaced; every step after it writes, so
        // the ledger keeps growing by one entry a step.
        let call = if n == 0 {
            ToolCall {
                name: "read_file".into(),
                arguments: json!({ "path": "marker.md" }),
            }
        } else {
            ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "notes.md", "content": format!("pass {n}\n") }),
            }
        };
        Ok(CompletionResponse {
            tool_calls: vec![call],
            usage: Some(usage()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "folding"
    }
}

/// The operator at the keyboard (0.69.0): sends `fold()` at the step boundaries
/// named, optionally an interrupt beside one of them, and counts the folds that
/// resulted.
///
/// Sending and counting are one object because a turn takes one observer, and
/// counting `Compacted` off the event stream keeps the "did it fold" half
/// independent of the store.
struct Operator {
    steer: Steer,
    /// Step-event indices after which a fold is asked for.
    fold_after: Vec<usize>,
    /// The step-event index after which the turn is also interrupted, if any.
    interrupt_after: Option<usize>,
    steps: AtomicUsize,
    folded: Mutex<Vec<u32>>,
}

impl Operator {
    fn folding_after(steer: Steer, fold_after: Vec<usize>) -> Self {
        Self {
            steer,
            fold_after,
            interrupt_after: None,
            steps: AtomicUsize::new(0),
            folded: Mutex::new(Vec::new()),
        }
    }

    fn also_interrupting_after(mut self, step: usize) -> Self {
        self.interrupt_after = Some(step);
        self
    }

    /// The `through_step` of every fold this turn emitted, in order.
    fn folds(&self) -> Vec<u32> {
        self.folded.lock().unwrap().clone()
    }
}

impl Observer for Operator {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::Step { .. } => {
                let n = self.steps.fetch_add(1, Ordering::SeqCst);
                // Ignored on a closed channel, for the reason every other observer
                // here ignores it: an observer must not panic.
                if self.fold_after.contains(&n) {
                    let _ = self.steer.fold();
                }
                // Sent after the fold and read in the same drain, which is the
                // arrangement F5 is about.
                if self.interrupt_after == Some(n) {
                    let _ = self.steer.interrupt();
                }
            }
            EventKind::Compacted { through_step, .. } => {
                self.folded.lock().unwrap().push(*through_step);
            }
            _ => {}
        }
        Flow::Continue
    }
}

/// A turn long enough to fold, under a threshold that never fires on its own.
///
/// `keep_recent: 1` so a fold is possible after one durable observation, and
/// `at_share: 0.99` so nothing this small crosses the threshold — any fold in
/// these tests is one somebody asked for.
fn foldable(goal: &str, root: &std::path::Path, max_steps: u32) -> TaskContract {
    TaskContract::workspace(goal, root)
        .with_max_steps(max_steps)
        .with_compaction(Compaction {
            at_share: 0.99,
            keep_recent: 1,
        })
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
    // Read by the first step of a foldable turn (0.69.0), so the oldest
    // observation carries a string nothing else does.
    std::fs::write(dir.path().join("marker.md"), format!("{FIRST_STEP}\n")).unwrap();
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
            &io_harness::TaskContract::workspace("stop there", ws.path()).with_max_steps(1),
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
    let steering = inbox.pending();
    assert_eq!(
        steering.messages,
        vec!["first".to_string(), "second".to_string()]
    );
    assert!(steering.interrupted);
    // Drained is drained. Per field rather than against a literal: `Steering` is
    // `#[non_exhaustive]`, so a struct expression here would not compile outside
    // the crate — which is the property the type exists to have.
    let drained = inbox.pending();
    assert!(drained.messages.is_empty());
    assert!(!drained.interrupted);
    assert!(!drained.fold);

    drop(inbox);
    assert!(steer.say("nobody home").is_err());
}

// --------------------------------------------------------------- F7 (0.69.0)

/// **F7 (0.69.0)** — the drained state is complete, and a fold nobody read is
/// visible.
///
/// The surface criterion for this release's one break. `pending()` returned
/// `(Vec<String>, bool)` through 0.68.0, and a third thing an operator can send
/// either grows that tuple — the same break again at the fourth — or is dropped
/// from it silently, which loses a request the operator was told had been sent.
/// This is the test that fails if it is dropped.
#[tokio::test]
async fn a_fold_nobody_read_is_still_in_the_inbox_and_a_late_one_is_an_error() {
    let (steer, inbox) = Steer::channel();
    steer.say("prefer the smaller diff").unwrap();
    steer.fold().unwrap();

    // All three, from one drain: the message the operator typed and the fold they
    // asked for, with no interrupt invented alongside them.
    let steering = inbox.pending();
    assert_eq!(
        steering.messages,
        vec!["prefer the smaller diff".to_string()]
    );
    assert!(steering.fold, "the fold request was dropped on the way out");
    assert!(!steering.interrupted);

    // Two folds in one drain are one fold: the second would summarise a ledger the
    // first has just replaced with a paragraph.
    steer.fold().unwrap();
    steer.fold().unwrap();
    let twice = inbox.pending();
    assert!(twice.fold);
    assert!(twice.messages.is_empty());

    let drained = inbox.pending();
    assert!(!drained.fold, "a drained fold was reported a second time");

    drop(inbox);
    assert!(
        steer.fold().is_err(),
        "a fold asked for after the turn ended was swallowed rather than refused"
    );
}

// ------------------------------------------------- a steered turn with a contract

/// The replacement this release's contract carries, distinctive enough that its
/// presence in the composed prompt cannot be a coincidence.
const REPLACEMENT: &str = "you are the release scribe and you write nothing else";

/// **F1 (0.67.0)** — a steered bounded turn honours the contract *and* reads the
/// steer.
///
/// Either half alone is what the crate could already do: `turn_bounded_observed`
/// honours a contract and hears no operator, `turn_steered` hears the operator and
/// builds its own contract. So both halves are asserted on one turn, because the
/// release is the conjunction and nothing else.
#[tokio::test]
async fn a_steered_bounded_turn_honours_its_contract_and_reads_the_steer() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Recording::default();
    let (steer, inbox) = Steer::channel();

    // A contract `turn_steered` could not have built: a replaced description and a
    // step bound of the caller's own.
    let contract = TaskContract::workspace("bring the docs up to date", ws.path())
        .with_system_prompt(SystemPrompt::Replace(REPLACEMENT.into()))
        .with_max_steps(3);

    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &SayOnFirstStep(steer, AtomicUsize::new(0)),
            &inbox,
        )
        .await
        .unwrap();

    // 1. The caller's contract composed the prompt, so it was not discarded for a
    //    `default_contract`.
    assert!(
        provider.system(0).contains(REPLACEMENT),
        "the contract's replaced description never reached the composer: {}",
        provider.system(0)
    );

    // 2. The operator's message reached the agent at a step boundary — not before
    //    the turn started, which is the case `turn_steered` already covers.
    assert!(
        !provider.user(0).contains(STEER),
        "the message was in the first step's context, so this says nothing about a boundary"
    );
    assert!(
        provider.user(1).contains(STEER),
        "the operator's message never reached the ledger: {}",
        provider.user(1)
    );

    // 3. And the contract's bound is what ended the turn, rather than the model
    //    running on until something else stopped it.
    assert!(
        matches!(turn.outcome, RunOutcome::StepCapReached { steps: 3 }),
        "the turn did not stop at the contract's step bound, got {:?}",
        turn.outcome
    );
    assert_eq!(
        provider.calls(),
        3,
        "the step bound was not the three asked for"
    );
}

/// **F4 (0.67.0)** — an interrupt ends a bounded steered turn as `Cancelled`, on a
/// whole step, and what it leaves behind is readable and carried on from.
///
/// This is `turn_steered`'s promise since 0.20.0, re-asserted through the new entry
/// point: a caller migrating to a contract must not silently lose the ability to
/// stop the turn.
///
/// **What "resumable" means here, stated rather than assumed.** `resume` *reports*
/// a cancelled run and drives nothing — the rule in `terminal_outcome` since
/// 0.12.0, for the reason a `denied` run is final: the caller asked for it, and
/// restarting the loop under them would be answering a question nobody asked. So
/// the promise the interrupt keeps is that the run is left whole and readable
/// rather than half-written, and that the conversation goes on from it. Both are
/// asserted below.
#[tokio::test]
async fn an_interrupt_ends_a_steered_bounded_turn_and_the_session_carries_on() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Insistent::default();
    let (steer, inbox) = Steer::channel();

    let contract = TaskContract::workspace("keep editing the notes", ws.path()).with_max_steps(5);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let interrupted = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &InterruptOnFirstStep(steer),
            &inbox,
        )
        .await
        .unwrap();

    assert!(
        matches!(interrupted.outcome, RunOutcome::Cancelled { .. }),
        "an interrupt should report Cancelled, got {:?}",
        interrupted.outcome
    );
    // One whole step, and the workspace holds all of it — an interrupt stops the
    // turn at the boundary, not in the middle of the step it is on.
    assert_eq!(store.last_step(interrupted.run_id).unwrap(), 1);
    assert_eq!(
        std::fs::read_to_string(ws.path().join("notes.md")).unwrap(),
        "pass 0\n",
        "the interrupted turn's one step is not whole in the workspace"
    );
    // Finished rather than left `running`, which is what makes it distinguishable
    // from a crashed process — and what makes it resumable.
    assert_eq!(
        store.run_status(interrupted.run_id).unwrap(),
        Some(RunStatus::Completed)
    );

    // Readable rather than corrupt: `resume` reads the run back and reports what
    // the operator asked for, without driving the loop or asking the provider.
    let more = Insistent::default();
    let read_back = io_harness::resume(&contract, &more, &store, interrupted.run_id)
        .await
        .unwrap();
    assert!(
        matches!(read_back.outcome, RunOutcome::Cancelled { steps: 1 }),
        "resume did not report the cancellation it was handed, got {:?}",
        read_back.outcome
    );
    assert_eq!(
        more.calls.load(Ordering::SeqCst),
        0,
        "resume re-drove a run its operator cancelled"
    );

    // And the conversation goes on from it: the interrupted turn is in the tree
    // with its outcome, and the next turn reads it like any other.
    let recorded = store.session_turn(interrupted.turn_id).unwrap().unwrap();
    assert_eq!(recorded.outcome.as_deref(), Some("cancelled"));

    let next = Recording::default();
    let (_steer2, inbox2) = Steer::channel();
    let after = session
        .turn_bounded_steered(
            &TaskContract::workspace("stop there", ws.path()).with_max_steps(1),
            &next,
            &store,
            &guarded(),
            &ApproveAll,
            &Ignore,
            &inbox2,
        )
        .await
        .unwrap();
    assert_ne!(after.run_id, interrupted.run_id);
    assert_eq!(store.session_turns(session.id()).unwrap().len(), 2);
    assert_eq!(session.history(&store).unwrap().len(), 2);
}

/// **F5 (0.67.0)** — the session's root wins over the contract's own, on both new
/// entry points.
///
/// `turn_bounded` has made this promise since 0.36.0: a turn is about the
/// conversation's workspace, and a contract naming another directory would be
/// answering about a different project. A steered twin that dropped `rooted` would
/// make the operator's own correction channel the way out of it.
#[tokio::test]
async fn both_steered_entry_points_run_in_the_sessions_workspace() {
    // The flat arm.
    let session_dir = workspace();
    let elsewhere = workspace();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, session_dir.path()).unwrap();
    let contract = TaskContract::workspace("edit the notes", elsewhere.path()).with_max_steps(1);
    let (_steer, inbox) = Steer::channel();

    session
        .turn_bounded_steered(
            &contract,
            &Recording::default(),
            &store,
            &guarded(),
            &ApproveAll,
            &Ignore,
            &inbox,
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(session_dir.path().join("notes.md")).unwrap(),
        "pass 0\n",
        "the steered bounded turn wrote outside the session's workspace"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.path().join("notes.md")).unwrap(),
        "# notes\n",
        "the contract's own root was used, so a steered turn escaped its conversation"
    );

    // The contained arm, same contract, same claim.
    let session_dir2 = workspace();
    let elsewhere2 = workspace();
    let mut session2 = Session::open(&store, session_dir2.path()).unwrap();
    let contract2 = TaskContract::workspace("edit the notes", elsewhere2.path()).with_max_steps(1);
    let (_steer2, inbox2) = Steer::channel();

    session2
        .turn_contained_bounded_steered(
            &contract2,
            &Recording::default(),
            &store,
            &guarded(),
            &ApproveAll,
            &Containment::new(10, 4, 3, 1_000_000),
            &Ignore,
            &inbox2,
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(session_dir2.path().join("notes.md")).unwrap(),
        "pass 0\n",
        "the steered contained turn wrote outside the session's workspace"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere2.path().join("notes.md")).unwrap(),
        "# notes\n",
        "the contract's own root was used, so a steered fan-out escaped its conversation"
    );
}

// ------------------------------------------------- the operator's fold (0.69.0)

/// **F1 (0.69.0)** — a fold sent mid-turn lands at the next step boundary, before
/// that step's request.
///
/// Both halves are asserted because either alone is something the crate could
/// already do: that it folded at all, and that the fold reached the request built
/// *after* the boundary that read it rather than the one after that. The
/// threshold is set where nothing this small crosses it, so the fold is
/// attributable to the operator and to nothing else.
#[tokio::test]
async fn a_fold_asked_for_mid_turn_lands_at_the_next_boundary() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Folding::default();
    let (steer, inbox) = Steer::channel();
    // Sent after the second step commits: with `keep_recent: 1` a fold needs more
    // than one entry to reach, and a request the loop cannot honour is consumed
    // rather than held.
    let operator = Operator::folding_after(steer, vec![1]);

    let contract = foldable("keep the notes moving", ws.path(), 4);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &operator,
            &inbox,
        )
        .await
        .unwrap();

    // 1. It folded, once, and at the boundary that read the request rather than at
    //    the turn's first step — which is what tells this apart from `fold_now`.
    assert_eq!(
        operator.folds().len(),
        1,
        "expected exactly one fold, got {:?}",
        operator.folds()
    );
    assert!(
        operator.folds()[0] >= 3,
        "the fold landed at step {}, which is earlier than the boundary the operator sent at",
        operator.folds()[0]
    );
    assert_eq!(
        provider.summarised(),
        1,
        "the summariser ran the wrong number of times"
    );
    assert_eq!(
        store.summaries(turn.run_id).unwrap().len(),
        1,
        "the fold left no durable summary"
    );

    // 2. The request built after that boundary carries the summary, and no longer
    //    carries the observation the fold replaced. The marker is the
    //    discriminating half: a request that merely stopped mentioning the first
    //    step would pass the first assertion on its own.
    let working = provider.working();
    // The second request is the first one built after the read committed, so it is
    // where the marker is if the ledger ever held it. Without this the assertion
    // below would pass against a marker that never arrived.
    assert!(
        working[1].contains(FIRST_STEP),
        "the first step's own observation never reached the ledger, so its absence later proves \
         nothing"
    );
    let after = working
        .iter()
        .find(|user| user.contains(FOLD_SUMMARY))
        .expect("no request after the fold carried the summary");
    assert!(
        !after.contains(FIRST_STEP),
        "the request carrying the summary still carried what the summary replaced"
    );
}

/// **F2 (0.69.0)** — the same turn with nothing sent does not fold.
///
/// The control F1 is meaningless without: it says the fold came from the operator
/// rather than from a fixture that folds on its own, and it is the criterion that
/// proves a caller who never asks sees exactly 0.68.0's behaviour.
#[tokio::test]
async fn without_a_send_the_same_turn_does_not_fold() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Folding::default();
    let (steer, inbox) = Steer::channel();
    // The same operator, sending at no boundary at all.
    let operator = Operator::folding_after(steer, vec![]);

    let contract = foldable("keep the notes moving", ws.path(), 4);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &operator,
            &inbox,
        )
        .await
        .unwrap();

    assert!(operator.folds().is_empty(), "an unasked-for fold happened");
    assert_eq!(provider.summarised(), 0, "the summariser was called anyway");
    assert!(store.summaries(turn.run_id).unwrap().is_empty());
    assert!(
        provider.working().last().unwrap().contains(FIRST_STEP),
        "the conversation was shortened without anybody asking"
    );
}

/// **F3 (0.69.0)** — an off setting stays off.
///
/// `Compaction { at_share: 1.0, .. }` never folds, and this trigger is not an
/// exception. The alternative reading — an explicit request beats an explicit off
/// — is the one somebody implements by accident, and it would make "off" mean two
/// things: the crate already promises this answer for the overflow recovery.
#[tokio::test]
async fn a_fold_asked_for_does_not_override_an_off_setting() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Folding::default();
    let (steer, inbox) = Steer::channel();
    let operator = Operator::folding_after(steer, vec![1]);

    let contract = TaskContract::workspace("keep the notes moving", ws.path())
        .with_max_steps(4)
        .with_compaction(Compaction {
            at_share: 1.0,
            keep_recent: 1,
        });
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &operator,
            &inbox,
        )
        .await
        .unwrap();

    assert!(operator.folds().is_empty(), "an off setting folded anyway");
    assert_eq!(provider.summarised(), 0, "an off setting bought a summary");
    assert!(store.summaries(turn.run_id).unwrap().is_empty());
    // And the turn is unharmed: a request that cannot be honoured is not an error.
    assert!(
        matches!(turn.outcome, RunOutcome::StepCapReached { steps: 4 }),
        "the turn did not run to its bound, got {:?}",
        turn.outcome
    );
}

/// **F4 (0.69.0)** — one send, one fold; a second send folds again.
///
/// The bug this catches is the flag being read rather than taken, which turns one
/// request into a mode where every step folds. Asserted over a turn long enough
/// for a second fold to be visible, with the threshold set high enough that any
/// fold at all is attributable to a send.
#[tokio::test]
async fn each_send_folds_once_and_not_every_step_after_it() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Folding::default();
    let (steer, inbox) = Steer::channel();
    // Two sends, two boundaries apart, over a turn of six steps — both at
    // boundaries where the ledger is long enough for a fold to reach.
    let operator = Operator::folding_after(steer, vec![1, 3]);

    let contract = foldable("keep the notes moving", ws.path(), 6);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &operator,
            &inbox,
        )
        .await
        .unwrap();

    let folds = operator.folds();
    assert_eq!(
        folds.len(),
        2,
        "two sends produced {} folds: {:?}",
        folds.len(),
        folds
    );
    assert!(
        folds[0] < folds[1],
        "both folds landed on the same step: {folds:?}"
    );
    assert_eq!(
        store.summaries(turn.run_id).unwrap().len(),
        2,
        "the second fold wrote no summary of its own"
    );
}

/// **F5 (0.69.0)** — an interrupt beside a fold ends the turn and does not fold.
///
/// Both are sent before the same boundary, so the drain holds both and the order
/// it answers them in is the behaviour. Stopping wins: a summariser call spent on
/// a turn nobody is going to read is money the run does not get back.
#[tokio::test]
async fn an_interrupt_beside_a_fold_stops_the_turn_and_buys_no_summary() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Folding::default();
    let (steer, inbox) = Steer::channel();
    // Sent at a boundary where a fold is genuinely reachable — F1 folds at this
    // one — so the absence below is of something that could have happened.
    let operator = Operator::folding_after(steer, vec![1]).also_interrupting_after(1);

    let contract = foldable("keep the notes moving", ws.path(), 6);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn_bounded_steered(
            &contract,
            &provider,
            &store,
            &guarded(),
            &ApproveAll,
            &operator,
            &inbox,
        )
        .await
        .unwrap();

    assert!(
        matches!(turn.outcome, RunOutcome::Cancelled { .. }),
        "the interrupt did not end the turn, got {:?}",
        turn.outcome
    );
    assert!(
        operator.folds().is_empty(),
        "the turn folded on its way out: {:?}",
        operator.folds()
    );
    assert_eq!(
        provider.summarised(),
        0,
        "a summary was bought for a turn that was being stopped"
    );
    assert!(store.summaries(turn.run_id).unwrap().is_empty());
}
