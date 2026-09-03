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
    assemble, entry_cap_chars, estimate_tokens, Assembled, Assembly, Collapse, Compaction,
    ContextBudget, Ledger, ObsKind, Observation, Origin, Piece,
};
use io_harness::provider::{CompletionRequest, CompletionResponse, Message, ToolCall};
use io_harness::tools::{Tool, ToolFuture, Toolbox, Workspace};
use io_harness::{
    run_with, ApproveAll, McpServer, MemoryEntry, MemoryKind, Policy, Provider, StallPolicy, Store,
    TaskContract, ToolSpec, Verification,
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

    /// The role-tagged conversation of the `n`th request (0-based), 0.64.0.
    ///
    /// The other half of what a turn sends. `observations` above reads the flat
    /// `user` string, which is the half a resumed run has always got right.
    fn messages(&self, n: usize) -> Vec<io_harness::Message> {
        let seen = self.seen.lock().unwrap();
        seen.get(n)
            .unwrap_or_else(|| panic!("the loop ran only {} turn(s), wanted turn {n}", seen.len()))
            .messages
            .clone()
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
    TaskContract::workspace("exercise the context assembler", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
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
    // Elision, not compaction: this is 0.13.0's claim and the budget is tight
    // enough that 0.43.0's fold would fire and add a completion of its own, which
    // a positional script cannot see coming. That the *fold* also stabilises the
    // prompt is asserted in `tests/compaction.rs`, against its own provider.
    let contract = never_passes(dir.path(), 21)
        .with_context_budget(tight(1_000))
        .with_compaction(Compaction {
            at_share: 1.0,
            ..Compaction::default()
        });
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
        Origin::File,
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Write,
        Some("notes.txt".into()),
        "\n[wrote notes.txt] (12 chars)\n",
        Origin::File,
    ));
    let policy = Policy::default()
        .layer("test")
        .allow_read("*")
        .deny_read("notes.txt");

    let ws = Workspace::with_policy(dir.path(), policy.clone());
    let out = assemble(
        &ledger,
        24_000,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&ws),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 3,
        },
    )
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

    // 0.55.0 — the read is the one kind that is no longer bounded here. It used
    // to keep its tail, on the reasoning that the end of a file is what a writer
    // needs; what the model then held had the shape of a whole file. It is now
    // refused whole, which is what the rest of this test's kinds are the control
    // for: a command's output and a search's matches are still cut.
    let read = provider.observations(1);
    assert!(
        !read.contains("TAIL-OF-FILE"),
        "a read that will not fit carries none of the file:\n{read}"
    );
    assert!(
        read.contains("[read big.txt error]") && read.contains("nothing was read"),
        "and says so, with the size and the ceiling:\n{read}"
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
        "[read big.txt error]",
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
            Origin::File,
        ));
    }

    const TURNS: u32 = 50;
    let started = Instant::now();
    for step in 0..TURNS {
        let out = assemble(
            &ledger,
            24_000,
            &[],
            &[],
            Assembly {
                collapse: Collapse::default(),
                ws: Some(&workspace),
                policy: &policy,
                store: &store,
                run_id: 1,
                step,
            },
        )
        .await
        .unwrap();
        assert!(out.carried > 0);
        // 0.76.0 — the structural property the duration below used to stand in
        // for, and the one worth gating a merge on: assembly is bounded by the
        // turn's budget rather than by the ledger's length. A 200-entry log of
        // 1,000 chars each is ~50,000 tokens of raw observation against a 24,000
        // budget, so a projection that grew with the ledger would exceed it here.
        assert!(
            out.est_tokens <= 24_000,
            "a 200-entry ledger assembled to {} tokens against a 24,000 budget: the section is \
             tracking the log's length rather than the turn's budget",
            out.est_tokens
        );
    }
    let per_turn = started.elapsed() / TURNS;

    // 0.76.0 — printed, never asserted. A wall-clock threshold on a shared CI
    // runner fails for the runner's reasons and says nothing about the code, and
    // this repository already applies exactly that rule to every other duration it
    // records: timing goes to `docs/MEASUREMENTS.md` with a machine beside it, and
    // the merge gate is the structural assertion above.
    println!("assembling a 200-entry log: {per_turn:?} per turn");
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
        Origin::Tool,
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Tool,
        Some("weather".into()),
        "\n[weather]\nCAIRO-SUN\n",
        Origin::Tool,
    ));
    ledger.push(Observation::new(
        3,
        ObsKind::Read,
        Some("a.txt".into()),
        "\n[read a.txt]\nFIRST\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        4,
        ObsKind::Read,
        Some("a.txt".into()),
        "\n[read a.txt]\nSECOND\n",
        Origin::File,
    ));

    let out = assemble(
        &ledger,
        24_000,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&workspace),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 5,
        },
    )
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
        Origin::File,
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Write,
        Some("../secret.txt".into()),
        "\n[wrote ../secret.txt] (3 chars)\n",
        Origin::File,
    ));

    let out = assemble(
        &ledger,
        24_000,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&workspace),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 3,
        },
    )
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

// ------------------------------------------- the memory block is replay-stable

/// The prompt a case produces must not depend on how many runs the store has
/// held. `MemoryEntry::run_id` is the store's `AUTOINCREMENT` row id, so the
/// second run of one case over one workspace carries notes attributed to run 2
/// where the first carried run 1 — and the rendered block goes into the request
/// *and* into `steps.prompt`. Rendering it made byte-identical replay impossible,
/// so the block must be a function of the notes' content alone.
#[tokio::test]
async fn the_rendered_note_block_is_byte_identical_whatever_run_id_the_notes_carry() {
    let store = Store::memory().unwrap();
    let policy = open_policy();

    let notes = |run_id: i64| {
        vec![
            MemoryEntry {
                key: "build-command".into(),
                value: "cargo test --workspace".into(),
                run_id,
                step: 3,
                created_at: "2026-01-01T00:00:00Z".into(),
                // 0.30.0 added these two. A `MemoryEntry` is a row this crate
                // returns and nothing takes, so naming them here is the whole
                // cost of the addition to a caller that constructs one.
                kind: MemoryKind::Fact,
                pinned: false,
            },
            MemoryEntry {
                key: "api-base".into(),
                value: "http://localhost:1".into(),
                run_id: run_id + 40,
                step: 7,
                created_at: "2026-01-02T00:00:00Z".into(),
                kind: MemoryKind::Fact,
                pinned: false,
            },
        ]
    };

    // An empty ledger, so the assembled text is the memory block and nothing else.
    let render = |run_id: i64| {
        let notes = notes(run_id);
        let store = &store;
        let policy = &policy;
        async move {
            assemble(
                &Ledger::new(),
                24_000,
                &notes,
                &[],
                Assembly {
                    collapse: Collapse::default(),
                    ws: None,
                    policy,
                    store,
                    run_id: 1,
                    step: 9,
                },
            )
            .await
            .unwrap()
        }
    };

    let first = render(1).await;
    let second = render(2).await;
    let far = render(9_999).await;

    assert_eq!(
        first.text, second.text,
        "run 2 of the same case must send the same bytes as run 1"
    );
    assert_eq!(first.text, far.text, "and so must run 10,000");
    assert_eq!(first.est_tokens, far.est_tokens);
    assert_eq!((first.recalled, far.recalled), (2, 2));

    // Pin the format, so "byte-identical" cannot be satisfied by rendering less.
    assert!(
        first
            .text
            .contains("- build-command: cargo test --workspace  (step 3)\n")
            && first
                .text
                .contains("- api-base: http://localhost:1  (step 7)\n"),
        "got:\n{}",
        first.text
    );
    assert!(
        !first.text.contains("run "),
        "no note may name a run, got:\n{}",
        first.text
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
    // 0.55.0 — sized to fit under the per-read ceiling (`entry_cap_chars` floors
    // at 2,000) rather than over it. What this test is about is a log that
    // outgrows the assembled ceiling across eight turns while the trace keeps all
    // of it; a file no single read could carry would now be refused, which is a
    // different claim and F9's.
    for i in 0..6 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            "z".repeat(1_500) + "\nneedle\n",
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

// -------------------------------------------------- the ceiling holds on a long run

/// F1/F2 — stub lines grow with a run's LENGTH rather than with what it observed,
/// so on a long run they would exceed the ceiling one elision at a time. Past a
/// slice of the budget they collapse into a single line. The live 0.10.0 run that
/// found this had reached 2,264 estimated tokens against a 1,500-token ceiling by
/// step 20, purely in stubs.
#[tokio::test]
async fn a_long_runs_stubs_collapse_so_the_ceiling_still_holds() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = open_policy();
    let workspace = Workspace::with_policy(dir.path(), policy.clone());

    // 400 observations of 40 subjects: nearly all superseded, so nearly all stubs.
    let mut ledger = Ledger::new();
    for i in 0..400u32 {
        ledger.push(Observation::new(
            i + 1,
            ObsKind::Grep,
            Some(format!("pattern-{}", i % 40)),
            format!("\n[grep \"pattern-{}\"]\n{}\n", i % 40, "m".repeat(200)),
            Origin::File,
        ));
    }

    const CEILING: u64 = 1_500;
    let out = assemble(
        &ledger,
        CEILING,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&workspace),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 401,
        },
    )
    .await
    .unwrap();

    assert!(out.stubbed > 300, "the fixture must be mostly stubs");
    assert!(
        out.collapsed,
        "past its slice of the budget the stub block must collapse"
    );
    assert!(
        out.est_tokens <= CEILING,
        "the ceiling must hold on a long run, got {} est. tokens",
        out.est_tokens
    );
    assert!(
        out.text.contains("earlier observation(s) elided"),
        "the collapse must say how many it stands for, got:\n{}",
        out.text
    );
    // The row says so too, so a trace reader can see why the section is short.
    let rows = store.context_events(1).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.detail.as_deref().unwrap_or("").contains("collapsed=true")),
        "the assembled row must record the collapse, got {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// 0.13.0 — the ledger survives an interruption.
//
// Through 0.12.0 the ledger lived only in memory, built fresh at the top of the
// workspace and sub-agent loops after the resume step was already known, so a
// resumed run re-derived its context from the workspace and asked the model a
// different question than the process before it would have. Recorded as a
// limitation in iterations/US-IO-HARNESS-0.12.0-I01.
// ---------------------------------------------------------------------------

/// The assertion the whole ledger half turns on: the same work, interrupted or
/// not, sends the model the same thing. Compared on the observation section of
/// the prompt — the only place the in-memory ledger is observable from outside —
/// because the durable rows alone cannot tell a restored ledger from a re-derived
/// one. This test fails if the restore is removed; the durable-row test below
/// does not, which is why both exist.
#[tokio::test]
async fn a_resumed_run_asks_the_model_what_an_uninterrupted_run_would_have() {
    let script = || {
        MockScript::new(vec![
            vec![call("read_file", json!({ "path": "a.txt" }))],
            vec![call(
                "write_file",
                json!({ "path": "b.txt", "content": "b" }),
            )],
            vec![call("read_file", json!({ "path": "b.txt" }))],
        ])
    };

    // Uninterrupted: three steps in one process.
    let whole_dir = ws();
    std::fs::write(whole_dir.path().join("a.txt"), "hello").unwrap();
    let whole_store = Store::memory().unwrap();
    let whole_provider = script();
    let whole = (
        run_with(
            &never_passes(whole_dir.path(), 3),
            &whole_provider,
            &whole_store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap(),
        whole_provider,
    );

    // Interrupted after one step, then resumed for the other two.
    let cut_dir = ws();
    std::fs::write(cut_dir.path().join("a.txt"), "hello").unwrap();
    let cut_store = Store::memory().unwrap();
    let provider = script();
    let first = run_with(
        &never_passes(cut_dir.path(), 1),
        &provider,
        &cut_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    io_harness::resume_with(
        &never_passes(cut_dir.path(), 3),
        &provider,
        &cut_store,
        first.run_id,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // Turn 3 of the whole run is turn 2 of the resumed provider: both are the
    // step that has two steps of history behind it.
    let uninterrupted = whole.1.observations(2);
    let resumed = provider.observations(2);
    assert!(
        uninterrupted.contains("hello"),
        "the fixture must carry step one's read into the later prompt, or this \
         proves nothing: {uninterrupted}"
    );
    assert_eq!(
        resumed, uninterrupted,
        "a resumed run assembles from the ledger it had, not from the workspace"
    );

    // And the durable rows are written once, not once per resume: the watermark
    // must not replay what the restore just read back.
    let rows = cut_store.observations(first.run_id).unwrap();
    assert_eq!(
        rows,
        whole_store.observations(whole.0.run_id).unwrap(),
        "the interruption leaves the same durable ledger, with nothing duplicated"
    );
}

/// F1 (0.64.0) — and the same is now true of the conversation, not only of the
/// prose.
///
/// The twin of the test above, and deliberately its twin: same fixture, same
/// interruption, same "turn 3 of the whole run is turn 2 of the resumed
/// provider" alignment. That one compares the observation section, which a
/// resumed run has assembled correctly since 0.13.0. This one compares
/// `messages`, which until this release a resumed run did not send at all —
/// every pre-crash step collapsed into user prose because the assistant turns
/// that pair with those results were held in memory and died with the process.
///
/// **Nothing is normalised.** Not a field, not an id, not an ordering. Tool-call
/// ids are minted from position inside each request and never stored, timestamps
/// do not appear in `messages`, and row ids do not either — so there is nothing
/// here that legitimately differs between the two runs, and a comparison that
/// needed a `retain` would be evidence about the fix rather than about the run.
#[tokio::test]
async fn a_resumed_run_sends_the_conversation_an_uninterrupted_run_would_have() {
    let script = || {
        MockScript::new(vec![
            vec![call("read_file", json!({ "path": "a.txt" }))],
            vec![call(
                "write_file",
                json!({ "path": "b.txt", "content": "b" }),
            )],
            vec![call("read_file", json!({ "path": "b.txt" }))],
        ])
    };

    let whole_dir = ws();
    std::fs::write(whole_dir.path().join("a.txt"), "hello").unwrap();
    let whole_store = Store::memory().unwrap();
    let whole_provider = script();
    run_with(
        &never_passes(whole_dir.path(), 3),
        &whole_provider,
        &whole_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let cut_dir = ws();
    std::fs::write(cut_dir.path().join("a.txt"), "hello").unwrap();
    let cut_store = Store::memory().unwrap();
    let provider = script();
    let first = run_with(
        &never_passes(cut_dir.path(), 1),
        &provider,
        &cut_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    io_harness::resume_with(
        &never_passes(cut_dir.path(), 3),
        &provider,
        &cut_store,
        first.run_id,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let uninterrupted = whole_provider.messages(2);
    let resumed = provider.messages(2);

    // The fixture has to be one this can fail on. A turn with no assistant
    // message in it would compare two empty transcripts and pass forever.
    assert!(
        uninterrupted
            .iter()
            .any(|m| matches!(m, io_harness::Message::Assistant { .. })),
        "the uninterrupted run must send assistant turns, or this proves nothing: \
         {uninterrupted:?}"
    );
    assert!(
        uninterrupted
            .iter()
            .any(|m| matches!(m, io_harness::Message::Results(_))),
        "and result batches: {uninterrupted:?}"
    );

    assert_eq!(
        resumed, uninterrupted,
        "a resumed run sends the same roles, the same assistant turns and the same \
         result batches the uninterrupted run sent"
    );
}

/// F3 (0.64.0) — a result that survives an elision keeps the position of the
/// call it answers, even when its own step's other result did not survive.
///
/// **This is the assertion the end-to-end test could not make, and a sabotage is
/// what showed that.** Counting ordinals only over the results a turn *carries*
/// leaves every run-level test green: a step's results are elided together, so
/// within a carried step the positions come out the same either way. The defect
/// needs one step whose results straddle the boundary — and the way to get one is
/// to supersede a single read rather than to squeeze a budget.
///
/// Step 1 reads `a.txt` and then `b.txt`; step 2 writes `a.txt`, which
/// invalidates the first read and stubs it. The surviving result is still the
/// **second** call of step 1, and saying it is the first would answer the wrong
/// call at the vendor.
#[tokio::test]
async fn a_surviving_result_keeps_the_position_of_the_call_it_answers() {
    let dir = ws();
    let store = Store::memory().unwrap();
    std::fs::write(dir.path().join("a.txt"), "NEW-CONTENT").unwrap();

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        1,
        ObsKind::Read,
        Some("a.txt".into()),
        "\n[read a.txt]\nOLD-A\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        1,
        ObsKind::Read,
        Some("b.txt".into()),
        "\n[read b.txt]\nB-CONTENT\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        2,
        ObsKind::Write,
        Some("a.txt".into()),
        "\n[wrote a.txt] (11 chars)\n",
        Origin::File,
    ));

    let policy = open_policy().deny_read("a.txt");
    let workspace = Workspace::with_policy(dir.path(), policy.clone());
    let out = assemble(
        &ledger,
        24_000,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&workspace),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 3,
        },
    )
    .await
    .unwrap();

    // The fixture must actually straddle: one of step 1's two results carried,
    // the other not. Without that this asserts nothing about ordinals.
    assert!(
        !out.text.contains("OLD-A"),
        "the superseded read must be stubbed, got:\n{}",
        out.text
    );
    assert!(
        out.text.contains("B-CONTENT"),
        "and the other read of the same step must be carried, got:\n{}",
        out.text
    );

    let step_one: Vec<&Piece> = out
        .emitted
        .iter()
        .filter(|e| e.step == 1)
        .map(|e| &e.piece)
        .collect();
    assert_eq!(
        step_one.len(),
        2,
        "both of step 1's entries are emitted, the stub included: {:?}",
        out.emitted
    );

    let surviving = out
        .emitted
        .iter()
        .find(|e| e.text.contains("B-CONTENT"))
        .expect("the carried read is emitted");
    assert_eq!(
        surviving.ordinal, 1,
        "the surviving result answers the SECOND call of step 1; calling it the first \
         would answer the wrong call: {:?}",
        out.emitted
    );
}

/// F3 (0.64.0) — and it still holds once the older observations are elided.
///
/// This is the case a fix that only handles the happy path gets wrong. What a
/// turn carries is decided per turn against the context budget: older entries
/// are replaced by one-line stubs, and past a ceiling they collapse into a
/// single line. Ordinals are counted over **every** result of a step whether or
/// not the turn carries it (`src/context.rs`), so a result that survives the
/// elision still names the position of the call it answers — but only if the
/// calls are there to be named. A resumed run under a tight budget is where both
/// halves have to be true at once.
///
/// The fixture asserts that elision actually happened before comparing anything.
/// Under a budget nothing exceeds, this would be the flat test again wearing a
/// different name.
///
/// **Two fixture properties this test cannot do without, and a sabotage found
/// both.** The first version scripted one call per step, so every ordinal was 0
/// and no ordinal defect could show; steps here issue two calls each. And the
/// equality comparison alone is blind to a *systematic* ordinal error, because
/// both arms run the same code and would be wrong together — so the correlation
/// is also asserted absolutely, by reading each result's content back against the
/// path named by the call it says it answers.
#[tokio::test]
async fn a_resumed_run_under_a_tight_budget_still_pairs_every_result_with_its_call() {
    // Sized like the ceiling test at the top of this file: each file is under the
    // per-read cap so the read succeeds, and four of them together are well over
    // the assembly budget so the older ones stub.
    let filler = format!("{}TAIL\n", "filler line\n".repeat(150));
    // Two calls per step, so ordinals 0 and 1 both exist. With one call per step
    // every ordinal is 0 and an ordinal that is counted wrongly still reads right.
    let script = || {
        MockScript::new(vec![
            vec![
                call("read_file", json!({ "path": "f0.txt" })),
                call("read_file", json!({ "path": "f1.txt" })),
            ],
            vec![
                call("read_file", json!({ "path": "f2.txt" })),
                call("read_file", json!({ "path": "f3.txt" })),
            ],
            vec![
                call("read_file", json!({ "path": "f4.txt" })),
                call("read_file", json!({ "path": "f5.txt" })),
            ],
            vec![
                call("read_file", json!({ "path": "f6.txt" })),
                call("read_file", json!({ "path": "f7.txt" })),
            ],
        ])
    };
    let seed = |dir: &std::path::Path| {
        for i in 0..8 {
            // Each file names itself, so a result can be checked against the call
            // it claims to answer rather than only against the other run.
            std::fs::write(
                dir.join(format!("f{i}.txt")),
                format!("MARKER-f{i}\n{filler}"),
            )
            .unwrap();
        }
    };
    let tight_contract = |dir: &std::path::Path, steps: u32| {
        never_passes(dir, steps).with_context_budget(tight(1_000))
    };

    let whole_dir = ws();
    seed(whole_dir.path());
    let whole_store = Store::memory().unwrap();
    let whole_provider = script();
    run_with(
        &tight_contract(whole_dir.path(), 4),
        &whole_provider,
        &whole_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The fixture must actually elide, or this asserts nothing about elision.
    let last = whole_provider.observations(3);
    assert!(
        last.contains("(elided:") || last.contains("earlier observation(s) elided"),
        "the budget must be tight enough to stub or collapse, got {last}"
    );

    let cut_dir = ws();
    seed(cut_dir.path());
    let cut_store = Store::memory().unwrap();
    let provider = script();
    let first = run_with(
        &tight_contract(cut_dir.path(), 1),
        &provider,
        &cut_store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    io_harness::resume_with(
        &tight_contract(cut_dir.path(), 4),
        &provider,
        &cut_store,
        first.run_id,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let uninterrupted = whole_provider.messages(3);
    let resumed = provider.messages(3);
    assert_eq!(
        resumed, uninterrupted,
        "an elided history role-tags the same way whether or not the process died"
    );

    // And the pairing itself, absolutely rather than by comparison: every result
    // names a call the turn before it actually made, AND carries that call's own
    // answer. Equality alone cannot see an ordinal counted wrongly in both arms.
    let mut pairs = 0;
    let mut checked = 0;
    for window in resumed.windows(2) {
        if let [Message::Assistant { calls, .. }, Message::Results(results)] = window {
            pairs += 1;
            for r in results {
                assert!(
                    r.call < calls.len(),
                    "a result names call {} of a turn that made {}: {resumed:?}",
                    r.call,
                    calls.len()
                );
                let path = calls[r.call]
                    .arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .expect("every scripted call names a path");
                let marker = format!("MARKER-{}", path.trim_end_matches(".txt"));
                // A carried result holds the file it read; an elided one is a stub
                // naming the read it stands in for. Either way it must be about
                // the call it says it answers.
                if r.content.contains("MARKER-") || r.content.contains(".txt") {
                    checked += 1;
                    assert!(
                        r.content.contains(&marker) || r.content.contains(path),
                        "the result naming call {} of {:?} carries {:?}",
                        r.call,
                        calls[r.call].arguments,
                        r.content
                    );
                }
            }
        }
    }
    assert!(
        pairs > 0,
        "the resumed transcript must contain at least one paired turn: {resumed:?}"
    );
    assert!(
        checked >= 2,
        "at least two results must have been checked against the call they name, or the \
         correlation assertion is decorative: {resumed:?}"
    );
}

/// The restore is a restore, not a re-derivation: what step one observed is in
/// the prompt step two is sent, across the process boundary. Asserted on the
/// provider's own received requests, which is the only place it is observable.
#[tokio::test]
async fn what_the_run_observed_before_the_interruption_is_in_the_prompt_after_it() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "the-observation-from-step-one").unwrap();
    let store = Store::memory().unwrap();

    let before = MockScript::new(vec![vec![call("read_file", json!({ "path": "a.txt" }))]]);
    let first = run_with(
        &never_passes(dir.path(), 1),
        &before,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let after = MockScript::new(vec![vec![call("read_file", json!({ "path": "a.txt" }))]]);
    io_harness::resume_with(
        &never_passes(dir.path(), 2),
        &after,
        &store,
        first.run_id,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        after.turns(),
        1,
        "the resume drove exactly the remaining step"
    );
    assert!(
        after
            .observations(0)
            .contains("the-observation-from-step-one"),
        "the resumed run carried its earlier observation into the prompt, got: {}",
        after.observations(0)
    );
}

// ------------------------------------------------- 0.49.0: the emitted pieces

/// Assemble a ledger with an open policy and no notes, at `step`.
async fn emitted_for(ledger: &Ledger, budget: u64) -> Assembled {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("s.db")).unwrap();
    let policy = open_policy();
    let ws = Workspace::with_policy(dir.path(), policy.clone());
    assemble(
        ledger,
        budget,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: Some(&ws),
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 9,
        },
    )
    .await
    .unwrap()
}

/// Every caller of this helper builds a read, a grep or a write, so it states the
/// origin those share rather than taking a sixth argument nothing would vary. A
/// test that needs a different one builds the `Observation` itself — which is the
/// point of there being no defaulting constructor.
fn observed(step: u32, kind: ObsKind, target: &str, text: &str) -> Observation {
    Observation::new(step, kind, Some(target.to_string()), text, Origin::File)
}

/// The transcript and the flat string are two renderings of ONE emission.
///
/// This is what makes the derived `user` assertable at all: if the pieces did not
/// concatenate to exactly the text, the conversation the model receives and the
/// shim a provider reads would be two different accounts of the same run.
#[tokio::test]
async fn the_emitted_pieces_reconstruct_the_assembled_text_exactly() {
    let mut ledger = Ledger::new();
    ledger.push(observed(1, ObsKind::Read, "a.txt", "\n[read a.txt]\nAAA\n"));
    ledger.push(observed(
        1,
        ObsKind::Grep,
        "todo",
        "\n[grep todo]\nno hits\n",
    ));
    ledger.push(Observation::new(
        1,
        ObsKind::Message,
        None,
        "\n[note] the model said something\n",
        Origin::Prose,
    ));
    ledger.push(observed(2, ObsKind::Read, "b.txt", "\n[read b.txt]\nBBB\n"));

    let out = emitted_for(&ledger, 24_000).await;
    let rebuilt: String = out.emitted.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(
        rebuilt, out.text,
        "the pieces must concatenate to the assembled text byte for byte"
    );
}

/// A result's ordinal is the index of the call it answers, counted over every
/// result of its step — and prose is not a result.
#[tokio::test]
async fn a_results_ordinal_is_the_index_of_the_call_it_answers() {
    let mut ledger = Ledger::new();
    ledger.push(observed(1, ObsKind::Read, "a.txt", "\n[read a.txt]\nAAA\n"));
    ledger.push(observed(
        1,
        ObsKind::Grep,
        "todo",
        "\n[grep todo]\nno hits\n",
    ));
    ledger.push(Observation::new(
        1,
        ObsKind::Message,
        None,
        "\n[note] prose\n",
        Origin::Prose,
    ));
    ledger.push(observed(2, ObsKind::Read, "b.txt", "\n[read b.txt]\nBBB\n"));

    let out = emitted_for(&ledger, 24_000).await;
    let shape: Vec<(u32, usize, bool)> = out
        .emitted
        .iter()
        .map(|e| (e.step, e.ordinal, e.piece == Piece::Result))
        .collect();
    assert_eq!(
        shape,
        vec![(1, 0, true), (1, 1, true), (1, 0, false), (2, 0, true)],
        "step 1's two results are calls 0 and 1, the note answers no call, and \
         step 2 starts counting again"
    );
}

/// **The elision case, and it is the one that matters.** A stubbed result still
/// occupies its call's position, so the results that survive still answer the
/// calls they actually answered.
///
/// Dropping the elided ones from the count would slide every later result up by
/// one — a transcript in which the model reads the grep's output as the answer to
/// its read. Nothing about that failure is visible in the assembled text, which is
/// why it is asserted here rather than left to a body test.
#[tokio::test]
async fn an_elided_result_keeps_its_calls_position() {
    let mut ledger = Ledger::new();
    // Two reads of the same path: the first is superseded by the second and is
    // elided, while the grep between them is carried.
    ledger.push(observed(1, ObsKind::Read, "a.txt", "\n[read a.txt]\nOLD\n"));
    ledger.push(observed(1, ObsKind::Grep, "todo", "\n[grep todo]\nhit\n"));
    ledger.push(observed(1, ObsKind::Read, "a.txt", "\n[read a.txt]\nNEW\n"));

    let out = emitted_for(&ledger, 24_000).await;
    let results: Vec<(usize, bool)> = out
        .emitted
        .iter()
        .filter(|e| e.piece == Piece::Result)
        .map(|e| (e.ordinal, e.text.contains("elided")))
        .collect();
    assert_eq!(
        results,
        vec![(0, true), (1, false), (2, false)],
        "the superseded read is elided IN PLACE at call 0, and the grep stays at \
         call 1 rather than sliding up: {:#?}",
        out.emitted
    );
}

// ------------------------------------------ 0.49.0: the transcript on the wire

/// Every request the mock was sent, whole.
fn requests(mock: &MockScript) -> Vec<CompletionRequest> {
    mock.seen.lock().unwrap().clone()
}

/// **F1** — a multi-step run sends a role-tagged transcript, and the shim beside it
/// is the string 0.48.0 sent.
///
/// The first request carries no transcript at all: there is nothing to tell the
/// model about yet, and sending a one-message conversation would be the flat
/// request said twice.
#[tokio::test]
async fn a_multi_step_run_sends_a_role_tagged_transcript() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "AAA\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "BBB\n").unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "a.txt" }))],
        vec![call("read_file", json!({ "path": "b.txt" }))],
    ]);
    let store = Store::memory().unwrap();
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let seen = requests(&provider);
    assert!(seen.len() >= 3, "the loop ran {} steps", seen.len());
    assert!(
        seen[0].messages.is_empty(),
        "the opening step has nothing to say back: {:#?}",
        seen[0].messages
    );

    // Step 2 knows about step 1: one assistant turn carrying the call it made, and
    // one results batch answering it.
    let m = &seen[1].messages;
    assert!(m.len() >= 3, "expected a conversation, got {m:#?}");
    assert!(matches!(m[0], Message::User(_)));
    match &m[1] {
        Message::Assistant { calls, .. } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "read_file");
            assert_eq!(calls[0].arguments["path"], "a.txt");
        }
        other => panic!("expected the assistant's own turn, got {other:?}"),
    }
    match &m[2] {
        Message::Results(results) => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].call, 0);
            assert!(
                results[0].content.contains("AAA"),
                "the result is the file it read: {}",
                results[0].content
            );
        }
        other => panic!("expected the results of that turn, got {other:?}"),
    }

    // And by step 3 both steps are there, in order.
    let calls: Vec<String> = seen[2]
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { calls, .. } => {
                Some(calls[0].arguments["path"].as_str().unwrap().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec!["a.txt", "b.txt"]);
}

/// **F5** — the derived `user` is the string this build would have sent before the
/// transcript existed, and the transcript is that same string's own pieces.
///
/// Asserted as a reconstruction rather than against a frozen literal, which is the
/// stronger claim: every byte of the flat prompt is somewhere in the conversation,
/// in the same order, and nothing was invented to put there.
#[tokio::test]
async fn the_derived_user_is_the_flat_prompt_the_transcript_was_built_from() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "AAA\n").unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "a.txt" }))],
        vec![call("read_file", json!({ "path": "a.txt" }))],
    ]);
    let store = Store::memory().unwrap();
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let seen = requests(&provider);
    let conversations = seen.iter().filter(|r| !r.messages.is_empty()).count();
    assert!(
        conversations > 0,
        "at least one request must carry a transcript, or this test passes vacuously"
    );
    for req in seen.iter().filter(|r| !r.messages.is_empty()) {
        let rebuilt: String = req
            .messages
            .iter()
            .map(|m| match m {
                Message::User(text) => text.clone(),
                Message::Assistant { .. } => String::new(),
                Message::Results(results) => results.iter().map(|r| r.content.as_str()).collect(),
            })
            .collect();
        assert_eq!(
            rebuilt, req.user,
            "the conversation's text must be the derived `user`, byte for byte"
        );
        assert!(
            req.user.contains("Call a tool to make progress"),
            "and the derived `user` is still the whole workspace prompt"
        );
    }
}

/// **F10** — a step whose completion carried several calls becomes ONE assistant
/// turn and ONE results batch, correlated pairwise in the model's call order.
#[tokio::test]
async fn parallel_calls_are_one_turn_and_one_batch_in_call_order() {
    let dir = ws();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.path().join(name), format!("CONTENT-{name}\n")).unwrap();
    }
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![vec![
        call("read_file", json!({ "path": "a.txt" })),
        call("read_file", json!({ "path": "b.txt" })),
        call("read_file", json!({ "path": "c.txt" })),
    ]]);
    let store = Store::memory().unwrap();
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let seen = requests(&provider);
    let m = &seen[1].messages;
    let assistants = m
        .iter()
        .filter(|x| matches!(x, Message::Assistant { .. }))
        .count();
    let batches = m
        .iter()
        .filter(|x| matches!(x, Message::Results(_)))
        .count();
    assert_eq!((assistants, batches), (1, 1), "one turn, one batch: {m:#?}");

    let Some(Message::Assistant { calls, .. }) =
        m.iter().find(|x| matches!(x, Message::Assistant { .. }))
    else {
        unreachable!()
    };
    let Some(Message::Results(results)) = m.iter().find(|x| matches!(x, Message::Results(_)))
    else {
        unreachable!()
    };
    assert_eq!(calls.len(), 3);
    assert_eq!(results.len(), 3);
    for (i, result) in results.iter().enumerate() {
        assert_eq!(result.call, i, "results are in the model's call order");
        let path = calls[i].arguments["path"].as_str().unwrap();
        let name = path.trim_end_matches(".txt");
        assert!(
            result.content.contains(&format!("CONTENT-{name}")),
            "result {i} must answer call {i} ({path}), got: {}",
            result.content
        );
    }
}

// ---------------------------------------------------------------------------
// F12 (0.55.0) — a stored read is whole in the prompt or a stub, never a
// fragment; every other kind is still bounded
// ---------------------------------------------------------------------------

/// A stored read squeezed out by a narrower budget is replaced by a stub that
/// says how to get the part that matters, and carries none of the file.
///
/// The three sentinels are the assertion: "no content" has to mean the middle
/// and the tail too, and the tail is the half that used to survive — `bound`
/// keeps the *end* of a read, on the reasoning that the end of a file is what a
/// writer needs. A tail under a header saying the file was read is the fragment
/// this release stops serving.
#[tokio::test]
async fn a_read_that_no_longer_fits_is_a_stub_and_not_a_tail() {
    let store = Store::memory().unwrap();
    let policy = Policy::permissive();
    let filler = "x".repeat(4_000);
    let mut ledger = Ledger::default();
    ledger.push(Observation::new(
        1,
        ObsKind::Read,
        Some("src/lib.rs".into()),
        format!("\n[read src/lib.rs]\nHEAD-SENTINEL\n{filler}\nMIDDLE-SENTINEL\n{filler}\nTAIL-SENTINEL\n"),
        Origin::File,
    ));
    // Something newer, so the older read is the one that loses the fit.
    ledger.push(Observation::new(
        2,
        ObsKind::Grep,
        Some("needle".into()),
        "\n[grep \"needle\"]\nsrc/other.rs:4: needle\n",
        Origin::File,
    ));

    let out = assemble(
        &ledger,
        // A ceiling too small to carry the read whole, and large enough to carry
        // the newer entry — which is exactly the squeeze the rule is about.
        200,
        &[],
        &[],
        Assembly {
            collapse: Collapse::default(),
            ws: None,
            policy: &policy,
            store: &store,
            run_id: 1,
            step: 3,
        },
    )
    .await
    .unwrap();

    for sentinel in ["HEAD-SENTINEL", "MIDDLE-SENTINEL", "TAIL-SENTINEL"] {
        assert!(
            !out.text.contains(sentinel),
            "a stubbed read carries none of the file, and {sentinel} is in it:\n{}",
            out.text
        );
    }
    assert!(
        out.text.contains("read src/lib.rs") && out.text.contains("offset"),
        "the stub names the file and the way to get part of it back:\n{}",
        out.text
    );
}

/// The negative half, and the reason the rule is not general: a command's output
/// and a search's matches were never documents, and a prefix of one is not a
/// lie. They are still bounded with the marker they have always had.
#[test]
fn a_search_and_a_command_are_still_bounded_rather_than_refused() {
    let long = "m".repeat(500);
    for kind in [ObsKind::Grep, ObsKind::Tool] {
        let bounded = io_harness::context::bound(&long, 100, kind);
        assert!(
            bounded.contains("truncated") && bounded.len() < long.len() + 200,
            "{kind:?} is still cut with its marker, got {bounded}"
        );
        assert!(
            bounded.starts_with("mmm"),
            "and it is the head that survives for these kinds: {bounded}"
        );
    }
}

// ------------------------------------------------ 0.77.0: the recorded origin

/// The origin each row carries, keyed by a fragment of the text it holds.
///
/// Read off the store rather than off the prompt, because the origin is the one
/// thing an observation carries that no rendering of it shows: a `[read a.txt]`
/// and a `[shell ...]` look the same distance apart in a prompt whether they were
/// attributed correctly or given one literal between them.
fn origin_holding(store: &Store, run_id: i64, needle: &str) -> Origin {
    store
        .observations(run_id)
        .unwrap()
        .into_iter()
        .find(|o| o.text.contains(needle))
        .unwrap_or_else(|| panic!("no observation holding {needle:?}"))
        .origin
}

/// (0.77.0) Three sources in one step leave three different origins.
///
/// Every dispatched result in the flat loop enters the ledger through a single
/// `Observation::new`, and this is the assertion that the line is a conduit rather
/// than a decision. A funnel that named an origin of its own — however carefully
/// chosen — would mark a file read, a command's output and a server's reply
/// identically, and would still satisfy every assertion this suite made before
/// this release, because nothing else in a prompt distinguishes them.
///
/// One step and not three, deliberately: the three calls share a completion, so
/// they share everything the funnel can see except what the arm handed it.
#[tokio::test]
async fn three_sources_in_one_step_leave_three_different_origins() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "FILE-CONTENT\n").unwrap();
    let contract = never_passes(dir.path(), 1).with_mcp([McpServer::stdio(
        "fix",
        fixture_server().display().to_string(),
    )]);
    // `rustc` for the shell half, for the reason `tests/shell.rs` gives: it is the
    // one binary guaranteed present wherever `cargo test` runs.
    let provider = MockScript::new(vec![vec![
        call("read_file", json!({ "path": "a.txt" })),
        call("shell", json!({ "line": "rustc --version" })),
        call("mcp__fix__echo", json!({ "text": "SERVER-SAID" })),
    ]]);
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let file = origin_holding(&store, result.run_id, "[read a.txt]");
    let shell = origin_holding(&store, result.run_id, "[shell `rustc --version`");
    let mcp = origin_holding(&store, result.run_id, "[mcp__fix__echo]");
    assert_eq!(file, Origin::File, "a read of a path is a file's content");
    assert_eq!(shell, Origin::Shell, "a command's output is a process's");
    assert_eq!(mcp, Origin::Mcp, "a server's reply is that server's");
    // The anti-vacuity half, stated as the difference rather than as three
    // equalities: this is the claim the release is actually making, and it is the
    // one a funnel with an origin literal of its own fails.
    assert!(
        file != shell && shell != mcp && file != mcp,
        "one step's three results must not share an origin: {file:?}, {shell:?}, {mcp:?}"
    );
}

/// (0.77.0) A replan directive is the harness talking, and is never external.
///
/// Asserted through `is_external` as well as on the variant, because the variant
/// is only half of what the recording is for: `is_external` is the one place that
/// decides what gets framed to the model as untrusted, and a sentence this crate
/// wrote about the run's own progress is not content from outside it. The failure
/// this guards against is the plausible one — the directive is an
/// `ObsKind::Message` sitting among tool results, and a site that reached for the
/// nearest external origin would look entirely reasonable in the diff.
#[tokio::test]
async fn a_replan_directive_is_harness_prose_and_never_marked_external() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "FILE-CONTENT\n").unwrap();
    // The same read every step: nothing changes in the workspace and the call
    // signature repeats, which is both halves of the stall signal.
    let repeat = || vec![call("read_file", json!({ "path": "a.txt" }))];
    let contract = never_passes(dir.path(), 4).with_stall_policy(StallPolicy {
        window: 2,
        max_replans: 1,
    });
    let provider = MockScript::new(vec![repeat(), repeat(), repeat(), repeat()]);
    let store = Store::memory().unwrap();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let directive = origin_holding(&store, result.run_id, "[no progress]");
    assert_eq!(directive, Origin::Prose, "the harness wrote every word of it");
    assert!(
        !directive.is_external(),
        "a directive this crate wrote must never be framed as content from outside the run"
    );
    // The control: the same run's reads are external, so the assertion above is
    // about this observation rather than about a build where nothing is.
    assert!(
        origin_holding(&store, result.run_id, "[read a.txt]").is_external(),
        "the reads beside it are external, or the negative above proves nothing"
    );
}
