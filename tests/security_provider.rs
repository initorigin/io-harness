//! The provider layer's security boundary, from outside the crate (0.74.0).
//!
//! Each test is named for the audit finding it closes and fails against 0.73.0's
//! behaviour rather than merely describing the new one. They are here rather than
//! in the module's own `#[cfg(test)]` blocks because every one of them is about
//! what a *caller* can observe: what a `{:?}` prints into a caller's log, what
//! leaves the process on the wire, and what a caller finds on disk afterwards.
//! The bounds that need a crate-internal socket or accumulator — M13's caps, L6's
//! token arithmetic — are unit-tested beside the code they bound.
//!
//! **No test here contains a real credential.** `gsk-not-a-real-key` is a
//! sentinel shaped like one, and every assertion is that it is *absent*.

use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, Record};
use io_harness::{Auth, Compatible, Error, Provider};

/// The sentinel. Shaped like a Groq key so a redaction that matched on a prefix
/// would be caught, and real enough that a leak is unambiguous in the assertion
/// message — but not a credential to anything.
const SENTINEL: &str = "gsk-not-a-real-key";

/// A model slug distinctive enough that "the rendering still says something
/// useful" is a real assertion rather than one any prose would satisfy.
const MODEL: &str = "zeta-42-instruct";

/// A provider that answers nothing, for the wrappers that need one to exist.
///
/// `Debug` because `Record<P>`'s hand-written impl prints the wrapped provider
/// as itself — redaction and all — so it carries a `P: Debug` bound.
#[derive(Debug)]
struct Silent;

impl Provider for Silent {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse::default())
    }

    fn name(&self) -> &str {
        "silent"
    }
}

fn request(user: &str) -> CompletionRequest {
    CompletionRequest {
        system: "s".into(),
        user: user.into(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// M17 — a credential never reaches a `Debug` rendering
// ---------------------------------------------------------------------------

/// Every public door onto a `Compatible` that takes a key, rendered.
///
/// A hand-written `Debug` that covers the type but not the constructors that
/// wrap it is a redaction with a hole in it, so this drives all of them rather
/// than the one the audit named.
#[test]
fn m17_no_constructor_of_compatible_prints_its_key() {
    let mut built = vec![
        (
            "new",
            Compatible::new("https://api.example/v1", Auth::Bearer, SENTINEL, MODEL),
        ),
        (
            "preset",
            Compatible::preset("groq", SENTINEL, MODEL).unwrap(),
        ),
        ("cerebras", Compatible::cerebras(SENTINEL, MODEL)),
        ("deepseek", Compatible::deepseek(SENTINEL, MODEL)),
        ("fireworks", Compatible::fireworks(SENTINEL, MODEL)),
        ("gemini", Compatible::gemini(SENTINEL, MODEL)),
        ("groq", Compatible::groq(SENTINEL, MODEL)),
        ("minimax", Compatible::minimax(SENTINEL, MODEL)),
        ("mistral", Compatible::mistral(SENTINEL, MODEL)),
        ("moonshot", Compatible::moonshot(SENTINEL, MODEL)),
        ("perplexity", Compatible::perplexity(SENTINEL, MODEL)),
        ("qwen", Compatible::qwen(SENTINEL, MODEL)),
        ("together", Compatible::together(SENTINEL, MODEL)),
        ("xai", Compatible::xai(SENTINEL, MODEL)),
        ("zhipu", Compatible::zhipu(SENTINEL, MODEL)),
    ];
    // The builders are part of the surface too: one that rebuilt the struct
    // field-by-field could reintroduce a derive.
    built.push((
        "with_name",
        Compatible::groq(SENTINEL, MODEL).with_name("lab"),
    ));
    built.push((
        "with_timeout",
        Compatible::groq(SENTINEL, MODEL).with_timeout(Duration::from_secs(1)),
    ));

    assert_eq!(built.len(), 17, "every keyed constructor is driven");
    for (label, provider) in &built {
        let rendered = format!("{provider:?}");
        assert!(
            !rendered.contains(SENTINEL),
            "{label} printed its credential: {rendered}"
        );
        // The control: the rendering is not empty prose that would pass this
        // assertion by saying nothing at all.
        assert!(
            rendered.contains("Compatible") && rendered.contains(MODEL),
            "{label} must still be diagnosable: {rendered}"
        );
    }
}

/// The alternate formatter is a second door onto the same impl, and a
/// `{:#?}` in a panic message is exactly where a key would surface.
#[test]
fn m17_the_pretty_formatter_redacts_what_the_plain_one_does() {
    let rendered = format!("{:#?}", Compatible::groq(SENTINEL, MODEL));
    assert!(!rendered.contains(SENTINEL), "{rendered}");
    assert!(rendered.contains("Compatible"), "{rendered}");
}

/// A key smuggled through the *base* rather than the key field. Gateway and
/// Azure-shaped deployments really do carry credentials in the URL, and a
/// `Debug` that redacted only the field would print this one verbatim.
#[test]
fn m17_a_credential_inside_the_base_url_is_redacted_too() {
    for base in [
        format!("https://user:{SENTINEL}@api.example/v1"),
        format!("https://api.example/v1?api-key={SENTINEL}"),
        format!("https://api.example/v1#{SENTINEL}"),
    ] {
        let provider = Compatible::new(&base, Auth::Bearer, SENTINEL, MODEL);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains(SENTINEL), "{base}: {rendered}");
        assert!(rendered.contains("api.example"), "{base}: {rendered}");
    }
}

/// A wrapper must not reach past the impl it wraps. `Record` derived `Debug`
/// through 0.73.0, so `{:?}` on a recorder printed the wrapped provider *and*
/// every exchange it had captured — the whole conversation, into whatever log
/// the `{:?}` went to.
#[tokio::test]
async fn m17_a_recorder_prints_a_count_rather_than_the_conversation() {
    let recorder = Record::new(Compatible::groq(SENTINEL, MODEL));
    // Not through the network: what is being asserted is the rendering, and
    // `Record` captures whatever its inner provider answered.
    let inner = Record::new(Silent);
    inner.complete(request(SENTINEL)).await.unwrap();

    let rendered = format!("{inner:?}");
    assert!(
        !rendered.contains(SENTINEL),
        "a recorder printed the conversation it captured: {rendered}"
    );
    assert!(
        rendered.contains("exchanges: 1"),
        "the count is what a recorder's `{{:?}}` is asking for: {rendered}"
    );

    // And the wrapped provider still renders as itself, redaction included.
    let wrapped = format!("{recorder:?}");
    assert!(!wrapped.contains(SENTINEL), "{wrapped}");
    assert!(wrapped.contains("Compatible"), "{wrapped}");
}

// ---------------------------------------------------------------------------
// L5 — a bearer credential is never sent in cleartext
// ---------------------------------------------------------------------------

/// 192.0.2.0/24 is TEST-NET-1: reserved for documentation and never routable, so
/// a guard that failed open would still not reach anything.
const REMOTE: &str = "http://192.0.2.10:8000/v1";

fn brief(base: &str, auth: Auth) -> Compatible {
    Compatible::new(base, auth, SENTINEL, MODEL).with_timeout(Duration::from_millis(200))
}

#[tokio::test]
async fn l5_a_bearer_key_is_refused_over_plaintext_http_to_a_remote_host() {
    let err = brief(REMOTE, Auth::Bearer)
        .complete(request("u"))
        .await
        .unwrap_err();
    let Error::Config(message) = &err else {
        panic!("a cleartext bearer must be a configuration refusal, got {err:?}");
    };
    assert!(message.contains("cleartext"), "{message}");
    // It names the alternatives rather than only the problem.
    assert!(message.contains("https://"), "{message}");
    assert!(message.contains("Auth::None"), "{message}");
    // And the refusal itself does not become the leak.
    assert!(!message.contains(SENTINEL), "{message}");
}

/// The refusal is about the credential, not about `http`. Both controls answer
/// something other than a configuration refusal — a transport failure or a
/// deadline, which is what "the request was actually attempted" looks like.
#[tokio::test]
async fn l5_the_refusal_stops_at_the_two_shapes_that_leak_nothing() {
    // Loopback: the eight local-runtime presets and a developer's own port.
    for base in [
        "http://127.0.0.1:1/v1",
        "http://localhost:1/v1",
        "http://[::1]:1/v1",
    ] {
        let err = brief(base, Auth::Bearer)
            .complete(request("u"))
            .await
            .unwrap_err();
        assert!(
            !matches!(err, Error::Config(_)),
            "{base} is on this machine and must not be refused: {err:?}"
        );
    }
    // No credential to leak.
    let err = brief(REMOTE, Auth::None)
        .complete(request("u"))
        .await
        .unwrap_err();
    assert!(!matches!(err, Error::Config(_)), "{err:?}");
    // Encrypted.
    let err = brief("https://192.0.2.10:8000/v1", Auth::Bearer)
        .complete(request("u"))
        .await
        .unwrap_err();
    assert!(!matches!(err, Error::Config(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// L3 — a recording is not world-readable
// ---------------------------------------------------------------------------

/// A recording holds the whole system prompt, every user message and every
/// response. Through 0.73.0 it was created by `std::fs::write`, which under a
/// default umask is 0644.
#[cfg(unix)]
#[tokio::test]
async fn l3_a_recording_is_created_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = std::env::temp_dir().join(format!("io-harness-l3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.json");
    let _ = std::fs::remove_file(&path);

    let recorder = Record::new(Silent);
    recorder.complete(request(SENTINEL)).await.unwrap();
    recorder.save(&path).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "a recording is private to its owner, got {mode:o}"
    );

    // The content is what makes the mode matter, so assert it is really there
    // rather than passing against an empty file nobody would care about.
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains(SENTINEL),
        "the recording is the conversation"
    );

    // A path that already exists at 0644 — a recording from an earlier run, or
    // one an attacker pre-created — is corrected rather than inherited, because
    // `mode` on the open applies only when the file is created.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    recorder.save(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a pre-existing file is narrowed, got {mode:o}");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}
