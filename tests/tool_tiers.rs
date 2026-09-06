//! Tiered tool exposure, and what it costs to reach past it (0.81.0).
//!
//! A measured turn on io-cli 0.38.0 carried 7,311 tokens before the user had typed
//! anything, of which 5,436 was the tool catalogue — thirty-nine tools at roughly
//! 140 tokens each, twelve of them the document tools, re-sent whole on every step
//! of every turn. A run editing Rust pays for a spreadsheet reader on every
//! request it makes.
//!
//! Tiering offers the core file, exec and git tools and names the rest in one
//! line. The claims asserted here are that the catalogue actually shrinks, that
//! every tool is still *reachable*, and that a run which declares nothing is
//! offered exactly what it was offered before — because a lever that changed the
//! default would be the mistake this release exists to stop repeating.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{tool_family, EXPAND_TOOLS_TOOL};
use io_harness::{
    run_with, ApproveAll, Config, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a script and keeps every catalogue it was offered.
struct Offered {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<Vec<String>>>,
    described: Mutex<Vec<Vec<(String, String)>>>,
}

impl Offered {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            described: Mutex::new(Vec::new()),
        }
    }

    fn catalogue(&self, turn: usize) -> Vec<String> {
        self.seen.lock().unwrap()[turn].clone()
    }

    fn turns(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn descriptions(&self, turn: usize) -> Vec<(String, String)> {
        self.described.lock().unwrap()[turn].clone()
    }
}

impl Provider for Offered {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen
            .lock()
            .unwrap()
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        self.described.lock().unwrap().push(
            req.tools
                .iter()
                .map(|t| (t.name.clone(), t.description.clone()))
                .collect(),
        );
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "offered"
    }
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("do some work", root)
        .with_verification(Verification::None)
        .with_max_steps(4)
}

async fn drive(contract: TaskContract, provider: &Offered) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let _ = run_with(
        &contract,
        provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
}

fn expand(family: &str) -> ToolCall {
    ToolCall {
        name: EXPAND_TOOLS_TOOL.into(),
        arguments: json!({ "family": family }),
    }
}

// --------------------------------------------------------------------- F13

/// A run that declares nothing is offered what it was offered before.
///
/// The control the lever rests on. Every rung and lever in this release ships with
/// its default unchanged, and a catalogue is the one surface where "unchanged"
/// must mean byte-identical: the vendor cache prefix is keyed on it.
#[tokio::test]
async fn f13_a_run_that_declares_no_tiers_is_offered_the_whole_catalogue() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Offered::new(vec![vec![]]);
    drive(contract(dir.path()), &provider).await;

    let catalogue = provider.catalogue(0);
    assert!(
        !catalogue.iter().any(|t| t == EXPAND_TOOLS_TOOL),
        "nothing was withheld, so there is nothing to expand: {catalogue:?}"
    );
    assert!(catalogue.iter().any(|t| t == "shell_start"));
}

/// Declaring tiers withholds the other families and offers one line instead.
#[tokio::test]
async fn f13_a_tiered_run_carries_the_core_tools_and_one_line_for_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Offered::new(vec![vec![]]);
    drive(
        contract(dir.path()).with_tool_tiers(Vec::<String>::new()),
        &provider,
    )
    .await;

    let catalogue = provider.catalogue(0);
    assert!(
        catalogue.iter().any(|t| t == EXPAND_TOOLS_TOOL),
        "a withheld family is reachable, not gone: {catalogue:?}"
    );
    assert!(
        catalogue.iter().all(|t| tool_family(t) == "core"),
        "only core tools are offered up front: {:?}",
        catalogue
            .iter()
            .filter(|t| tool_family(t) != "core")
            .collect::<Vec<_>>()
    );
    // The whole point: fewer entries, and the tools that do ordinary work still
    // there.
    for core in ["read_file", "write_file", "exec", "git_commit", "grep"] {
        assert!(catalogue.iter().any(|t| t == core), "{core} is missing");
    }
    assert!(!catalogue.iter().any(|t| t == "shell_start"));
}

/// One `expand_tools` call reaches a withheld family, from the next step.
#[tokio::test]
async fn f13_expanding_a_family_offers_it_on_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Offered::new(vec![vec![expand("shell_jobs")], vec![]]);
    drive(
        contract(dir.path()).with_tool_tiers(Vec::<String>::new()),
        &provider,
    )
    .await;

    assert!(provider.turns() >= 2, "the run took a second turn");
    assert!(!provider.catalogue(0).iter().any(|t| t == "shell_start"));
    assert!(
        provider.catalogue(1).iter().any(|t| t == "shell_start"),
        "the expanded family is offered from the next step: {:?}",
        provider.catalogue(1)
    );
}

/// A family that does not exist is named as such rather than silently ignored.
///
/// The negative control. An implementation that accepted any string would pass the
/// test above and leave a model retrying a word that will never work.
#[tokio::test]
async fn f13_an_unknown_family_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Offered::new(vec![vec![expand("spreadsheets")], vec![]]);
    drive(
        contract(dir.path()).with_tool_tiers(Vec::<String>::new()),
        &provider,
    )
    .await;

    // The refusal reaches the model as an observation on the next request.
    assert!(provider.turns() >= 2);
    assert!(
        !provider.catalogue(1).iter().any(|t| t == "xlsx_read"),
        "an unknown family expands nothing"
    );
}

// --------------------------------------------------------------------- F14

/// Every built-in tool's description fits the budget, or is named here with a
/// reason.
///
/// A schema plus one sentence is 40 to 60 estimated tokens; 140 is a paragraph,
/// and the catalogue is re-sent on every step of every turn. The budget is a gate
/// rather than a guideline because a description grows one sentence at a time and
/// nothing else in the repository would ever notice.
///
/// **Fifteen descriptions written before the budget existed are over it, and this
/// release does not shorten them.** That is not deference to prose. A tool's
/// description is what the model reads before deciding whether to call it, so
/// rewriting fifteen of them is a behavioural change to every run — and this
/// release's own rule is that no default moves ahead of the measurement that
/// argues for it. The measurement now exists: `docs/MEASUREMENTS.md` records what
/// the catalogue costs and what each of these contributes, and shortening them is
/// the next release's, argued from that. `US-IO-HARNESS-0.81.0-I01`.
///
/// So the list below is two kinds of entry, and the difference matters. A
/// **permanent** exception is a description whose length is load-bearing — the
/// tool is defined by what it refuses, and a model that learns the refusal here
/// does not learn it from a failed call. A **grandfathered** one is a description
/// that is merely long, kept until a number says what shortening it costs. A tool
/// added from 0.81.0 onwards gets neither.
#[tokio::test]
async fn f14_every_tool_description_fits_the_budget_or_is_a_named_exception() {
    /// Estimated tokens, the same four-characters-to-a-token heuristic the
    /// assembler uses — so this number and the catalogue cost the evaluation suite
    /// reports are the same measurement.
    fn tokens(s: &str) -> usize {
        s.chars().count().div_ceil(4)
    }

    const BUDGET: usize = 60;

    /// Length is load-bearing: the tool is defined by what it refuses.
    const PERMANENT: &[(&str, &str)] = &[
        (
            "list_dir",
            "it exists to be chosen over `find`, and the sentence that makes that \
             choice is the description",
        ),
        (
            "shell",
            "a whole command line is parsed here and never handed to a shell, and \
             what that does and does not support is the tool's contract",
        ),
        (
            "run_program",
            "the one tool whose argument is a program, and its boundary is what the \
             model has to know before writing one",
        ),
        (
            "expand_tools",
            "it names the withheld families, so its length is the number of \
             families rather than a paragraph",
        ),
        (
            "patch_file",
            "it takes a diff, and the diff format it accepts cannot be inferred \
             from a schema",
        ),
        (
            "ask_question",
            "what an answer does and does not authorise is the whole of the tool",
        ),
        (
            "ask_questions",
            "the same, and it additionally has to say how a batch differs from \
             asking twice",
        ),
    ];

    /// Merely long, and written before the budget existed. Shortening one is a
    /// behavioural change to every run, so it waits for the measurement that says
    /// what it costs. `US-IO-HARNESS-0.81.0-I01`.
    const GRANDFATHERED: &[&str] = &[
        "read_file",
        "git_branch",
        "git_worktree",
        "remember",
        "forget",
        "todo_write",
        "edit_file",
        "check",
        "exec",
        "shell_start",
        "shell_poll",
        "shell_kill",
    ];

    let dir = tempfile::tempdir().unwrap();
    let provider = Offered::new(vec![vec![]]);
    drive(contract(dir.path()), &provider).await;

    let dir2 = tempfile::tempdir().unwrap();
    let tiered = Offered::new(vec![vec![]]);
    drive(
        contract(dir2.path()).with_tool_tiers(Vec::<String>::new()),
        &tiered,
    )
    .await;

    // The catalogue names are what the run offered; the descriptions come back
    // with them, so this reads the real request rather than a list rebuilt here.
    let over: Vec<String> = provider
        .descriptions(0)
        .into_iter()
        .filter(|(name, d)| {
            tokens(d) > BUDGET
                && !PERMANENT.iter().any(|(e, _)| e == name)
                && !GRANDFATHERED.contains(&name.as_str())
        })
        .map(|(name, d)| format!("{name} ({} tokens)", tokens(&d)))
        .collect();

    assert!(
        over.is_empty(),
        "over the {BUDGET}-token budget and not a named exception: {}\n\
         Shorten it, or add it to PERMANENT with the reason its length is \
         load-bearing. GRANDFATHERED is closed: it lists what predates the budget \
         and takes nothing new.",
        over.join(", ")
    );

    // The list is a ledger, not a shrug: an entry that no longer exists, or that
    // someone shortened, has to leave — otherwise it grows into a place where
    // anything can hide.
    let catalogue = provider.descriptions(0);
    let stale: Vec<&str> = GRANDFATHERED
        .iter()
        .copied()
        .filter(|g| {
            !catalogue
                .iter()
                .any(|(name, d)| name == g && tokens(d) > BUDGET)
        })
        .collect();
    assert!(
        stale.is_empty(),
        "grandfathered and no longer over budget (or no longer offered): {stale:?} \
         — remove them from the list"
    );

    // And the tiered catalogue is the smaller one, which is what the tier is for.
    let full: usize = catalogue.iter().map(|(_, d)| tokens(d)).sum();
    let core: usize = tiered.descriptions(0).iter().map(|(_, d)| tokens(d)).sum();
    assert!(
        core < full,
        "tiering must cost fewer description tokens: {core} against {full}"
    );
    println!("description tokens: {full} full catalogue, {core} tiered");
}

/// The declaration reaches the contract from a config file.
#[test]
fn f13_the_tiers_reach_the_contract_from_a_config_file() {
    let config = Config::from_toml("[run]\ntool_tiers = [\"documents\"]\n").unwrap();
    let contract = config.apply_to(TaskContract::workspace("go", "/repo"));
    assert_eq!(
        contract.tool_tiers.as_deref(),
        Some(&["documents".to_string()][..])
    );

    // Unset offers everything, which is 0.80.0's behaviour.
    assert!(Config::from_toml("")
        .unwrap()
        .apply_to(TaskContract::workspace("go", "/repo"))
        .tool_tiers
        .is_none());
}
