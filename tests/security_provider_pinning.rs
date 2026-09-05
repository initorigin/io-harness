//! M10's residual — a provider dials the endpoint it was graded at.
//!
//! `NetGuard::check_target` resolves a provider endpoint, grades every address it
//! resolved to, and hands the set back; through 0.79.0 the run's gate dropped that
//! set and every provider then resolved the name a second time inside its own
//! `reqwest::Client`. A name that answered with a routable address when the run
//! authorised it and with `127.0.0.1` when the client dialled reached loopback
//! with a permission decision in between. The providers now hold a
//! `net::PinnedClient`, which resolves and grades once more at the dial and pins
//! the client to what *that* grading returned.
//!
//! # What this file can and cannot see
//!
//! An integration test compiles against the published surface, and the published
//! surface has no way to say "resolve this name to that address" — so the pin
//! itself, and the number of clients one provider builds, are asserted by unit
//! tests in `src/net.rs` where the type is visible. What is observable from out
//! here is the half that matters to a caller: an endpoint whose host is on this
//! machine is **refused rather than dialled**, and the socket is never opened.
//!
//! [`Compatible`] and [`Reference`] are the only two providers whose endpoint a
//! caller sets, so they are the two driven here. The other three are pinned to
//! their vendor's URL by construction and hold the same type.

use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use io_harness::provider::CompletionRequest;
use io_harness::{Auth, Compatible, Error, Provider, Reference};

/// Short, because every request in this file is expected to fail before it is
/// answered — either refused with no socket at all, or dialled at a server that
/// closes on it.
const BRIEF: Duration = Duration::from_millis(500);

/// A listener that counts the connections it accepts and answers each with
/// nothing at all.
///
/// The count is the evidence, and closing without a response is deliberate: what
/// is under test is whether a connection was made, not what came back down it.
/// Returns the bound port and the counter.
fn counting_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            // Drain what the client wrote before closing, so it sees an orderly
            // end rather than a reset. The count is bound because a discarded
            // `read` amount is a lint, and here it genuinely does not matter.
            let mut buf = [0u8; 1024];
            let _drained = stream.read(&mut buf).unwrap_or(0);
        }
    });
    (port, accepted)
}

/// The accept count, given up to a second for the server thread to reach its
/// `accept` — a client's `connect` completes off the kernel's backlog and can
/// return before userspace has seen it.
///
/// Returns as soon as the count is non-zero, so the assertion that *no*
/// connection was made pays the whole second and the assertion that one was does
/// not.
fn accepts_within_a_second(counter: &AtomicUsize) -> usize {
    for _ in 0..100 {
        let seen = counter.load(Ordering::SeqCst);
        if seen > 0 {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    counter.load(Ordering::SeqCst)
}

#[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
fn request() -> CompletionRequest {
    CompletionRequest {
        system: "s".into(),
        user: "u".into(),
        ..Default::default()
    }
}

/// `Auth::None`, so the cleartext-bearer refusal (0.74.0) is not what answers —
/// that one fires before the dial and would mask everything below it.
fn provider_at(base: &str) -> Compatible {
    Compatible::new(base, Auth::None, "", "some-model").with_timeout(BRIEF)
}

// ---------------------------------------------------------------------------
// The refusal
// ---------------------------------------------------------------------------

/// A provider endpoint naming a host on this machine is refused, and no socket is
/// opened.
///
/// Against `develop` this fails twice: the error is a transport failure rather
/// than [`Error::Refused`], because the provider held a plain `reqwest::Client`
/// and dialled whatever the name answered with — and the listener records the
/// connection that proves it.
#[tokio::test]
async fn m10_a_provider_endpoint_on_this_machine_is_refused_rather_than_dialled() {
    let (port, accepted) = counting_listener();
    let err = provider_at(&format!("http://localhost:{port}/v1"))
        .complete(request())
        .await
        .unwrap_err();

    let Error::Refused { target, .. } = &err else {
        panic!("a local provider endpoint must be refused, got {err:?}");
    };
    assert!(target.contains("localhost"), "{target}");
    assert_eq!(
        accepts_within_a_second(&accepted),
        0,
        "the refusal came after the dial, which is no refusal at all"
    );
}

/// The refusal says what would lift it.
///
/// A floor that refuses an operator's own local model runtime without naming the
/// widening is a floor they can only get past by reading this crate's source.
#[tokio::test]
async fn m10_the_refusal_names_the_floor_and_the_variable_that_lifts_it() {
    let (port, _) = counting_listener();
    let err = provider_at(&format!("http://localhost:{port}/v1"))
        .complete(request())
        .await
        .unwrap_err();

    let Error::Refused { rule, layer, .. } = &err else {
        panic!("expected a refusal, got {err:?}");
    };
    assert_eq!(layer.as_deref(), Some("local-address floor"));
    let rule = rule.as_deref().unwrap_or_default();
    assert!(
        rule.contains("IO_HARNESS_ALLOW_LOCAL_ADDRESSES"),
        "a refusal nobody can act on: {rule}"
    );
}

/// The catalogue endpoint is governed the same way.
///
/// [`Reference`] is a second host the run authorises before its first step, and
/// it holds its own client — so it needed its own pin rather than inheriting the
/// provider's.
#[tokio::test]
async fn m10_a_reference_catalogue_on_this_machine_is_refused_too() {
    let (port, accepted) = counting_listener();
    let err = Reference::at(format!("http://localhost:{port}/v1/models"))
        .with_timeout(BRIEF)
        .models()
        .await
        .unwrap_err();

    assert!(
        matches!(err, Error::Refused { .. }),
        "a local catalogue endpoint must be refused, got {err:?}"
    );
    assert_eq!(accepts_within_a_second(&accepted), 0);
}

// ---------------------------------------------------------------------------
// The controls
// ---------------------------------------------------------------------------

/// The positive control, and the one that keeps the three above from passing by
/// refusing everything.
///
/// An IP literal is its own resolution: there is no second answer for the pin to
/// disagree with, so there is no window to close and nothing here to decide that
/// the run's gate has not already decided. It is dialled, exactly as it was
/// before this change — which is also what keeps every `127.0.0.1` fixture in
/// this crate's own suite working.
///
/// Passes on `develop` as well. That is what a control is.
#[tokio::test]
async fn m10_a_literal_endpoint_is_still_dialled() {
    let (port, accepted) = counting_listener();
    let err = provider_at(&format!("http://127.0.0.1:{port}/v1"))
        .complete(request())
        .await
        .unwrap_err();

    assert!(
        !matches!(err, Error::Refused { .. }),
        "a literal endpoint is decided at the gate, not a second time at the dial: {err:?}"
    );
    assert_eq!(
        accepts_within_a_second(&accepted),
        1,
        "the request never reached the server it named"
    );
}

/// A routable name is not refused by the pin either.
///
/// The second control, against the reading of the change that would pass every
/// test above by refusing every name: `192.0.2.10` is TEST-NET-1 — reserved for
/// documentation, never routed — so this reaches nothing, and the point is that
/// what it fails with is a transport error rather than a floor refusal.
///
/// Written as a literal rather than a name so the test issues no DNS query; the
/// grading is the same either way, since the floor grades addresses.
#[tokio::test]
async fn m10_an_address_the_floor_permits_is_not_refused() {
    let err = provider_at("http://192.0.2.10:8000/v1")
        .complete(request())
        .await
        .unwrap_err();
    assert!(
        !matches!(err, Error::Refused { .. }),
        "TEST-NET-1 is not on the floor: {err:?}"
    );
}
