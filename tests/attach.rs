//! 0.33.0 — two processes, one run.
//!
//! A second process attaches to a run that is still going, receives the same
//! serialised events the owning process receives, and answers the approval, the
//! question or the plan it is holding — without killing it and without taking it
//! over.
//!
//! The tests that matter here are the ones that could pass against a harness that
//! did none of that. "The attached reader saw some events" passes against a
//! reconstruction assembled from the trace; "the run finished" passes against a
//! run that ignored the answer entirely. Each of those has a negative control, and
//! the two kills are real kills.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use io_harness::approve::PlanVerdict;
use io_harness::{
    Attach, Broadcast, Decision, EventKind, Flow, Ignore, Observer, Plan, PlanStep, Question,
    RunEvent, Store, Waiting,
};
use tempfile::TempDir;

/// Collects every event it is handed, so a stream read back out of the store can
/// be compared against the one the owning process actually received.
#[derive(Default)]
struct Recorder(Mutex<Vec<RunEvent>>);

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

impl Recorder {
    fn seen(&self) -> Vec<RunEvent> {
        self.0.lock().unwrap().clone()
    }
}

fn ws() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// A handful of events, emitted the way a run emits them, through whatever
/// observer is given.
fn emit_a_few(observer: &dyn Observer, run_id: i64) {
    observer.event(&RunEvent::new(
        run_id,
        0,
        EventKind::Started {
            goal: "port it".into(),
            provider: "mock".into(),
        },
    ));
    observer.event(&RunEvent::new(
        run_id,
        1,
        EventKind::ApprovalRequested {
            act: "write".into(),
            target: "src/a.rs".into(),
        },
    ));
    observer.event(&RunEvent::new(
        run_id,
        1,
        EventKind::Step {
            decision: "wrote src/a.rs".into(),
            tool_call: "write_file:{}".into(),
            tokens: 412,
            changed: true,
        },
    ));
}

// ---------------------------------------------------------------------------
// F1 — the attached stream is the same stream, not a reconstruction.
// ---------------------------------------------------------------------------

#[test]
fn an_attached_reader_gets_the_same_events_the_in_process_observer_got() {
    let dir = ws();
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();

    let recorder = Recorder::default();
    let broadcasting = Broadcast::new(Store::open(&path).unwrap(), &recorder);
    emit_a_few(&broadcasting, run_id);

    // A different connection, as a different process would have.
    let reader = Store::open(&path).unwrap();
    let read_back = Attach::to(&reader, run_id).poll().unwrap();

    // Equality of the values, not of the count or the tags. A reconstruction
    // assembled from the trace's own tables would have the right number of
    // roughly-right-looking events and would fail here.
    assert_eq!(
        read_back,
        recorder.seen(),
        "the attached stream must be the events the owning observer received"
    );
}

/// F1's negative control. Without a `Broadcast` there is nothing to read, which is
/// what proves the equality above came from the durable stream rather than from a
/// reader that can reconstruct events on its own.
#[test]
fn without_a_broadcast_an_attached_reader_sees_nothing() {
    let dir = ws();
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();

    let recorder = Recorder::default();
    emit_a_few(&recorder, run_id);
    assert_eq!(recorder.seen().len(), 3, "the observer still saw them");

    let reader = Store::open(&path).unwrap();
    assert!(
        Attach::to(&reader, run_id).poll().unwrap().is_empty(),
        "a run that does not broadcast leaves no durable stream to attach to"
    );
}

// ---------------------------------------------------------------------------
// F2 — the cursor is the caller's to choose, and it advances.
// ---------------------------------------------------------------------------

#[test]
fn attaching_from_zero_returns_the_whole_backlog() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    emit_a_few(&Broadcast::new(Store::memory().unwrap(), &Ignore), run_id);
    // The broadcast above went to its own memory store, so seed this one directly.
    emit_into(&store, run_id, 3);

    let mut view = Attach::to(&store, run_id);
    assert_eq!(view.poll().unwrap().len(), 3);
    assert!(view.cursor() > 0, "the cursor advanced to the last event");
    assert!(
        view.poll().unwrap().is_empty(),
        "a second poll with nothing new repeats nothing"
    );
}

#[test]
fn attaching_from_now_skips_what_already_happened() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    emit_into(&store, run_id, 3);

    let mut view = Attach::to(&store, run_id).from_now().unwrap();
    assert!(
        view.poll().unwrap().is_empty(),
        "the backlog is not this reader's"
    );

    emit_into(&store, run_id, 2);
    assert_eq!(
        view.poll().unwrap().len(),
        2,
        "but everything after it is"
    );
}

#[test]
fn attaching_from_a_recorded_cursor_resumes_exactly_there() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    emit_into(&store, run_id, 4);

    // Read two, remember where we were, and come back as a restarted reader would.
    let mut first = Attach::to(&store, run_id);
    let all = first.poll().unwrap();
    assert_eq!(all.len(), 4);
    let midpoint = store.events_since(run_id, 0, 2).unwrap()[1].0;

    let resumed = Attach::to(&store, run_id)
        .from_cursor(midpoint)
        .poll()
        .unwrap();
    assert_eq!(resumed, all[2..], "exactly what came after the cursor");
}

#[test]
fn a_tree_reader_sees_its_children_interleaved() {
    let store = Store::memory().unwrap();
    let root = store.start_run("fan out", "mock").unwrap();
    let child = store
        .start_child_run("a sub-task", "mock", root, 1)
        .unwrap();

    store
        .put_event(&RunEvent::new(root, 1, EventKind::Stalled))
        .unwrap();
    store
        .put_event(&RunEvent::at_depth(child, 1, 1, EventKind::Stalled))
        .unwrap();
    store
        .put_event(&RunEvent::new(root, 2, EventKind::Stalled))
        .unwrap();

    let seen = Attach::to_tree(&store, root).poll().unwrap();
    assert_eq!(
        seen.iter().map(|e| e.run_id).collect::<Vec<_>>(),
        vec![root, child, root],
        "in the order they happened, not grouped by run"
    );
    // And the single-run reader is genuinely narrower, so the tree read is doing
    // something rather than being the same query with a longer name.
    assert_eq!(Attach::to(&store, root).poll().unwrap().len(), 2);
}

/// Write `n` plain events straight into the store.
fn emit_into(store: &Store, run_id: i64, n: u32) {
    for step in 0..n {
        store
            .put_event(&RunEvent::new(run_id, step, EventKind::Stalled))
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// F5 — first answer wins, and the loser is told.
// ---------------------------------------------------------------------------

#[test]
fn two_processes_answering_one_approval_produce_exactly_one_winner() {
    let dir = ws();
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let run_id = store.start_run("ship it", "mock").unwrap();
    let request_id = store
        .put_pending(run_id, 3, "write", "deploy/prod.yaml", None)
        .unwrap();

    // Two connections, as two processes would have.
    let a = Store::open(&path).unwrap();
    let b = Store::open(&path).unwrap();
    let first = Attach::to(&a, run_id)
        .answer_approval(request_id, Decision::approve())
        .unwrap();
    let second = Attach::to(&b, run_id)
        .answer_approval(request_id, Decision::deny("no"))
        .unwrap();

    assert_ne!(first, second, "exactly one of them landed");
    assert!(first || second, "and one of them did");
    assert_eq!(
        store.pending(request_id).unwrap().unwrap().resolved.as_deref(),
        Some("approve"),
        "the first answer is what stands"
    );
}

#[test]
fn two_processes_answering_one_question_produce_exactly_one_winner() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    let id = store
        .put_question(run_id, 2, &Question::new("which database?"))
        .unwrap();

    let first = Attach::to(&store, run_id)
        .answer_question(id, "postgres")
        .unwrap();
    let second = Attach::to(&store, run_id)
        .answer_question(id, "sqlite")
        .unwrap();

    assert_ne!(first, second);
    assert!(first || second);
    assert_eq!(
        store.question(id).unwrap().unwrap().answer.as_deref(),
        Some("postgres"),
    );
}

#[test]
fn two_processes_deciding_one_plan_produce_exactly_one_winner() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    let id = store
        .put_plan(run_id, 1, &Plan::new([PlanStep::new("read first")]))
        .unwrap();

    let first = Attach::to(&store, run_id)
        .answer_plan(id, PlanVerdict::Approve)
        .unwrap();
    let second = Attach::to(&store, run_id)
        .answer_plan(id, PlanVerdict::Cancel)
        .unwrap();

    assert_ne!(first, second);
    assert!(first || second);
    assert_eq!(
        store.plan(id).unwrap().unwrap().verdict,
        Some(PlanVerdict::Approve),
    );
}

/// Deferring from an attached process is refused rather than written. It would
/// report an answer while leaving the run exactly as it was.
#[test]
fn an_attached_process_cannot_defer() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("ship it", "mock").unwrap();
    let id = store.put_pending(run_id, 1, "write", "a.txt", None).unwrap();

    assert!(Attach::to(&store, run_id)
        .answer_approval(id, Decision::Defer)
        .is_err());
    assert!(
        store.pending(id).unwrap().unwrap().resolved.is_none(),
        "and nothing was written"
    );
}

// ---------------------------------------------------------------------------
// `waiting()` — all three, including the plan.
// ---------------------------------------------------------------------------

#[test]
fn waiting_reports_an_approval_a_question_and_a_plan() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    store
        .put_pending(run_id, 1, "write", "a.txt", None)
        .unwrap();
    store
        .put_question(run_id, 2, &Question::new("which database?"))
        .unwrap();
    store
        .put_plan(run_id, 3, &Plan::new([PlanStep::new("read first")]))
        .unwrap();

    let waiting = Attach::to(&store, run_id).waiting().unwrap();
    assert_eq!(waiting.len(), 3, "a pending plan is a waiting run too");
    assert!(matches!(waiting[0], Waiting::Approval { .. }));
    assert!(matches!(waiting[1], Waiting::Question { .. }));
    assert!(matches!(waiting[2], Waiting::Plan { .. }));
}

/// The discriminating half: an *answered* one is not waiting. Without this,
/// `waiting()` returning three rows would pass against an implementation that
/// reported every row it had ever written — which, since 0.33.0 writes the
/// approval row before the approver is consulted, is now most of them.
#[test]
fn waiting_excludes_everything_that_has_been_answered() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();
    let approval = store
        .put_pending(run_id, 1, "write", "a.txt", None)
        .unwrap();
    let question = store
        .put_question(run_id, 2, &Question::new("which database?"))
        .unwrap();
    let plan = store
        .put_plan(run_id, 3, &Plan::new([PlanStep::new("read first")]))
        .unwrap();

    assert!(store.resolve_pending(approval, "approve").unwrap());
    assert!(store.answer_question(question, "postgres", "human").unwrap());
    assert!(store.decide_plan(plan, &PlanVerdict::Approve, "human").unwrap());

    assert!(
        Attach::to(&store, run_id).waiting().unwrap().is_empty(),
        "a run that has been answered is not holding anything"
    );
}

// ---------------------------------------------------------------------------
// F8 — ownership cannot be taken, structurally.
// ---------------------------------------------------------------------------

/// `Attach` has no method that starts, resumes or steps a run. That is the
/// mechanism the release rests on, so it is read off the source rather than
/// asserted in prose — the technique `tests/public_api.rs` has used since 0.16.0.
#[test]
fn attach_has_no_method_that_drives_a_run() {
    let source = std::fs::read_to_string("src/attach.rs").unwrap();
    let offenders = driving_methods(&source);
    assert!(
        offenders.is_empty(),
        "an attached process reads and decides; it must not be able to drive a run, \
         but these could: {offenders:?}"
    );
}

/// F8's control. The same check against a source with one such method spliced in
/// must fail and name it — a check that passes immediately has not been shown to
/// discriminate.
#[test]
fn the_no_driving_check_catches_a_driving_method() {
    let mut source = std::fs::read_to_string("src/attach.rs").unwrap();
    source.push_str(
        "impl Attach<'_> {\n    pub fn resume_it(&self) -> Result<()> { Ok(()) }\n}\n",
    );
    assert_eq!(
        driving_methods(&source),
        vec!["resume_it".to_string()],
        "the check must name the method it caught"
    );
}

/// Every `pub fn` in the source whose name says it drives a run.
///
/// Line-ending normalised, because the Windows checkout is CRLF and a
/// source-reading test that forgets it fails on exactly one runner.
fn driving_methods(source: &str) -> Vec<String> {
    source
        .replace("\r\n", "\n")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub fn "))
        .filter_map(|l| l.split(['(', '<']).next())
        .filter(|name| {
            name.starts_with("run") || name.starts_with("resume") || name.starts_with("step")
        })
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// N4 — the durable stream costs one row per event, and nothing when unused.
// ---------------------------------------------------------------------------

#[test]
fn broadcasting_writes_one_row_per_event_and_nothing_without_it() {
    let dir = ws();
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let run_id = store.start_run("port it", "mock").unwrap();

    emit_a_few(&Ignore, run_id);
    assert!(
        store.events_since(run_id, 0, 100).unwrap().is_empty(),
        "a run that does not broadcast writes no rows at all"
    );

    let recorder = Recorder::default();
    emit_a_few(&Broadcast::new(Store::open(&path).unwrap(), &recorder), run_id);
    assert_eq!(
        store.events_since(run_id, 0, 100).unwrap().len(),
        recorder.seen().len(),
        "one row per event the observer received"
    );
}

// ---------------------------------------------------------------------------
// F3 / F4 / F6 / F7 — the live fixture. Unix only: they are real kills.
// ---------------------------------------------------------------------------

/// A spawned fixture that is killed when it goes out of scope.
///
/// These fixtures park forever by design, so a test that fails before answering
/// would otherwise leak a process that never exits — on a developer's machine and
/// on a CI runner alike. `Child` is not killed on drop, so this does it.
struct Fixture {
    child: Option<std::process::Child>,
    /// Cached, because `try_wait` reaps: once it has answered, asking again
    /// answers `None` forever and a later `finished()` would wait for a process
    /// that is already gone.
    status: Option<std::process::ExitStatus>,
}

impl Fixture {
    fn new(child: std::process::Child) -> Self {
        Self {
            child: Some(child),
            status: None,
        }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    /// Whether it has finished, without blocking.
    fn exited(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.as_mut().unwrap().try_wait().unwrap();
        }
        self.status.is_some()
    }

    /// Wait for it to finish and return what it printed. Bounded, so a fixture
    /// that never exits is a named failure rather than a run that stalls until the
    /// job is killed.
    fn finished(mut self) -> String {
        until("the fixture to exit", || self.exited().then_some(()));
        let status = self.status.unwrap();
        let mut child = self.child.take().unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert!(
            status.success(),
            "the fixture must exit 0, got {status}: {out}"
        );
        out
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Some(child), None) = (self.child.as_mut(), self.status) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Locate the compiled `attach_fixture` example next to this test binary.
/// Standard cargo layout: `target/<profile>/deps/<test>` and
/// `target/<profile>/examples/<name>`. It joins `crash_fixture`,
/// `plan_gate_fixture` and `fleet_fixture` on the list of examples that
/// `cargo test --all-features --lib --tests` does not build — run
/// `cargo build --all-features --examples` first.
fn fixture_bin() -> std::path::PathBuf {
    let me = std::env::current_exe().unwrap();
    let profile_dir = me.parent().unwrap().parent().unwrap();
    let mut p = profile_dir.join("examples").join("attach_fixture");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

/// Wait for `f` to answer, or fail after ten seconds with the reason.
fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Start the fixture in `mode` and wait until it is parked, returning its handle,
/// its run id and the store the parent reads through.
fn start_parked(mode: &str, dir: &TempDir) -> (Fixture, i64, Store, String) {
    let path = dir.path().join("runs.db");
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let bin = fixture_bin();
    assert!(
        bin.exists(),
        "build the fixture first: cargo build --all-features --examples ({})",
        bin.display()
    );
    let child = Command::new(&bin)
        .args([mode, path.to_str().unwrap(), root.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fixture must be built: cargo build --all-features --examples");

    let store = Store::open(&path).unwrap();
    let run_id = until("the run row", || {
        store.runs().ok().and_then(|r| r.first().copied())
    });
    (Fixture::new(child), run_id, store, root.to_string_lossy().into_owned())
}


/// F3 — a second process answers a live approval and the owner carries on.
///
/// Both directions, because "the run finished" passes against a run that ignored
/// the answer entirely. Only the pair discriminates: the same fixture, the same
/// attach, one decision each way, and the effect must follow the decision.
#[cfg(unix)]
#[test]
fn an_attached_process_answers_a_live_approval_and_the_run_finishes() {
    for (approve, expected, wrote) in [(true, "approve", "wrote=true"), (false, "deny", "wrote=false")]
    {
        let dir = ws();
        let (mut child, run_id, store, _root) = start_parked("approve", &dir);

        // The stream reached us before we answered — this is the observer half.
        until("the approval request in the stream", || {
            Attach::to(&store, run_id)
                .poll()
                .ok()?
                .iter()
                .any(|e| matches!(e.kind, EventKind::ApprovalRequested { .. }))
                .then_some(())
        });

        // Keep answering until it exits. A denied write is not the end of a run:
        // the model reads the refusal and tries again, so it parks on a fresh
        // approval each step until its step cap. Answering once would prove the
        // approve case and hang the deny one.
        let answered = answer_until_it_exits(&store, run_id, &mut child, approve);
        assert!(answered >= 1, "at least one answer must have landed");

        // Never killed, never resumed: it finishes on its own.
        let out = child.finished();
        assert!(
            out.contains(&format!("decisions={expected}")) || out.contains(&format!(",{expected}")),
            "the run must act on the answer it was given, got: {out}"
        );
        assert!(out.contains(wrote), "and the effect must follow it: {out}");
    }
}

/// Answer every approval the run parks on, with the same verdict, until it exits.
/// Returns how many answers this process landed.
#[cfg(unix)]
fn answer_until_it_exits(store: &Store, run_id: i64, child: &mut Fixture, approve: bool) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut landed = 0;
    while !child.exited() {
        assert!(Instant::now() < deadline, "the fixture never exited");
        for waiting in Attach::to(store, run_id).waiting().unwrap() {
            if let Waiting::Approval { request_id, .. } = waiting {
                let decision = match approve {
                    true => Decision::approve(),
                    false => Decision::deny("not that path"),
                };
                if Attach::to(store, run_id)
                    .answer_approval(request_id, decision)
                    .unwrap()
                {
                    landed += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    landed
}

/// F4 — a live question is answerable, and the answer reaches the model.
#[cfg(unix)]
#[test]
fn an_attached_process_answers_a_live_question() {
    let dir = ws();
    let (child, run_id, store, _root) = start_parked("question", &dir);

    let question_id = until("the question", || {
        match Attach::to(&store, run_id).waiting().ok()?.first()? {
            Waiting::Question { question_id, .. } => Some(*question_id),
            _ => None,
        }
    });
    assert!(Attach::to(&store, run_id)
        .answer_question(question_id, "out.txt")
        .unwrap());

    // The approval that follows the answer, from the same attached process.
    let request_id = until("the approval after the answer", || {
        Attach::to(&store, run_id)
            .waiting()
            .ok()?
            .into_iter()
            .find_map(|w| match w {
                Waiting::Approval { request_id, .. } => Some(request_id),
                _ => None,
            })
    });
    assert!(Attach::to(&store, run_id)
        .answer_approval(request_id, Decision::approve())
        .unwrap());

    let out = child.finished();
    assert!(out.contains("wrote=true"), "got: {out}");
    let answered = store.question(question_id).unwrap().unwrap();
    assert_eq!(
        answered.answered_by.as_deref(),
        Some("attached"),
        "the row must name who actually answered, not who was asked"
    );
}

/// F4, second half — a live plan is answerable too. The plan was the third thing a
/// run can hold and the one it would have been easiest to leave out.
#[cfg(unix)]
#[test]
fn an_attached_process_answers_a_live_plan() {
    let dir = ws();
    let (child, run_id, store, _root) = start_parked("plan", &dir);

    let plan_id = until("the plan", || {
        match Attach::to(&store, run_id).waiting().ok()?.first()? {
            Waiting::Plan { plan_id, .. } => Some(*plan_id),
            _ => None,
        }
    });
    assert!(Attach::to(&store, run_id)
        .answer_plan(plan_id, PlanVerdict::Approve)
        .unwrap());

    let request_id = until("the approval after the plan", || {
        Attach::to(&store, run_id)
            .waiting()
            .ok()?
            .into_iter()
            .find_map(|w| match w {
                Waiting::Approval { request_id, .. } => Some(request_id),
                _ => None,
            })
    });
    assert!(Attach::to(&store, run_id)
        .answer_approval(request_id, Decision::approve())
        .unwrap());

    let out = child.finished();
    assert!(out.contains("wrote=true"), "the run left its plan phase: {out}");
    assert_eq!(
        store.plan(plan_id).unwrap().unwrap().decided_by.as_deref(),
        Some("attached"),
    );
}

/// F6 — the observer dying changes nothing.
#[cfg(unix)]
#[test]
fn killing_the_attached_process_does_not_disturb_the_run() {
    let dir = ws();
    let path = dir.path().join("runs.db");
    let (owner, run_id, store, _root) = start_parked("approve", &dir);

    // A third process attaches and polls.
    let mut watcher = Command::new(fixture_bin())
        .args(["watch", path.to_str().unwrap(), &run_id.to_string()])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let request_id = until("the approval", || {
        match Attach::to(&store, run_id).waiting().ok()?.first()? {
            Waiting::Approval { request_id, .. } => Some(*request_id),
            _ => None,
        }
    });

    // SIGKILL the observer, mid-poll. It holds no lock, no slot and no lease.
    kill9(watcher.id());
    watcher.wait().unwrap();

    // The owner is untouched and still answerable.
    assert!(Attach::to(&store, run_id)
        .answer_approval(request_id, Decision::approve())
        .unwrap());
    let out = owner.finished();
    assert!(out.contains("wrote=true"), "got: {out}");

    // And the stream is continuous across the kill: strictly ascending cursors,
    // no gap and no repeat.
    let rows = store.events_since(run_id, 0, 10_000).unwrap();
    assert!(rows.len() > 2);
    assert!(
        rows.windows(2).all(|w| w[1].0 > w[0].0),
        "cursors must strictly ascend across an observer's death"
    );
}

/// F7 — the owner dying leaves exactly the resumable run 0.7.0 guaranteed.
#[cfg(unix)]
#[test]
fn killing_the_owner_leaves_an_unresolved_row_a_resume_can_still_consume() {
    let dir = ws();
    let (owner, run_id, store, _root) = start_parked("approve", &dir);

    let request_id = until("the approval", || {
        match Attach::to(&store, run_id).waiting().ok()?.first()? {
            Waiting::Approval { request_id, .. } => Some(*request_id),
            _ => None,
        }
    });
    kill9(owner.id());
    drop(owner);

    // The discriminating assertion is the ABSENCE of a resolution. The row is now
    // written *before* the approver is consulted, so a row existing proves nothing;
    // a run whose pending row had been resolved up front and then abandoned would
    // resume into a decision nobody made.
    let pending = store.pending(request_id).unwrap().unwrap();
    assert!(
        pending.resolved.is_none(),
        "an abandoned approval must be unresolved, not pre-decided"
    );
    assert_eq!(pending.run_id, run_id);

    // And it is still exactly the 0.7.0 shape: a resume consumes it.
    assert!(store.resolve_pending(request_id, "approve").unwrap());
    assert!(
        !store.resolve_pending(request_id, "approve").unwrap(),
        "and only once"
    );
}

#[cfg(unix)]
fn kill9(pid: u32) {
    Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
}
