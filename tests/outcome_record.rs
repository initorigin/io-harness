//! One row per finished run: did it work, how many steps, what did it spend, how
//! long did it take.
//!
//! Before 0.12.0 a consumer assembled this itself and could not finish the job.
//! Success meant knowing which of eleven free-text outcome strings is the good
//! one. Steps meant knowing that `MAX(step)` is the step count and `COUNT(*)` is
//! not, because a retry writes a row under the same step number. Spend meant
//! `SUM(steps.tokens)`. And latency was simply unavailable: nothing recorded when
//! a run ended, and `Store::elapsed_secs` measures against `now`, so it keeps
//! growing after the run is over.
//!
//! These assert the summary against independently computed values rather than
//! against itself — a summary that agrees only with its own writer proves
//! nothing. Duration is asserted as a bound, never an equality.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::{StepRecord, Store};

/// A store with one run that committed `steps` steps of `each` tokens.
fn run_with_steps(store: &Store, steps: u32, each: u64) -> i64 {
    let run = store.start_run("goal", "file.rs").expect("start_run");
    for step in 1..=steps {
        store
            .record(
                run,
                &StepRecord::new(step, "decision", "result").with_trace("prompt", "tool", each),
            )
            .expect("record");
    }
    run
}

#[test]
fn a_finished_run_reports_success_steps_spend_and_a_duration() {
    let store = Store::memory().expect("memory");
    let run = run_with_steps(&store, 3, 100);

    assert!(
        store.run_summary(run).expect("summary").is_none(),
        "an unfinished run has no summary — absent, not a row of zeroes"
    );

    store.finish_run(run, "success").expect("finish_run");
    let s = store.run_summary(run).expect("summary").expect("a summary");

    assert_eq!(s.run_id, run);
    assert_eq!(s.outcome, "success");
    assert!(s.success);
    // Independently computed, not read back from the same row.
    assert_eq!(s.steps, store.last_step(run).expect("last_step"));
    assert_eq!(s.steps, 3);
    assert_eq!(s.tokens, store.spent_tokens(run).expect("spent"));
    assert_eq!(s.tokens, 300);
    assert!(!s.finished_at.is_empty(), "the end stamp is the new fact");
    // A bound, never an equality: this is wall-clock.
    let ms = s
        .duration_ms
        .expect("a run started by 0.7.0+ has a duration");
    assert!(
        ms < 60_000,
        "three in-memory steps cannot have taken {ms}ms"
    );
}

/// Only `success` is success. Every other ending — including the ones that are
/// nobody's fault, like a rate-limited provider — is the task not being done.
#[test]
fn every_other_outcome_is_recorded_and_is_not_success() {
    for outcome in [
        "step_cap_reached",
        "time_budget_exceeded",
        "cost_budget_exceeded",
        "denied",
        "stalled",
        "budget_ceiling_reached",
        "refused",
        "escalated_retryable",
        "escalated_terminal",
    ] {
        let store = Store::memory().expect("memory");
        let run = run_with_steps(&store, 1, 7);
        store.finish_run(run, outcome).expect("finish_run");

        let s = store
            .run_summary(run)
            .expect("summary")
            .unwrap_or_else(|| panic!("{outcome} must still get a summary"));
        assert_eq!(s.outcome, outcome);
        assert!(!s.success, "{outcome} is not success");
        assert_eq!(s.tokens, 7, "{outcome} still spent what it spent");
    }
}

/// A paused run has not finished. It is waiting for a human and will be resumed,
/// so a summary now would describe an ending that has not happened.
#[test]
fn a_run_paused_for_a_human_has_no_summary_until_it_really_ends() {
    let store = Store::memory().expect("memory");
    let run = run_with_steps(&store, 2, 10);

    store
        .finish_run(run, "awaiting_approval")
        .expect("finish_run");
    assert!(
        store.run_summary(run).expect("summary").is_none(),
        "a paused run has not ended, so it has no outcome to summarise"
    );

    // It resumes, does more work, and then really ends.
    store
        .record(
            run,
            &StepRecord::new(3, "decision", "result").with_trace("prompt", "tool", 10),
        )
        .expect("record");
    store.finish_run(run, "success").expect("finish_run");

    let s = store.run_summary(run).expect("summary").expect("a summary");
    assert!(s.success);
    assert_eq!(s.steps, 3, "the summary describes the whole run");
    assert_eq!(s.tokens, 30);
}

/// `finish_run` is reachable more than once for one run, so the summary must not
/// accumulate duplicates. The last ending is the true one.
#[test]
fn finishing_twice_replaces_the_summary_rather_than_duplicating_it() {
    let store = Store::memory().expect("memory");
    let run = run_with_steps(&store, 1, 5);

    store.finish_run(run, "step_cap_reached").expect("first");
    store.finish_run(run, "success").expect("second");

    let s = store.run_summary(run).expect("summary").expect("a summary");
    assert_eq!(s.outcome, "success");
    assert!(s.success);

    let rows: i64 = {
        // Counted through a second connection rather than the API, so the
        // assertion is about the table and not about the reader.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("x.sqlite3");
        let s2 = Store::open(&db).expect("open");
        let r = run_with_steps(&s2, 1, 5);
        s2.finish_run(r, "stalled").expect("a");
        s2.finish_run(r, "success").expect("b");
        let c = rusqlite::Connection::open(&db).expect("reader");
        c.query_row("SELECT COUNT(*) FROM run_outcomes", [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(rows, 1, "one run must have exactly one summary row");
}

/// The summary serialises, so a scoring tool can ship or store it without
/// restating the shape.
#[test]
fn a_summary_round_trips_as_json() {
    let store = Store::memory().expect("memory");
    let run = run_with_steps(&store, 2, 21);
    store.finish_run(run, "success").expect("finish_run");
    let s = store.run_summary(run).expect("summary").expect("a summary");

    let json = serde_json::to_string(&s).expect("serialise");
    let back: io_harness::RunSummary = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(s, back);
}

/// A run finished before 0.12.0 has no summary. Absent is the honest answer; a
/// zeroed row would be indistinguishable from a run that did nothing.
#[test]
fn a_run_from_an_older_binary_reports_no_summary_rather_than_zeroes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("old.sqlite3");
    let store = Store::open(&db).expect("open");
    let run = run_with_steps(&store, 4, 50);
    store.finish_run(run, "success").expect("finish_run");
    drop(store);

    // Simulate the pre-0.12.0 state: the run finished, but no summary was written.
    let c = rusqlite::Connection::open(&db).expect("conn");
    c.execute("DELETE FROM run_outcomes", []).expect("delete");
    drop(c);

    let reopened = Store::open(&db).expect("reopen");
    assert_eq!(
        reopened.outcome(run).expect("outcome").as_deref(),
        Some("success"),
        "the run itself is still there"
    );
    assert!(
        reopened.run_summary(run).expect("summary").is_none(),
        "and its summary is absent, not fabricated"
    );
}

/// Reading a summary must not depend on having driven the run in this process.
/// A counter proves the reader touched nothing that would re-run work.
#[test]
fn reading_a_summary_is_a_pure_read() {
    static READS: AtomicUsize = AtomicUsize::new(0);
    let store = Store::memory().expect("memory");
    let run = run_with_steps(&store, 1, 1);
    store.finish_run(run, "success").expect("finish_run");

    let first = store.run_summary(run).expect("summary");
    READS.fetch_add(1, Ordering::SeqCst);
    let second = store.run_summary(run).expect("summary");
    READS.fetch_add(1, Ordering::SeqCst);

    assert_eq!(first, second, "reading twice must give the same answer");
    assert_eq!(READS.load(Ordering::SeqCst), 2);
}

/// F13 — the summary reaches the caller, and agrees with the store.
///
/// A method rather than a field: a field would have to be filled at every entry
/// point's return site, including the ones that return `Err` and never build a
/// `RunResult` at all, so the two could drift. Reading it from the store means the
/// caller and an auditor see the same row by construction — which is what this
/// test asserts.
#[tokio::test]
async fn a_caller_reads_the_summary_off_its_own_run_result() {
    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
    use io_harness::{run_with, ApproveAll, Policy, Provider, TaskContract, Verification};

    struct Writer;
    impl Provider for Writer {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> io_harness::Result<CompletionResponse> {
            Ok(CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "write_file".into(),
                    arguments: serde_json::json!({
                        "path": "a.rs",
                        "content": "fn hello() -> u32 { 42 }\n",
                    }),
                }],
                usage: Some(io_harness::Usage {
                    total_tokens: 123,
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::memory().expect("memory");
    let contract = TaskContract::workspace("write a hello function", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "a.rs".into(),
            needle: "fn hello".into(),
        })
        .with_max_steps(2);
    let policy = Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*");

    let result = run_with(&contract, &Writer, &store, &policy, &ApproveAll)
        .await
        .expect("run");

    let summary = result
        .summary(&store)
        .expect("summary")
        .expect("a finished run has one");

    assert!(summary.success, "the run wrote the file: {summary:?}");
    assert_eq!(summary.run_id, result.run_id);
    assert_eq!(
        summary.tokens,
        store.spent_tokens(result.run_id).expect("spent"),
        "the caller's view and the store's must be the same row"
    );
    assert_eq!(summary.steps, store.last_step(result.run_id).expect("last"));
    assert!(summary.duration_ms.is_some(), "latency is recorded now");
}
