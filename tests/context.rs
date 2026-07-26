//! What the model is sent, turn by turn (0.10.0).
//!
//! Until 0.10 the workspace loop kept one string, appended every tool result to
//! it, and re-sent the whole thing every turn: the prompt tracked the step count,
//! a file read twice was sent twice, and a read the agent had already written over
//! was still presented as current. These tests drive the real loop with the
//! scripted mock provider the rest of the suite uses and assert on the prompts the
//! provider actually received — the only place the difference is observable.
//!
//! The other half of every assertion is the trace: bounding what the model sees
//! must never bound what an operator can audit, so wherever a prompt is asserted
//! to be smaller, `steps.result` is asserted to still hold everything.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use io_harness::context::{
    assemble, entry_cap_chars, estimate_tokens, ContextBudget, Ledger, ObsKind, Observation,
};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox, Workspace};
use io_harness::{
    run_with, ApproveAll, McpServer, Policy, Provider, Store, TaskContract, ToolSpec, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls, one turn at a time, and keeps every
/// request it was sent — the prompts are what these tests are about.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The observation section of the `n`th request (0-based).
    fn observations(&self, n: usize) -> String {
        let seen = self.seen.lock().unwrap();
        let req = seen
            .get(n)
            .unwrap_or_else(|| panic!("the loop ran only {} turn(s), wanted turn {n}", seen.len()));
        section(&req.user)
    }

    fn turns(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// Cut the observation section out of a workspace prompt, so a size assertion is
/// about the log rather than about the fixed framing around it.
fn section(user: &str) -> String {
    let head = "Observations so far (results of your tool calls):\n";
    let from = user.find(head).expect("the workspace prompt frame") + head.len();
    let rest = &user[from..];
    let to = rest.find("\n\nCall a tool").unwrap_or(rest.len());
    rest[..to].to_string()
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A contract that can never be satisfied, so the loop runs its whole step budget
/// and every scripted turn is reached.
fn never_passes(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace(
        "exercise the context assembler",
        root,
        Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        },
    )
    .with_max_steps(steps)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// A small ceiling, so a handful of ordinary observations is enough to exceed it.
/// The per-entry cap floors at 2,000 chars whatever the ceiling, so the fixtures
/// below are sized against that floor rather than against this number.
fn tight(tokens: u64) -> ContextBudget {
    ContextBudget {
        max_tokens: tokens,
        share: 0.5,
    }
}

/// A registered tool returning a fixed string, for the "bounded at entry" case.
struct Fixed(String);

impl Tool for Fixed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "firehose".into(),
            description: "Returns a lot of text.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        let out = self.0.clone();
        Box::pin(async move { Ok(out) })
    }
}

/// Where `cargo test` left the MCP fixture example binary (same derivation as
/// `tests/mcp.rs`; an example rather than a bin, so `CARGO_BIN_EXE_*` is absent).
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join("examples").join(format!(
        "mcp_fixture_server{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.exists(),
        "fixture server not built at {}. `cargo test` builds examples.",
        path.display()
    );
    path
}

// ---------------------------------------------------------------- F1: the ceiling holds

/// F1 — raw observations far exceeding the ceiling still produce a request inside
/// it, and the trace still holds every one of them in full. The two halves are one
/// test on purpose: a bound that shrinks the audit trail is not the bound this
/// release asked for.
#[tokio::test]
async fn the_assembled_prompt_stays_inside_the_ceiling_while_the_trace_keeps_everything() {
    let dir = ws();
    // Five files, each under the per-entry cap but together well over the ceiling.
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("{}SENTINEL-{i}\n", "filler line\n".repeat(150)),
        )
        .unwrap();
    }
    let contract = never_passes(dir.path(), 6).with_context_budget(tight(1_000));
    let provider = MockScript::new(
        (0..5)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i}.txt") }))])
            .collect(),
    );
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    // 1,000 tokens of ceiling is 4,000 chars of carried observations; the raw log
    // is more than twice that. The slack is for the one-line stubs, which are the
    // only part of the section that grows with the run's length.
    let last = provider.observations(provider.turns() - 1);
    // A row holds the step's own observations; the whole log is the rows
    // concatenated in step order, which is what makes the delta lossless.
    let raw: String = store
        .steps(result.run_id)
        .unwrap()
        .iter()
        .map(|s| s.result.as_str())
        .collect();
    assert!(
        raw.chars().count() > 9_000,
        "the fixture must exceed the ceiling to be testing anything (raw {} chars)",
        raw.chars().count()
    );
    assert!(
        estimate_tokens(&last) <= 1_400,
        "the assembled section must stay inside the ceiling, got {} est. tokens:\n{last}",
        estimate_tokens(&last)
    );

    // The newest read is carried; the oldest is not.
    assert!(last.contains("SENTINEL-4"), "the newest read must be whole");
    assert!(
        !last.contains("SENTINEL-0"),
        "the oldest read must have been elided, got:\n{last}"
    );
    assert!(
        last.contains("[read f0.txt] (elided:"),
        "an elided observation must still be named and explained, got:\n{last}"
    );

    // The trace holds all five, unelided.
    for i in 0..5 {
        assert!(
            raw.contains(&format!("SENTINEL-{i}")),
            "steps.result must hold every observation in full; SENTINEL-{i} is missing"
        );
    }
    assert!(
        !raw.contains("(elided:"),
        "eliding is a decision about the request, never about the trace"
    );
}

// ---------------------------------------------------------------- F2: it stabilises

/// F2 — over many turns of small results the request stops growing with the step
/// count, while the log behind it keeps growing. The comparison is of *growth*: the
/// old loop's prompt grew exactly as fast as the log.
#[tokio::test]
async fn prompt_size_stabilises_across_many_turns_instead_of_tracking_step_count() {
    let dir = ws();
    for i in 0..20 {
        std::fs::write(
            dir.path().join(format!("f{i:02}.txt")),
            format!("{}line-{i}\n", "small\n".repeat(90)),
        )
        .unwrap();
    }
    let contract = never_passes(dir.path(), 21).with_context_budget(tight(1_000));
    let provider = MockScript::new(
        (0..20)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i:02}.txt") }))])
            .collect(),
    );
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let early = provider.observations(7).chars().count();
    let late = provider.observations(20).chars().count();
    let steps = store.steps(result.run_id).unwrap();
    // Cumulative through each step: the log the run had accumulated by then, which
    // is what "the log itself keeps growing" means now that a row is a delta.
    let log_through = |n: usize| -> usize {
        steps[..=n]
            .iter()
            .map(|s| s.result.chars().count())
            .sum::<usize>()
    };
    let early_log = log_through(6);
    let late_log = log_through(19);

    assert!(
        late_log > early_log * 2,
        "the log itself must keep growing, else this test proves nothing \
         (early {early_log}, late {late_log})"
    );
    assert!(
        late < early * 3 / 2,
        "the prompt must stabilise rather than track the step count \
         (early {early}, late {late}; the log grew {early_log} -> {late_log})"
    );
}

// ---------------------------------------------------------------- F3: supersession

/// F3 — two observations of one target are one answer. The later is carried whole;
/// the earlier is a stub that names the step that superseded it, so the model can
/// tell "you already have this" from "this was dropped".
#[tokio::test]
async fn a_read_superseded_by_a_later_read_of_the_same_path_becomes_a_stub() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "SENTINEL-BODY\n").unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "a.rs" }))],
        vec![call("read_file", json!({ "path": "a.rs" }))],
    ]);
    let store = Store::memory().unwrap();
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let third = provider.observations(2);
    assert_eq!(
        third.matches("SENTINEL-BODY").count(),
        1,
        "one path's contents must be sent once, got:\n{third}"
    );
    assert!(
        third.contains("[read a.rs] (elided: superseded by the read at step 2)"),
        "the stub must name the step that superseded it, got:\n{third}"
    );
}

// ---------------------------------------------------------------- F4: invalidation

/// F4 — a write makes an earlier read of that path wrong. The next turn carries
/// the file's *current* contents, re-read through the policy at assembly time, and
/// says which write invalidated which read.
#[tokio::test]
async fn a_write_invalidates_the_earlier_read_so_the_next_turn_sees_the_new_contents() {
    let dir = ws();
    std::fs::write(dir.path().join("a.rs"), "OLD-CONTENT\n").unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "a.rs" }))],
        vec![call(
            "write_file",
            json!({ "path": "a.rs", "content": "NEW-CONTENT\n" }),
        )],
    ]);
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let third = provider.observations(2);
    assert!(
        third.contains("NEW-CONTENT"),
        "the invalidated read must be refreshed, got:\n{third}"
    );
    assert!(
        !third.contains("OLD-CONTENT"),
        "the stale contents must not be presented as current, got:\n{third}"
    );
    assert!(
        third.contains("invalidated by the write at step 2"),
        "the refresh must say why it happened, got:\n{third}"
    );

    let rows = store.context_events(result.run_id).unwrap();
    assert!(
        rows.iter().any(|r| r.kind == "reread"),
        "a re-read must be in the trace, got {rows:?}"
    );
    // One assembled row per turn — not one per observation, and not one per stub.
    assert_eq!(
        rows.iter().filter(|r| r.kind == "assembled").count(),
        3,
        "exactly one assembled row per turn, got {rows:?}"
    );
    let assembled = rows.iter().find(|r| r.step == 3 && r.kind == "assembled");
    let assembled = assembled.expect("the third turn's row");
    assert!(assembled.est_tokens.unwrap_or(0) > 0);
    assert!(
        assembled
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("reread=1"),
        "the row must summarise the turn, got {assembled:?}"
    );
}

/// F4 (refusal half) — freshening a stale read is still a read, so the policy
/// decides it. A refused re-read is a stub naming the invalidating step *and* the
/// reason, and lands in the trace as `reread_refused` rather than silently
/// carrying contents the policy no longer permits.
#[tokio::test]
async fn a_policy_refused_reread_is_a_stub_naming_the_invalidating_step_and_the_reason() {
    let dir = ws();
    std::fs::write(dir.path().join("notes.txt"), "NEW-CONTENT\n").unwrap();
    let store = Store::memory().unwrap();
    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        1,
        ObsKind::Read,
        Some("notes.txt".into()),
        "\n[read notes.txt]\nOLD-CONTENT\n",
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Write,
        Some("notes.txt".into()),
        "\n[wrote notes.txt] (12 chars)\n",
    ));
    let policy = Policy::default()
        .layer("test")
        .allow_read("*")
        .deny_read("notes.txt");

    let ws = Workspace::with_policy(dir.path(), policy.clone());
    let out = assemble(&ledger, 24_000, Some(&ws), &policy, &store, 1, 3)
        .await
        .unwrap();

    assert!(
        out.text.contains("invalidated by the write at step 2"),
        "the stub must name the invalidating step, got:\n{}",
        out.text
    );
    assert!(
        out.text.contains("the policy denies reading it"),
        "the stub must say why the re-read did not happen, got:\n{}",
        out.text
    );
    assert!(
        !out.text.contains("OLD-CONTENT") && !out.text.contains("NEW-CONTENT"),
        "a refused re-read must carry no contents at all, got:\n{}",
        out.text
    );
    let rows = store.context_events(1).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.kind == "reread_refused" && r.detail.as_deref().unwrap().contains("notes")),
        "the refusal must be in the trace, got {rows:?}"
    );
}

// ---------------------------------------------------------------- F5: bounded at entry

/// F5 — every kind of observation is bounded where it enters the context, with the
/// cut visible to the model, and the trace keeps the whole entry. Before 0.10 two
/// of these kinds (`find`, `write_file`) had no cap at all and the other caps were
/// four unrelated constants.
#[tokio::test]
async fn every_observation_kind_is_bounded_where_it_enters_the_context() {
    let dir = ws();
    let cap = entry_cap_chars(tight(4_000).effective_tokens(None));

    // read: a file well over the cap, ending in a marker the tail must keep.
    std::fs::write(
        dir.path().join("big.txt"),
        format!("{}TAIL-OF-FILE\n", "x".repeat(cap * 2)),
    )
    .unwrap();
    // grep: a hundred long matching lines, of which the hit ceiling keeps 50.
    std::fs::write(
        dir.path().join("hits.txt"),
        "needle in a line long enough that fifty of them exceed the cap by themselves\n"
            .repeat(100),
    )
    .unwrap();
    // find: enough long filenames that the path list alone exceeds the cap.
    for i in 0..120 {
        std::fs::write(
            dir.path()
                .join(format!("padded_filename_number_{i:04}.dat")),
            "",
        )
        .unwrap();
    }
    // skill: a body over the cap.
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("verbose.md"),
        format!("How to be verbose.\n{}", "s".repeat(cap * 2)),
    )
    .unwrap();

    let contract = never_passes(dir.path(), 7)
        .with_context_budget(tight(4_000))
        .with_skills(&skills)
        .with_tools(Toolbox::new().with(Fixed("t".repeat(cap * 2))))
        .with_constraint("bounded");
    let provider = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "big.txt" }))],
        vec![call("grep", json!({ "pattern": "needle" }))],
        vec![call("find", json!({ "name_glob": "*.dat" }))],
        vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "w".repeat(cap * 2) }),
        )],
        vec![call("read_skill", json!({ "name": "verbose" }))],
        vec![call("firehose", json!({}))],
    ]);
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    // Each observation is asserted on the turn *after* it was made, where it is
    // the newest entry and so is carried whole if anything is.
    let cut = |turn: usize, header: &str| {
        let obs = provider.observations(turn);
        let at = obs
            .find(header)
            .unwrap_or_else(|| panic!("no {header} in turn {turn}:\n{obs}"));
        let entry = &obs[at..];
        // One entry runs to the next entry's bracketed header — and a truncation
        // marker is bracketed too, so it does not count as the start of one.
        let mut end = entry.len();
        let mut from = 1;
        while let Some(rel) = entry[from..].find("\n[") {
            let cut_at = from + rel;
            if !entry[cut_at + 1..].starts_with("[truncated") {
                end = cut_at + 1;
                break;
            }
            from = cut_at + 2;
        }
        let entry = &entry[..end];
        assert!(
            entry.chars().count() <= cap + 200,
            "{header} must be bounded at entry ({} chars, cap {cap})",
            entry.chars().count()
        );
        assert!(
            entry.contains("truncated") || entry.chars().count() < cap,
            "a cut must be visible to the model: {header}"
        );
        entry.to_string()
    };

    let read = cut(1, "[read big.txt]");
    assert!(
        read.contains("TAIL-OF-FILE"),
        "a read keeps its tail — the end of a file is what a writer needs:\n{read}"
    );
    let grep = cut(2, "[grep \"needle\"]");
    assert!(
        grep.matches("hits.txt").count() <= 50,
        "the fifty-hit relevance ceiling still applies on top of the char cap"
    );
    assert!(grep.contains("truncated"), "the grep result must be cut");
    let found = cut(3, "[find \"*.dat\"]");
    assert!(found.contains("truncated"), "the find result must be cut");
    // A write observation reports the size it wrote rather than echoing it, so it
    // is short by construction — but it is now bounded by the same rule as the
    // rest rather than by luck.
    let wrote = cut(4, "[wrote out.txt]");
    assert!(wrote.contains("chars"), "got {wrote}");
    let skill = cut(5, "[skill verbose]");
    assert!(skill.contains("truncated"), "the skill body must be cut");
    let tool = cut(6, "[firehose]");
    assert!(tool.contains("truncated"), "the tool result must be cut");

    // The trace keeps every entry whole — bounded at entry, never stubbed.
    // A row holds the step's own observations; the whole log is the rows
    // concatenated in step order, which is what makes the delta lossless.
    let raw: String = store
        .steps(result.run_id)
        .unwrap()
        .iter()
        .map(|s| s.result.as_str())
        .collect();
    for header in [
        "[read big.txt]",
        "[grep \"needle\"]",
        "[find \"*.dat\"]",
        "[wrote out.txt]",
        "[skill verbose]",
        "[firehose]",
    ] {
        assert!(raw.contains(header), "the trace must hold {header}");
    }
    assert!(
        !raw.contains("(elided:"),
        "the trace must hold the unelided log"
    );
}

/// F5 (MCP half) — a server's reply is bounded on the same terms as everything
/// else, against a real server process rather than a mock at the protocol level.
#[tokio::test]
async fn an_mcp_result_is_bounded_where_it_enters_the_context() {
    let dir = ws();
    let cap = entry_cap_chars(tight(4_000).effective_tokens(None));
    let contract = never_passes(dir.path(), 2)
        .with_context_budget(tight(4_000))
        .with_mcp([McpServer::stdio(
            "fix",
            fixture_server().display().to_string(),
        )]);
    let provider = MockScript::new(vec![vec![call(
        "mcp__fix__echo",
        json!({ "text": "e".repeat(cap * 2) }),
    )]]);
    let store = Store::memory().unwrap();
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let obs = provider.observations(1);
    assert!(
        obs.contains("[mcp__fix__echo]") && obs.contains("truncated"),
        "the server's reply must arrive bounded and marked, got {} chars",
        obs.chars().count()
    );
    assert!(
        obs.chars().count() <= cap + 200,
        "the server's reply must be bounded at entry ({} chars, cap {cap})",
        obs.chars().count()
    );
}

// ---------------------------------------------------------------- NF3: assembly cost

/// NF3 — assembly is per-turn work on the hot path, so its cost is bounded here
/// the way `tests/policy.rs` bounds policy dispatch: a real measurement with a
/// generous ceiling, so a change that makes it quadratic in the log fails rather
/// than merely feeling slow.
#[tokio::test]
async fn assembling_one_turn_costs_a_bounded_amount_of_time() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = open_policy();
    let workspace = Workspace::with_policy(dir.path(), policy.clone());
    let mut ledger = Ledger::new();
    for i in 0..200u32 {
        ledger.push(Observation::new(
            i + 1,
            if i % 3 == 0 {
                ObsKind::Read
            } else {
                ObsKind::Grep
            },
            Some(format!("f{}.txt", i % 40)),
            format!("\n[entry {i}]\n{}\n", "y".repeat(1_000)),
        ));
    }

    const TURNS: u32 = 50;
    let started = Instant::now();
    for step in 0..TURNS {
        let out = assemble(&ledger, 24_000, Some(&workspace), &policy, &store, 1, step)
            .await
            .unwrap();
        assert!(out.carried > 0);
    }
    let per_turn = started.elapsed() / TURNS;

    assert!(
        per_turn < std::time::Duration::from_millis(25),
        "assembling a 200-entry log took {per_turn:?} per turn, over the 25ms bound"
    );
}

// ---------------------------------------------------------------- unit-level budget maths

/// The budget's arithmetic is the load-bearing part of every bound above, so it is
/// asserted directly too — including the two cases the loop rarely reaches.
#[test]
fn the_budget_derives_the_prompt_ceiling_and_the_entry_cap_from_one_number() {
    let b = ContextBudget::default();
    assert_eq!(b.max_tokens, 24_000);
    assert_eq!(b.effective_tokens(None), 24_000);
    assert_eq!(b.effective_tokens(Some(20_000)), 10_000);
    assert_eq!(b.effective_tokens(Some(4)), 2_000, "the floor holds");
    assert_eq!(entry_cap_chars(b.effective_tokens(None)), 12_000);
    assert_eq!(entry_cap_chars(b.effective_tokens(Some(4))), 2_000);
    assert_eq!(estimate_tokens(&"x".repeat(12_000)), 3_000);
}

// -------------------------------------------------- supersession is about subjects

/// Supersession collapses two answers about one subject. A registered or MCP
/// tool's target is its NAME, not its subject: called twice with different
/// arguments it gave two different answers, and stubbing the first as
/// "superseded" would throw one away.
#[tokio::test]
async fn two_calls_to_one_tool_keep_both_answers_while_two_reads_of_a_path_collapse() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = open_policy();
    let workspace = Workspace::with_policy(dir.path(), policy.clone());
    std::fs::write(dir.path().join("a.txt"), "SECOND").unwrap();

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        1,
        ObsKind::Tool,
        Some("weather".into()),
        "\n[weather]\nLONDON-RAIN\n",
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Tool,
        Some("weather".into()),
        "\n[weather]\nCAIRO-SUN\n",
    ));
    ledger.push(Observation::new(
        3,
        ObsKind::Read,
        Some("a.txt".into()),
        "\n[read a.txt]\nFIRST\n",
    ));
    ledger.push(Observation::new(
        4,
        ObsKind::Read,
        Some("a.txt".into()),
        "\n[read a.txt]\nSECOND\n",
    ));

    let out = assemble(&ledger, 24_000, Some(&workspace), &policy, &store, 1, 5)
        .await
        .unwrap();

    assert!(
        out.text.contains("LONDON-RAIN") && out.text.contains("CAIRO-SUN"),
        "both tool answers must survive, got:\n{}",
        out.text
    );
    assert!(
        out.text.contains("SECOND") && !out.text.contains("FIRST"),
        "the later read must replace the earlier one, got:\n{}",
        out.text
    );
    assert!(
        out.text.contains("superseded by the read at step 4"),
        "the superseded read must say what replaced it, got:\n{}",
        out.text
    );
}

// -------------------------------------------------- the re-read stays contained

/// The assembly-time re-read must be as contained as the read it refreshes. The
/// policy here is deliberately permissive — it is the workspace's own path
/// resolution, not the policy, that has to stop a target pointing outside the
/// root, which is why reading the filesystem directly would have been the wrong
/// half of the pair to copy.
#[tokio::test]
async fn a_re_read_cannot_escape_the_workspace_root() {
    let outer = ws();
    let root = outer.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP-SECRET").unwrap();

    let store = Store::memory().unwrap();
    let policy = open_policy();
    let workspace = Workspace::with_policy(&root, policy.clone());

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        1,
        ObsKind::Read,
        Some("../secret.txt".into()),
        "\n[read ../secret.txt]\nOLD\n",
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Write,
        Some("../secret.txt".into()),
        "\n[wrote ../secret.txt] (3 chars)\n",
    ));

    let out = assemble(&ledger, 24_000, Some(&workspace), &policy, &store, 1, 3)
        .await
        .unwrap();

    assert!(
        !out.text.contains("TOP-SECRET"),
        "a re-read must not reach outside the root, got:\n{}",
        out.text
    );
    assert!(
        out.text.contains("invalidated by the write at step 2"),
        "the stub must still name the invalidating step, got:\n{}",
        out.text
    );
    let rows = store.context_events(1).unwrap();
    assert!(
        rows.iter().any(|r| r.kind == "reread_refused"),
        "the refused re-read must be in the trace, got {rows:?}"
    );
}

// -------------------------------------------------- the tree loop is bounded too

/// T05 — a sub-agent runs the same assembler as the workspace loop. 0.5.0 spawns
/// up to a hundred children, each of which kept its own unbounded log, so an
/// unbounded tree loop is the multiplied version of the problem this release
/// exists to fix.
#[tokio::test]
async fn a_sub_agent_loops_prompt_stays_inside_the_ceiling() {
    use io_harness::{run_tree, Containment};

    let dir = ws();
    for i in 0..6 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            "z".repeat(6_000) + "\nneedle\n",
        )
        .unwrap();
    }

    // Every turn reads a different large file, so the log outgrows the ceiling.
    let script = MockScript::new(
        (0..8)
            .map(|i| {
                vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": format!("f{}.txt", i % 6) }),
                }]
            })
            .collect(),
    );
    let contract = never_passes(dir.path(), 8).with_context_budget(ContextBudget {
        max_tokens: 1_000,
        share: 0.5,
    });

    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(4, 2, 2, 1_000_000),
    )
    .await
    .unwrap();

    let last = section(&{
        let seen = script.seen.lock().unwrap();
        seen.last().unwrap().user.clone()
    });
    assert!(
        script.turns() >= 4,
        "the tree loop must have run several turns, got {}",
        script.turns()
    );
    assert!(
        estimate_tokens(&last) <= 1_400,
        "a sub-agent's assembled section must stay inside its ceiling, got {} est. tokens",
        estimate_tokens(&last)
    );
    // And the trace still holds every read in full, one row per step.
    let raw: String = store
        .steps(result.run_id)
        .unwrap()
        .iter()
        .map(|s| s.result.as_str())
        .collect();
    assert!(
        raw.chars().count() > 9_000,
        "the tree loop's trace must keep the whole log (got {} chars)",
        raw.chars().count()
    );
}
