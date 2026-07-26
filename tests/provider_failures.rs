//! The provider failure taxonomy, as a caller sees it.
//!
//! Before 0.11.0 every provider failure was `Provider(String)`: a 429, a 503, a
//! 401 and a DNS failure were the same variant carrying different prose, so
//! nothing above the provider could branch on them. What a caller now gets is a
//! [`ProviderErrorKind`], the HTTP status when there was one, and the server's
//! `Retry-After` when it sent one — and this file pins that shape, because it is
//! the shape the retry and provider-fallback logic branches on.
//!
//! The failures themselves are served over a real socket by the `failures` module
//! in `src/provider/mod.rs`: every provider is pinned to its vendor's URL in the
//! public API, so only a crate-internal test can point one at a local server, and
//! a fixture that merely returned an error would test nothing about the status
//! parsing, the header parsing, or the deadline.

use std::time::Duration;

use io_harness::{Error, ProviderErrorKind};

/// What a caller actually does with one of these.
fn worth_retrying(e: &Error) -> bool {
    matches!(e, Error::Provider { kind, .. } if kind.is_retryable())
}

#[test]
fn a_status_failure_carries_the_status_a_caller_needs_to_branch_on() {
    let e = Error::provider_status(429, Some(Duration::from_secs(30)), "slow down");
    let Error::Provider {
        kind,
        status,
        retry_after,
        message,
    } = &e
    else {
        panic!("expected a provider error");
    };
    assert_eq!(*kind, ProviderErrorKind::RateLimited);
    assert_eq!(*status, Some(429));
    assert_eq!(*retry_after, Some(Duration::from_secs(30)));
    assert_eq!(message, "slow down");
    assert!(worth_retrying(&e));
}

#[test]
fn a_failure_with_no_status_says_so_rather_than_inventing_one() {
    for e in [
        Error::provider_transport("connection refused"),
        Error::provider_malformed("nothing parsed"),
        Error::provider(ProviderErrorKind::Timeout, "deadline passed"),
    ] {
        let Error::Provider { status, .. } = &e else {
            panic!("expected a provider error");
        };
        assert_eq!(*status, None, "{e}");
        assert!(worth_retrying(&e), "{e}");
    }
}

#[test]
fn a_wrong_key_and_a_bad_request_are_terminal_not_retried() {
    for e in [
        Error::provider_status(401, None, "invalid api key"),
        Error::provider_status(403, None, "not entitled to this model"),
        Error::provider_status(400, None, "unknown field"),
        Error::provider_status(422, None, "schema violation"),
    ] {
        assert!(!worth_retrying(&e), "{e}");
    }
}

#[test]
fn every_status_maps_to_one_kind_and_only_that_kind() {
    use ProviderErrorKind::*;
    for (status, want) in [
        (429, RateLimited),
        (401, Auth),
        (403, Auth),
        (400, Request),
        (404, Request),
        (409, Request),
        (422, Request),
        (500, Server),
        (502, Server),
        (503, Server),
        (504, Server),
    ] {
        assert_eq!(ProviderErrorKind::from_status(status), want, "{status}");
    }
}

#[test]
fn every_kind_states_whether_a_retry_is_worth_it() {
    use ProviderErrorKind::*;
    for kind in [
        Transport,
        Timeout,
        RateLimited,
        Server,
        Auth,
        Request,
        Malformed,
    ] {
        // Exhaustive on purpose: a kind added later cannot slip in without a
        // deliberate decision about retrying it — this stops compiling.
        let expected = match kind {
            Transport | Timeout | RateLimited | Server | Malformed => true,
            Auth | Request => false,
        };
        assert_eq!(kind.is_retryable(), expected, "{kind:?}");
    }
}

#[test]
fn the_rendering_names_the_kind_and_the_status_without_being_the_api() {
    let shown = Error::provider_status(503, None, "upstream unavailable").to_string();
    assert!(shown.contains("Server"), "{shown}");
    assert!(shown.contains("503"), "{shown}");
    assert!(shown.contains("upstream unavailable"), "{shown}");

    // No status, no phantom status in the text.
    let shown = Error::provider_transport("dns failure").to_string();
    assert!(shown.contains("Transport"), "{shown}");
    assert!(!shown.contains("HTTP"), "{shown}");
}

#[test]
fn a_failure_that_is_not_the_providers_is_not_a_provider_error() {
    let e = Error::Config("OPENAI_API_KEY is not set".into());
    assert!(!worth_retrying(&e));
}
