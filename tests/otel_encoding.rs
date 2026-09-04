//! F4 of 0.78.0, on the half of the OTLP/HTTP encoding that is public.
//!
//! Four rules of the encoding are things a plausible implementation gets wrong
//! and no type system catches: the id format, the timestamp format, the enum
//! format, and the URL. Three of them are assertions about a JSON body built
//! from crate-private types and their tests live in `src/otel.rs`'s own `mod
//! tests`, because nothing in the encoding layer belongs in
//! `docs/public-api.txt` and making an item `pub` to be testable would put it
//! there. The fourth is reachable from outside — [`OtelConfig::traces_url`] —
//! and it is here.
//!
//! Each rule is checked by a function over the value rather than by an
//! assertion written inline, so a `control_` test can feed the same function a
//! deliberately wrong value and prove the check says no. A checker nobody has
//! watched fail is a checker nobody has shown to work.

// The whole exporter is behind the feature, so a build that did not ask for an
// outbound network capability does not compile these either.
#![cfg(feature = "otel")]

use io_harness::{OtelConfig, OTEL_DEFAULT_ENDPOINT};

/// The path OTLP/HTTP appends to a collector's base URL for trace data.
///
/// Typed out here rather than read from the crate: the constant in `src/otel.rs`
/// is private, and a test that asked the implementation for the value it is
/// checking would pass through any change to it. This is the specification's
/// word, and it is the second place it is written on purpose.
const TRACES_PATH: &str = "/v1/traces";

/// Whether `url` is `endpoint` with the trace path appended, exactly once.
///
/// The two failures this separates are the ones that look identical in a
/// configuration file and produce different requests. An endpoint written as a
/// full trace URL — the value a reader copies out of a collector's own
/// documentation — yields `…/v1/traces/v1/traces`, which a collector answers
/// with a 404 that reads like the collector is down. An endpoint with a
/// trailing slash yields `…//v1/traces`, which some gateways route and some
/// do not.
fn traces_url_fault(endpoint: &str, url: &str) -> Result<(), String> {
    if endpoint.contains(TRACES_PATH) {
        return Err(format!(
            "the endpoint already contains {TRACES_PATH}: {endpoint}"
        ));
    }
    if !url.ends_with(TRACES_PATH) {
        return Err(format!("the URL does not end in {TRACES_PATH}: {url}"));
    }
    if url.matches(TRACES_PATH).count() != 1 {
        return Err(format!(
            "the URL contains {TRACES_PATH} more than once: {url}"
        ));
    }
    let expected = format!("{}{TRACES_PATH}", endpoint.trim_end_matches('/'));
    if url != expected {
        return Err(format!("expected {expected}, got {url}"));
    }
    Ok(())
}

#[test]
fn f4_the_traces_url_is_the_endpoint_plus_v1_traces() {
    for endpoint in [
        OTEL_DEFAULT_ENDPOINT,
        "http://otel-collector.internal:4318",
        "https://ingest.example.com",
        // A gateway that serves the collector under a prefix. The path is
        // appended to whatever the endpoint is, not to its host.
        "https://gateway.example.com/otlp",
    ] {
        let url = OtelConfig::new(endpoint).traces_url();
        if let Err(fault) = traces_url_fault(endpoint, &url) {
            panic!("{endpoint} produced a bad trace URL: {fault}");
        }
    }
}

#[test]
fn f4_a_trailing_slash_on_the_endpoint_produces_one_url() {
    let with = OtelConfig::new("http://localhost:4318/");
    let without = OtelConfig::new("http://localhost:4318");

    assert_eq!(with.traces_url(), without.traces_url());
    assert_eq!(with.traces_url(), "http://localhost:4318/v1/traces");
    // The endpoint the config reports is the normalised one, so a caller that
    // logs it and a caller that posts to it are looking at the same collector.
    assert_eq!(with.endpoint(), "http://localhost:4318");
}

#[test]
fn f4_the_default_endpoint_is_a_host_and_a_port_and_not_a_path() {
    // Port 4318 is OTLP/HTTP's. 4317 is gRPC's, and a JSON body posted there
    // fails in a way that does not name the port as the reason.
    assert_eq!(OTEL_DEFAULT_ENDPOINT, "http://localhost:4318");
    assert!(!OTEL_DEFAULT_ENDPOINT.contains(TRACES_PATH));
    assert_eq!(
        OtelConfig::default().traces_url(),
        "http://localhost:4318/v1/traces"
    );
}

#[test]
fn control_an_endpoint_that_already_carries_the_path_is_rejected() {
    // The mistake this exists for: the endpoint copied from a collector's
    // documentation, which quotes the full trace URL.
    let endpoint = "http://localhost:4318/v1/traces";
    let err = traces_url_fault(endpoint, "http://localhost:4318/v1/traces/v1/traces")
        .expect_err("an endpoint carrying the path is a fault");
    assert!(err.contains("already contains"), "{err}");
}

#[test]
fn control_a_url_missing_or_repeating_the_path_is_rejected() {
    let base = "http://localhost:4318";

    let err = traces_url_fault(base, "http://localhost:4318")
        .expect_err("a URL with no trace path is a fault");
    assert!(err.contains("does not end in"), "{err}");

    // Ends in the path and still wrong: the doubled path a naive `format!` over
    // an unnormalised endpoint produces.
    let err = traces_url_fault(base, "http://localhost:4318/v1/traces/v1/traces")
        .expect_err("a doubled trace path is a fault");
    assert!(err.contains("more than once"), "{err}");

    // The double slash. Ends in the path, contains it once, wrong host path.
    let err = traces_url_fault(base, "http://localhost:4318//v1/traces")
        .expect_err("a doubled separator is a fault");
    assert!(err.contains("expected"), "{err}");

    // A different collector entirely, which is what a config read from the
    // wrong table looks like.
    let err = traces_url_fault(base, "http://other:4318/v1/traces")
        .expect_err("a URL for another endpoint is a fault");
    assert!(err.contains("expected"), "{err}");

    // The other half: the checker must not reject the correct URL.
    assert!(traces_url_fault(base, "http://localhost:4318/v1/traces").is_ok());
}
