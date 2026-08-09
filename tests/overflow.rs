//! A context overflow is classified, and answered by compacting and re-asking
//! once (0.43.0).
//!
//! Every vendor reports an over-window request as a plain 4xx, which
//! `ProviderErrorKind::from_status` correctly calls terminal: the server has read
//! these exact bytes and refused them. That reasoning is right and its conclusion
//! is wrong for this one case, because the answer is not to resend the same bytes
//! — it is to send fewer of them.
//!
//! So the release adds a kind, and the loop adds a recovery. `F6` is the half
//! that matters most in the wrong direction: a classifier that swallowed an
//! ordinary 400 would make the loop compact and re-send a request the server had
//! already read and refused, which is worse than the failure it replaces.

use io_harness::ProviderErrorKind;

// ---------------------------------------------------------------- F6

#[test]
fn an_over_window_rejection_is_told_apart_from_every_other_rejection() {
    // The wordings the three built-in wires actually send.
    for message in [
        "This model's maximum context length is 8192 tokens, however you requested 9001",
        "context_length_exceeded",
        "prompt is too long: 250000 tokens > 200000 maximum",
        "Please reduce the length of the messages.",
        "input exceeds the context window",
    ] {
        assert_eq!(
            ProviderErrorKind::from_response(400, message),
            ProviderErrorKind::ContextOverflow,
            "{message}"
        );
    }
    // 413 as well: some gateways answer with it rather than a 400.
    assert_eq!(
        ProviderErrorKind::from_response(413, "maximum context length"),
        ProviderErrorKind::ContextOverflow
    );
}

#[test]
fn an_ordinary_400_is_left_exactly_where_it_was() {
    // The negative control, and the one that must not move. A false positive here
    // makes the loop compact and re-send a request the server already refused.
    for message in [
        "unknown parameter: temperture",
        "invalid tool schema for `write_file`",
        "messages: at least one message is required",
        "",
    ] {
        assert_eq!(
            ProviderErrorKind::from_response(400, message),
            ProviderErrorKind::Request,
            "{message:?}"
        );
    }
}

#[test]
fn a_status_that_means_something_else_is_not_reclassified_by_its_wording() {
    // Even carrying a signature verbatim: a 429 is a rate limit whatever it says,
    // and a 500 is the server's own admission of fault.
    assert_eq!(
        ProviderErrorKind::from_response(429, "too many tokens"),
        ProviderErrorKind::RateLimited
    );
    assert_eq!(
        ProviderErrorKind::from_response(500, "maximum context length"),
        ProviderErrorKind::Server
    );
    assert_eq!(
        ProviderErrorKind::from_response(401, "context window"),
        ProviderErrorKind::Auth
    );
}

#[test]
fn from_status_behaves_exactly_as_it_did_on_0_42_0() {
    // `from_response` is new; `from_status` is not, and nothing about it moved.
    // Asserted over the whole range it maps rather than at a few points.
    for status in 400u16..600 {
        let expected = match status {
            429 => ProviderErrorKind::RateLimited,
            401 | 403 => ProviderErrorKind::Auth,
            500..=599 => ProviderErrorKind::Server,
            _ => ProviderErrorKind::Request,
        };
        assert_eq!(ProviderErrorKind::from_status(status), expected, "{status}");
        // And with no signature in the message the two agree everywhere.
        assert_eq!(
            ProviderErrorKind::from_response(status, "something went wrong"),
            expected,
            "{status}"
        );
    }
}

#[test]
fn an_overflow_is_not_retryable_and_the_reason_is_the_point() {
    assert!(!ProviderErrorKind::ContextOverflow.is_retryable());
    // The other terminal kinds keep their answers, and the retryable ones keep
    // theirs — this release changed the set by exactly one member.
    assert!(!ProviderErrorKind::Auth.is_retryable());
    assert!(!ProviderErrorKind::Request.is_retryable());
    for kind in [
        ProviderErrorKind::Transport,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::RateLimited,
        ProviderErrorKind::Server,
        ProviderErrorKind::Malformed,
    ] {
        assert!(kind.is_retryable(), "{kind:?}");
    }
}

#[test]
fn the_classification_reaches_the_error_every_provider_builds() {
    // The funnel: no built-in provider classifies a status itself, so this is
    // where the three wires are proven not to have drifted apart.
    let over = io_harness::Error::provider_status(400, None, "maximum context length is 8192");
    match over {
        io_harness::Error::Provider { kind, status, .. } => {
            assert_eq!(kind, ProviderErrorKind::ContextOverflow);
            assert_eq!(status, Some(400));
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    let ordinary = io_harness::Error::provider_status(400, None, "unknown parameter");
    match ordinary {
        io_harness::Error::Provider { kind, .. } => assert_eq!(kind, ProviderErrorKind::Request),
        other => panic!("expected a provider error, got {other:?}"),
    }
}

// ---------------------------------------------------------------- the recovery

mod the_recovery {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
    use io_harness::{
        run_with, ApproveAll, Compaction, ContextBudget, EventKind, Flow, Observer, Policy,
        Provider, RunEvent, RunOutcome, Store, TaskContract, Verification,
    };
    use serde_json::json;

    const SUMMARISER: &str = "compacting an agent's own working notes";

    /// Refuses any request whose user block is over `ceiling` chars, with the
    /// wording a vendor sends, and serves anything under it. So a recovery is a
    /// real change in what was sent rather than a re-send that happened to work.
    struct Fussy {
        steps: Vec<Vec<ToolCall>>,
        at: AtomicUsize,
        ceiling: usize,
        /// Every user block it was handed, refused or served.
        seen: Arc<Mutex<Vec<usize>>>,
        refusals: Arc<AtomicUsize>,
        /// When true, refuses whatever the size — nothing can be made to fit.
        implacable: bool,
    }

    impl Fussy {
        fn new(steps: Vec<Vec<ToolCall>>, ceiling: usize) -> Self {
            Self {
                steps,
                at: AtomicUsize::new(0),
                ceiling,
                seen: Arc::new(Mutex::new(Vec::new())),
                refusals: Arc::new(AtomicUsize::new(0)),
                implacable: false,
            }
        }

        fn implacable(steps: Vec<Vec<ToolCall>>) -> Self {
            Self {
                implacable: true,
                ..Self::new(steps, 0)
            }
        }
    }

    impl Provider for Fussy {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let usage = Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            });
            // A summarising request is always served: it is the recovery, and a
            // provider that refused it would be refusing the way out.
            if req.system.contains(SUMMARISER) {
                return Ok(CompletionResponse {
                    text: Some("Read four files; nothing decided yet; the port is open.".into()),
                    usage,
                    ..Default::default()
                });
            }
            self.seen.lock().unwrap().push(req.user.len());
            if self.implacable || req.user.len() > self.ceiling {
                self.refusals.fetch_add(1, Ordering::SeqCst);
                return Err(io_harness::Error::provider_status(
                    400,
                    None,
                    "This model's maximum context length is 8192 tokens, however you requested more",
                ));
            }
            let i = self.at.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
                usage,
                ..Default::default()
            })
        }
    }

    #[derive(Default)]
    struct Folds(Arc<Mutex<Vec<u32>>>);

    impl Observer for Folds {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Compacted { through_step, .. } = &event.kind {
                self.0.lock().unwrap().push(*through_step);
            }
            Flow::Continue
        }
    }

    const NAMES: [&str; 6] = [
        "alpha.txt",
        "beta.txt",
        "gamma.txt",
        "delta.txt",
        "epsilon.txt",
        "zeta.txt",
    ];

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in NAMES {
            std::fs::write(
                dir.path().join(name),
                format!("{name}\n{}", "padding line\n".repeat(120)),
            )
            .unwrap();
        }
        dir
    }

    fn script() -> Vec<Vec<ToolCall>> {
        NAMES
            .iter()
            .map(|n| {
                vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": n }),
                }]
            })
            .collect()
    }

    fn contract(root: &std::path::Path) -> TaskContract {
        TaskContract::workspace("read the files and report", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "unreachable.txt".into(),
                needle: "never".into(),
            })
            .with_max_steps(NAMES.len() as u32)
            // Large enough that the *threshold* never fires: the only thing that
            // can make this run fold is the provider refusing the request, which
            // is what F4 is about.
            .with_context_budget(ContextBudget::default())
            // One kept whole: a forced fold has to be able to remove enough for
            // the second request to be materially smaller than the refused one.
            .with_compaction(Compaction {
                at_share: 0.8,
                keep_recent: 1,
            })
            .with_max_retries(0)
    }

    fn open_policy() -> Policy {
        Policy::default()
            .layer("test")
            .allow_read("*")
            .allow_write("*")
    }

    // ------------------------------------------------------------------ F4

    /// F4 — an overflow is classified, and the run survives it.
    ///
    /// The provider refuses anything over a byte ceiling and serves anything
    /// under it, so the request that succeeded is provably smaller than the one
    /// that failed — a recovery, not a re-send that got lucky.
    #[tokio::test]
    async fn a_request_that_did_not_fit_is_compacted_and_asked_again() {
        let dir = workspace();
        // Crossed only once several reads have accumulated, so the fold has
        // something to remove when it happens.
        let provider = Fussy::new(script(), 5_000);
        let store = Store::memory().unwrap();
        let folds = Folds::default();
        let seen_folds = Arc::clone(&folds.0);

        let result = io_harness::run_with_observed(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &folds,
        )
        .await
        .unwrap();

        let refusals = provider.refusals.load(Ordering::SeqCst);
        assert!(
            refusals > 0,
            "the provider never refused; nothing was recovered from"
        );
        assert!(
            !seen_folds.lock().unwrap().is_empty(),
            "a refusal produced no fold"
        );
        assert!(
            !matches!(result.outcome, RunOutcome::Escalated { .. }),
            "the run died on a request it could have made smaller: {:?}",
            result.outcome
        );

        // The discriminating pair: the request after the refusal is smaller than
        // the one refused.
        let sizes = provider.seen.lock().unwrap().clone();
        let refused_at = sizes
            .iter()
            .position(|n| *n > 5_000)
            .expect("no request was over the ceiling");
        let after = sizes
            .get(refused_at + 1)
            .copied()
            .expect("nothing was sent after the refusal");
        assert!(
            after < sizes[refused_at],
            "the retry sent {after} chars against the refused {}",
            sizes[refused_at]
        );

        // And the fold is a durable row, not only an event.
        assert!(!store.summaries(result.run_id).unwrap().is_empty());
    }

    /// F4's negative control — a run whose requests all fit never compacts, and
    /// the provider is never refused.
    #[tokio::test]
    async fn a_run_under_the_ceiling_never_compacts() {
        let dir = workspace();
        let provider = Fussy::new(script(), 1_000_000);
        let store = Store::memory().unwrap();
        let folds = Folds::default();
        let seen_folds = Arc::clone(&folds.0);

        let result = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

        assert_eq!(provider.refusals.load(Ordering::SeqCst), 0);
        assert!(seen_folds.lock().unwrap().is_empty());
        assert!(store.summaries(result.run_id).unwrap().is_empty());
        assert!(!matches!(result.outcome, RunOutcome::Escalated { .. }));
    }

    // ------------------------------------------------------------------ F5

    /// F5 — the recovery happens once.
    ///
    /// A provider that refuses every request whatever its size: the run escalates
    /// after exactly two working attempts for that step and one fold, not a loop.
    #[tokio::test]
    async fn a_request_that_cannot_be_made_to_fit_escalates_after_one_recovery() {
        let dir = workspace();
        let provider = Fussy::implacable(script());
        let store = Store::memory().unwrap();

        let failed = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await;

        assert!(
            failed.is_err(),
            "an unanswerable request must still escalate"
        );
        assert_eq!(
            provider.refusals.load(Ordering::SeqCst),
            2,
            "the step asked more than twice, or gave up without recovering"
        );

        let run_id = store.last_run().unwrap().unwrap();
        let outcome = store.outcome(run_id).unwrap().unwrap();
        assert!(
            outcome.starts_with("escalated"),
            "the run should end escalated, not {outcome}"
        );
    }

    /// F5's other half — with folding off, an overflow is terminal on the first
    /// refusal, exactly as it was on 0.42.0.
    #[tokio::test]
    async fn with_folding_off_an_overflow_is_terminal_at_once() {
        let dir = workspace();
        let provider = Fussy::implacable(script());
        let store = Store::memory().unwrap();

        let failed = run_with(
            &contract(dir.path()).with_compaction(Compaction {
                at_share: 1.0,
                ..Compaction::default()
            }),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await;

        assert!(failed.is_err());
        assert_eq!(
            provider.refusals.load(Ordering::SeqCst),
            1,
            "a caller who turned folding off asked for 0.42.0's behaviour and got a second attempt"
        );
    }
}
