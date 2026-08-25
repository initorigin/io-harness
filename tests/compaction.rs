//! Summarising compaction (0.43.0): when the run's history is folded into a
//! written paragraph, and what happens when it is not.
//!
//! The threshold half of `F2` — "under the threshold, nothing happens" — is
//! asserted here against the setting itself, because a knob that reads as "never"
//! and computes as "always" is the failure mode a run-level test would find
//! expensively and late. The run-level half, where a real loop folds or does not,
//! is `the_fold` module below.

use io_harness::{Compaction, ContextBudget, TaskContract};

#[test]
fn compaction_is_on_by_default_and_says_so() {
    let folding = Compaction::default();
    assert_eq!(folding.at_share, 0.8);
    assert_eq!(folding.keep_recent, 8);
    assert!(folding.enabled());

    // And a contract built the ordinary way carries it, which is the half that
    // decides whether the default is a default or a decoration.
    let contract = TaskContract::workspace("port the parser", "/repo");
    assert!(contract.compaction.enabled());
    assert_eq!(contract.compaction, Compaction::default());
}

#[test]
fn a_share_of_one_never_folds_and_is_0_42_0s_behaviour_exactly() {
    let never = Compaction {
        at_share: 1.0,
        ..Compaction::default()
    };
    assert!(!never.enabled());
    // Unreachable by construction: the assembler bounds the section at the
    // budget, so a ledger cannot exceed the whole of it.
    assert_eq!(never.threshold_tokens(24_000), u64::MAX);

    let contract = TaskContract::workspace("port the parser", "/repo").with_compaction(never);
    assert!(!contract.compaction.enabled());
}

#[test]
fn a_nonsense_share_reads_as_never_rather_than_as_always() {
    // `NaN` compares false against everything, so a threshold derived from it
    // would be crossed by nothing — or, with the comparison the other way round,
    // by everything. Both are wrong; "never" is the one that cannot spend money.
    for share in [f32::NAN, f32::INFINITY, -0.5, 0.0, 4.0] {
        let odd = Compaction {
            at_share: share,
            ..Compaction::default()
        };
        assert!(!odd.enabled(), "{share} must not enable a fold");
        assert_eq!(odd.threshold_tokens(24_000), u64::MAX, "{share}");
    }
}

#[test]
fn the_threshold_is_taken_from_the_budget_the_assembler_will_use() {
    let budget = ContextBudget::default();
    let folding = Compaction::default();

    // No run token budget: the section's ceiling is `max_tokens` flat, so the
    // fold happens at 80% of that and not at 80% of some other number.
    assert_eq!(budget.effective_tokens(None), 24_000);
    assert_eq!(
        folding.threshold_tokens(budget.effective_tokens(None)),
        19_200
    );

    // With a run running low, the ceiling shrinks and the fold threshold shrinks
    // with it — the two cannot drift apart because there is one derivation.
    assert_eq!(budget.effective_tokens(Some(20_000)), 10_000);
    assert_eq!(
        folding.threshold_tokens(budget.effective_tokens(Some(20_000))),
        8_000
    );
}

#[test]
fn keeping_nothing_recent_is_floored_at_one() {
    // A fold that kept nothing whole would hand the model a paragraph about work
    // it can no longer see.
    let greedy = Compaction {
        at_share: 0.5,
        keep_recent: 0,
    };
    assert_eq!(greedy.keep(), 1);
    assert_eq!(Compaction::default().keep(), 8);
}

#[test]
fn compaction_round_trips_through_a_config_file() {
    // Both fields default, like `ContextBudget`, so an operator can set one and
    // leave the other alone.
    let tight: Compaction = serde_json::from_str(r#"{"at_share": 0.6}"#).unwrap();
    assert_eq!(tight.at_share, 0.6);
    assert_eq!(tight.keep_recent, Compaction::default().keep_recent);

    let round: Compaction =
        serde_json::from_str(&serde_json::to_string(&Compaction::default()).unwrap()).unwrap();
    assert_eq!(round, Compaction::default());

    // `deny_unknown_fields`, so a misspelled key is refused rather than ignored —
    // a fold silently never happening is the failure this crate refuses to ship.
    assert!(serde_json::from_str::<Compaction>(r#"{"at_shore": 0.6}"#).is_err());
}

// ---------------------------------------------------------------- the fold

mod the_fold {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
    use io_harness::{
        resume_with, run_with, ApproveAll, Compaction, ContextBudget, EventKind, Flow, Observer,
        Policy, Provider, RunEvent, Store, TaskContract, Verification,
    };
    use serde_json::json;

    /// The one sentence the summarising model writes. Distinctive so a later
    /// request can be asked whether it carries the summary rather than merely
    /// something summary-shaped.
    const SUMMARY_SENTENCE: &str = "ZZ-SUMMARY-ZZ read alpha.txt and kept the token enum.";

    /// A string that appears in exactly one observation — the oldest — and
    /// nowhere else. The fold's discriminating assertion is that a later request
    /// carries the summary and not this.
    const MARKER: &str = "QQ-ONLY-IN-THE-OLDEST-READ-QQ";

    /// How the summarising request is told apart from a working one, without the
    /// test re-implementing the prompt: the system block the crate sends says
    /// this and no working request does.
    const SUMMARISER: &str = "compacting an agent's own working notes";

    /// Records every request, answers a summarising one with [`SUMMARY_SENTENCE`]
    /// and a working one from a script.
    struct Recorder {
        steps: Vec<Vec<ToolCall>>,
        at: AtomicUsize,
        /// Every `(system, user)` the provider was handed, in order.
        seen: Arc<Mutex<Vec<(String, String)>>>,
        /// How many of those were summarising calls.
        summarised: Arc<AtomicUsize>,
        /// A working-call index at which the completion never returns, so the run
        /// can be dropped mid-step and left `running` — the one state `resume`
        /// will drive, since every clean stop is terminal to it by design.
        park_at: Option<usize>,
    }

    impl Recorder {
        fn new(steps: Vec<Vec<ToolCall>>) -> Self {
            Self {
                steps,
                at: AtomicUsize::new(0),
                seen: Arc::new(Mutex::new(Vec::new())),
                summarised: Arc::new(AtomicUsize::new(0)),
                park_at: None,
            }
        }

        fn parking_at(mut self, call: usize) -> Self {
            self.park_at = Some(call);
            self
        }

        /// Requests that were not the summariser's, in order.
        fn working(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(system, _)| !system.contains(SUMMARISER))
                .map(|(_, user)| user.clone())
                .collect()
        }

        fn summarising(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(system, _)| system.contains(SUMMARISER))
                .map(|(_, user)| user.clone())
                .collect()
        }
    }

    impl Provider for Recorder {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let summarising = req.system.contains(SUMMARISER);
            self.seen
                .lock()
                .unwrap()
                .push((req.system.clone(), req.user.clone()));
            let usage = Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            });
            if summarising {
                self.summarised.fetch_add(1, Ordering::SeqCst);
                return Ok(CompletionResponse {
                    text: Some(SUMMARY_SENTENCE.into()),
                    usage,
                    ..Default::default()
                });
            }
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            if self.park_at == Some(i) {
                // Never returns. The caller drops the run's future instead, which
                // is the only interruption that leaves a run resumable.
                std::future::pending::<()>().await;
            }
            Ok(CompletionResponse {
                tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
                usage,
                ..Default::default()
            })
        }
    }

    /// Counts folds off the event stream rather than off the store, so the two
    /// halves of the claim are independent.
    #[derive(Default)]
    struct Folds(Arc<Mutex<Vec<(u32, u64, u64)>>>);

    impl Observer for Folds {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Compacted {
                through_step,
                before_tokens,
                after_tokens,
            } = &event.kind
            {
                self.0
                    .lock()
                    .unwrap()
                    .push((*through_step, *before_tokens, *after_tokens));
            }
            Flow::Continue
        }
    }

    fn read(path: &str) -> ToolCall {
        ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": path }),
        }
    }

    /// A workspace of files large enough that a handful of reads crosses a small
    /// budget. `alpha.txt` is the only one carrying [`MARKER`].
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (i, name) in NAMES.iter().enumerate() {
            let body = if i == 0 {
                format!("{MARKER}\n{}", "alpha padding line\n".repeat(90))
            } else {
                format!("{name} padding\n{}", "ordinary padding line\n".repeat(90))
            };
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    const NAMES: [&str; 8] = [
        "alpha.txt",
        "beta.txt",
        "gamma.txt",
        "delta.txt",
        "epsilon.txt",
        "zeta.txt",
        "eta.txt",
        "theta.txt",
    ];

    fn script() -> Vec<Vec<ToolCall>> {
        NAMES.iter().map(|n| vec![read(n)]).collect()
    }

    /// Never satisfied, so the loop runs its whole step budget.
    fn contract(root: &std::path::Path, folding: Compaction) -> TaskContract {
        TaskContract::workspace("read the files and report", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "unreachable.txt".into(),
                needle: "never".into(),
            })
            .with_max_steps(NAMES.len() as u32)
            // Small enough that a few reads cross it, so the fold is reached
            // without a test that has to be large to be slow.
            .with_context_budget(ContextBudget {
                max_tokens: 2_000,
                share: 0.5,
            })
            .with_compaction(folding)
    }

    fn open_policy() -> Policy {
        Policy::default()
            .layer("test")
            .allow_read("*")
            .allow_write("*")
    }

    // ------------------------------------------------------------------ F1

    /// F1 — the fold replaces the oldest observations with a written summary, and
    /// the next request carries it.
    ///
    /// The pair is what discriminates. Either half alone is satisfied by
    /// stubbing: a request that lacks `MARKER` proves only that *something*
    /// dropped the observation, which is what 0.42.0 already did. It is the
    /// summary's own sentence being present in the same request that says the
    /// fold wrote a paragraph rather than a byte count.
    #[tokio::test]
    async fn a_fold_writes_a_summary_and_the_next_request_carries_it() {
        let dir = workspace();
        let provider = Recorder::new(script());
        let store = Store::memory().unwrap();
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);

        let result = io_harness::run_with_observed(
            &contract(
                dir.path(),
                Compaction {
                    at_share: 0.8,
                    keep_recent: 2,
                },
            ),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &folds,
        )
        .await
        .unwrap();

        let folded = seen.lock().unwrap().clone();
        assert!(
            !folded.is_empty(),
            "the run never folded; nothing below is meaningful"
        );
        assert!(
            folded[0].2 < folded[0].1,
            "a fold must shrink the section: {folded:?}"
        );

        // Half one: the summarising model was handed the observation being folded.
        let summarising = provider.summarising();
        assert!(
            summarising.iter().any(|user| user.contains(MARKER)),
            "the summariser was never shown the oldest read"
        );

        // Half two, and the discriminating one: a later working request carries
        // the summary's own sentence, and no longer carries the folded text.
        let working = provider.working();
        let after = working
            .iter()
            .rev()
            .find(|user| user.contains(SUMMARY_SENTENCE))
            .expect("no request after the fold carried the summary");
        assert!(
            !after.contains(MARKER),
            "the folded observation is still being re-sent whole"
        );

        // And the durable half agrees with the event.
        let rows = store.summaries(result.run_id).unwrap();
        assert_eq!(rows.len(), folded.len(), "one row per fold");
        assert_eq!(rows[0].through_step, folded[0].0);
        assert_eq!(rows[0].text, SUMMARY_SENTENCE);
    }

    // ------------------------------------------------------------------ F2

    /// F2, the run-level half — under the threshold nothing happens and nothing
    /// is spent.
    ///
    /// The same workspace, the same script, the same store, and the fold switched
    /// off: zero rows, zero events, and the provider asked exactly as many times
    /// as there were steps.
    #[tokio::test]
    async fn with_folding_off_nothing_folds_and_nothing_extra_is_spent() {
        let dir = workspace();
        let provider = Recorder::new(script());
        let store = Store::memory().unwrap();
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);

        let result = io_harness::run_with_observed(
            &contract(
                dir.path(),
                Compaction {
                    at_share: 1.0,
                    ..Compaction::default()
                },
            ),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &folds,
        )
        .await
        .unwrap();

        assert!(seen.lock().unwrap().is_empty(), "a disabled fold fired");
        assert!(store.summaries(result.run_id).unwrap().is_empty());
        assert_eq!(
            provider.summarised.load(Ordering::SeqCst),
            0,
            "a disabled fold called the provider"
        );
        assert_eq!(
            provider.working().len(),
            NAMES.len(),
            "one completion per step and not one more"
        );
    }

    /// F2, the other arm — a run whose ledger never crosses the share does not
    /// fold either, with folding fully on.
    #[tokio::test]
    async fn a_short_run_under_the_share_never_folds() {
        let dir = workspace();
        let provider = Recorder::new(vec![vec![read("alpha.txt")]]);
        let store = Store::memory().unwrap();

        let result = run_with(
            // A large budget and one read: the ledger cannot reach 80% of it.
            &TaskContract::workspace("read one file", dir.path())
                .with_verification(Verification::WorkspaceFileContains {
                    file: "unreachable.txt".into(),
                    needle: "never".into(),
                })
                .with_max_steps(1)
                .with_context_budget(ContextBudget::default()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

        assert!(store.summaries(result.run_id).unwrap().is_empty());
        assert_eq!(provider.summarised.load(Ordering::SeqCst), 0);
    }

    // ------------------------------------------------------------------ F3

    /// F3 — the summary is stored once and re-read.
    ///
    /// The run is cancelled at the step boundary right after it folds — the
    /// crate's one *resumable* stop, since `resume` treats every escalation as
    /// terminal by design — and then resumed under the same run id. The
    /// summarising model must have been called exactly once in total across both
    /// passes, and the resumed run's request must still carry the paragraph.
    ///
    /// What makes that true is `restore_ledger` replaying the stored folds. A
    /// resume that rebuilt the ledger from `ledger_observations` alone would hand
    /// the model back every observation the run had already paid to summarise, and
    /// then buy the same paragraph again the next time the threshold was crossed.
    #[tokio::test]
    async fn a_resumed_run_re_reads_the_summary_instead_of_buying_it_again() {
        let dir = workspace();
        let folding = Compaction {
            at_share: 0.8,
            keep_recent: 2,
        };
        let contract = contract(dir.path(), folding);
        let store = Store::memory().unwrap();

        // First pass: fold, then park forever on the completion of that same
        // step, and drop the run. The step never commits, the run row stays
        // `running`, and a resume re-enters the step the fold happened on.
        let first = Recorder::new(script()).parking_at(4);
        let dropped = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_with(&contract, &first, &store, &open_policy(), &ApproveAll),
        )
        .await;
        assert!(
            dropped.is_err(),
            "the first pass was meant to be dropped mid-step"
        );
        let run_id = store.last_run().unwrap().expect("a run row");
        let rows = store.summaries(run_id).unwrap();
        assert_eq!(rows.len(), 1, "one fold, one row: {rows:?}");
        assert_eq!(first.summarised.load(Ordering::SeqCst), 1);
        let fold_step = rows[0].through_step;

        // Second pass: same run id, and a provider that would happily write a
        // second paragraph if it were asked for one.
        // Bounded to one step past the one it re-enters. A longer resume would
        // fold again — legitimately, on a larger prefix — and "called once in
        // total" would stop being a statement about re-buying the same paragraph.
        let second = Recorder::new(script());
        let short = contract.clone().with_max_steps(fold_step + 1);
        resume_with(&short, &second, &store, run_id, &open_policy(), &ApproveAll)
            .await
            .unwrap();
        assert!(
            !second.working().is_empty(),
            "the resume drove no steps; nothing below is meaningful"
        );

        assert_eq!(
            second.summarised.load(Ordering::SeqCst),
            0,
            "the resume paid for the summary a second time"
        );
        assert_eq!(
            store.summaries(run_id).unwrap().len(),
            1,
            "the resume wrote a second row"
        );
        assert!(
            second
                .working()
                .iter()
                .any(|user| user.contains(SUMMARY_SENTENCE)),
            "the resumed run lost the summary it had already paid for"
        );
        // Nothing is asserted here about `MARKER`. The scripted provider restarts
        // its script on the resume, so the resumed run legitimately re-reads
        // `alpha.txt` and the marker reappears as a *new* observation. F1 is where
        // "the folded text stops being re-sent" is proven.
    }

    // ------------------------------------------------------------------ N4

    /// N4 — a fold costs one provider call, and it is where spend already is.
    ///
    /// Not a new counter and not a new table: the summarising call lands an
    /// ordinary `provider_calls` row for the step it happened in, and its tokens
    /// are inside `spent_tokens`. A fold that spent money invisibly is the one
    /// defect this release could ship without noticing.
    #[tokio::test]
    async fn a_fold_lands_one_ordinary_provider_call_row_and_is_billed() {
        let dir = workspace();
        let provider = Recorder::new(script());
        let store = Store::memory().unwrap();
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);

        let result = io_harness::run_with_observed(
            &contract(
                dir.path(),
                Compaction {
                    at_share: 0.8,
                    keep_recent: 2,
                },
            ),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &folds,
        )
        .await
        .unwrap();

        let folded = seen.lock().unwrap().clone();
        assert!(!folded.is_empty(), "the run never folded");

        let calls = store.provider_calls(result.run_id).unwrap();
        let working = provider.working().len();
        assert_eq!(
            calls.len(),
            working + folded.len(),
            "each fold is exactly one extra row: {} calls for {working} steps and {} folds",
            calls.len(),
            folded.len()
        );
        for (at, _, _) in &folded {
            assert_eq!(
                calls.iter().filter(|c| c.step == *at).count(),
                2,
                "the fold's row is filed under the step it happened in, not a step of its own"
            );
        }
        assert_eq!(
            store.spent_tokens(result.run_id).unwrap(),
            calls
                .iter()
                .filter_map(|c| c.usage)
                .map(|u| u.total_tokens)
                .sum::<u64>(),
            "the fold's tokens are inside what the run is billed"
        );
    }
}

// ------------------------------------------------------- the fold that was asked for

/// Caller-triggered compaction (0.68.0): `TaskContract::fold_now`.
///
/// Every test here drives a **session turn** rather than a bare run, because the
/// thing an operator asks to fold is the conversation, and a conversation only
/// reaches a ledger through `seed_conversation`. A run-level fixture would fold
/// this run's own reads and prove nothing about the case the release exists for.
///
/// The threshold is deliberately left far out of reach in every test but the
/// overflow one. If the ledger crossed it, a fold would happen whether or not
/// anybody asked, and none of these assertions would discriminate.
mod requested {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
    use io_harness::{
        ApproveAll, Compaction, ContextBudget, EventKind, Flow, Observer, Policy, Provider,
        RunEvent, Session, Store, TaskContract, Verification,
    };
    use serde_json::json;

    const SUMMARISER: &str = "compacting an agent's own working notes";
    const SUMMARY_SENTENCE: &str = "YY-ASKED-FOR-IT-YY the thread so far was about the parser.";
    /// Appears in the oldest turn of the conversation and nowhere else, so "the
    /// seed was folded" is distinguishable from "the seed was dropped".
    const MARKER: &str = "WW-OLDEST-TURN-ONLY-WW";

    /// Answers a summarising request with [`SUMMARY_SENTENCE`], a conversational
    /// one with a sentence, and a working one from a script. Records every
    /// request so a test can ask what the *first* working one carried.
    struct Talker {
        steps: Vec<Vec<ToolCall>>,
        at: AtomicUsize,
        seen: Arc<Mutex<Vec<(String, String)>>>,
        summarised: Arc<AtomicUsize>,
    }

    impl Talker {
        fn new(steps: Vec<Vec<ToolCall>>) -> Self {
            Self {
                steps,
                at: AtomicUsize::new(0),
                seen: Arc::new(Mutex::new(Vec::new())),
                summarised: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The user blocks of the requests that were not the summariser's, in
        /// order. `working()[0]` is the turn's first request, which is the one
        /// this release is about.
        fn working(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(system, _)| !system.contains(SUMMARISER))
                .map(|(_, user)| user.clone())
                .collect()
        }

        fn summarising_calls(&self) -> usize {
            self.summarised.load(Ordering::SeqCst)
        }
    }

    impl Provider for Talker {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let summarising = req.system.contains(SUMMARISER);
            self.seen
                .lock()
                .unwrap()
                .push((req.system.clone(), req.user.clone()));
            let usage = Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            });
            if summarising {
                self.summarised.fetch_add(1, Ordering::SeqCst);
                return Ok(CompletionResponse {
                    text: Some(SUMMARY_SENTENCE.into()),
                    usage,
                    ..Default::default()
                });
            }
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            match self.steps.get(i) {
                Some(calls) => Ok(CompletionResponse {
                    tool_calls: calls.clone(),
                    usage,
                    ..Default::default()
                }),
                None => Ok(CompletionResponse {
                    text: Some("nothing further".into()),
                    usage,
                    ..Default::default()
                }),
            }
        }
    }

    #[derive(Default)]
    struct Folds(Arc<Mutex<Vec<(u32, u64, u64)>>>);

    impl Observer for Folds {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Compacted {
                through_step,
                before_tokens,
                after_tokens,
            } = &event.kind
            {
                self.0
                    .lock()
                    .unwrap()
                    .push((*through_step, *before_tokens, *after_tokens));
            }
            Flow::Continue
        }
    }

    fn read(path: &str) -> ToolCall {
        ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": path }),
        }
    }

    fn open_policy() -> Policy {
        Policy::default()
            .layer("test")
            .allow_read("*")
            .allow_write("*")
    }

    /// A workspace with one small file, so a working step has something to do
    /// that does not itself grow the ledger enough to cross a threshold.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "one short line\n").unwrap();
        dir
    }

    /// Six conversational turns, the oldest carrying [`MARKER`], so the next
    /// turn's seed is twelve observations deep.
    ///
    /// Conversational rather than working turns on purpose: what is being folded
    /// has to be the conversation, and this is how a conversation gets into a
    /// session.
    async fn converse(session: &mut Session, store: &Store, policy: &Policy) {
        for i in 0..6 {
            let prompt = if i == 0 {
                format!("{MARKER} where did we get to on the parser?")
            } else {
                format!("and then what happened at stage {i}?")
            };
            let talker = Talker::new(Vec::new());
            session
                .turn(&prompt, &talker, store, policy, &ApproveAll)
                .await
                .unwrap();
        }
    }

    /// The measured turn's contract. Verification is set and never satisfied, so
    /// the turn is work rather than a reply — a classifying turn answers before
    /// the loop and never reaches a fold at all.
    ///
    /// The context budget is the default, so the threshold is 19,200 tokens and a
    /// twelve-entry conversation is nowhere near it. Any fold in these tests is
    /// therefore one that was asked for.
    fn measured(root: &std::path::Path, fold_now: bool, folding: Compaction) -> TaskContract {
        TaskContract::workspace("summarise and continue", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "unreachable.txt".into(),
                needle: "never".into(),
            })
            .with_max_steps(1)
            .with_context_budget(ContextBudget::default())
            .with_compaction(folding)
            .with_fold_now(fold_now)
    }

    fn keeping_two() -> Compaction {
        Compaction {
            at_share: 0.8,
            keep_recent: 2,
        }
    }

    // ------------------------------------------------------------------ F1

    /// F1 — a requested fold lands before the turn's first request.
    ///
    /// Both halves matter and neither alone would do. "Folded" is satisfied by a
    /// threshold nobody asked about; "before the first request" is satisfied by a
    /// turn that never folded at all. The discriminating assertion is that
    /// `working()[0]` — the turn's very first request — already carries the
    /// summary and no longer carries the oldest turn's text, with the threshold
    /// set where it cannot have been the cause.
    #[tokio::test]
    async fn a_requested_fold_lands_before_the_turns_first_request() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let talker = Talker::new(vec![vec![read("notes.txt")]]);
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);
        session
            .turn_bounded_observed(
                &measured(dir.path(), true, keeping_two()),
                &talker,
                &store,
                &policy,
                &ApproveAll,
                &folds,
            )
            .await
            .unwrap();

        let folded = seen.lock().unwrap().clone();
        assert_eq!(
            folded.len(),
            1,
            "the request should have folded exactly once: {folded:?}"
        );
        assert!(
            folded[0].2 < folded[0].1,
            "a fold must shrink the section: {folded:?}"
        );

        let working = talker.working();
        let first = working
            .first()
            .expect("the turn made no working request at all");
        assert!(
            first.contains(SUMMARY_SENTENCE),
            "the turn's FIRST request did not carry the summary: {first}"
        );
        assert!(
            !first.contains(MARKER),
            "the folded conversation is still being sent whole in the first request"
        );
    }

    // ------------------------------------------------------------------ F2

    /// F2 — the same turn without the flag does not fold.
    ///
    /// The control F1 is meaningless without: same conversation, same contract,
    /// same threshold, `fold_now` off. If this folded, F1 would be measuring the
    /// threshold rather than the request.
    #[tokio::test]
    async fn without_the_flag_the_same_turn_does_not_fold() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let talker = Talker::new(vec![vec![read("notes.txt")]]);
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);
        session
            .turn_bounded_observed(
                &measured(dir.path(), false, keeping_two()),
                &talker,
                &store,
                &policy,
                &ApproveAll,
                &folds,
            )
            .await
            .unwrap();

        assert!(
            seen.lock().unwrap().is_empty(),
            "a turn nobody asked to fold folded anyway"
        );
        assert_eq!(
            talker.summarising_calls(),
            0,
            "the summariser was called for a fold nobody asked for"
        );
        let first = talker.working().first().cloned().unwrap_or_default();
        assert!(
            first.contains(MARKER),
            "the conversation should have reached the first request whole: {first}"
        );
    }

    // ------------------------------------------------------------------ F3

    /// F3 — an off setting stays off.
    ///
    /// `Compaction { at_share: 1.0, .. }` is 0.42.0's behaviour, and
    /// `docs/CONTRACT.md` promises that includes the overflow recovery. A
    /// caller-triggered fold is a third trigger for the same machinery, and the
    /// machinery is what was turned off — so the request is a no-op, and the turn
    /// proceeds rather than erroring.
    #[tokio::test]
    async fn a_requested_fold_does_not_override_an_off_setting() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let talker = Talker::new(vec![vec![read("notes.txt")]]);
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);
        let off = Compaction {
            at_share: 1.0,
            ..Compaction::default()
        };
        session
            .turn_bounded_observed(
                &measured(dir.path(), true, off),
                &talker,
                &store,
                &policy,
                &ApproveAll,
                &folds,
            )
            .await
            .expect("an off setting must be a no-op, not an error");

        assert!(
            seen.lock().unwrap().is_empty(),
            "folding was off and a fold happened anyway"
        );
        assert_eq!(
            talker.summarising_calls(),
            0,
            "an off setting still paid for a summariser"
        );
    }

    // ------------------------------------------------------------------ F4

    /// F4 — the request is honoured once, not every step.
    ///
    /// A flag on a contract is read once per step unless something consumes it,
    /// and a fold at every step of a run is the bug that shape invites. Three
    /// steps, one fold, with the threshold far enough away that a second fold
    /// could only have come from the flag being read again.
    #[tokio::test]
    async fn the_request_is_consumed_and_does_not_fold_every_step() {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let talker = Talker::new(vec![
            vec![read("notes.txt")],
            vec![read("notes.txt")],
            vec![read("notes.txt")],
        ]);
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);
        let contract = measured(dir.path(), true, keeping_two()).with_max_steps(3);
        session
            .turn_bounded_observed(
                &contract,
                &talker,
                &store,
                &policy,
                &ApproveAll,
                &folds,
            )
            .await
            .unwrap();

        let folded = seen.lock().unwrap().clone();
        assert_eq!(
            folded.len(),
            1,
            "one request, one fold — found {}: {folded:?}",
            folded.len()
        );
        assert_eq!(
            talker.summarising_calls(),
            1,
            "one fold should have cost exactly one summarising call"
        );
    }

    // ------------------------------------------------------------------ F7

    /// F7 — the seed becomes durable before the first step, and the watermark
    /// says so.
    ///
    /// Asserted directly rather than left to be inferred from F1, because F1 and
    /// F6 both rest on it and neither would say which of the two changes carried
    /// them. The observer opens its **own** connection — a `&Store` cannot cross
    /// into one, and two connections to one WAL file is the shape this crate
    /// already uses — and reads the run's observations at the moment the fold is
    /// announced, which is the moment `compact_ledger` ran.
    #[tokio::test]
    async fn the_seed_is_durable_before_the_first_step() {
        struct AtTheFold {
            path: std::path::PathBuf,
            /// The observations the store held when the fold was announced.
            texts: Arc<Mutex<Vec<String>>>,
        }

        impl Observer for AtTheFold {
            fn event(&self, event: &RunEvent) -> Flow {
                if matches!(event.kind, EventKind::Compacted { .. }) {
                    let store = Store::open(&self.path).expect("a second connection");
                    let seen = store.observations(event.run_id).expect("observations");
                    *self.texts.lock().unwrap() =
                        seen.into_iter().map(|o| o.text).collect::<Vec<_>>();
                }
                Flow::Continue
            }
        }

        let dir = workspace();
        // File-backed, because the assertion is that a SECOND reader can see the
        // rows — an in-memory store is private to its own connection and would
        // make this test pass by never being asked.
        let db = dir.path().join("runs.db");
        let store = Store::open(&db).unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();
        converse(&mut session, &store, &policy).await;

        let talker = Talker::new(vec![vec![read("notes.txt")]]);
        let watcher = AtTheFold {
            path: db.clone(),
            texts: Arc::new(Mutex::new(Vec::new())),
        };
        let texts = Arc::clone(&watcher.texts);
        session
            .turn_bounded_observed(
                &measured(dir.path(), true, keeping_two()),
                &talker,
                &store,
                &policy,
                &ApproveAll,
                &watcher,
            )
            .await
            .unwrap();

        let at_the_fold = texts.lock().unwrap().clone();
        assert!(
            !at_the_fold.is_empty(),
            "the fold never happened, so this asserts nothing"
        );
        assert!(
            at_the_fold.iter().any(|t| t.contains(MARKER)),
            "the seeded conversation was not durable when the fold ran: {at_the_fold:?}"
        );
        assert!(
            at_the_fold.len() >= 12,
            "twelve seeded observations were expected to be durable, found {}",
            at_the_fold.len()
        );
    }

    // ------------------------------------------------------------------ F6

    /// F6 — a seeded turn that overflows the window now recovers, and before this
    /// release it could not.
    ///
    /// This is the defect the release fixes, and it is nothing to do with the new
    /// flag. A fold may only replace entries the store already holds, and until
    /// 0.68.0 the seeded conversation sat **above** the watermark for the whole of
    /// step one — `written` was `0` until the first `persist_ledger`, which runs
    /// at the *end* of that step. So `compact_ledger` returned at its `count == 0`
    /// guard before `forced` was ever read, and the overflow recovery — whose
    /// entire job is to fold a request the vendor has just refused — folded
    /// nothing and re-sent the same bytes. A session turn whose conversation
    /// exceeded the window was unrecoverable, which is precisely the turn the
    /// recovery exists for.
    ///
    /// The assertion is the recovery working end to end: a refusal, a fold, and a
    /// second request that is smaller than the first and is served.
    #[tokio::test]
    async fn a_seeded_turn_that_overflows_folds_and_recovers() {
        /// Refuses a working request over `ceiling` chars with a vendor's own
        /// wording, and always serves the summariser — a provider that refused
        /// the summariser would be refusing the way out.
        struct Fussy {
            ceiling: usize,
            sizes: Arc<Mutex<Vec<usize>>>,
            refusals: Arc<AtomicUsize>,
        }

        impl Provider for Fussy {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> io_harness::Result<CompletionResponse> {
                let usage = Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    total_tokens: 12,
                    ..Default::default()
                });
                if req.system.contains(SUMMARISER) {
                    return Ok(CompletionResponse {
                        text: Some(SUMMARY_SENTENCE.into()),
                        usage,
                        ..Default::default()
                    });
                }
                self.sizes.lock().unwrap().push(req.user.len());
                if req.user.len() > self.ceiling {
                    self.refusals.fetch_add(1, Ordering::SeqCst);
                    return Err(io_harness::Error::provider_status(
                        400,
                        None,
                        "This model's maximum context length is 8192 tokens, however you requested more",
                    ));
                }
                Ok(CompletionResponse {
                    text: Some("nothing further".into()),
                    usage,
                    ..Default::default()
                })
            }
        }

        let dir = workspace();
        let store = Store::memory().unwrap();
        let policy = open_policy();
        let mut session = Session::open(&store, dir.path()).unwrap();

        // A conversation long enough that the seed alone is what overflows. Each
        // turn is padded, so the folded paragraph is dramatically smaller than
        // what it stands in for and the recovery has somewhere to get to.
        for i in 0..6 {
            let padding = "and we discussed the tokeniser at some length. ".repeat(30);
            let prompt = if i == 0 {
                format!("{MARKER} {padding}")
            } else {
                format!("stage {i}: {padding}")
            };
            let talker = Talker::new(Vec::new());
            session
                .turn(&prompt, &talker, &store, &policy, &ApproveAll)
                .await
                .unwrap();
        }

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let refusals = Arc::new(AtomicUsize::new(0));
        let fussy = Fussy {
            // Under the seeded conversation whole, over it once folded.
            ceiling: 2_000,
            sizes: Arc::clone(&sizes),
            refusals: Arc::clone(&refusals),
        };
        let folds = Folds::default();
        let seen = Arc::clone(&folds.0);

        // `fold_now` is off. The recovery is what asks for this fold, and the
        // release's claim is that it can now be answered.
        session
            .turn_bounded_observed(
                &measured(dir.path(), false, keeping_two()),
                &fussy,
                &store,
                &policy,
                &ApproveAll,
                &folds,
            )
            .await
            .expect("the turn should have recovered rather than escalated");

        assert_eq!(
            refusals.load(Ordering::SeqCst),
            1,
            "expected exactly one refusal, then a recovery"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the refusal should have forced exactly one fold"
        );
        let sizes = sizes.lock().unwrap().clone();
        assert_eq!(sizes.len(), 2, "a refusal and one retry: {sizes:?}");
        assert!(
            sizes[1] < sizes[0],
            "the retry must be smaller than what was refused: {sizes:?}"
        );
    }
}
