//! The request deadline, reached the way a caller reaches it.
//!
//! This file is an *integration* test on purpose. The deadline was documented as
//! overridable since 0.11.0 and was not: `net::http_client_with_timeout` and each
//! provider's `at` constructor are crate-private, so every existing test of the
//! deadline proved only that the crate can override its own default. Nothing
//! outside could. A unit test cannot catch that class of mistake — it lives on the
//! wrong side of the boundary it is checking — so the check for it has to compile
//! against the published surface and nothing else.
//!
//! Two things are asserted: that the default is a value a caller can read, and
//! that `with_timeout` actually replaces it. The second needs a request that would
//! otherwise never end, so the provider is pointed at a socket that accepts and
//! then writes nothing — the same fixture shape as the `failures` module in
//! `src/provider/mod.rs`, which reaches the socket by an endpoint override this
//! side of the wall does not have. The public API pins each provider to its
//! vendor's URL, deliberately, so the connection is diverted with this process's
//! proxy environment instead: where the bytes go is not what is under test, only
//! whether the caller's deadline is the one that ends the call.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use io_harness::provider::{openai, CompletionRequest};
use io_harness::{Anthropic, Error, OpenAi, OpenRouter, Provider, ProviderErrorKind as Kind};

/// Short enough to fire while the test is watching, long enough that a loaded
/// runner still gets the connection open first.
const SHORT: Duration = Duration::from_millis(500);

/// The upper bound on a call that should end after `SHORT`.
///
/// Deliberately far looser than the deadline it brackets: this asserts "the
/// caller's 500ms, not the built-in ten minutes", which is the only claim the test
/// makes, and it must stay true on a Windows CI runner with every core busy. An
/// exact duration here would be a scheduler-sensitivity test wearing a timeout
/// test's name.
const BOUND: Duration = Duration::from_secs(30);

/// A local listener that accepts every connection and never answers, installed as
/// this process's proxy so the providers' pinned vendor URLs dial it.
///
/// Called once, from the one async test, so no second test is reading the
/// environment while this writes it.
fn stall_every_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    // Hold each accepted connection open and write nothing to it. `incoming()`
    // never ends, so the collect is the hold.
    std::thread::spawn(move || {
        let _held: Vec<_> = listener.incoming().filter_map(|s| s.ok()).collect();
    });

    for key in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
        std::env::set_var(key, &proxy);
    }
    // A runner that exempts everything from its proxy would send the request to
    // the real vendor instead, which is a network call and not this test.
    for key in ["no_proxy", "NO_PROXY"] {
        std::env::set_var(key, "");
    }
}

#[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
fn request() -> CompletionRequest {
    CompletionRequest {
        system: "s".into(),
        user: "u".into(),
        tools: Vec::new(),
        ..Default::default()
    }
}

/// Assert that `provider`'s call ends as a timeout, well inside [`BOUND`].
async fn ends_as_a_timeout(name: &str, provider: impl Provider) {
    let started = Instant::now();
    let result = provider.complete(request()).await;
    let elapsed = started.elapsed();

    match result {
        Err(Error::Provider { kind, status, .. }) => {
            assert_eq!(kind, Kind::Timeout, "{name}: {result:?}");
            assert_eq!(status, None, "{name}: a deadline is not a status");
            assert!(kind.is_retryable(), "{name}");
        }
        other => panic!("{name}: expected a provider timeout, got {other:?}"),
    }
    assert!(
        elapsed < BOUND,
        "{name}: the caller's deadline did not end the call — {elapsed:?}"
    );
}

/// The default a caller is told to reason about is one a caller can read.
#[test]
fn the_default_deadline_is_public() {
    assert_eq!(openai::REQUEST_TIMEOUT, Duration::from_secs(600));
    // One value, reachable beside each provider's `with_timeout`.
    assert_eq!(
        io_harness::provider::anthropic::REQUEST_TIMEOUT,
        openai::REQUEST_TIMEOUT
    );
    assert_eq!(
        io_harness::provider::openrouter::REQUEST_TIMEOUT,
        openai::REQUEST_TIMEOUT
    );
    // The override the rest of this file makes is a real one.
    assert!(SHORT < openai::REQUEST_TIMEOUT);
}

/// Every provider takes a caller's deadline, and it is the one that fires.
///
/// One test rather than three: it writes the process environment, and a sibling
/// test running concurrently would be reading it.
#[tokio::test]
async fn with_timeout_replaces_the_default_on_every_provider() {
    stall_every_connection();

    ends_as_a_timeout("openrouter", OpenRouter::new("k", "m").with_timeout(SHORT)).await;
    ends_as_a_timeout("anthropic", Anthropic::new("k", "m").with_timeout(SHORT)).await;
    ends_as_a_timeout("openai", OpenAi::new("k", "m").with_timeout(SHORT)).await;
}
