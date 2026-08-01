//! Provider parity: the *same* task contract runs to verified success through
//! four different [`Provider`] implementations, selected only at run
//! construction, and the trace records which one ran.
//!
//! The four providers are deterministic mocks named after the real vendors, so
//! the test is offline. It proves the loop is provider-agnostic — nothing about
//! the contract changes when the provider does — and that the provider label is
//! persisted. The real OpenRouter/Anthropic/OpenAI wire formats are unit-tested
//! in `src/provider/*`; live cross-vendor proof is limited to OpenRouter (only
//! one live key), see `examples/edit_file.rs`.
//!
//! The fourth is [`Compatible`], the OpenAI-shaped provider that covers every
//! vendor without a module of its own. Its place in the loop is not a literal:
//! the name driven through the run is the label a *real* `Compatible` reports,
//! and `f8_the_real_compatible_carries_the_trait_shape_the_loop_drives` checks
//! the same instance against the trait surface `run` uses. Its wire path is
//! proven against a local socket in `src/provider/compatible.rs`
//! (`f1_a_streamed_completion_arrives_through_the_real_http_and_sse_path`), so
//! it is not repeated here — a test binary cannot dial a vendor.

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run, Auth, Compatible, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

/// A mock provider that reports a chosen vendor name and always writes a
/// compiling file — standing in for a real provider so parity is offline.
///
/// The name is borrowed rather than `'static` so a real provider's own
/// [`Provider::name`] can be driven through the loop.
struct NamedMock<'a> {
    name: &'a str,
}

impl Provider for NamedMock<'_> {
    fn name(&self) -> &str {
        self.name
    }

    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": "pub fn hello() -> u32 { 42 }\n" }),
            }],
            ..Default::default()
        })
    }
}

fn contract(file: &std::path::Path) -> TaskContract {
    // One fixed single-file contract — identical for every provider.
    TaskContract::new(
        "add a hello function returning 42",
        file,
        // 0.18.0: the Rust-specific criteria are gone; the project's own
        // compiler is invoked by argv like any other language's would be.
        Verification::Command {
            argv: vec![
                "rustc".into(),
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "hello.rs".into(),
            ],
            expect_exit: 0,
        },
    )
    .with_max_steps(3)
}

#[tokio::test]
async fn the_same_contract_verifies_under_every_provider_and_records_which_ran() {
    // The fourth entry is the label a real `Compatible` preset reports, so the
    // loop drives the string that would actually reach the trace rather than one
    // a reader hopes matches. The three before it are unchanged, and their still
    // passing here is what shows this is a fourth path and not a rewrite.
    let compatible = Compatible::groq("test-key", "llama-3.3-70b");
    for provider_name in ["openrouter", "anthropic", "openai", compatible.name()] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.rs");
        let store = Store::memory().unwrap();
        let provider = NamedMock {
            name: provider_name,
        };

        // Selection is *only* which provider is passed here — the contract is
        // constructed the same way regardless.
        let result = run(&contract(&file), &provider, &store).await.unwrap();

        assert_eq!(
            result.outcome,
            RunOutcome::Success { steps: 1 },
            "provider {provider_name} should reach verified success"
        );
        assert_eq!(
            store.provider(result.run_id).unwrap().as_deref(),
            Some(provider_name),
            "the trace should record which provider ran"
        );
    }
}

/// Exactly the bound [`run`] declares, so a provider this accepts is a provider
/// `run` accepts — and the acceptance is checked by the compiler rather than
/// asserted in prose.
fn as_run_takes_it<P: Provider>(provider: &P) -> (&str, Option<&str>, Vec<&str>) {
    (provider.name(), provider.endpoint(), provider.endpoints())
}

/// The fourth provider in the loop above is a mock carrying a real
/// [`Compatible`]'s label; this is the real instance itself, checked against the
/// surface the loop depends on.
///
/// Two things would make the loop's fourth entry decorative: `Compatible` not
/// fitting `run`'s bound at all, or its label coming from somewhere other than
/// [`Provider::name`]. Both are ruled out here without a socket — the wire path
/// is proven against a local server in `src/provider/compatible.rs`, and a test
/// binary has no vendor to dial.
#[test]
fn f8_the_real_compatible_carries_the_trait_shape_the_loop_drives() {
    let provider = Compatible::groq("test-key", "llama-3.3-70b");
    let (name, endpoint, endpoints) = as_run_takes_it(&provider);

    assert_eq!(name, "groq", "the preset's label is what reaches the trace");
    // `run` authorises every URL a provider reports before the first step, so a
    // provider that named no host would be reached outside the boundary.
    assert_eq!(endpoint, Some(provider.base()));
    assert_eq!(endpoints, vec![provider.base()]);
    assert_eq!(
        provider.auth(),
        &Auth::Bearer,
        "a hosted vendor takes a key"
    );
}

/// The named negative control for the test above.
///
/// A label that were a constant, or one derived from the URL, would pass
/// `f8_the_real_compatible_carries_the_trait_shape_the_loop_drives` while
/// recording the wrong vendor for every run. A `Compatible` pointed at a base
/// nobody named must therefore *not* claim a vendor.
#[test]
fn f8_a_compatible_pointed_at_no_preset_claims_no_vendor_label() {
    let anonymous = Compatible::new("https://api.example.test/v1/", Auth::None, "", "some-model");
    assert_eq!(
        anonymous.name(),
        "compatible",
        "an unnamed provider is honest about what is known, not a vendor"
    );
    assert_ne!(anonymous.name(), "groq");
    assert_eq!(
        anonymous.base(),
        "https://api.example.test/v1",
        "the trailing slash is trimmed, so appended paths do not double it"
    );
    assert_eq!(
        anonymous.with_name("lab").name(),
        "lab",
        "the label is settable, which is why the preset's is worth asserting"
    );
}
