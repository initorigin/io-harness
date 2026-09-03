//! The fleet: a concurrency cap that throttles, a total cap that refuses, and a
//! queue that survives the process (0.32.0).
//!
//! Two claims here are easy to test badly and both are tested for the shape of
//! the failure rather than for the happy outcome.
//!
//! **"The cap throttles rather than refuses"** is not proven by a fan-out that
//! completes — a harness that had simply stopped enforcing anything completes
//! too. Every throttling test here is paired with a control that moves only the
//! *other* cap and must refuse.
//!
//! **"The queue is durable"** is not proven by a resumed tree that finishes. A
//! queue silently re-derived from the spawn calls the model repeats produces the
//! same outcome, the same children and the same final state. The observable that
//! discriminates is the *depth reported before the model is asked anything*, and
//! whether deleting a row from the store changes it. So the durability test
//! asserts an absence: one row is deleted, and the entry that row stood for must
//! be missing from what the resumed process reports.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_tree_observed, run_tree, run_tree_observed, ApproveAll, Containment, Policy, Provider,
    RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A spawn call with an explicit step cap.
///
/// The cap is a parameter and not a constant because it decides whether a test
/// can measure anything. A child resumed straight into its cap never reaches the
/// provider, so any concurrency measurement taken over an adopted child with
/// `max_steps: 1` is vacuous — it measures a child that made no calls.
fn spawn_call(goal: &str, file: &str, max_steps: u32) -> ToolCall {
    ToolCall {
        name: "spawn_agent".into(),
        arguments: json!({
            "goal": goal,
            "verify_file": file,
            "verify_contains": "never-satisfied",
            "max_steps": max_steps
        }),
    }
}

/// A provider that measures how many completions are genuinely in flight at
/// once, and records every prompt it was handed.
///
/// The parent (goal containing `FLEET-ROOT`) is told to spawn `fanout` children
/// in one step; each child does nothing and reaches its one-step cap. Every call
/// yields, so concurrent children really do overlap and the peak is a measurement
/// rather than an assumption.
struct FanoutProbe {
    fanout: usize,
    active: AtomicUsize,
    peak: AtomicUsize,
    prompts: Mutex<Vec<String>>,
}

impl FanoutProbe {
    fn new(fanout: usize) -> Self {
        Self {
            fanout,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
        }
    }

    /// Peak concurrency among *children*. The root is excluded because it holds
    /// no slot: it is never one of the agents the cap is counting, and including
    /// it would let a cap of one read as two — or, when the root is idle awaiting
    /// its children, as zero.
    fn child_peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

impl Provider for FanoutProbe {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.prompts.lock().unwrap().push(req.user.clone());
        let usage = Some(Usage {
            total_tokens: 1,
            ..Default::default()
        });
        if !req.user.contains("FLEET-ROOT") {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Ok(CompletionResponse {
                usage,
                ..Default::default()
            });
        }
        tokio::task::yield_now().await;
        Ok(CompletionResponse {
            tool_calls: (0..self.fanout)
                .map(|i| spawn_call(&format!("child-{i}"), &format!("c{i}.txt"), 3))
                .collect(),
            usage,
            ..Default::default()
        })
    }
}

/// Records every event, in order.
#[derive(Default)]
struct Recorder(Mutex<Vec<RunEvent>>);

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.clone());
        Flow::Continue
    }
}

impl Recorder {
    fn events(&self) -> Vec<RunEvent> {
        self.0.lock().unwrap().clone()
    }

    /// Every `Fleet` payload, in order.
    fn fleet(&self) -> Vec<(u32, u32, u32, u32)> {
        self.events()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Fleet {
                    tier,
                    working,
                    queued,
                    done,
                } => Some((*tier, *working, *queued, *done)),
                _ => None,
            })
            .collect()
    }

    fn refusals(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::SpawnRefused { cap } => Some(cap.clone()),
                _ => None,
            })
            .collect()
    }
}

fn fanout_contract(dir: &std::path::Path) -> TaskContract {
    TaskContract::workspace("FLEET-ROOT: fan out across the fleet.", dir)
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1)
}

// ------------------------------------------------------ F1: it throttles

/// F1. A hundred and twenty children under a concurrency cap of eight all run.
/// Nothing is refused, and the measured peak never crosses the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_concurrency_cap_runs_the_whole_fleet_eight_at_a_time() {
    let dir = ws();
    let fanout = 120usize;
    let probe = FanoutProbe::new(fanout);
    let store = Store::memory().unwrap();
    let watch = Recorder::default();

    let result = run_tree_observed(
        &fanout_contract(dir.path()),
        &probe,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        // Room for all of them in the tree; eight working at a time.
        &Containment::new(fanout as u32 + 1, 8, 3, 10_000_000),
        &watch,
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome,
        RunOutcome::VerificationFailed { .. }
    ));
    assert_eq!(
        store.children(result.run_id).unwrap().len(),
        fanout,
        "every child ran; a cap that throttles finishes the fleet"
    );
    assert!(
        watch.refusals().is_empty(),
        "throttling is not refusing: {:?}",
        watch.refusals()
    );
    let peak = probe.child_peak();
    assert!(peak > 1, "children actually overlapped (peak {peak})");
    assert!(peak <= 8, "never more than the cap at once (peak {peak})");
}

/// F1's negative control. The identical fan-out with the *total* cap at eight
/// instead. Without this, the test above would pass against a harness that had
/// simply stopped enforcing anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_total_cap_still_refuses_the_same_fleet() {
    let dir = ws();
    let fanout = 120usize;
    let probe = FanoutProbe::new(fanout);
    let store = Store::memory().unwrap();
    let watch = Recorder::default();

    let result = run_tree_observed(
        &fanout_contract(dir.path()),
        &probe,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        // Eight agents in the whole tree — the root and seven children — but
        // sixty-four allowed to work at once. The cap that bites is the total one.
        &Containment::new(8, 64, 3, 10_000_000),
        &watch,
    )
    .await
    .unwrap();

    assert_eq!(store.children(result.run_id).unwrap().len(), 7);
    assert_eq!(
        watch.refusals().len(),
        fanout - 7,
        "every spawn past the total cap is refused, not queued"
    );
    assert!(watch.refusals().iter().all(|c| c == "agents"));
}

/// F2. Concurrency of one means strictly one at a time, and still refuses
/// nothing. The two caps move independently, which a single test moving both
/// could not show.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrency_of_one_serialises_the_fleet_without_refusing_any_of_it() {
    let dir = ws();
    let fanout = 12usize;
    let probe = FanoutProbe::new(fanout);
    let store = Store::memory().unwrap();
    let watch = Recorder::default();

    let result = run_tree_observed(
        &fanout_contract(dir.path()),
        &probe,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(200, 1, 3, 10_000_000),
        &watch,
    )
    .await
    .unwrap();

    assert_eq!(store.children(result.run_id).unwrap().len(), fanout);
    assert!(watch.refusals().is_empty());
    assert_eq!(
        probe.child_peak(),
        1,
        "one slot per tier means one child in flight"
    );
}

// ------------------------------------------------- F6: the counters per tier

/// F6. The counters reach the observer per tier, and they are internally
/// consistent: `working` never crosses the cap, every tier ends drained, and
/// `done` accounts for every child that tier ran.
///
/// The last of those is what catches a counter decremented on the happy path
/// only — the failure that leaves a fleet looking permanently half-finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tiers_are_counted_separately_and_every_tier_drains() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let watch = Recorder::default();

    // A two-tier tree: the root spawns three, and each of those spawns two.
    struct TwoTiers;
    impl Provider for TwoTiers {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let usage = Some(Usage {
                total_tokens: 1,
                ..Default::default()
            });
            let calls = if req.user.contains("FLEET-ROOT") {
                (0..3)
                    .map(|i| spawn_call(&format!("mid-{i}"), &format!("m{i}.txt"), 1))
                    .collect()
            } else if req.user.contains("mid-") {
                (0..2)
                    .map(|i| spawn_call(&format!("leaf-{i}"), &format!("l{i}.txt"), 1))
                    .collect()
            } else {
                Vec::new()
            };
            tokio::task::yield_now().await;
            Ok(CompletionResponse {
                tool_calls: calls,
                usage,
                ..Default::default()
            })
        }
    }

    let contract = TaskContract::workspace("FLEET-ROOT: two tiers.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1);
    run_tree_observed(
        &contract,
        &TwoTiers,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(100, 2, 3, 10_000_000),
        &watch,
    )
    .await
    .unwrap();

    let fleet = watch.fleet();
    let tiers: Vec<u32> = {
        let mut t: Vec<u32> = fleet.iter().map(|(tier, ..)| *tier).collect();
        t.sort_unstable();
        t.dedup();
        t
    };
    assert_eq!(tiers, vec![1, 2], "both tiers report, and separately");

    for &(tier, working, _, _) in &fleet {
        assert!(
            working <= 2,
            "tier {tier} reported {working} working, over the cap of 2"
        );
    }

    // The last event for each tier: drained, and accounting for every child.
    let mut last: BTreeMap<u32, (u32, u32, u32)> = BTreeMap::new();
    for &(tier, working, queued, done) in &fleet {
        last.insert(tier, (working, queued, done));
    }
    assert_eq!(
        last.get(&1),
        Some(&(0, 0, 3)),
        "tier 1 ran three children and gave every slot back"
    );
    assert_eq!(
        last.get(&2),
        Some(&(0, 0, 6)),
        "tier 2 ran two children under each of three parents"
    );
}

// ------------------------------------ F3/F4/F5: the queue outlives the process

fn fixture_bin() -> std::path::PathBuf {
    let me = std::env::current_exe().unwrap();
    let profile_dir = me.parent().unwrap().parent().unwrap();
    let mut p = profile_dir.join("examples").join("fleet_fixture");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

/// Run the fixture until its queue is non-empty, then SIGKILL it. Returns the
/// database path (the temp dir is returned too, so it outlives the call).
async fn crashed_fleet() -> (tempfile::TempDir, std::path::PathBuf) {
    let bin = fixture_bin();
    assert!(
        bin.exists(),
        "fleet_fixture example not built at {bin:?} — run `cargo test`, which builds examples"
    );
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runs.db");
    let root = dir.path().join("ws");
    std::fs::create_dir(&root).unwrap();

    let mut child = tokio::process::Command::new(&bin)
        .arg(&db)
        .arg(&root)
        .spawn()
        .expect("spawn fleet_fixture");

    // Wait until the backlog is durable.
    //
    // 0.76.0 — issue #232's first site, and the reason it is worth care: the old
    // form polled a fixed 200 × 50 ms and then asserted "the fixture never filled
    // its queue" on whatever it had. That sentence names a cause the test has no
    // evidence for. A missed deadline on a loaded runner and a fan-out that
    // genuinely queues three arrive at the same assertion with the same message,
    // and it failed twice on CI for the first reason while reading as the second.
    //
    // Two changes, and both matter. The ceiling becomes a liveness bound rather
    // than the thing being measured, so a slow host takes longer and still says
    // what it found. And every failed read is *kept* instead of discarded: the
    // fixture writes to this database while the test reads it, so `Store::open`
    // and `queued_agents` can legitimately answer `SQLITE_BUSY` — swallowing
    // those with `if let Ok` burned poll iterations and then blamed the fixture.
    let mut last: Result<usize, String> = Err("the store was never opened".into());
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        last = Store::open(&db)
            .map_err(|e| e.to_string())
            .and_then(|store| store.queued_agents(1).map(|q| q.len()).map_err(|e| e.to_string()));
        if last.as_ref().is_ok_and(|&n| n == 4) || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    child.kill().await.expect("SIGKILL the fixture");
    assert_eq!(
        last,
        Ok(4),
        "the queue never reached 4 within 60s; the last thing this test could read was {last:?}"
    );
    (dir, db)
}

/// F3. A child that only ever waited is not charged. Asserted against the durable
/// rows rather than a counter: the counter died with the process, the rows did
/// not, and only one of the two is what a resume and an audit read.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_child_leaves_no_run_and_no_spend_behind() {
    let (_dir, db) = crashed_fleet().await;
    let store = Store::open(&db).unwrap();

    // Five children were asked for. One is a run; four are waits.
    let children = store.children(1).unwrap();
    assert_eq!(children.len(), 1, "one child was admitted");
    let waiting: Vec<String> = store
        .queued_agents(1)
        .unwrap()
        .into_iter()
        .map(|(tier, goal)| {
            assert_eq!(tier, 1, "the fan-out is one tier down from the root");
            goal
        })
        .collect();
    assert_eq!(
        waiting,
        vec!["child-1", "child-2", "child-3", "child-4"],
        "FIFO, and the admitted child-0 is not among them"
    );

    // The charge, against the spend rows. The tree's whole recorded spend is the
    // one admitted child's one committed step; four children contributed nothing
    // because nothing about them was started.
    let admitted_spend: u64 = store
        .steps(children[0])
        .unwrap()
        .iter()
        .map(|s| s.tokens)
        .sum();
    assert_eq!(admitted_spend, 90);
    assert_eq!(
        store.spent_tokens_tree(1).unwrap(),
        admitted_spend,
        "a queued child adds nothing to the tree's spend"
    );
    assert_eq!(
        store.agent_count_tree(1).unwrap(),
        2,
        "the root and the one child that started — a wait is not an agent"
    );
}

/// F4. The queue survives the crash *at the depth it had*, and the depth comes
/// out of the store.
///
/// The discriminating assertion is the absence. One `agent_queue` row is deleted
/// before the resume; the depth the resumed process reports must be one smaller,
/// and it must arrive before the resumed provider has been handed a single
/// request. A queue silently re-derived from the replayed spawn calls would be
/// unchanged by the deletion, and could not exist before the first provider call.
/// Either fact alone passes on a re-derivation; both together do not.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_fleet_reports_the_backlog_the_store_holds_not_the_one_it_could_rederive() {
    let (_dir, db) = crashed_fleet().await;

    // Sabotage the store, not the code: one waiting child is removed from the
    // durable queue. Nothing else in the tree changes, and the model will still
    // ask for all five.
    {
        let store = Store::open(&db).unwrap();
        store.dequeue_agent(1, 1, "child-3").unwrap();
        let left: Vec<String> = store
            .queued_agents(1)
            .unwrap()
            .into_iter()
            .map(|(_, g)| g)
            .collect();
        assert_eq!(left, vec!["child-1", "child-2", "child-4"]);
    }

    let store = Store::open(&db).unwrap();
    let dir = db.parent().unwrap().join("ws");
    let probe = FanoutProbe::new(5);
    let watch = Recorder::default();
    let _ = resume_tree_observed(
        &fanout_contract(&dir),
        &probe,
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(6, 1, 3, 1_000_000),
        &watch,
    )
    .await
    .unwrap();

    // The first thing the fleet reports is the backlog it inherited.
    let events = watch.events();
    let first_fleet = events
        .iter()
        .position(|e| matches!(e.kind, EventKind::Fleet { .. }))
        .expect("the resume reported a fleet");
    let EventKind::Fleet { tier, queued, .. } = &events[first_fleet].kind else {
        unreachable!()
    };
    assert_eq!(*tier, 1);
    assert_eq!(
        *queued, 3,
        "the depth is the store's three, not the five the model is about to ask for \
         and not the four the fixture left"
    );

    // And it arrived before the model was asked anything. A re-derived depth
    // cannot: there is nothing to derive it from until a spawn call comes back.
    let first_prompt = events
        .iter()
        .position(|e| matches!(e.kind, EventKind::Step { .. }))
        .unwrap_or(usize::MAX);
    assert!(
        first_fleet < first_prompt,
        "the inherited backlog was reported after the run had already stepped"
    );
    assert!(
        probe.calls() > 0,
        "the resume really did run — otherwise the ordering above is vacuous"
    );
}

/// F5. The replay does not double the backlog it just restored.
///
/// The store cannot hold a duplicate — the unique index forbids it — so the
/// number that could go wrong is the *counter*. If the ledger counted each
/// replayed wait as a fresh one, the reported depth would climb past what the
/// store holds, and an operator would watch a queue that grows every time the
/// process restarts.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_spawn_step_does_not_double_the_restored_backlog() {
    let (_dir, db) = crashed_fleet().await;
    let store = Store::open(&db).unwrap();
    let dir = db.parent().unwrap().join("ws");
    let probe = FanoutProbe::new(5);
    let watch = Recorder::default();

    let _ = resume_tree_observed(
        &fanout_contract(&dir),
        &probe,
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(6, 1, 3, 1_000_000),
        &watch,
    )
    .await
    .unwrap();

    // Deliberately skipping the first event, which is the restored depth itself.
    // Asserting over the whole stream would let the restored number satisfy this
    // on its own, and the claim under test is about what the *replay* did with it.
    let fleet = watch.fleet();
    let after_restore: Vec<u32> = fleet.iter().skip(1).map(|&(_, _, q, _)| q).collect();
    assert!(!after_restore.is_empty(), "the replay reported nothing");
    assert_eq!(
        after_restore.iter().copied().max(),
        Some(4),
        "the replay re-queued the four the store already held rather than adding four more          (a doubled backlog reads 8; a backlog that was never restored reads 0): {after_restore:?}"
    );
    // And the tree drained: the queue is empty and the rows are gone.
    assert!(store.queued_agents(1).unwrap().is_empty());
    assert_eq!(
        store.children(1).unwrap().len(),
        5,
        "every child the fixture asked for eventually ran"
    );
}

/// F5, the other half. A restored wait that is admitted *without ever waiting
/// again* still clears its row and comes off the count.
///
/// This is the case a resume under a roomier cap produces, and it is the one the
/// obvious implementation misses: the queue row was written by a process that is
/// dead, the slot it was waiting for died with it, so on the replay the child is
/// admitted immediately and never passes through the waiting path that deletes
/// rows. Left alone the row survives a tree that fully drained, and the reported
/// backlog never reaches zero — a fleet that looks permanently stuck at four.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_wait_admitted_without_waiting_still_clears_its_row() {
    let (_dir, db) = crashed_fleet().await;
    let store = Store::open(&db).unwrap();
    let dir = db.parent().unwrap().join("ws");
    let watch = Recorder::default();

    let _ = resume_tree_observed(
        &fanout_contract(&dir),
        &FanoutProbe::new(5),
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        // Five slots where the dead process had one: nothing has to wait.
        &Containment::new(6, 5, 3, 1_000_000),
        &watch,
    )
    .await
    .unwrap();

    assert!(
        store.queued_agents(1).unwrap().is_empty(),
        "a fully drained tree left rows behind: {:?}",
        store.queued_agents(1).unwrap()
    );
    let fleet = watch.fleet();
    assert_eq!(
        fleet.first().map(|&(_, _, q, _)| q),
        Some(4),
        "the resume still inherited the backlog"
    );
    assert_eq!(
        fleet.last().map(|&(_, _, q, _)| q),
        Some(0),
        "the count came down with the rows: {fleet:?}"
    );
}

/// F8. An adopted child takes a slot like any other.
///
/// The resumed tree has one mid-flight child and four fresh siblings under a
/// concurrency of one. If adoption skipped admission — the easy mistake, since an
/// adopted child is already registered and already has a run id — the resumed
/// child would run *alongside* the first fresh sibling and the measured peak
/// would be two. The throttle would then be a different number before and after a
/// restart, which is the failure a durable queue exists to prevent.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_child_takes_a_slot_so_the_throttle_survives_the_restart() {
    let (_dir, db) = crashed_fleet().await;
    let store = Store::open(&db).unwrap();
    let dir = db.parent().unwrap().join("ws");
    let probe = FanoutProbe::new(5);
    let watch = Recorder::default();

    let _ = resume_tree_observed(
        &fanout_contract(&dir),
        &probe,
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(6, 1, 3, 1_000_000),
        &watch,
    )
    .await
    .unwrap();

    assert_eq!(
        store.children(1).unwrap().len(),
        5,
        "the adopted child and its four siblings all ran"
    );
    assert_eq!(
        probe.child_peak(),
        1,
        "the adopted child holds the only slot; its siblings wait for it"
    );
    assert!(
        watch.fleet().iter().all(|&(_, working, _, _)| working <= 1),
        "a tier reported more agents working than it has slots"
    );
}

// ------------------------------------------------ N3: the fast path writes nothing

/// N3. A child admitted immediately never touches `agent_queue`. The row exists
/// only for a child that actually waits, so the fast path costs one `try_acquire`
/// and no statement at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fleet_that_never_queues_writes_no_queue_rows() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let probe = FanoutProbe::new(4);

    let result = run_tree(
        &fanout_contract(dir.path()),
        &probe,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        // Four children, sixty-four slots: nothing ever waits.
        &Containment::new(100, 64, 3, 10_000_000),
    )
    .await
    .unwrap();

    assert_eq!(store.children(result.run_id).unwrap().len(), 4);
    assert!(
        store.queued_agents(result.run_id).unwrap().is_empty(),
        "a fleet that never waited wrote a queue row anyway"
    );
}
