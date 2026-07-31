//! Live agency: a plan you can watch, a question you can answer, and two named
//! agents on two different models (0.21.0).
//!
//! The suite proves the mechanisms against scripted providers. Only a live run proves
//! the thing that actually matters about this release: that a real model, given these
//! tools, *reaches for them* — writes a plan before it starts working, and asks rather
//! than guessing when the task is genuinely ambiguous. No amount of offline testing can
//! establish that, because offline the model is a script that always does.
//!
//! Three claims, each asserted here so a regression fails the example rather than
//! printing output nobody reads:
//!
//! 1. **A plan is durable and readable mid-run.** The observer reads it from a *second*
//!    connection to the same database at the moment it is written, which is what a UI in
//!    another process has. Reading it afterwards would pass even if the write landed at
//!    the very end.
//! 2. **A question is answered and the answer changes the outcome.** The responder
//!    answers with one of two filenames, and the file the agent then writes is the one
//!    the answer named.
//! 3. **Two named agents put two different models on the wire.** Asserted from the
//!    `provider_calls` rows in the store, not from the roster that asked for them.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! # Optional: two models to prove per-agent selection. Defaults below if unset.
//! export IO_CHEAP_MODEL=anthropic/claude-haiku-4.5
//! export IO_STRONG_MODEL=anthropic/claude-sonnet-4
//! cargo run --example agency_live
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::{
    run_tree, AgentDef, Agents, AnswerFuture, ApproveAll, Containment, EventKind, Flow, Observer,
    OpenRouter, Policy, Question, Responder, RunEvent, Store, TaskContract, TodoState,
    Verification,
};

/// Answers the agent's question with a fixed choice, and records what it was asked so
/// the example can assert the question actually happened.
#[derive(Debug)]
struct Operator {
    answer: String,
    asked: Mutex<Vec<String>>,
}

impl Operator {
    fn new(answer: &str) -> Self {
        Self {
            answer: answer.to_string(),
            asked: Mutex::new(Vec::new()),
        }
    }
}

impl Responder for Operator {
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
        self.asked.lock().unwrap().push(question.question.clone());
        println!("  the agent asked: {}", question.question);
        if !question.choices.is_empty() {
            println!("  it offered: {:?}", question.choices);
        }
        println!("  answering: {}", self.answer);
        Box::pin(async { Some(self.answer.clone()) })
    }
}

/// Reads the plan from its own connection each time one is written, mid-run.
struct PlanWatcher {
    db: std::path::PathBuf,
    /// Every plan seen, as it was seen, from a second connection.
    seen_mid_run: Mutex<Vec<Vec<(String, TodoState)>>>,
    events: AtomicUsize,
}

impl Observer for PlanWatcher {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::TodoWrote { items } = &event.kind {
            self.events.fetch_add(1, Ordering::SeqCst);
            println!("  the agent wrote a plan ({} items):", items.len());
            for item in items {
                println!("    [{}] {}", item.state.as_str(), item.text);
            }
            // The point of the whole feature: another process can read this NOW.
            if let Ok(other) = Store::open(&self.db) {
                if let Ok(plan) = other.todos(event.run_id) {
                    self.seen_mid_run
                        .lock()
                        .unwrap()
                        .push(plan.into_iter().map(|i| (i.text, i.state)).collect());
                }
            }
        }
        if let EventKind::QuestionAsked { question, .. } = &event.kind {
            println!("  [event] question asked: {question}");
        }
        Flow::Continue
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = OpenRouter::from_env()?;
    let dir = tempfile::tempdir().expect("a temp workspace");
    let root = dir.path();
    let db = root.join("trace.db");

    let policy = Policy::default()
        .layer("live")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("git*");

    // ---------------------------------------------------------------- 1 and 2
    // One run that must plan and must ask: two candidate files exist and the goal
    // deliberately does not say which one to write, so guessing is a coin flip and
    // asking is the only way to be right.
    println!("\n=== a plan, and a question ===");
    std::fs::write(root.join("alpha.txt"), "alpha\n").expect("fixture");
    std::fs::write(root.join("beta.txt"), "beta\n").expect("fixture");

    let store = Store::open(&db)?;
    let responder = std::sync::Arc::new(Operator::new("beta.txt"));
    let watcher = PlanWatcher {
        db: db.clone(),
        seen_mid_run: Mutex::new(Vec::new()),
        events: AtomicUsize::new(0),
    };

    let contract = TaskContract::workspace(
        "Append the line `reviewed` to exactly one of the two .txt files in this \
         workspace. Both alpha.txt and beta.txt exist and the choice matters, so \
         before you edit anything: write your plan with todo_write, and use \
         ask_question to find out which file the operator means. Do not guess.",
        root,
    )
    .with_max_steps(8)
    .with_responder(responder.clone());

    // Driven with the watcher attached, because the mid-run read is the claim: the
    // observer opens its own connection the moment a plan is written and reads it back
    // the way a UI in another process would.
    let result =
        io_harness::run_with_observed(&contract, &provider, &store, &policy, &ApproveAll, &watcher)
            .await?;
    println!("  outcome: {:?}", result.outcome);

    // -- claim 2: it asked, and it acted on the answer --
    let asked = responder.asked.lock().unwrap().clone();
    assert!(
        !asked.is_empty(),
        "the model never asked anything — this release's claim is that it can, and a \
         live model that guesses instead is the finding worth recording"
    );
    let beta = std::fs::read_to_string(root.join("beta.txt")).expect("beta.txt");
    let alpha = std::fs::read_to_string(root.join("alpha.txt")).expect("alpha.txt");
    println!("  alpha.txt: {alpha:?}");
    println!("  beta.txt:  {beta:?}");
    assert!(
        beta.contains("reviewed"),
        "the answer named beta.txt, so that is the file that should have been edited"
    );
    assert!(
        !alpha.contains("reviewed"),
        "alpha.txt was not the answer and must not have been edited"
    );
    println!("  OK — the question was asked and the answer decided the edit");

    // -- claim 1: the plan is in the store, and was readable while the run ran --
    let plan = store.todos(result.run_id)?;
    assert!(
        !plan.is_empty(),
        "the model never wrote a plan; nothing here can be asserted about one"
    );
    let mid_run = watcher.seen_mid_run.lock().unwrap().clone();
    assert!(
        !mid_run.is_empty(),
        "the plan must have been readable from a second connection WHILE the run was \
         going — that is the whole reason the table exists. {} TodoWrote event(s) were \
         seen.",
        watcher.events.load(Ordering::SeqCst)
    );
    assert!(
        mid_run.iter().all(|p| !p.is_empty()),
        "every mid-run read must have seen a whole plan, never an empty or half-written \
         one: the write is one transaction precisely so a reader cannot catch it midway"
    );
    println!(
        "  read the plan from a second connection {} time(s) mid-run",
        mid_run.len()
    );
    println!("  final plan in the store ({} items):", plan.len());
    for item in &plan {
        println!("    [{}] {}", item.state.as_str(), item.text);
    }
    let questions = store.questions(result.run_id)?;
    assert_eq!(
        questions.len(),
        asked.len(),
        "every question the responder saw must be in the trace"
    );
    assert!(
        questions
            .iter()
            .all(|q| q.answered_by.as_deref() == Some("responder")),
        "answered in-process, so the trace must say so rather than saying a human did"
    );
    println!("  OK — the plan and the question are both durable");

    // ---------------------------------------------------------------- 3
    // Two named agents, two models, read back from the trace.
    println!("\n=== two named agents on two models ===");
    let cheap = std::env::var("IO_CHEAP_MODEL")
        .unwrap_or_else(|_| "anthropic/claude-haiku-4.5".to_string());
    let strong = std::env::var("IO_STRONG_MODEL")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4".to_string());
    println!("  searcher -> {cheap}");
    println!("  author   -> {strong}");

    let tree_dir = tempfile::tempdir().expect("a second workspace");
    let tree_root = tree_dir.path();
    std::fs::write(tree_root.join("data.txt"), "the answer is 42\n").expect("fixture");
    let tree_store = Store::open(tree_root.join("trace.db"))?;

    let tree_contract = TaskContract::workspace(
        "Find what data.txt says, then write it into result.txt. Use spawn_agent \
         twice: once with agent=\"searcher\" to read data.txt and report what it \
         says, then once with agent=\"author\" to write result.txt. The searcher \
         cannot write, so do not ask it to.",
        tree_root,
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "result.txt".into(),
        needle: "42".into(),
    })
    .with_max_steps(8)
    .with_agents(
        Agents::new()
            .with(
                AgentDef::new("searcher")
                    .with_role("You locate information and report it. You never edit files.")
                    .with_model(&cheap)
                    // It reads. It cannot write, whatever it is asked to do.
                    .deny_write()
                    .with_max_steps(4),
            )
            .with(
                AgentDef::new("author")
                    .with_role("You write the file you are told to write, and nothing else.")
                    .with_model(&strong)
                    .with_max_steps(4),
            ),
    );

    let tree_result = run_tree(
        &tree_contract,
        &provider,
        &tree_store,
        &policy,
        &ApproveAll,
        &Containment::new(8, 2, 2, 400_000),
    )
    .await?;
    println!("  outcome: {:?}", tree_result.outcome);

    // Every model that served a call in this tree, root and children.
    let mut models: Vec<String> = tree_store
        .provider_calls(tree_result.run_id)?
        .into_iter()
        .filter_map(|c| c.model)
        .collect();
    let children = tree_store.children(tree_result.run_id)?;
    for child in &children {
        models.extend(
            tree_store
                .provider_calls(*child)?
                .into_iter()
                .filter_map(|c| c.model),
        );
    }
    println!("  models recorded in the trace: {models:?}");
    assert!(
        !children.is_empty(),
        "the model never spawned anything, so there is no per-agent model to prove"
    );

    // The vendor may substitute or alias a model, so the assertion is on the family
    // rather than on byte equality — what is being proven is that the two agents did
    // NOT both get the run's default.
    let short = |m: &str| m.rsplit('/').next().unwrap_or(m).to_string();
    let distinct: std::collections::BTreeSet<String> = models.iter().map(|m| short(m)).collect();
    println!("  distinct models: {distinct:?}");
    if cheap == strong {
        println!("  (IO_CHEAP_MODEL == IO_STRONG_MODEL, so a single model is expected here)");
    } else {
        assert!(
            distinct.len() >= 2,
            "two definitions named two different models, so at least two should appear \
             in the trace; got {distinct:?}"
        );
        println!("  OK — two named agents were served by two different models");
    }

    // The searcher could not write, whatever it was told.
    let refusals: Vec<_> = children
        .iter()
        .flat_map(|c| tree_store.events(*c).unwrap_or_default())
        .filter(|e| e.kind == "refusal")
        .collect();
    if refusals.is_empty() {
        println!("  (the searcher never attempted a write, so its deny was not exercised)");
    } else {
        println!(
            "  the searcher's writes were refused {} time(s):",
            refusals.len()
        );
        for r in &refusals {
            println!(
                "    {} {} (rule {:?}, layer {:?})",
                r.act, r.target, r.rule, r.layer
            );
        }
    }

    // Which definition each child ran as is in the trace.
    let spawns: Vec<String> = tree_store
        .agent_events(tree_result.run_id)?
        .into_iter()
        .filter(|e| e.kind == "spawn")
        .filter_map(|e| e.detail)
        .collect();
    println!("  spawns recorded: {spawns:?}");
    assert!(
        spawns
            .iter()
            .any(|d| d.contains("searcher") || d.contains("author")),
        "the trace must record which definition a child was spawned from, got {spawns:?}"
    );

    println!("\nAll live claims held.");
    Ok(())
}
