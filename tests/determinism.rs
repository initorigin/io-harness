//! The same case, run twice, producing the same trace.
//!
//! This is the claim every future measurement rests on: if two runs of one case
//! can differ, nothing built on top of them can attribute a difference to the
//! change being measured. So the positive test here is the least interesting
//! thing in the file. What matters is that the comparison is PROVEN CAPABLE OF
//! FAILING — a determinism test that cannot fail is decoration, and would pass
//! just as happily against a harness that had regressed.
//!
//! Two negative controls reproduce, in the comparison, the exact divergences this
//! release removed from the engine:
//!
//! - `run_id` rendered into a memory note (`src/context.rs`), which put an
//!   `AUTOINCREMENT` value into the prompt and therefore into `steps.prompt`.
//! - children composed in completion order rather than spawn order
//!   (`buffer_unordered` in `src/run.rs`), which reordered `steps.result`.
//!
//! Both are asserted to break `canonical_trace` equality. If someone reintroduces
//! either, the comparison notices.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, Record, Replay, ToolCall};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

/// Writes a different file each turn, so the run makes real progress and its
/// trace has something to compare.
struct Script {
    at: AtomicUsize,
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some(format!("turn {i}")),
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({
                    "path": format!("src/f{i}.rs"),
                    "content": format!("fn hello{i}() -> u32 {{ {i} }}\n"),
                }),
            }],
            ..Default::default()
        })
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace(
        "write a few files",
        root,
        Verification::WorkspaceFileContains {
            file: "src/f2.rs".into(),
            needle: "fn hello2".into(),
        },
    )
    .with_max_steps(4)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Record one run, then replay it twice into two FRESH stores and compare.
///
/// Fresh stores are part of the guarantee, not a convenience: run ids are
/// `AUTOINCREMENT`, and a child agent's id is embedded in its parent's composed
/// observation, which is real content the model was shown. `canonical_trace`
/// documents this.
#[tokio::test]
async fn one_case_replayed_twice_produces_the_same_trace() {
    let cassette = tempfile::tempdir().unwrap();
    let path = cassette.path().join("run.json");

    // Record.
    {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Record::new(Script {
            at: AtomicUsize::new(0),
        });
        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, RunOutcome::Success { .. }),
            "the recorded run must actually do something: {result:?}"
        );
        provider.save(&path).unwrap();
    }

    let mut traces = Vec::new();
    for _ in 0..2 {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Replay::load(&path).unwrap();
        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, RunOutcome::Success { .. }),
            "a replayed run must reach the same outcome: {result:?}"
        );
        traces.push(store.canonical_trace(result.run_id).unwrap());
    }

    assert_eq!(
        traces[0], traces[1],
        "two replays of one recording must produce the same canonical trace"
    );
    assert!(
        !traces[0].is_empty(),
        "an empty trace would make the comparison vacuous"
    );
    assert!(
        traces[0].contains("fn hello0"),
        "the trace must contain the run's real content, not just its shape:\n{}",
        traces[0]
    );
}

/// A replayed run must also match the run that was RECORDED, not merely match
/// another replay. Two replays could agree while both differing from reality.
#[tokio::test]
async fn a_replay_matches_the_run_it_was_recorded_from() {
    let cassette = tempfile::tempdir().unwrap();
    let path = cassette.path().join("run.json");

    let recorded = {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Record::new(Script {
            at: AtomicUsize::new(0),
        });
        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        provider.save(&path).unwrap();
        store.canonical_trace(result.run_id).unwrap()
    };

    let replayed = {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Replay::load(&path).unwrap();
        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        store.canonical_trace(result.run_id).unwrap()
    };

    assert_eq!(
        recorded, replayed,
        "a replay must reproduce the run it came from, not just agree with itself"
    );
}

// ------------------------------------------------- the negative controls

/// NEGATIVE CONTROL 1 — the comparison notices a `run_id` in the prompt.
///
/// Reproduces the defect this release removed from `src/context.rs`, which
/// rendered every durable memory note as `"… (run {run_id}, step {step})"`. Since
/// `run_id` is an `AUTOINCREMENT`, the same case run twice sent the model
/// different bytes, and that string was persisted into `steps.prompt`.
///
/// If `canonical_trace` did not compare `prompt`, this would pass — and the
/// positive tests above would keep passing through a reintroduction of the bug.
#[test]
fn the_comparison_fails_when_a_run_id_reappears_in_the_prompt() {
    let a = trace_with_prompt("- build: cargo test  (step 1)");
    let b = trace_with_prompt("- build: cargo test  (run 2, step 1)");

    assert_ne!(
        a, b,
        "a run id in the rendered prompt MUST break the comparison; if this \
         passes, the determinism tests are decoration"
    );
}

/// NEGATIVE CONTROL 2 — the comparison notices reordered child results.
///
/// Reproduces the defect this release removed from `src/run.rs`, where the
/// sub-agent fan-out used `buffer_unordered` and composed children in whatever
/// order they finished. The observations are identical in content and differ only
/// in order, which is exactly the case a set-based or count-based comparison would
/// miss.
#[test]
fn the_comparison_fails_when_children_are_composed_in_a_different_order() {
    let spawn_order =
        trace_with_result("[child 2 \"ordered-0\" -> Success]\n[child 3 \"ordered-1\" -> Success]");
    let completion_order =
        trace_with_result("[child 3 \"ordered-1\" -> Success]\n[child 2 \"ordered-0\" -> Success]");

    assert_ne!(
        spawn_order, completion_order,
        "reordered child results MUST break the comparison — same content, \
         different order, which is precisely what buffer_unordered produced"
    );
}

/// A control on the controls: two stores built the same way DO compare equal, so
/// the two assertions above are detecting the injected difference and not merely
/// the fact that any two stores differ.
#[test]
fn two_identically_built_traces_compare_equal() {
    assert_eq!(
        trace_with_prompt("- build: cargo test  (step 1)"),
        trace_with_prompt("- build: cargo test  (step 1)")
    );
    assert_eq!(
        trace_with_result("[child 2 \"a\" -> Success]"),
        trace_with_result("[child 2 \"a\" -> Success]")
    );
}

/// A one-step trace whose committed prompt is `prompt`.
fn trace_with_prompt(prompt: &str) -> String {
    let store = Store::memory().unwrap();
    let run = store.start_run("goal", "f.rs").unwrap();
    store
        .record(
            run,
            &io_harness::StepRecord::new(1, "read", "ok").with_trace(prompt, "read_file:{}", 10),
        )
        .unwrap();
    store.canonical_trace(run).unwrap()
}

/// A one-step trace whose committed result is `result`.
fn trace_with_result(result: &str) -> String {
    let store = Store::memory().unwrap();
    let run = store.start_run("goal", "f.rs").unwrap();
    store
        .record(
            run,
            &io_harness::StepRecord::new(1, "spawned", result).with_trace(
                "p",
                "spawn_agent:{}",
                10,
            ),
        )
        .unwrap();
    store.canonical_trace(run).unwrap()
}

/// A resume does NOT reproduce the uninterrupted run's prompts, and a replay is
/// right to say so.
///
/// This started as the criterion "an interrupted replay reproduces the
/// uninterrupted one" and the test disproved it. The cause is not replay: the
/// observation ledger the context assembler builds is in-memory
/// (`ContextLedger::new()`, src/run.rs:1183 and :1758) and 0.7.0's resume does not
/// restore it. A resumed run therefore re-assembles its context from the workspace
/// rather than from what the first process had accumulated, and sends a prompt the
/// recording never saw.
///
/// That is a real limitation of resume, older than this release and outside its
/// scope to fix — checkpointing the ledger is context-assembly work. What matters
/// here is the behaviour on the boundary: the replay reports the divergence as a
/// typed, non-retryable error instead of quietly serving some other answer. A
/// replay that guessed would produce a plausible trace that reproduced nothing,
/// which is the failure mode this whole file exists to prevent.
#[tokio::test]
async fn a_resumed_replay_reports_divergence_rather_than_inventing_an_answer() {
    let cassette = tempfile::tempdir().unwrap();
    let path = cassette.path().join("run.json");

    {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Record::new(Script {
            at: AtomicUsize::new(0),
        });
        run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        provider.save(&path).unwrap();
    }

    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Replay::load(&path).unwrap();

    let cut_short = contract(dir.path()).with_max_steps(1);
    let first = run_with(&cut_short, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(first.outcome, RunOutcome::StepCapReached { .. }),
        "the cap must stop it mid-task, not finish it: {first:?}"
    );

    let resumed_provider = Replay::load(&path).unwrap();
    let err = io_harness::resume(
        &contract(dir.path()),
        &resumed_provider,
        &store,
        first.run_id,
    )
    .await
    .expect_err(
        "a resumed run assembles a prompt the recording never saw, so the replay          must refuse rather than answer",
    );

    let rendered = err.to_string();
    assert!(
        rendered.contains("diverged"),
        "the error must name the divergence so it is debuggable, got: {rendered}"
    );
}
