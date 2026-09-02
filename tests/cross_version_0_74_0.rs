//! 0.75.0's cross-version fixture: a store this tree wrote is read by a real io-harness
//! 0.74.0 from crates.io, and then written into by it.
//!
//! 0.75.0 adds two things a store can see, and calls both additive:
//!
//! * **`memory_token_cache`** — a new table beside `memory`, one row per entry, holding
//!   the normalised token lines the ranking and the duplicate check used to recompute
//!   on every step. Keyed `UNIQUE(workspace, key)` and stamped with the entry's own
//!   `created_at`, which is what makes a stale line detectable.
//! * **five latency columns on `steps`** — `span_ms`, `provider_ms`, `tool_ms`,
//!   `gate_ms`, `store_ms`, nullable and written by the transaction that commits the
//!   step.
//!
//! Neither bumps `CHECKPOINT_FORMAT`. **Additive is a claim, and this file is the only
//! thing in the tree that tests it.** Every other test that touches either surface reads
//! rows the same tree has just written, which is precisely the shape of suite that was
//! green all the way through 0.72.0 while a serialization change made every question in
//! a 0.72.0 store unreadable to a real 0.71.0 binary. A release's own belief that it
//! touched nothing persisted is worth exactly as much as the previous binary that
//! checks it.
//!
//! **The backwards direction only, on purpose.** The four fixtures before this one each
//! carry a forwards half as well — a committed database written by the previous release
//! and read back here. There is none for 0.74.0 and no `write` mode in the generator to
//! produce one. A release that only adds a table and some nullable columns reads an
//! older store by construction: the table is created empty by the migration, the columns
//! read back `NULL`, and the forwards half has passed from its first run since 0.72.0
//! without ever being in a position to fail. The direction that has caught something is
//! the one where the *older* binary is handed the newer store, and that one needs a
//! binary rather than a file.
//!
//! Two tests, because 0.75.0 puts the older binary in two different positions:
//!
//! * [`f11_a_current_store_is_read_by_a_0_74_0_binary`] — 0.74.0 opens a store this tree
//!   wrote, holding memory entries with cache rows beside them and committed steps with
//!   the attribution columns populated, and must lose nothing and refuse nothing.
//! * [`f11_a_0_74_0_write_into_a_current_store_is_read_back_correctly`] — 0.74.0 then
//!   *writes* into that store. It knows nothing of `memory_token_cache`, so it leaves an
//!   entry with no cached line and an entry whose cached line describes a value that is
//!   no longer there. What this tree reads back afterwards is the assertion.
//!
//! Both are `#[ignore]`d because both need `tests/fixtures/gen-0.74.0` built, which
//! resolves `io-harness =0.74.0` from crates.io. CI's `cross-version-0.74.0` job builds
//! it and runs this file with `-- --ignored`; by hand it is
//! `cargo build --manifest-path tests/fixtures/gen-0.74.0/Cargo.toml` and then
//! `cargo test --test cross_version_0_74_0 -- --ignored`.
//!
//! Nothing here reads a committed fixture, so nothing here can leave a `-wal` or `-shm`
//! sidecar in the tree: every database these tests open is created inside a temp
//! directory that goes away with the test.
//!
//! Expectations are the *current* tree's own readings, taken from the same store before
//! the older binary is handed it, rather than literals. Two binaries reading one store
//! and disagreeing is the finding; a literal in this file would only record which of
//! them was written down.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RetryPolicy, RunOutcome, StepRecord, Store,
    TaskContract, Verification, CHECKPOINT_FORMAT,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// The workspace the memory entries are keyed to. A fixed string rather than the run's
/// contract root: durable memory is keyed by a workspace's canonical path, and a temp
/// directory's path would put this machine's `/var/folders/...` into an argument the
/// generator is spawned with and into every JSON value compared below.
const MEM_WORKSPACE: &str = "fixture-workspace";

/// The goal every run this file starts by hand records. Fixed rather than absent: a
/// `NULL` and a value that round-tripped are different facts, and only the second proves
/// the column survived.
const GOAL: &str = "port the parser";

/// The entries the current tree writes, in the order it writes them.
///
/// The keys are in alphabetical order *and* in insertion order, which is deliberate and
/// not decoration. `memory_list` orders by `created_at` and breaks a tie on the key, and
/// three entries written back to back can share a millisecond — so keys whose two
/// orderings disagree would give this test a run-to-run order, which is a flake rather
/// than a finding.
///
/// The values are chosen so no two of them restate each other: `memory_similar` reports
/// the entry that most restates a text, and entries that overlap would make the probes
/// in the second test answer about the wrong row.
const ENTRIES: [(&str, &str); 3] = [
    (
        "build-command",
        "the test command is cargo test with all features",
    ),
    (
        "deploy-target",
        "the deploys go to the staging cluster every friday",
    ),
    (
        "review-day",
        "the maintainer reviews pull requests on tuesdays",
    ),
];

/// A provider that answers a fixed script of tool calls and nothing else. Enough to
/// drive real steps through the real loop, which is the only way to reach the
/// attribution columns: they are written out of the loop's staging cell, and a direct
/// caller of `record` leaves all five `NULL` — see `tests/step_attribution.rs`.
struct Mock {
    script: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.script.get(i).cloned().unwrap_or_default(),
            text: Some("nothing to do".into()),
            usage: Some(Usage {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                total_tokens: 1_100,
                ..Default::default()
            }),
            model: Some("model-a".into()),
            finish_reason: Some("stop".into()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn write_call(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

/// A two-step run through the real loop: one step that edits, one that satisfies the
/// gate. Returns its run id.
///
/// The retry policy is zeroed so a transient nothing cannot turn a two-step run into a
/// three-step one and move the numbers this test compares between two binaries.
async fn two_steps(store: &Store, dir: &Path) -> i64 {
    let provider = Mock {
        script: vec![
            vec![write_call("src.txt", "one\n")],
            vec![write_call("NOTES.md", "done")],
        ],
        at: AtomicUsize::new(0),
    };
    let contract = TaskContract::workspace("write the notes", dir)
        .with_verification(Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        })
        .with_max_steps(4)
        .with_retry_policy(RetryPolicy {
            base: Duration::ZERO,
            max: Duration::ZERO,
        });
    let result = run_with(
        &contract,
        &provider,
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    result.run_id
}

/// The pinned 0.74.0 binary, or a panic naming the step that was skipped.
fn generator() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.74.0/target/debug/gen-0-74-0");
    assert!(
        path.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.74.0/Cargo.toml ({path:?})"
    );
    path
}

/// Run the pinned binary and parse its JSON, or fail with what it printed.
///
/// A non-zero exit is the loudest possible form of the thing under test — it is the
/// previous release refusing a store this one wrote — so its stderr is the failure
/// message rather than a swallowed detail.
fn run_0_74_0(args: &[&std::ffi::OsStr]) -> Value {
    let generator = generator();
    let out = std::process::Command::new(&generator)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "0.74.0 failed against a 0.75.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// What 0.74.0 sees in `db`, for the workspace the memory entries are under.
fn read_with_0_74_0(db: &Path) -> Value {
    run_0_74_0(&["read".as_ref(), db.as_ref(), MEM_WORKSPACE.as_ref()])
}

/// One memory entry written into `db` by 0.74.0, its way.
fn remember_with_0_74_0(db: &Path, key: &str, value: &str) {
    let wrote = run_0_74_0(&[
        "remember".as_ref(),
        db.as_ref(),
        MEM_WORKSPACE.as_ref(),
        key.as_ref(),
        value.as_ref(),
    ]);
    assert_eq!(wrote["writer"], "io-harness 0.74.0");
    assert_eq!(
        wrote["evicted"],
        json!([]),
        "a cap evicted something, so the entry this test is about to ask after may not \
         be the one that is missing"
    );
}

/// The run in `runs` with this id.
fn run_by_id(seen: &Value, run_id: i64) -> &Value {
    seen["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == run_id)
        .unwrap_or_else(|| panic!("0.74.0 did not see run {run_id} at all: {seen:#?}"))
}

// ---------- backwards: this tree wrote it, 0.74.0 reads it ----------

/// F11 — a store this tree wrote, holding both surfaces 0.75.0 moves, is opened and read
/// by a real 0.74.0 binary, which must lose nothing and refuse nothing.
///
/// The store is built so that neither addition can be absent from the rows the older
/// binary reads:
///
/// * **committed steps with the attribution columns populated**, which only the real
///   loop produces — a direct `record` stages nothing and writes five `NULL`s, and five
///   `NULL`s at the end of a row are not the case under test. A row whose record is
///   longer than the older binary's schema describes is.
/// * **memory entries**, each of which puts a `memory_token_cache` row beside it in a
///   table 0.74.0 has no name for.
/// * **a run left `running`**, so `CHECKPOINT_FORMAT` is not merely reported by the older
///   binary but acted on: it has a checkpoint to judge resumable.
#[tokio::test]
#[ignore = "needs tests/fixtures/gen-0.74.0 built; CI's cross-version-0.74.0 job runs it"]
async fn f11_a_current_store_is_read_by_a_0_74_0_binary() {
    // Built before anything is written, so a forgotten build step fails in one line
    // rather than after a run.
    generator();

    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.75.0.sqlite3");

    // Everything the current tree reads out of its own store, taken here so the
    // comparison below is between two binaries rather than against a literal somebody
    // wrote down. The store is dropped at the end of the block: the older binary opens
    // the file, and an open connection of this tree's is one more variable in what it
    // finds there.
    let (driven, parked, ours) = {
        let store = Store::open(&db).unwrap();

        // Asserted against the literal so a silent bump cannot pass as a successful
        // upgrade. 0.75.0 adds a table and five columns and claims neither is a
        // checkpoint-layout change; this is the number that claim is about.
        assert_eq!(CHECKPOINT_FORMAT, 7);

        let driven = two_steps(&store, workspace.path()).await;
        let spent = store.step_attributions(driven).unwrap();
        assert_eq!(
            spent.len(),
            2,
            "both steps must carry an attribution, or the `steps` rows handed to 0.74.0 \
             have nothing new on them and this test asserts nothing: {spent:?}"
        );

        // The run that writes the memory, and the run left parked — one run, because a
        // run that is still `running` is the interesting thing to hand an older binary
        // twice over: it is a checkpoint to judge and an unfinished trace to read.
        let parked = store.start_run(GOAL, MEM_WORKSPACE).unwrap();
        for (i, (key, value)) in ENTRIES.iter().enumerate() {
            store
                .memory_put(MEM_WORKSPACE, key, value, parked, i as u32 + 1)
                .unwrap();
        }
        store
            .record(
                parked,
                &StepRecord::new(1, "read the schema", "a cache table names a workspace")
                    .with_trace("what does the schema hold?", "", 512),
            )
            .unwrap();

        let ours = json!({
            "driven": {
                "status": store.status(driven).unwrap(),
                "outcome": store.outcome(driven).unwrap(),
                "last_step": store.last_step(driven).unwrap(),
                "canonical_trace": store.canonical_trace(driven).unwrap(),
                "steps": store.steps(driven).unwrap().iter().map(|s| json!({
                    "step": s.step,
                    "decision": s.decision,
                    "result": s.result,
                    "prompt": s.prompt,
                    "tool_call": s.tool_call,
                    "tokens": s.tokens,
                })).collect::<Vec<_>>(),
            },
            "memory": store.memory_list(MEM_WORKSPACE).unwrap().iter().map(|e| json!({
                "key": e.key,
                "value": e.value,
                "run_id": e.run_id,
                "step": e.step,
                "created_at": e.created_at,
                "kind": format!("{:?}", e.kind),
                "pinned": e.pinned,
            })).collect::<Vec<_>>(),
        });
        (driven, parked, ours)
    };

    let seen = read_with_0_74_0(&db);
    assert_eq!(seen["reader"], "io-harness 0.74.0");

    // ---- (3) the format, which is what decides whether any of the rest is reachable ----

    // The previous release agrees about the format number, which is the cheapest
    // possible statement of "nothing locked it out".
    assert_eq!(seen["checkpoint_format"], CHECKPOINT_FORMAT);
    assert_eq!(
        run_by_id(&seen, parked)["resumable"],
        true,
        "0.74.0 refuses to resume a 0.75.0 checkpoint, which is what a CHECKPOINT_FORMAT \
         bump would look like from the outside"
    );
    assert_eq!(
        run_by_id(&seen, parked)["status"],
        "running",
        "the parked run must still be parked, or the resumability above was decided \
         about a run nobody could resume anyway"
    );

    // ---- (1a) the memory entries, with the new cache table sitting beside them ----

    // Compared whole, in order. `memory_list` is a total order since 0.57.0, so an
    // older reader that returned every entry and reordered them has still lost
    // something a per-key lookup would not show.
    assert_eq!(
        seen["memory"], ours["memory"],
        "0.74.0 does not read back the memory entries this tree wrote"
    );
    let keys: Vec<&str> = seen["memory"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        ENTRIES.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "the entries reached 0.74.0 in a different order than they were written"
    );

    // ---- (1b) the committed steps, whose rows carry five columns 0.74.0 cannot name --

    let driven_seen = run_by_id(&seen, driven);
    assert_eq!(
        driven_seen["steps"], ours["driven"]["steps"],
        "a step row carrying 0.75.0's latency columns did not read back column for \
         column in 0.74.0"
    );
    assert_eq!(driven_seen["status"], ours["driven"]["status"]);
    assert_eq!(driven_seen["outcome"], ours["driven"]["outcome"]);
    assert_eq!(driven_seen["last_step"], ours["driven"]["last_step"]);
    // The crate's own answer to "is this the same trace", rendered by both binaries over
    // the same rows. A reader that got every column right and the ordering wrong still
    // fails here.
    assert_eq!(
        driven_seen["canonical_trace"], ours["driven"]["canonical_trace"],
        "0.74.0 does not render the trace this tree renders from the same rows"
    );
}

/// F11 — and then 0.74.0 writes. It knows nothing of `memory_token_cache`, so a
/// `memory_put` from it leaves the store in the two states no release before 0.75.0
/// could produce, and both must read back correctly here.
///
/// * **A key that was not there.** The `memory` row exists and no cache row does.
/// * **A key that was.** The `memory` row's value and `created_at` moved and the cache
///   row still describes the value that is gone. This is the dangerous one: a reader
///   that trusted the cache would compare a text against words no entry holds any more,
///   and would answer confidently and wrongly rather than emptily.
///
/// The stale-line case is asserted in both directions, because only one of them fails
/// for a reader that serves the cache blindly and only the other fails for a reader that
/// serves nothing at all:
///
/// * the *new* value must find the overwritten entry — a blind cache misses it;
/// * the *old* value must find nothing — a blind cache reports the entry that no longer
///   says it.
#[test]
#[ignore = "needs tests/fixtures/gen-0.74.0 built; CI's cross-version-0.74.0 job runs it"]
fn f11_a_0_74_0_write_into_a_current_store_is_read_back_correctly() {
    generator();

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.75.0.sqlite3");

    // The values, named so the two probes below read as the question they are asking.
    // `OLD` and `NEW` share only `the`, which is well under the 60% of a union that
    // `memory_similar` requires — so a probe that finds the wrong one found it because a
    // stale cache line was served, not because the two texts are alike.
    const OLD: &str = ENTRIES[0].1;
    const NEW: &str = ENTRIES[1].1;
    const FRESH: &str = ENTRIES[2].1;

    let written_at = {
        let store = Store::open(&db).unwrap();
        let run = store.start_run(GOAL, MEM_WORKSPACE).unwrap();
        // One entry, written by this tree, which is what puts a cache row in the new
        // table for 0.74.0 to leave behind.
        store
            .memory_put(MEM_WORKSPACE, "build-command", OLD, run, 1)
            .unwrap();
        let entry = store.memory_get(MEM_WORKSPACE, "build-command").unwrap();
        entry.expect("just written").created_at
    };

    // 0.74.0 overwrites it, and writes a key that was never there.
    remember_with_0_74_0(&db, "build-command", NEW);
    remember_with_0_74_0(&db, "review-day", FRESH);

    let store = Store::open(&db).unwrap();

    // The plain read first: whatever the cache says, the value in `memory` is the value
    // 0.74.0 put there. A failure here is not a cache problem at all, and separating it
    // stops the probes below from being blamed for it.
    let overwritten = store
        .memory_get(MEM_WORKSPACE, "build-command")
        .unwrap()
        .expect("0.74.0 overwrote it, it did not remove it");
    assert_eq!(overwritten.value, NEW);
    let fresh = store
        .memory_get(MEM_WORKSPACE, "review-day")
        .unwrap()
        .expect("0.74.0 wrote it");
    assert_eq!(fresh.value, FRESH);

    // The stamp is what tells a 0.75.0 reader its cached line is stale, so a run where
    // the two writes landed in the same millisecond would exercise the cache-hit path
    // and prove nothing about the stale one. A process spawn separates them; asserted
    // rather than assumed, because a run that silently proved nothing is worse than one
    // that says so.
    assert_ne!(
        overwritten.created_at, written_at,
        "the 0.74.0 overwrite landed in the same millisecond as this tree's write, so \
         its cache line is not stale and this test asserted nothing"
    );

    // ---- the overwritten entry: its cache line describes a value that is gone ----

    let by_new = store
        .memory_similar(MEM_WORKSPACE, "release-day", NEW)
        .unwrap();
    assert_eq!(
        by_new.map(|e| e.key).as_deref(),
        Some("build-command"),
        "the value 0.74.0 actually stored does not find the entry holding it — the \
         reader served a cache line describing the value that was replaced"
    );

    let by_old = store
        .memory_similar(MEM_WORKSPACE, "how-to-test", OLD)
        .unwrap();
    assert_eq!(
        by_old.map(|e| e.key),
        None,
        "a text no entry holds any more was reported as a restatement — the reader \
         answered out of a stale cache line rather than out of the store"
    );

    // ---- the new entry: there is no cache line for it at all ----

    let by_fresh = store
        .memory_similar(MEM_WORKSPACE, "when-to-review", FRESH)
        .unwrap();
    assert_eq!(
        by_fresh.map(|e| e.key).as_deref(),
        Some("review-day"),
        "an entry a 0.74.0 binary wrote is invisible to the comparison — a missing \
         cache row must be a miss the reader recomputes, not an entry that stops \
         existing"
    );

    // And the store still lists both, in order, with the values that are actually in
    // it. The cache is an optimisation; an entry it has no line for is still an entry.
    let listed: Vec<(String, String)> = store
        .memory_list(MEM_WORKSPACE)
        .unwrap()
        .into_iter()
        .map(|e| (e.key, e.value))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("build-command".to_string(), NEW.to_string()),
            ("review-day".to_string(), FRESH.to_string()),
        ],
    );
}
