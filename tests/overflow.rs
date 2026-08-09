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
