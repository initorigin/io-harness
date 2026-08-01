//! Writes — and reads back — the fixture stores that `tests/cross_version.rs`
//! uses to prove 0.30.0's additive schema change works in both directions, using
//! a real io-harness 0.29.0 from crates.io.
//!
//! The point of the crate is that the *other* binary is the previous release, not
//! this tree with a 0.29.0 label on it. 0.30.0 adds a table for the memory recall
//! record and a nullable column for the pinned flag; the two claims that need a
//! 0.29.0 binary to be evidence rather than assertion are:
//!
//! * **forwards** — a database this binary wrote opens, reads and resumes under
//!   0.30.0 with nothing to migrate, and
//! * **backwards** — a database 0.30.0 wrote is still read by this binary, which
//!   never queries the new table and never selects the new column.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-29-0 write <output-dir>   # produce the committed fixtures
//! gen-0-29-0 read  <database>     # print what 0.29.0 can see in a store, as JSON
//! ```
//!
//! `write` produces two databases:
//!
//! * `aggregates.sqlite3` — six finished runs with a **known composition**: which
//!   outcome each ended with, how many gate phases failed and which, how many
//!   fallbacks, replans and resumes happened, and three durable memory entries.
//!   Written through the `Store` API directly rather than through the run loop,
//!   because the composition is the specification here and a scripted agent run
//!   would produce a composition nobody chose. 0.30.0's aggregates are asserted
//!   against the literals in `composition`, never against what the accessors say.
//! * `interrupted.sqlite3` — a sub-agent tree stopped mid-flight by its step cap,
//!   with one child finished and one not, left resumable. This is the half of F8
//!   that a table of rows cannot cover: a resume across the release boundary.
//!
//! Every fixture gets a JSON sidecar. `composition` holds what this generator
//! *intended* — the expectations — and `read_back` holds what 0.29.0's own API
//! returned from the finished store, so a 0.30.0 reader can be compared against
//! the previous release's answers rather than against its own.
//!
//! Fully offline and deterministic: scripted provider, no network, no API key,
//! no wall-clock value in any expectation.

use std::path::Path;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_tree, ApproveAll, CheckpointEvent, Containment, ContextEvent, Policy, Provider,
    ProviderCall, RunOutcome, SandboxEvent, Store, TaskContract, Verification,
};
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one
/// useful behaviour — print why and exit non-zero — so there is nothing for a
/// typed error to decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The model every recorded call reports. Fixed rather than absent: a `NULL`
/// `provider_calls.model` and a model that round-tripped are different facts, and
/// only the second proves the column survived.
const MODEL: &str = "fixture-model";

/// The workspace the memory entries are keyed to. Relative-looking and fixed, so
/// nothing about the generating machine reaches the committed fixture.
const WORKSPACE: &str = "/repo";

// ---------- the aggregates fixture ----------

/// One run of the known composition: how it ended, which gate phases failed under
/// it, and which recoveries it recorded.
struct Planned {
    goal: &'static str,
    outcome: &'static str,
    /// Gate phases that failed, in order. `"test-run"`, `"compile"` and
    /// `"criterion-compile"` are the three 0.8.1 named and `verify.rs` still emits.
    gate_failures: &'static [&'static str],
    /// Provider names recorded as having *served* a step after a fallback. One
    /// row each; the run's own provider is not one of them.
    fell_back_to: &'static [&'static str],
    /// How many times the agent was nudged to change approach.
    replans: u32,
    /// How many times the run was resumed from a checkpoint.
    resumes: u32,
}

/// Six runs, chosen so every aggregate 0.30.0 ships discriminates.
///
/// * Outcomes are not uniform — four successes and two different failures — so an
///   accessor that counts runs instead of grouping them disagrees.
/// * Two successes have no gate failure and two do, so "succeeded" and "verified
///   first try" are different numbers. An implementation that conflates them
///   reads 4 where the fixture says 2.
/// * One *failed* run carries a gate failure, so a gate-failure total taken only
///   over successful runs disagrees too.
/// * The phases are deliberately unbalanced (2 / 1 / 1) so a grouping that loses
///   the phase and counts rows cannot accidentally agree.
const PLAN: &[Planned] = &[
    Planned {
        goal: "verified on the first attempt",
        outcome: "success",
        gate_failures: &[],
        fell_back_to: &[],
        replans: 0,
        resumes: 0,
    },
    Planned {
        goal: "verified after two failed test runs",
        outcome: "success",
        gate_failures: &["test-run", "test-run"],
        fell_back_to: &["fixture-secondary"],
        replans: 1,
        resumes: 0,
    },
    Planned {
        goal: "verified after the criterion did not compile",
        outcome: "success",
        gate_failures: &["criterion-compile"],
        fell_back_to: &[],
        replans: 0,
        resumes: 1,
    },
    Planned {
        goal: "ran out of steps with the build broken",
        outcome: "step_cap_reached",
        gate_failures: &["compile"],
        fell_back_to: &["fixture-secondary"],
        replans: 1,
        resumes: 0,
    },
    Planned {
        goal: "went in circles and gave up",
        outcome: "stalled",
        gate_failures: &[],
        fell_back_to: &[],
        replans: 0,
        resumes: 1,
    },
    Planned {
        goal: "verified on the first attempt again",
        outcome: "success",
        gate_failures: &[],
        fell_back_to: &["fixture-secondary"],
        replans: 0,
        resumes: 0,
    },
];

/// The three durable memory entries. 0.30.0 reads these back as entries with no
/// kind recorded and no pinned flag set, which is the whole backwards-compatible
/// half of the memory change: an entry written before the column existed has to
/// arrive as something, and that something must be the default rather than an
/// error.
const MEMORY: &[(&str, &str)] = &[
    ("test-command", "cargo test --features documents"),
    ("build-dir", "target/ is on the external disk"),
    ("owner-decision", "the parser stays in-crate"),
];

/// Write the aggregates fixture and its sidecar.
fn aggregates() -> Res<()> {
    let store = Store::open("aggregates.sqlite3")?;

    let mut run_ids = Vec::new();
    for planned in PLAN {
        let run_id = store.start_run(planned.goal, WORKSPACE)?;

        // One priced call per run, so the store this fixture ships is not empty on
        // the axis 0.18.0 already grouped — a database with outcomes and no spend
        // would not exercise the two side by side.
        store.record_provider_call(
            run_id,
            &ProviderCall {
                step: 1,
                attempt: 1,
                provider: "fixture".into(),
                model: Some(MODEL.into()),
                usage: Some(Usage {
                    prompt_tokens: 1_000,
                    completion_tokens: 100,
                    total_tokens: 1_100,
                    cache_read_tokens: 7,
                    cache_write_tokens: 3,
                    reasoning_tokens: 5,
                    server_tool_requests: 1,
                }),
                latency_ms: 42,
                ttft_ms: Some(11),
                finish_reason: Some("stop".into()),
                failure: None,
            },
        )?;

        for (i, phase) in planned.gate_failures.iter().enumerate() {
            store.record_sandbox_event(&SandboxEvent::gate_phase_failed(
                run_id,
                i as u32 + 1,
                phase,
            ))?;
        }
        for (i, provider) in planned.fell_back_to.iter().enumerate() {
            store.record_context_event(run_id, &ContextEvent::served(i as u32 + 1, *provider))?;
        }
        for i in 0..planned.replans {
            store.record_context_event(
                run_id,
                &ContextEvent::replan(i + 1, "no progress in the last 3 steps"),
            )?;
        }
        for i in 0..planned.resumes {
            store.record_checkpoint_event(&CheckpointEvent::resume(
                run_id,
                i + 1,
                "restarted after a crash",
            ))?;
        }

        store.finish_run(run_id, planned.outcome)?;
        run_ids.push(run_id);
    }

    for (key, value) in MEMORY {
        // Attributed to the first run, which is a real run id in this database —
        // a memory entry pointing at a run that does not exist would be a fixture
        // that cannot be joined.
        store.memory_put(WORKSPACE, key, value, run_ids[0], 3)?;
    }

    // What 0.29.0 itself sees. The forwards direction of F8 compares 0.30.0's
    // answers to these, so the expectation is the previous release's own reading
    // rather than this generator's intent.
    let read_back = json!({
        "memory": store.memory_list(WORKSPACE)?.iter().map(|e| json!({
            "key": e.key,
            "value": e.value,
            "run_id": e.run_id,
            "step": e.step,
        })).collect::<Vec<_>>(),
        "runs": run_ids.iter().map(|id| {
            let summary = store.run_summary(*id)?.expect("every run above finished");
            Ok(json!({
                "run_id": summary.run_id,
                "outcome": summary.outcome,
                "success": summary.success,
                "tokens": summary.tokens,
                "gate_failures": store.sandbox_events(*id)?
                    .iter()
                    .filter(|e| e.kind == "gate_phase_failed")
                    .filter_map(|e| e.detail.clone())
                    .collect::<Vec<_>>(),
                "context_kinds": store.context_events(*id)?
                    .iter()
                    .map(|e| e.kind.clone())
                    .collect::<Vec<_>>(),
                "resumes": store.checkpoint_events(*id)?
                    .iter()
                    .filter(|e| e.kind == "resume")
                    .count(),
            }))
        }).collect::<Res<Vec<Value>>>()?,
    });

    // What the generator *chose*, as literals. F7's expectations come from here:
    // an aggregate checked against what the accessor returned proves nothing.
    let composition = json!({
        "runs": PLAN.len(),
        "succeeded": PLAN.iter().filter(|p| p.outcome == "success").count(),
        "verified_first_try": PLAN
            .iter()
            .filter(|p| p.outcome == "success" && p.gate_failures.is_empty())
            .count(),
        "by_outcome": {
            "success": PLAN.iter().filter(|p| p.outcome == "success").count(),
            "step_cap_reached": PLAN.iter().filter(|p| p.outcome == "step_cap_reached").count(),
            "stalled": PLAN.iter().filter(|p| p.outcome == "stalled").count(),
        },
        "gate_failures_by_phase": {
            "test-run": 2,
            "criterion-compile": 1,
            "compile": 1,
        },
        "recovery": {
            "fallbacks": PLAN.iter().map(|p| p.fell_back_to.len()).sum::<usize>(),
            "replans": PLAN.iter().map(|p| p.replans).sum::<u32>(),
            "resumes": PLAN.iter().map(|p| p.resumes).sum::<u32>(),
        },
        "memory_keys": MEMORY.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "workspace": WORKSPACE,
        "run_ids": run_ids,
    });

    sidecar(
        "aggregates.json",
        &json!({ "composition": composition, "read_back": read_back }),
    )?;
    drop(store);
    Ok(())
}

// ---------- the resumable fixture ----------

/// A stateless tree provider that decides from the prompt, so a resumed or
/// replayed step behaves identically: a COORDINATOR fans out to two children, and
/// a child whose goal reads `FILE=x CONTENT=y` writes `y` into `x`.
///
/// The children are asymmetric on purpose. One is asked for the content its own
/// verification wants and finishes; the other is asked for content that can never
/// satisfy it and runs out of steps. A tree where everything finished is not
/// interrupted, and one where nothing finished does not prove a *completed* child
/// is adopted on resume rather than re-run.
struct TreeProvider;

impl Provider for TreeProvider {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("COORDINATOR") {
            return Ok(CompletionResponse {
                tool_calls: vec![
                    spawn("FILE=a.txt CONTENT=ALPHA", "a.txt", "ALPHA"),
                    // Asked for DRAFT, verified on BETA: this child cannot win.
                    spawn("FILE=b.txt CONTENT=DRAFT", "b.txt", "BETA"),
                ],
                usage: Some(tree_usage(120, 40)),
                model: Some(MODEL.into()),
                ..Default::default()
            });
        }
        if let Some(idx) = req.user.find("FILE=") {
            let rest = &req.user[idx + 5..];
            let file = rest
                .split_whitespace()
                .next()
                .unwrap_or("x.txt")
                .to_string();
            let content = rest
                .find("CONTENT=")
                .map(|c| {
                    rest[c + 8..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            return Ok(CompletionResponse {
                tool_calls: vec![call(
                    "write_file",
                    json!({ "path": file, "content": format!("{content}\n") }),
                )],
                usage: Some(tree_usage(60, 20)),
                model: Some(MODEL.into()),
                ..Default::default()
            });
        }
        Ok(CompletionResponse::default())
    }
    fn name(&self) -> &str {
        "fixture"
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn spawn(goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({
            "goal": goal,
            "verify_file": file,
            "verify_contains": needle,
            // Bounded explicitly: the losing child would otherwise burn the
            // contract's default budget writing DRAFT twelve times, and the
            // fixture wants a short fixed trace.
            "max_steps": 2,
        }),
    )
}

fn tree_usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cache_read_tokens: 7,
        cache_write_tokens: 3,
        reasoning_tokens: 5,
        server_tool_requests: 0,
    }
}

/// A coordinator, two children, and a step cap of one on the root.
///
/// The step cap is the crash model, and it is the honest one for a *committed*
/// fixture: a run cut off by dropping its future leaves whatever the WAL happened
/// to hold at that instant, which is neither reproducible nor a single file.
/// Reaching the cap stops the run at a step boundary with every committed row
/// flushed, so the database on disk is exactly what a restarted process finds.
async fn interrupted(ws: &str) -> Res<()> {
    std::fs::create_dir_all(ws)?;

    let store = Store::open("interrupted.sqlite3")?;
    let result = run_tree(
        &TaskContract::workspace(
            "COORDINATOR: delegate to sub-agents; do not write files yourself.",
            ws,
        )
        .with_verification(Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        })
        .with_max_steps(1),
        &TreeProvider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        // Generous enough that nothing here is stopped by containment — this
        // fixture must be interrupted by its step cap, or it is a fixture about
        // refusing to spawn.
        &Containment::new(10, 4, 3, 1_000_000),
    )
    .await?;
    let root = result.run_id;
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "the interrupted fixture must stop short, not finish: {:?}",
        result.outcome
    );

    let mut children = Vec::new();
    for id in store.children(root)? {
        children.push(json!({
            "run_id": id,
            "wrote": store.edits(id)?.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            "depth": store.depth(id)?,
            "status": store.status(id)?,
            "outcome": store.outcome(id)?,
            "last_step": store.last_step(id)?,
            "spent_tokens": store.spent_tokens(id)?,
        }));
    }

    sidecar(
        "interrupted.json",
        &json!({
            "root_run_id": root,
            "workspace": ws,
            "root_status": store.status(root)?,
            "root_outcome": store.outcome(root)?,
            "root_last_step": store.last_step(root)?,
            "tree_run_ids": store.tree_run_ids(root)?,
            "spent_tokens_tree": store.spent_tokens_tree(root)?,
            "children": children,
            "workspace_files": {
                "a.txt": std::fs::read_to_string(Path::new(ws).join("a.txt")).ok(),
                "b.txt": std::fs::read_to_string(Path::new(ws).join("b.txt")).ok(),
            },
        }),
    )?;

    drop(store);
    Ok(())
}

// ---------- reading a store back, as 0.29.0 sees it ----------

/// Print everything 0.29.0 can see in `db`, as JSON on stdout.
///
/// The backwards half of F8 runs this against a database the *current* tree
/// wrote. 0.29.0 has no idea the recall table or the pinned column exist, so a
/// clean read here is the evidence that the 0.30.0 migration is additive in fact
/// and not only in intention. Anything that opens, selects and resumes is fair
/// game; nothing here writes.
fn read(db: &str) -> Res<()> {
    let store = Store::open(db)?;
    let mut runs = Vec::new();
    // Run ids are dense from 1 in a fixture database, and a summary that is not
    // there is skipped rather than fatal — this mode must be able to read a store
    // it did not write.
    for run_id in 1..=64 {
        if let Some(summary) = store.run_summary(run_id)? {
            runs.push(json!({
                "run_id": summary.run_id,
                "outcome": summary.outcome,
                "success": summary.success,
                "tokens": summary.tokens,
            }));
        }
    }
    let out = json!({
        "reader": "io-harness 0.29.0",
        "memory": store.memory_list(WORKSPACE)?.iter().map(|e| json!({
            "key": e.key,
            "value": e.value,
            "run_id": e.run_id,
            "step": e.step,
        })).collect::<Vec<_>>(),
        "runs": runs,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Write a sidecar next to its database, pretty-printed and newline-terminated so
/// a diff of a regenerated fixture is readable line by line.
fn sidecar(path: &str, value: &Value) -> Res<()> {
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Res<()> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: gen-0-29-0 write <output-dir> | gen-0-29-0 read <database>")?;
    let target = args
        .next()
        .ok_or("usage: gen-0-29-0 write <output-dir> | gen-0-29-0 read <database>")?;

    match mode.as_str() {
        "write" => {
            std::fs::create_dir_all(&target)?;
            // Everything after this names files relatively. `runs.file` stores the
            // contract root verbatim, so an absolute root would bake this machine's
            // home directory into a committed fixture.
            std::env::set_current_dir(&target)?;
            aggregates()?;
            interrupted("interrupted-workspace").await?;
            Ok(())
        }
        "read" => read(&target),
        other => Err(format!("unknown mode `{other}`: expected `write` or `read`").into()),
    }
}
