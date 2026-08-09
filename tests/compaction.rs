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
