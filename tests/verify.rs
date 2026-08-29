//! A provider's credential never reaches a formatter, and holding a provider
//! never demands one (0.70.0, F9).
//!
//! Two claims, and they are independent — which is why both halves are here.
//!
//! The first is a **leak that had already shipped**: [`Compatible`] derived
//! `Debug` while holding a plain `api_key`, so `{:?}` on one printed the
//! operator's key verbatim, and printed it again through anything that derives
//! `Debug` around it ([`Record`], [`Fallback`], a caller's own config struct).
//! The other three providers derived nothing and leaked nothing, but nothing
//! stopped the next one from being written the same way. So all four are
//! asserted, not just the one that was wrong.
//!
//! The second is the bound: `ModelReviewer` derived `Debug`, which put
//! `P: Debug` on the impls that make it a [`Reviewer`] — and since the fix above
//! is precisely that a provider must *not* derive `Debug`, that bound shut every
//! provider out of the review gate. Hand-writing the `Debug` removes the bound
//! for **any** `P`, including an out-of-tree provider this crate never sees,
//! which is what `reviewer_and_approver_accept_a_provider_with_no_debug` holds
//! open.
//!
//! Every absence assertion below carries a positive control in the same
//! assertion pair: the key must be gone *and* the model must be there. An
//! absence test whose subject prints nothing at all passes for the wrong reason.

use io_harness::provider::{CompletionRequest, CompletionResponse, Fallback, Provider, Record};
use io_harness::{
    Anthropic, Approver, Compatible, ModelApprover, ModelReviewer, OpenAi, OpenRouter, Reviewer,
};

/// The key every provider below is built with. Distinctive enough that a
/// substring search cannot match it by accident, and it says what it is so a
/// failure message is self-explaining.
const SENTINEL: &str = "sk-SENTINEL-DO-NOT-PRINT";

/// Both formats, because they are different code paths in `std`: `{:#?}`
/// pretty-prints through the same `debug_struct` builder but a hand-written impl
/// that forgot `finish_non_exhaustive` — or one written with `write!` instead of
/// the builder — can differ between them.
fn both_forms<T: std::fmt::Debug>(value: &T) -> [String; 2] {
    [format!("{value:?}"), format!("{value:#?}")]
}

/// The key is absent and the model is present, in both formats.
///
/// The second half is the control. `f.debug_struct("X").finish()` would satisfy
/// the first half forever while telling an operator nothing.
fn hides_key_shows<T: std::fmt::Debug>(value: &T, expected: &[&str]) {
    for rendered in both_forms(value) {
        assert!(
            !rendered.contains(SENTINEL),
            "the API key reached a formatter: {rendered}"
        );
        for needle in expected {
            assert!(
                rendered.contains(needle),
                "{needle:?} is what an operator debugging a misconfiguration needs, and it is \
                 not in the rendering: {rendered}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// F9a — no shipped provider prints its credential
// ---------------------------------------------------------------------------

#[test]
fn openrouter_debug_hides_the_key() {
    let provider = OpenRouter::new(SENTINEL, "anthropic/claude-sonnet-4");
    hides_key_shows(
        &provider,
        &["anthropic/claude-sonnet-4", "openrouter.ai/api/v1"],
    );
}

#[test]
fn openai_debug_hides_the_key() {
    let provider = OpenAi::new(SENTINEL, "gpt-4o");
    hides_key_shows(&provider, &["gpt-4o", "api.openai.com/v1"]);
}

#[test]
fn anthropic_debug_hides_the_key() {
    let provider = Anthropic::new(SENTINEL, "claude-sonnet-4");
    hides_key_shows(&provider, &["claude-sonnet-4", "api.anthropic.com/v1"]);
}

/// The one that was actually leaking before 0.70.0: it derived `Debug`.
#[test]
fn compatible_debug_hides_the_key() {
    let provider = Compatible::groq(SENTINEL, "llama-3.3-70b");
    hides_key_shows(&provider, &["llama-3.3-70b", "groq"]);
}

// ---------------------------------------------------------------------------
// F9b — and the leak is closed through the wrappers, not only at the source
// ---------------------------------------------------------------------------

/// `Record` and `Fallback` both *derive* `Debug`, so before 0.70.0 they were the
/// long way round to the same key: a caller who never formatted a `Compatible`
/// still leaked one by formatting the wrapper it was inside.
///
/// Asserted through both, because they derive independently — fixing one would
/// not fix the other, and a run can be configured with both at once.
#[test]
fn wrapping_a_provider_does_not_reopen_the_leak() {
    let recorded = Record::new(Compatible::groq(SENTINEL, "llama-3.3-70b"));
    hides_key_shows(&recorded, &["llama-3.3-70b"]);

    let paired = Fallback::new(
        Compatible::groq(SENTINEL, "llama-3.3-70b"),
        OpenRouter::new(SENTINEL, "anthropic/claude-sonnet-4"),
    );
    hides_key_shows(&paired, &["llama-3.3-70b", "anthropic/claude-sonnet-4"]);
}

// ---------------------------------------------------------------------------
// F9c — the bound is gone for a provider this crate has never seen
// ---------------------------------------------------------------------------

/// A provider with **no `Debug` at all**, which is the whole point of it.
///
/// The four shipped providers now hand-write one, so a test built on them would
/// pass whether or not the bound was dropped. This type is the only thing here
/// that distinguishes the two, and it stands in for the out-of-tree provider the
/// issue was reported from.
struct NoDebugProvider;

impl Provider for NoDebugProvider {
    async fn complete(&self, _: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse::default())
    }
}

/// Constructing is not the claim — coercing to the trait object is.
///
/// `Reviewer` has `Debug` as a **supertrait**, so `&dyn Reviewer` is what forces
/// `ModelReviewer<NoDebugProvider>: Debug` to be proved. `Approver` has no such
/// supertrait, and its bound was gratuitous from the start; it is checked in the
/// same test because the two types drifted together and would drift back
/// together.
#[test]
fn reviewer_and_approver_accept_a_provider_with_no_debug() {
    let reviewer = ModelReviewer::new(NoDebugProvider, "a-reviewing-model");
    let reviewer: &dyn Reviewer = &reviewer;
    assert_eq!(reviewer.model(), Some("a-reviewing-model"));
    assert_eq!(
        format!("{reviewer:?}"),
        "ModelReviewer { model: \"a-reviewing-model\", .. }",
        "the hand-written Debug prints the model and nothing of the provider"
    );

    let approver = ModelApprover::new(NoDebugProvider, "an-approving-model");
    // Formatted as the concrete type, not through the trait object: `Approver`
    // deliberately has no `Debug` supertrait, so `&dyn Approver` cannot be
    // formatted at all and asking for it here would assert the opposite of what
    // this release decided. The coercion below is still the claim — it is what
    // proves a provider with no `Debug` reaches the trait object.
    assert_eq!(
        format!("{approver:?}"),
        "ModelApprover { model: \"an-approving-model\", allow_self: false, .. }"
    );
    let approver: &dyn Approver = &approver;
    assert_eq!(approver.model(), Some("an-approving-model"));
}

/// And the same two over a real provider — the construction the issue reported
/// as not compiling, which the doctests on both types also carry.
#[test]
fn a_shipped_provider_can_be_reviewed_and_approved_with() {
    let reviewer = ModelReviewer::new(OpenRouter::new(SENTINEL, "a-model"), "a-different-model");
    let reviewer: &dyn Reviewer = &reviewer;
    hides_key_shows(&reviewer, &["a-different-model"]);

    let approver = ModelApprover::new(Anthropic::new(SENTINEL, "a-model"), "a-different-model");
    hides_key_shows(&approver, &["a-different-model"]);
    // The coercion is the half the issue reported; the redaction above is
    // asserted on the concrete type, because `Approver` has no `Debug`
    // supertrait to format through.
    let approver: &dyn Approver = &approver;
    assert_eq!(approver.model(), Some("a-different-model"));
}
