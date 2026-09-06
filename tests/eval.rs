//! The in-crate evaluation suite, running on a recording (0.81.0).
//!
//! Deterministic replay has shipped since 0.12.0 and nothing sat on top of it, so
//! three of the largest behavioural levers in this crate — Context Collapse, the
//! tool mask and CodeAct — are live and unmeasured. This is the layer that measures
//! them, and these are its own gates.
//!
//! **Every recording here is made in the test that replays it, and that is not
//! laziness.** `Recording::load` refuses a recording from another release series,
//! so a cassette checked into this repository would expire at the next minor bump —
//! which is every release. Record, save, reset, replay, exactly as
//! `tests/determinism.rs` does.
//!
//! **No test here needs a key and none opens a socket.** The suite runs under a
//! policy that refuses `Act::Net` and reports how many network acts were refused;
//! zero is the assertion, and a number above zero would be a case reaching the
//! network whatever it was reaching for.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::eval::{
    ApproverCatchRate, Case, CatalogueCost, PromptTokens, Retention, Scorer, Suite, Transcript,
};
use io_harness::provider::{
    CompletionRequest, CompletionResponse, Record, Replay, ToolCall, Usage,
};
use io_harness::tools::{FIND_TOOL, READ_FILE_TOOL, WRITE_FILE_TOOL};
use io_harness::{ApproveAll, Policy, Provider, TaskContract, ToolSpec, Verification};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// The shape `tests/replay.rs` uses: a fixed script, one answer per turn.
///
/// Deliberately the same shape rather than a third one. It exists here only to
/// *produce* the recording every test then replays.
struct Canned {
    responses: Vec<CompletionResponse>,
    at: AtomicUsize,
}

impl Canned {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for Canned {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .responses
            .get(i)
            .cloned()
            .unwrap_or_else(|| self.responses.last().cloned().unwrap_or_default()))
    }

    fn name(&self) -> &str {
        "canned"
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A turn that makes tool calls, reporting usage so a scorer over `provider_calls`
/// is not silently reading `None`.
fn turn(calls: Vec<ToolCall>) -> CompletionResponse {
    CompletionResponse {
        tool_calls: calls,
        usage: Some(Usage {
            prompt_tokens: 900,
            completion_tokens: 20,
            total_tokens: 920,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hello from the readme\n").unwrap();
    dir
}

/// The contract every case in this file runs: read the readme, then write a file.
///
/// It carries a real verification rather than [`Verification::None`], because the
/// store's `success` is "the gate passed" — a run with no gate ends `Finished` and
/// is not a success. A case whose outcome nothing decides is not a case.
fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("read README.md and then write out.txt", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "done".into(),
        })
        .with_max_steps(4)
}

/// The script both steps of that contract need.
fn script() -> Vec<CompletionResponse> {
    vec![
        turn(vec![call(READ_FILE_TOOL, json!({"path": "README.md"}))]),
        turn(vec![call(
            WRITE_FILE_TOOL,
            json!({"path": "out.txt", "content": "done"}),
        )]),
        // The run ends when the model stops calling tools.
        CompletionResponse {
            text: Some("finished".into()),
            usage: Some(Usage {
                prompt_tokens: 900,
                completion_tokens: 4,
                total_tokens: 904,
                ..Default::default()
            }),
            ..Default::default()
        },
    ]
}

/// Record one run of `contract` against the script, and hand back a `Replay` of it
/// with the workspace reset to what it was before.
///
/// The reset is load-bearing: the replay key is the request's own bytes, and a
/// workspace still holding `out.txt` produces a different observation section and
/// therefore a different key.
///
/// **So is recording under the same policy the replay runs under.** The system
/// prompt carries a boundary section built from the policy, so recording under a
/// permissive one and replaying under a deny-net one misses on the very first
/// request — which is the divergence error, correctly reported, about a difference
/// that has nothing to do with the case.
async fn recorded(dir: &tempfile::TempDir) -> (Replay, PathBuf) {
    let path = dir.path().join("recording.json");
    let recorder = Record::new(Canned::new(script()));
    let store = io_harness::Store::memory().unwrap();
    let _ = io_harness::run_with(
        &contract(dir.path()),
        &recorder,
        &store,
        &offline(dir.path()),
        &ApproveAll,
    )
    .await
    .unwrap();
    recorder.save(&path).unwrap();

    let _ = std::fs::remove_file(dir.path().join("out.txt"));
    (Replay::load(&path).unwrap(), path)
}

/// A policy that permits the workspace and refuses the network.
///
/// The suite's own control: a deterministic arm that dialled anything would show
/// up as a refusal here rather than as a passing test.
fn offline(_root: &Path) -> Policy {
    Policy::permissive().deny_net("*")
}

fn case(root: &Path) -> Case {
    Case::new("reads the readme then writes a file", contract(root))
        .needing(["README.md", "hello from the readme"])
        // Two steps: read, then write — the gate passes the moment `out.txt` says
        // `done`, so the script's third turn is never asked for.
        .expecting(true, 2)
}

// ---------------------------------------------------------------------- F3

/// A case runs from a recording, with no key and no socket.
#[tokio::test]
async fn f3_a_case_runs_from_a_recording_with_no_key_and_no_network() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    let report = Suite::new()
        .with_case(case(dir.path()))
        .with_scorer(PromptTokens)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(report.len(), 1);
    assert!(report[0].passed, "{}", report[0].why);
    assert_eq!(report[0].scores.len(), 1);
    assert!(report[0].scores[0].value > 0.0);
}

/// The step count catches a replay that ran out of answers.
///
/// A `Replay` serves its last recorded answer forever once a key is exhausted, so
/// a diverged case ends with the right outcome at its step ceiling. Only the count
/// says so — this is the negative control for `Case::expect_steps`, and without it
/// every test in this file could pass on a run that looped.
#[tokio::test]
async fn f3_a_case_that_takes_the_wrong_number_of_steps_fails_on_the_count() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    let wrong = case(dir.path()).expecting(true, 3);
    let report = Suite::new()
        .with_case(wrong)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    assert!(!report[0].passed);
    assert!(
        report[0].why.contains("step"),
        "the failure must name the count: {}",
        report[0].why
    );
}

// ---------------------------------------------------------------------- F4

/// The same suite scores identically twice.
///
/// Run in one process, against one recording, comparing whole `Score` values —
/// including the detail strings, because a number that is stable while its
/// explanation moves is a report a reader cannot trust either.
#[tokio::test]
async fn f4_the_same_suite_scores_identically_on_two_consecutive_runs() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    let build = || {
        Suite::new()
            .with_case(case(dir.path()))
            .with_scorer(PromptTokens)
            .with_scorer(Retention)
            .with_scorer(CatalogueCost)
    };

    let first = build()
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();
    let _ = std::fs::remove_file(dir.path().join("out.txt"));
    let second = build()
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.passed, b.passed);
        assert_eq!(
            a.scores, b.scores,
            "scores moved between two identical runs"
        );
    }
}

/// A case whose recording cannot answer it fails loudly rather than quietly.
#[tokio::test]
async fn f4_a_case_the_recording_cannot_answer_fails_naming_the_divergence() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    // A different goal is a different system block and a different flat prompt,
    // so the very first request misses.
    let diverged = Case::new(
        "asks something the recording never heard",
        TaskContract::workspace("do something else entirely", dir.path())
            .with_verification(Verification::None)
            .with_max_steps(4),
    );

    let err = Suite::new()
        .with_case(diverged)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .expect_err("a miss is an error, never an invented answer");
    assert!(
        err.to_string().contains("diverged"),
        "the error must say what happened: {err}"
    );
}

// ---------------------------------------------------------------------- F5

/// All four scorers answer, and each answers about the thing it claims to.
#[tokio::test]
async fn f5_the_four_scorers_each_report_what_they_name() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    let report = Suite::new()
        .with_case(case(dir.path()))
        .with_scorer(PromptTokens)
        .with_scorer(Retention)
        .with_scorer(CatalogueCost)
        .with_scorer(ApproverCatchRate)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    let by = |name: &str| {
        report[0]
            .scores
            .iter()
            .find(|s| s.scorer == name)
            .unwrap_or_else(|| panic!("no {name} score"))
            .clone()
    };

    let tokens = by("prompt_tokens");
    assert!(tokens.value > 0.0, "{}", tokens.detail);

    // The case declared it needs the file's name and its contents, and this run
    // compacts nothing, so both survive.
    let retention = by("retention");
    assert_eq!(retention.value, 1.0, "{}", retention.detail);

    // The catalogue is a component of the whole request, and on an ordinary turn
    // it is the larger component — which is the request-floor finding this suite
    // exists to make arguable rather than anecdotal.
    let catalogue = by("catalogue_cost");
    assert!(
        catalogue.value > 0.0 && catalogue.value < tokens.value,
        "the catalogue is part of the request, not all of it: {} vs {}",
        catalogue.value,
        tokens.value
    );
    // Two requests, so the catalogue is sent twice and is more than half of one of
    // them. That ratio is the request-floor finding this suite exists to make
    // arguable rather than anecdotal.
    assert!(
        catalogue.value * 2.0 > tokens.value * 0.5,
        "the catalogue is the larger half of an ordinary turn: {} per request \
         against {} for two whole requests",
        catalogue.value,
        tokens.value
    );

    // Nothing dangerous was injected, so the catch rate is zero and says why —
    // never 1.0, which would read as "caught everything".
    let catch = by("approver_catch_rate");
    assert_eq!(catch.value, 0.0);
    assert!(
        catch.detail.contains("nothing to catch"),
        "{}",
        catch.detail
    );
}

/// Retention reports what was lost, by name.
///
/// The negative control for the scorer that every compaction rung will be judged
/// by: a fact the run never saw must score below 1.0 and be named, or the scorer
/// would report a perfect score for a prompt that carried nothing.
#[tokio::test]
async fn f5_retention_names_the_fact_a_prompt_did_not_carry() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    let demanding = case(dir.path()).needing(["README.md", "a fact this run never saw"]);
    let report = Suite::new()
        .with_case(demanding)
        .with_scorer(Retention)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    let score = &report[0].scores[0];
    assert_eq!(score.value, 0.5);
    assert!(
        score.detail.contains("a fact this run never saw"),
        "{}",
        score.detail
    );
}

/// The deterministic arm refuses nothing on the network, because it reaches for
/// nothing.
#[tokio::test]
async fn f5_the_deterministic_arm_makes_no_network_act_to_refuse() {
    let dir = workspace();
    let (replay, _path) = recorded(&dir).await;

    /// Reports the transcript's own network count, which no shipped scorer does —
    /// it is the suite's control rather than a measurement.
    struct Dialled;
    impl Scorer for Dialled {
        fn name(&self) -> &str {
            "net_refusals"
        }
        fn score(&self, case: &Case, t: &Transcript) -> io_harness::eval::Score {
            io_harness::eval::Score {
                scorer: self.name().into(),
                case: case.name.clone(),
                value: t.net_refusals as f64,
                detail: format!("{} network act(s) refused", t.net_refusals),
            }
        }
    }

    let report = Suite::new()
        .with_case(case(dir.path()))
        .with_scorer(Dialled)
        .run(&replay, &offline(dir.path()), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        report[0].scores[0].value, 0.0,
        "a replayed case must not reach the network: {}",
        report[0].scores[0].detail
    );
}

// ---------------------------------------------------------------------- F6

/// What a graduated projection costs against a fold, and what each keeps.
///
/// `#[ignore]`d and `n5_`-prefixed: it prints and asserts nothing, because the
/// numbers depend on the case set and a threshold asserted on one machine is a
/// flake on another. `docs/MEASUREMENTS.md` records the output and the command.
///
/// **Each arm records its own cassette.** The replay key is the request's own
/// bytes, so an arm that compacts differently cannot answer from another arm's
/// recording — the arms are comparable because the *script* is identical, which
/// fixes the model's answers by construction. That is the honest form of this
/// comparison and its stated limit: it measures what each rung costs and keeps,
/// never whether the model would have answered better.
#[tokio::test]
#[ignore]
async fn n5_what_each_compaction_rung_costs_and_keeps() {
    use io_harness::context::{Collapse, Ladder, Snip};

    let arms: Vec<(&str, Ladder, Collapse)> = vec![
        ("none (0.80.0)", Ladder::default(), Collapse::default()),
        (
            "collapse only",
            Ladder::default(),
            Collapse { keep_chars: 400 },
        ),
        (
            "reduce",
            Ladder {
                reduce: true,
                ..Ladder::default()
            },
            Collapse::default(),
        ),
        (
            "snip",
            Ladder {
                snip: Some(Snip {
                    older_than_steps: 2,
                }),
                ..Ladder::default()
            },
            Collapse::default(),
        ),
        (
            "microcompact",
            Ladder {
                microcompact: true,
                ..Ladder::default()
            },
            Collapse::default(),
        ),
    ];

    // A long case with a tight ceiling, because a rung that never fires reports
    // the same number as every other rung — which is what the first run of this
    // measurement produced, and is a fact about the case set rather than about the
    // ladder.
    let long_script = || {
        let mut turns: Vec<CompletionResponse> = (0..10)
            .map(|i| {
                turn(vec![
                    call(FIND_TOOL, json!({"name_glob": format!("*{i}.rs")})),
                    call(READ_FILE_TOOL, json!({"path": "README.md"})),
                ])
            })
            .collect();
        turns.push(turn(vec![call(
            WRITE_FILE_TOOL,
            json!({"path": "out.txt", "content": "done"}),
        )]));
        turns
    };

    println!("arm | prompt tokens | retention");
    for (name, ladder, collapse) in arms {
        let dir = workspace();
        let shaped = |c: TaskContract| {
            c.with_ladder(ladder)
                .with_collapse(collapse)
                .with_max_steps(12)
                .with_context_budget(io_harness::ContextBudget {
                    max_tokens: 1_500,
                    share: 0.5,
                })
        };
        let path = dir.path().join("recording.json");
        let recorder = Record::new(Canned::new(long_script()));
        let store = io_harness::Store::memory().unwrap();
        let result = io_harness::run_with(
            &shaped(contract(dir.path())),
            &recorder,
            &store,
            &offline(dir.path()),
            &ApproveAll,
        )
        .await
        .unwrap();
        recorder.save(&path).unwrap();
        let steps = store.last_step(result.run_id).unwrap();
        let _ = std::fs::remove_file(dir.path().join("out.txt"));
        let replay = Replay::load(&path).unwrap();

        let report = Suite::new()
            .with_case(
                Case::new(name, shaped(contract(dir.path())))
                    .needing(["hello from the readme"])
                    .expecting(true, steps),
            )
            .with_scorer(PromptTokens)
            .with_scorer(Retention)
            .run(&replay, &offline(dir.path()), &ApproveAll)
            .await
            .unwrap();

        let value = |scorer: &str| {
            report[0]
                .scores
                .iter()
                .find(|s| s.scorer == scorer)
                .map(|s| s.value)
                .unwrap_or_default()
        };
        println!(
            "{name} | {} | {}",
            value("prompt_tokens"),
            value("retention")
        );
    }
}

/// What the tool mask costs, now that the catalogue it offers is deliberately
/// byte-identical to an unmasked one.
///
/// 0.76.0 decided a masked run offers exactly the catalogue an unmasked one does,
/// so that the vendor's cache prefix does not move. That decision has been
/// argued from first principles and never measured; this prints the number.
#[tokio::test]
#[ignore]
async fn n5_what_the_tool_mask_costs() {
    use io_harness::tools::ToolMask;

    for (name, mask) in [
        ("unmasked", ToolMask::none()),
        // A tool this case never calls. Masking one it *does* call measures
        // something else entirely — the run loses a step to a refusal and the
        // request count doubles — which is a real number about masking a needed
        // tool and not the question here. The question is what the mask costs a run
        // that never notices it, and the answer has to isolate the catalogue.
        ("masked", ToolMask::withholding(["git_worktree"])),
    ] {
        let dir = workspace();
        let shaped = |c: TaskContract| c.with_tool_mask(mask.clone());
        let path = dir.path().join("recording.json");
        let recorder = Record::new(Canned::new(script()));
        let store = io_harness::Store::memory().unwrap();
        let _ = io_harness::run_with(
            &shaped(contract(dir.path())),
            &recorder,
            &store,
            &offline(dir.path()),
            &ApproveAll,
        )
        .await;
        recorder.save(&path).unwrap();
        let _ = std::fs::remove_file(dir.path().join("out.txt"));

        let replay = Replay::load(&path).unwrap();
        let report = Suite::new()
            .with_case(Case::new(name, shaped(contract(dir.path()))).expecting(true, 2))
            .with_scorer(CatalogueCost)
            .with_scorer(PromptTokens)
            .run(&replay, &offline(dir.path()), &ApproveAll)
            .await
            .unwrap();
        println!(
            "{name} | catalogue {} | request {}",
            report[0].scores[0].value, report[0].scores[1].value
        );
    }
}

// ---------------------------------------------------------------------- F7

/// The live arm: one case against a real provider.
///
/// **It is never a gate.** `#[ignore]`d, keyed on `OPENROUTER_API_KEY` being
/// present, and no acceptance criterion in this release is evidenced by it. CI does
/// not run it and must not: a test that needs a key and a network is a test whose
/// failure says nothing about the code.
///
/// It exists because the deterministic arm answers what a request *costs* and
/// cannot answer what an answer is *worth*. A recording fixes the model's replies
/// by construction, which is what makes the scores stable and what stops them
/// saying anything about quality. The questions that need a model — does a
/// graduated projection produce a better answer than a fold, does `ModelApprover`
/// catch an injected dangerous act — are answered here or not at all.
///
/// Run it with:
///
/// ```text
/// set -a && . ./.env && set +a
/// cargo test --test eval -- --ignored --nocapture n5_the_live_arm
/// ```
#[tokio::test]
#[ignore]
async fn n5_the_live_arm_scores_a_case_against_a_real_provider() {
    let Ok(key) = std::env::var("OPENROUTER_API_KEY") else {
        println!("no OPENROUTER_API_KEY; the live arm is skipped, which is not a pass");
        return;
    };
    let dir = workspace();
    let provider = io_harness::provider::OpenRouter::new(key, "openai/gpt-4o-mini");

    let report = Suite::new()
        .with_case(
            Case::new(
                "a real model reads the readme and writes a file",
                contract(dir.path()),
            )
            .needing(["README.md"])
            // A live model chooses its own path, so the step count is a
            // ceiling here rather than a claim. The case is scored, not
            // decided.
            .expecting(true, 2),
        )
        .with_scorer(PromptTokens)
        .with_scorer(Retention)
        .with_scorer(CatalogueCost)
        .run(&provider, &Policy::permissive(), &ApproveAll)
        .await
        .unwrap();

    for score in &report[0].scores {
        println!("{}: {} — {}", score.scorer, score.value, score.detail);
    }
    println!(
        "case {}: {}",
        report[0].case,
        if report[0].passed {
            "as declared".to_string()
        } else {
            report[0].why.clone()
        }
    );
}

/// A tool spec that exists only to keep the catalogue scorer honest about what it
/// measures: the serialised array, not the tool count.
#[test]
fn f5_the_catalogue_scorer_measures_bytes_rather_than_tool_count() {
    let case = Case::new(
        "two shapes, one count",
        TaskContract::workspace("go", "/repo"),
    );
    let short = Transcript {
        requests: vec![CompletionRequest {
            tools: vec![ToolSpec {
                name: "a".into(),
                description: "b".into(),
                parameters: json!({}),
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let long = Transcript {
        requests: vec![CompletionRequest {
            tools: vec![ToolSpec {
                name: "a".into(),
                description: "a paragraph of description that a schema plus one \
                              sentence would not have needed at all"
                    .into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    assert!(
        CatalogueCost.score(&case, &long).value > CatalogueCost.score(&case, &short).value,
        "one tool with a paragraph costs more than one tool with a sentence, which \
         is the whole argument for a description budget"
    );
}
