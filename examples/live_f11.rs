//! F11: one live task that needs a registered tool *and* a skill, plus the
//! negative control that removes both.
//!
//! ```text
//! cargo run --example live_f11            # both runs
//! cargo run --example live_f11 positive   # just the full contract
//! cargo run --example live_f11 control    # just the negative control
//! ```
//!
//! The task is to make `record()` in `src/berth.rs` return the berth record for
//! one request id. Two things are missing from the workspace, and each is
//! missing in a different way:
//!
//! * the *datum* — which quay, what draft limit, which pilot window — exists
//!   only inside [`BerthTable::invoke`], on no disk the agent can reach and in
//!   neither the goal nor the criterion;
//! * the *shape* — which fields, in which order, under which labels — is stated
//!   only in the body of the `berth-record` skill. The catalogue in the system
//!   prompt carries that skill's name and a description that deliberately says
//!   nothing about the format.
//!
//! The criterion has to gate on both without naming either, or the control
//! would pass by copying its own success criterion. So it does not compare the
//! record to an expected string: it compares an FNV-1a digest of it, over the
//! alphanumerics only. The digest is one-way, so the constant in the criterion
//! discloses nothing; folding away punctuation, case, and spacing keeps the gate
//! from turning into a whitespace quiz. An agent that has the tool and the skill
//! can hit it; an agent that has neither cannot guess it.

// The Rust-specific `Verification` variants are deprecated in 0.17.0 and removed
// in 0.18.0. They are kept here deliberately: these files are what F10 asserts
// still work, and the fixtures are loose `.rs` files rather than cargo projects,
// so `Verification::Command { argv: ["cargo", "test"], .. }` — the replacement —
// has no project to run in. See docs/guide/verification.md for the migration.
#![allow(deprecated)]

use std::sync::{Arc, Mutex};

use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Policy, RunOutcome, RunResult, Skills, Store, TaskContract, ToolSpec,
    Verification,
};
use serde_json::{json, Value};

/// The request id. It is in the goal on purpose — it is the *key*, not the
/// datum, and the agent needs something to look up.
const ID: &str = "BRT-2291";

/// What the tool answers with, and the only place the three values exist.
const REPLY: &str = "BRT-2291 is assigned to quay Kattegat. Draft limit 11.6. Pilot window 0430.";

/// The record a run that used both extension points produces: the tool's three
/// values, in the skill's shape. Never sent to the model — it is here so the
/// example can compute the digest the criterion checks, and so the report at the
/// end can say whether the model landed on it exactly.
const RECORD: &str = "quay=Kattegat; draft=11.6; window=0430";

/// One value from `REPLY` that a search of the workspace cannot produce.
const DATUM: &str = "Kattegat";

/// The shape, stated once, here and nowhere else the agent can see. The
/// description is what reaches the system prompt; it names the subject and
/// withholds the format, which is the whole point of a catalogue.
const SKILL: &str = "\
---
name: berth-record
description: How this operator records a berth assignment. Read before writing one.
---

A berth record is a single line holding exactly three fields, in this order,
separated by `; `:

1. `quay=<quay name>`
2. `draft=<draft limit>`
3. `window=<pilot window>`

Copy each value exactly as the lookup reports it: no units, no re-spelled
numbers, no re-punctuated times. Nothing else goes on the line.
";

/// A second skill, irrelevant to this task on purpose. Which one applies is the
/// model's judgement; the harness does not match or rank.
const MOORING: &str = "\
# How this operator logs a mooring inspection

One paragraph, past tense, naming the inspector and the hour.
";

/// The digest the criterion computes, as Rust source. Kept as text so the
/// criterion the model reads and the constant this example computes come from
/// one algorithm written once — see [`digest`], which is its twin in the
/// example's own process.
const DIGEST_SRC: &str = "\
fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| b.to_ascii_lowercase())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}";

/// FNV-1a over the alphanumerics of `s`, lowercased. The twin of [`DIGEST_SRC`];
/// if the two ever disagree the positive run stops passing, which is the check.
fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| b.to_ascii_lowercase())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The caller's own capability: the berth book, which is not a file.
struct BerthTable {
    asked: Arc<Mutex<Vec<String>>>,
}

impl Tool for BerthTable {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lookup_berth".into(),
            description: "Look up the berth assignment for one request id.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A request id, e.g. BRT-2291." }
                },
                "required": ["id"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a Value) -> ToolFuture<'a> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Box::pin(async move {
            self.asked
                .lock()
                .expect("no panic holds this lock")
                .push(id.clone());
            if id == ID {
                return Ok(REPLY.to_string());
            }
            Err(io_harness::Error::Config(format!(
                "no request {id:?}; this book knows {ID}"
            )))
        })
    }
}

/// The goal, identical for both runs. It names the two places the missing
/// pieces live and neither piece itself.
const GOAL: &str = "src/berth.rs must make record() return the berth record for request \
    BRT-2291. Two things are not in this repository. The format of a berth record is \
    stated in a skill, so read the skill that applies before you write. The assignment \
    for BRT-2291 is held by the lookup_berth tool. Invent neither.";

/// Lay out a fresh workspace: the stub the agent edits, and nothing else.
fn workspace() -> std::io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(
        dir.path().join("src/berth.rs"),
        "pub fn record() -> &'static str {\n    \"\"\n}\n",
    )?;
    Ok(dir)
}

/// The trace, the produced file, and what the two of them prove.
fn report(store: &Store, result: &RunResult, root: &std::path::Path) -> io_harness::Result<()> {
    println!("  outcome: {:?}", result.outcome);
    for step in store.steps(result.run_id)? {
        let call = step.tool_call.replace('\n', " ");
        println!(
            "  step {}: {} | {}",
            step.step,
            step.decision,
            &call[..call.len().min(180)]
        );
    }
    for e in store
        .events(result.run_id)?
        .iter()
        .filter(|e| e.kind == "refusal")
    {
        println!(
            "  refused: {} {} (rule {})",
            e.act,
            e.target,
            e.rule.clone().unwrap_or_else(|| "-".into())
        );
    }
    let written = std::fs::read_to_string(root.join("src/berth.rs")).unwrap_or_default();
    println!("  src/berth.rs:\n{}", indent(&written));
    println!("  carries the datum {DATUM:?}: {}", written.contains(DATUM));
    println!(
        "  carries the skill's exact shape: {}",
        written.contains(RECORD)
    );
    Ok(())
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let only = std::env::args().nth(1).unwrap_or_default();
    let provider = io_harness::OpenRouter::from_env()?;

    let skills = tempfile::tempdir()?;
    std::fs::write(skills.path().join("berth-record.md"), SKILL)?;
    std::fs::create_dir_all(skills.path().join("mooring-log"))?;
    std::fs::write(skills.path().join("mooring-log/SKILL.md"), MOORING)?;

    let test_src = format!(
        "{DIGEST_SRC}\n#[test]\nfn t() {{ assert_eq!(digest(&record()), {}); }}\n",
        digest(RECORD)
    );

    println!("catalogue the system prompt carries:");
    println!("{}", Skills::discover(skills.path())?.catalog());
    println!("\ncriterion the model is shown:");
    println!("{}", indent(&test_src));

    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("lookup_berth");

    if only != "control" {
        println!("\n=== run 1: the full contract (tool + skill) ===");
        let dir = workspace()?;
        let asked = Arc::new(Mutex::new(Vec::new()));
        let contract = TaskContract::workspace(
            GOAL,
            dir.path(),
            Verification::WorkspaceTestPasses {
                files: vec!["src/berth.rs".into()],
                test_src: test_src.clone(),
            },
        )
        .with_tools(Toolbox::new().with(BerthTable {
            asked: Arc::clone(&asked),
        }))
        .with_skills(skills.path())
        .with_max_steps(6)
        .with_token_budget(200_000);

        let store = Store::open(dir.path().join("runs.db"))?;
        let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
        report(&store, &result, dir.path())?;
        println!(
            "  the tool was asked for: {:?}",
            asked.lock().expect("no poisoning")
        );
        if !matches!(result.outcome, RunOutcome::Success { .. }) {
            eprintln!("  note: the positive run did not succeed; the trace above is the finding");
        }
    }

    if only != "positive" {
        println!("\n=== run 2: the same contract, with_tools and with_skills removed ===");
        let dir = workspace()?;
        let contract = TaskContract::workspace(
            GOAL,
            dir.path(),
            Verification::WorkspaceTestPasses {
                files: vec!["src/berth.rs".into()],
                test_src,
            },
        )
        .with_max_steps(6)
        .with_token_budget(200_000);

        let store = Store::open(dir.path().join("runs.db"))?;
        let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
        report(&store, &result, dir.path())?;
    }
    Ok(())
}
