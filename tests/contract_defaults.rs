//! 0.71.0 — the two step budgets and the retry budget a contract is born with
//! are public constants, and the constructors read them.
//!
//! Every assertion here compares a constant against what a **constructor
//! produces**, never against the literal the constructor used to carry. A test
//! that reads `assert_eq!(DEFAULT_MAX_STEPS, 8)` proves only that a number
//! equals itself: replace the constructor's `DEFAULT_MAX_STEPS` with a bare `3`
//! and that test still passes while every caller's step budget silently changed.
//! That is the drift this release exists to end, so it is the thing asserted.

use io_harness::{
    OutputSchema, TaskContract, Verification, DEFAULT_MAX_RETRIES, DEFAULT_MAX_STEPS,
    DEFAULT_WORKSPACE_MAX_STEPS,
};

fn single() -> TaskContract {
    TaskContract::new("fix the parser", "src/parse.rs", Verification::None)
}

fn workspace() -> TaskContract {
    TaskContract::workspace("port the parser", "/tmp/repo")
}

#[test]
fn new_takes_its_step_budget_from_the_constant() {
    assert_eq!(single().max_steps, DEFAULT_MAX_STEPS);
}

#[test]
fn workspace_takes_its_step_budget_from_the_constant() {
    assert_eq!(workspace().max_steps, DEFAULT_WORKSPACE_MAX_STEPS);
}

#[test]
fn both_constructors_take_the_same_retry_budget_from_the_constant() {
    assert_eq!(single().max_retries, DEFAULT_MAX_RETRIES);
    assert_eq!(workspace().max_retries, DEFAULT_MAX_RETRIES);
}

/// The split is deliberate — a repo task spends turns finding the files a
/// single-file task is handed — so the two budgets are asserted to be different,
/// from the constructors themselves. Collapsing them into one constant would
/// silently change the budget of every existing caller of one constructor.
#[test]
fn the_two_step_budgets_stay_distinct() {
    assert!(
        workspace().max_steps > single().max_steps,
        "a workspace task must start with more room than a single-file one"
    );
}

/// What the constants are actually for: asking for a budget relative to the
/// default without hard-coding what the default is.
#[test]
fn a_caller_can_scale_the_default_without_knowing_it() {
    let patient = single().with_max_steps(DEFAULT_MAX_STEPS * 2);
    assert_eq!(patient.max_steps, single().max_steps * 2);
}

/// 0.77.0, F1. Declaring no output schema is the default, from both
/// constructors, and the default is what every release before 0.77.0 did: no
/// declaration reaches a vendor and nothing is validated locally.
///
/// Asserted from the constructors rather than from `Default`, for the reason
/// this whole file exists — a constructor that stopped reading the default is
/// the drift being watched for, not the literal it was written with.
#[test]
fn neither_constructor_declares_an_output_schema() {
    assert!(single().output_schema.is_none());
    assert!(workspace().output_schema.is_none());
}

/// And declaring one is the only way to get one. A schema on the contract came
/// from a caller saying so, never from a default that happened to be set
/// somewhere — which is what makes the absence above a real negative control
/// rather than a coincidence of construction order.
#[test]
fn a_declared_output_schema_is_the_one_the_caller_built() {
    let document = serde_json::json!({
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
    });
    let schema = OutputSchema::new(document.clone()).expect("a supported schema");

    let declared = single().with_output_schema(schema);

    assert_eq!(
        declared.output_schema.as_ref().map(OutputSchema::as_value),
        Some(&document),
        "the document sent to a vendor must be the one the caller wrote"
    );
}
