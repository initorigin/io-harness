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
    assert_eq!(folding.threshold_tokens(budget.effective_tokens(None)), 19_200);

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
        resume_with, run_with, ApproveAll, Compaction, ContextBudget, EventKind, Flow, Observer, Policy,
        Provider, RunEvent, Store, TaskContract, Verification,
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
        "alpha.txt", "beta.txt", "gamma.txt", "delta.txt", "epsilon.txt", "zeta.txt", "eta.txt",
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
        Policy::default().layer("test").allow_read("*").allow_write("*")
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
            &contract(dir.path(), Compaction { at_share: 0.8, keep_recent: 2 }),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &folds,
        )
        .await
        .unwrap();

        let folded = seen.lock().unwrap().clone();
        assert!(!folded.is_empty(), "the run never folded; nothing below is meaningful");
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
            &contract(dir.path(), Compaction { at_share: 1.0, ..Compaction::default() }),
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
        assert!(dropped.is_err(), "the first pass was meant to be dropped mid-step");
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
            &contract(dir.path(), Compaction { at_share: 0.8, keep_recent: 2 }),
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
            calls.iter().filter_map(|c| c.usage).map(|u| u.total_tokens).sum::<u64>(),
            "the fold's tokens are inside what the run is billed"
        );
    }
}
