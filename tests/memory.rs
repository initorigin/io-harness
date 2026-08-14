//! 0.10.0: durable, cross-run memory. Everything a run learns and writes
//! deliberately is keyed to a *workspace*, not a run id, so a second run over
//! the same workspace starts knowing what the first one found out — and two
//! workspaces never leak into each other.
//!
//! The cap arithmetic and the migration are proven in `src/state.rs`'s unit
//! tests (they need the private connection and the cap constants). These are the
//! promises a caller sees: recall across runs, isolation, overwrite, forget.

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, ContextBudget, MemoryKind, Policy, Provider, Store, TaskContract,
    MEMORY_MAX_ENTRIES as MAX_ENTRIES,
};
use serde_json::json;

#[test]
fn a_fact_written_by_one_run_is_readable_by_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.db");

    // Run 1 learns something and writes it down, then the process goes away.
    {
        let store = Store::open(&path).unwrap();
        store
            .memory_put("/ws/app", "build_cmd", "cargo test --lib", 1, 4)
            .unwrap();
    }

    // Run 2 is a different process, a different run id, the same workspace.
    let store = Store::open(&path).unwrap();
    let entry = store
        .memory_get("/ws/app", "build_cmd")
        .unwrap()
        .expect("the earlier run's fact survived");
    assert_eq!(entry.value, "cargo test --lib");
    // Attribution survives too, so the reader knows where the fact came from.
    assert_eq!(entry.run_id, 1);
    assert_eq!(entry.step, 4);
    assert!(!entry.created_at.is_empty());

    // And it is in the workspace's listing, not just findable by key.
    let listed = store.memory_list("/ws/app").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], entry);
}

#[test]
fn two_workspaces_never_see_each_others_entries() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws/a", "k", "a's value", 1, 1).unwrap();
    store.memory_put("/ws/b", "k", "b's value", 1, 1).unwrap();

    assert_eq!(
        store.memory_get("/ws/a", "k").unwrap().unwrap().value,
        "a's value"
    );
    assert_eq!(
        store.memory_get("/ws/b", "k").unwrap().unwrap().value,
        "b's value"
    );
    assert_eq!(store.memory_list("/ws/a").unwrap().len(), 1);
    assert_eq!(store.memory_list("/ws/b").unwrap().len(), 1);
    // A workspace nobody wrote to holds nothing.
    assert!(store.memory_list("/ws/c").unwrap().is_empty());
    assert!(store.memory_get("/ws/c", "k").unwrap().is_none());
}

#[test]
fn re_putting_a_key_replaces_the_value_and_re_attributes_it() {
    let store = Store::memory().unwrap();
    store
        .memory_put("/ws", "api_base", "http://localhost:1", 1, 2)
        .unwrap();
    let evicted = store
        .memory_put("/ws", "api_base", "http://localhost:2", 9, 7)
        .unwrap();
    assert!(evicted.is_empty(), "an overwrite evicts nothing");

    // One row, not two — the key is unique within its workspace.
    let entries = store.memory_list("/ws").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].value, "http://localhost:2");
    // The latest writer owns the fact.
    assert_eq!(entries[0].run_id, 9);
    assert_eq!(entries[0].step, 7);
}

#[test]
fn a_write_past_the_entry_cap_reports_the_keys_it_evicted() {
    let store = Store::memory().unwrap();
    for i in 0..MAX_ENTRIES {
        assert!(store
            .memory_put("/ws", &format!("k{i}"), "v", 1, 1)
            .unwrap()
            .is_empty());
    }
    // The write that overflows names its cost, so the caller can trace it.
    let evicted = store.memory_put("/ws", "newest", "v", 2, 1).unwrap();
    assert_eq!(evicted, vec!["k0"]);
    assert_eq!(store.memory_list("/ws").unwrap().len(), MAX_ENTRIES);
    assert!(store.memory_get("/ws", "k0").unwrap().is_none());
    // The just-written entry is still there — a write is never a silent no-op.
    assert!(store.memory_get("/ws", "newest").unwrap().is_some());
}

#[test]
fn memory_delete_returns_true_then_false_and_the_key_stays_gone() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws", "k", "v", 1, 1).unwrap();

    assert!(store.memory_delete("/ws", "k").unwrap());
    // Deleting again is honest about there being nothing left to delete.
    assert!(!store.memory_delete("/ws", "k").unwrap());
    assert!(!store.memory_delete("/ws", "never-existed").unwrap());

    assert!(store.memory_get("/ws", "k").unwrap().is_none());
    assert!(store.memory_list("/ws").unwrap().is_empty());
}

#[test]
fn memory_clear_empties_one_workspace_and_reports_the_count() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws/a", "k1", "v", 1, 1).unwrap();
    store.memory_put("/ws/a", "k2", "v", 1, 1).unwrap();
    store.memory_put("/ws/a", "k3", "v", 1, 1).unwrap();
    store.memory_put("/ws/b", "k1", "v", 1, 1).unwrap();

    assert_eq!(store.memory_clear("/ws/a").unwrap(), 3);
    assert!(store.memory_list("/ws/a").unwrap().is_empty());
    // The other workspace is untouched.
    assert_eq!(store.memory_list("/ws/b").unwrap().len(), 1);
    // Clearing an empty workspace is a no-op, not an error.
    assert_eq!(store.memory_clear("/ws/a").unwrap(), 0);
}

#[test]
fn an_oversized_value_is_remembered_truncated_rather_than_refused() {
    let store = Store::memory().unwrap();
    // Multibyte throughout: a byte-wise cut would not be valid UTF-8 at all.
    let huge = "日".repeat(50_000);
    store.memory_put("/ws", "log", &huge, 1, 1).unwrap();

    let stored = store.memory_get("/ws", "log").unwrap().unwrap().value;
    assert!(stored.chars().count() < huge.chars().count());
    assert!(
        stored.ends_with("…[truncated]"),
        "the cut is visible: {stored:.40}"
    );
    // Every kept char is a whole char, never a half of one.
    assert!(stored.chars().take_while(|c| *c == '日').count() > 0);
    assert!(stored
        .trim_end_matches("…[truncated]")
        .chars()
        .all(|c| c == '日'));
}

// ------------------------------------------------ end to end, through a real run

mod live {
    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
    use io_harness::{
        run_with, ApproveAll, ContextBudget, Policy, Provider, Store, TaskContract, Verification,
    };
    use serde_json::json;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Plays a script of tool calls and keeps every request it was sent.
    struct Script {
        steps: Vec<Vec<ToolCall>>,
        at: AtomicUsize,
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl Script {
        fn new(steps: Vec<Vec<ToolCall>>) -> Self {
            Self {
                steps,
                at: AtomicUsize::new(0),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn prompts(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.user.clone())
                .collect()
        }
    }

    impl Provider for Script {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(req);
            Ok(CompletionResponse {
                tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
                ..Default::default()
            })
        }
    }

    fn never_passes(root: &Path, steps: u32) -> TaskContract {
        TaskContract::workspace("exercise durable memory", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "unreachable.txt".into(),
                needle: "never".into(),
            })
            .with_max_steps(steps)
            .with_context_budget(ContextBudget::default())
    }

    fn remember(key: &str, value: &str) -> Vec<ToolCall> {
        vec![ToolCall {
            name: "remember".into(),
            arguments: json!({ "key": key, "value": value }),
        }]
    }

    /// F6 — the point of the pillar: a second run over one workspace starts
    /// knowing what the first established, without re-running the work.
    #[tokio::test]
    async fn a_second_run_over_the_workspace_recalls_what_the_first_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs.db")).unwrap();
        let policy = Policy::default()
            .layer("t")
            .allow_read("*")
            .allow_write("*");

        let first = Script::new(vec![remember(
            "build-command",
            "cargo test --workspace, not cargo build",
        )]);
        let contract = never_passes(dir.path(), 1);
        let a = run_with(&contract, &first, &store, &policy, &ApproveAll)
            .await
            .unwrap();

        // The first run's own prompts never carried the note: it had none to carry.
        assert!(
            !first.prompts()[0].contains("[memory]"),
            "an empty memory must render no block at all"
        );

        let second = Script::new(vec![vec![], vec![]]);
        let b = run_with(&contract, &second, &store, &policy, &ApproveAll)
            .await
            .unwrap();
        assert_ne!(a.run_id, b.run_id, "these must be two different runs");

        let first_prompt = &second.prompts()[0];
        assert!(
            first_prompt.contains("[memory]") && first_prompt.contains("cargo test --workspace"),
            "the second run must open already knowing it, got:\n{first_prompt}"
        );
        assert!(
            first_prompt.contains("not instructions"),
            "the block must say what it is, got:\n{first_prompt}"
        );
        // A note names the step it was written on and nothing else. The run id is
        // the store's autoincrement row id, so rendering it would make the same
        // case replayed over the same workspace send different prompt bytes.
        assert!(
            first_prompt
                .contains("- build-command: cargo test --workspace, not cargo build  (step 1)"),
            "a note must render as key, value and step only, got:\n{first_prompt}"
        );
        assert!(
            !first_prompt.contains(&format!("run {}", a.run_id)),
            "the prompt must not name the run that wrote a note, got:\n{first_prompt}"
        );

        // And the second run did not re-execute the call that established it.
        let calls: Vec<String> = store
            .steps(b.run_id)
            .unwrap()
            .iter()
            .map(|s| s.tool_call.clone())
            .collect();
        assert!(
            calls.iter().all(|c| !c.contains("remember")),
            "the second run re-did the work it should have recalled: {calls:?}"
        );

        // The write and the recall are both in the trace.
        let kinds: Vec<String> = store
            .context_events(a.run_id)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"memory_write".to_string()), "got {kinds:?}");
        let kinds: Vec<String> = store
            .context_events(b.run_id)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            kinds.contains(&"memory_recall".to_string()),
            "got {kinds:?}"
        );
    }

    /// F7 — what the operator deletes stays deleted, including for later runs.
    #[tokio::test]
    async fn a_deleted_note_does_not_come_back_in_a_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs.db")).unwrap();
        let policy = Policy::default().layer("t").allow_read("*");
        let contract = never_passes(dir.path(), 1);

        let first = Script::new(vec![remember("stale", "SHOULD-NOT-SURVIVE")]);
        run_with(&contract, &first, &store, &policy, &ApproveAll)
            .await
            .unwrap();

        let key = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(store.memory_delete(&key, "stale").unwrap());

        let second = Script::new(vec![vec![]]);
        run_with(&contract, &second, &store, &policy, &ApproveAll)
            .await
            .unwrap();
        assert!(
            !second.prompts()[0].contains("SHOULD-NOT-SURVIVE"),
            "a deleted note must not be recalled, got:\n{}",
            second.prompts()[0]
        );
    }

    /// Two workspaces share a store and must not share memory.
    #[tokio::test]
    async fn two_workspaces_do_not_share_memory_through_one_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs.db")).unwrap();
        let policy = Policy::default().layer("t").allow_read("*");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let first = Script::new(vec![remember("secret", "ONLY-IN-A")]);
        run_with(&never_passes(&a, 1), &first, &store, &policy, &ApproveAll)
            .await
            .unwrap();

        let second = Script::new(vec![vec![]]);
        run_with(&never_passes(&b, 1), &second, &store, &policy, &ApproveAll)
            .await
            .unwrap();
        assert!(
            !second.prompts()[0].contains("ONLY-IN-A"),
            "workspace b must not see a's notes, got:\n{}",
            second.prompts()[0]
        );
    }

    /// The block is capped like everything else, and says how much it dropped.
    #[tokio::test]
    async fn an_over_long_memory_block_is_cut_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs.db")).unwrap();
        let policy = Policy::default().layer("t").allow_read("*");
        let key = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for i in 0..40 {
            store
                .memory_put(&key, &format!("k{i}"), &"n".repeat(300), 1, 1)
                .unwrap();
        }

        // A small ceiling, so a quarter of it cannot hold forty notes.
        let contract = never_passes(dir.path(), 1).with_context_budget(ContextBudget {
            max_tokens: 1_000,
            share: 0.5,
        });
        let script = Script::new(vec![vec![]]);
        run_with(&contract, &script, &store, &policy, &ApproveAll)
            .await
            .unwrap();

        let prompt = &script.prompts()[0];
        assert!(
            prompt.contains("older note(s) elided to fit"),
            "the cut must be visible, got:\n{prompt}"
        );
        assert!(
            prompt.contains("k39"),
            "the newest notes are the ones kept, got:\n{prompt}"
        );
    }
}

// ---------------------------------------------------------------------------
// 0.30.0 — kind, pinned, and the recall record
// ---------------------------------------------------------------------------

/// Plays a fixed list of tool calls, one per turn, then answers with nothing.
struct Script(Vec<Vec<ToolCall>>, std::sync::atomic::AtomicUsize);

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.0.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
    fn name(&self) -> &str {
        "script"
    }
}

fn remember(key: &str, value: &str) -> ToolCall {
    ToolCall {
        // The tool's wire name, spelled as the model spells it.
        name: "remember".into(),
        arguments: json!({ "key": key, "value": value }),
    }
}

/// The workspace key the run loop stores memory under: the canonical root.
fn ws_key(root: &std::path::Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

async fn run_in(root: &std::path::Path, store: &Store, script: Vec<Vec<ToolCall>>) -> i64 {
    let contract = TaskContract::workspace("write a note", root)
        .with_max_steps(2)
        // A small prompt ceiling, so the memory block's quarter of it is small
        // enough that a full-size note does not fit beside three short ones.
        // Without it every note fits and the recall test asserts nothing.
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        });
    run_with(
        &contract,
        &Script(script, std::sync::atomic::AtomicUsize::new(0)),
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the run itself must not error")
    .run_id
}

/// 0.30.0 F5 — a pinned entry survives a run that tries to overwrite it.
#[tokio::test]
async fn a_pinned_entry_survives_a_run_that_tries_to_overwrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let ws = ws_key(dir.path());

    store
        .memory_write(&ws, "retries", "three", 1, 1, MemoryKind::Decision)
        .unwrap();
    assert!(store.memory_pin(&ws, "retries", true).unwrap());

    let run = run_in(dir.path(), &store, vec![vec![remember("retries", "one")]]).await;

    // Half one: the operator's value is what a later reader gets.
    let entry = store.memory_get(&ws, "retries").unwrap().unwrap();
    assert_eq!(entry.value, "three");
    assert_eq!(
        entry.kind,
        MemoryKind::Decision,
        "and it is still a decision"
    );
    assert!(entry.pinned);

    // Half two: the attempt is in the trace. A silent refusal would leave the
    // agent believing it had corrected something — which is the whole failure
    // this flag exists to prevent, and it is invisible without this row.
    let kinds: Vec<String> = store
        .context_events(run)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.iter().any(|k| k == "memory_refused"),
        "the refusal must be recorded: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "memory_write"),
        "and a write that did not happen must not be recorded as one: {kinds:?}"
    );
}

/// 0.30.0 F5, the control. The same run against the same key *unpinned* writes.
#[tokio::test]
async fn the_same_write_lands_when_the_entry_is_not_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let ws = ws_key(dir.path());

    store
        .memory_write(&ws, "retries", "three", 1, 1, MemoryKind::Decision)
        .unwrap();

    let run = run_in(dir.path(), &store, vec![vec![remember("retries", "one")]]).await;

    assert_eq!(
        store.memory_get(&ws, "retries").unwrap().unwrap().value,
        "one"
    );
    assert_eq!(
        store.memory_get(&ws, "retries").unwrap().unwrap().kind,
        MemoryKind::Fact,
        "a run's own write is a fact, whatever the entry was before"
    );
    let kinds: Vec<String> = store
        .context_events(run)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(kinds.iter().any(|k| k == "memory_write"), "{kinds:?}");
}

/// 0.30.0 F6 — the recall record names the run and the entries it drew on.
#[tokio::test]
async fn the_recall_record_names_which_entries_a_run_actually_used() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let ws = ws_key(dir.path());

    // Six entries. The three oldest are each individually larger than the memory
    // block's whole ceiling, so the newest-first fit stops at the first of them —
    // which makes "three of six" a property of the sizes rather than of a token
    // estimate this test would otherwise be pinned to.
    for i in 0..3 {
        store
            .memory_put(
                &ws,
                &format!("huge-{i}"),
                &"x".repeat(io_harness::MEMORY_MAX_ENTRY_CHARS),
                1,
                1,
            )
            .unwrap();
    }
    for i in 0..3 {
        store
            .memory_put(&ws, &format!("small-{i}"), "short", 1, 1)
            .unwrap();
    }

    let first = run_in(dir.path(), &store, vec![vec![]]).await;
    let recalled: Vec<String> = store
        .memory_recalls(first)
        .unwrap()
        .iter()
        .map(|r| r.key.clone())
        .collect();
    assert_eq!(
        recalled,
        ["small-0", "small-1", "small-2"],
        "the three that fit, named — not counted"
    );
    for r in store.memory_recalls(first).unwrap() {
        assert_eq!(r.run_id, first);
        assert_eq!(r.workspace, ws);
    }

    // A second run over the same workspace records its own, and does not disturb
    // the first: a recall is a fact about a run, not a flag on an entry.
    let second = run_in(dir.path(), &store, vec![vec![]]).await;
    assert_ne!(first, second);
    assert_eq!(store.memory_recalls(first).unwrap().len(), 3);
    assert_eq!(
        store
            .memory_recalls(second)
            .unwrap()
            .iter()
            .map(|r| r.key.clone())
            .collect::<Vec<_>>(),
        ["small-0", "small-1", "small-2"]
    );
    assert!(
        store.memory_recalls(second + 999).unwrap().is_empty(),
        "a run that never happened recalled nothing"
    );
}

// ---------------------------------------------------------------------------
// 0.56.0 F6 — the operator's caps reach the write the model actually makes
// ---------------------------------------------------------------------------

async fn run_under(
    root: &std::path::Path,
    store: &Store,
    script: Vec<Vec<ToolCall>>,
    limits: Option<io_harness::MemoryLimits>,
) -> i64 {
    let mut contract = TaskContract::workspace("write some notes", root)
        .with_max_steps(2)
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        });
    if let Some(limits) = limits {
        contract = contract.with_memory_limits(limits);
    }
    run_with(
        &contract,
        &Script(script, std::sync::atomic::AtomicUsize::new(0)),
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the run itself must not error")
    .run_id
}

/// The set run and the unset run, over the same workspace and the same script.
/// The contract's caps have to reach the store through the tool arm, or the
/// projection tested in `tests/config.rs` is a number nothing reads.
#[tokio::test]
async fn a_contracts_memory_caps_bound_what_a_run_may_remember() {
    let notes = vec![vec![
        remember("a", "first"),
        remember("b", "second"),
        remember("c", "third"),
        remember("d", "fourth"),
    ]];

    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    run_under(
        dir.path(),
        &store,
        notes.clone(),
        Some(io_harness::MemoryLimits {
            max_entries: 2,
            ..Default::default()
        }),
    )
    .await;
    let kept = store.memory_list(&ws_key(dir.path())).unwrap();
    assert_eq!(kept.len(), 2, "the operator's cap bounds the store");
    // The newest survive: nothing has been recalled, so the tie-break is the
    // write clock and the two oldest are the candidates.
    let keys: Vec<String> = kept.into_iter().map(|e| e.key).collect();
    assert_eq!(keys, vec!["c".to_string(), "d".to_string()]);

    // The control: the same four notes with nothing set are all kept, because
    // the default cap is sixty-four.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    run_under(dir.path(), &store, notes, None).await;
    assert_eq!(store.memory_list(&ws_key(dir.path())).unwrap().len(), 4);
}

// ---------------------------------------------------------------------------
// 0.56.0 F9–F11 — a run can unlearn
// ---------------------------------------------------------------------------

fn forget(key: &str) -> ToolCall {
    ToolCall {
        name: "forget".into(),
        arguments: json!({ "key": key }),
    }
}

/// F9. The note is gone from the store, gone from the next turn's prompt, and
/// the observation names it. Withdrawing a key that was never there says so
/// rather than reporting a removal that did not happen.
#[tokio::test]
async fn a_run_withdraws_a_note_and_a_key_that_was_never_there_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let ws = ws_key(dir.path());
    store.memory_put(&ws, "retries", "three", 1, 1).unwrap();

    let run = run_under(
        dir.path(),
        &store,
        vec![vec![forget("retries")], vec![forget("never-written")]],
        None,
    )
    .await;

    assert!(store.memory_get(&ws, "retries").unwrap().is_none());
    let kinds: Vec<String> = store
        .context_events(run)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| *k == "memory_forget").count(),
        1,
        "one withdrawal, and the key that was never there is not a second: {kinds:?}"
    );

    // S9's finding. Counting trace rows alone leaves the *message* unasserted,
    // and the message is what the model acts on: reporting a removal that did
    // not happen tells an agent it has corrected something it has not. Neither
    // arm writes a `memory_forget` row for an absent key, so a sabotage that
    // reported success to the model survived the assertion above.
    let said: Vec<String> = store
        .observations(run)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    let all = said.join("\n");
    assert!(
        all.contains("[forget retries]"),
        "the withdrawal names the key it took: {all}"
    );
    assert!(
        all.contains("[forget: nothing to forget]"),
        "and the key that was never there is told so, not told it was removed: {all}"
    );
    assert!(
        !all.contains("[forget never-written]"),
        "the absent answer must not wear the prefix a real removal wears, or a \
         model skimming the head of the observation reads them as the same: {all}"
    );
}

/// F10. A pinned entry is not a run's to withdraw, and nothing is withdrawn
/// while a plan is unapproved. Both halves asserted on the store rather than on
/// the message, because a refusal that says the right thing and removes the
/// entry anyway would pass a text assertion.
#[tokio::test]
async fn a_pinned_note_survives_a_forget_and_the_refusal_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let ws = ws_key(dir.path());
    store
        .memory_write(
            &ws,
            "owner",
            "the platform team",
            1,
            1,
            MemoryKind::Decision,
        )
        .unwrap();
    assert!(store.memory_pin(&ws, "owner", true).unwrap());

    let run = run_under(dir.path(), &store, vec![vec![forget("owner")]], None).await;

    let entry = store.memory_get(&ws, "owner").unwrap().unwrap();
    assert_eq!(entry.value, "the platform team");
    assert_eq!(entry.kind, MemoryKind::Decision);
    assert!(entry.pinned);

    let kinds: Vec<String> = store
        .context_events(run)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.iter().any(|k| k == "memory_refused"),
        "the refusal is in the trace: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "memory_forget"),
        "and a withdrawal that did not happen is not recorded as one: {kinds:?}"
    );
}

/// F11, half one. A rewind puts back what a forget took. `memory_restore` had
/// been an `UPDATE` since 0.36.0, which restores an entry a run *edited* and
/// silently does nothing for one a run *removed*.
#[test]
fn a_rewind_puts_back_the_note_a_forget_took() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("runs.db")).unwrap();
    let ws = ws_key(dir.path());
    let first = store.start_run("learn it", &ws).unwrap();
    store
        .memory_write(&ws, "retries", "three", first, 1, MemoryKind::Decision)
        .unwrap();

    // A LATER run withdraws it, so the restore point says "there was a value
    // here" rather than "this run created it".
    let second = store.start_run("unlearn it", &ws).unwrap();
    assert_eq!(
        store.memory_forget(&ws, "retries", second, 3).unwrap(),
        io_harness::MemoryForget::Removed
    );
    assert!(store.memory_get(&ws, "retries").unwrap().is_none());

    let workspace = io_harness::tools::Workspace::new(dir.path());
    let done = io_harness::rewind_run(&workspace, &store, second).unwrap();
    assert_eq!(done.memory_restored, ["retries"]);

    let back = store
        .memory_get(&ws, "retries")
        .unwrap()
        .expect("the withdrawal is undone by the mechanism every other write uses");
    assert_eq!(back.value, "three");
    assert_eq!(
        back.kind,
        MemoryKind::Decision,
        "and its kind came back too"
    );
    assert!(!back.pinned, "a pinned entry could not have been forgotten");
}
