//! Provider parity: the *same* task contract runs to verified success through
//! three different [`Provider`] implementations, selected only at run
//! construction, and the trace records which one ran.
//!
//! The three providers are deterministic mocks named after the real vendors, so
//! the test is offline. It proves the loop is provider-agnostic — nothing about
//! the contract changes when the provider does — and that the provider label is
//! persisted. The real OpenRouter/Anthropic/OpenAI wire formats are unit-tested
//! in `src/provider/*`; live cross-vendor proof is limited to OpenRouter (only
//! one live key), see `examples/edit_file.rs`.

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

/// A mock provider that reports a chosen vendor name and always writes a
/// compiling file — standing in for a real provider so parity is offline.
struct NamedMock {
    name: &'static str,
}

impl Provider for NamedMock {
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
        Verification::RustTestPasses {
            test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
        },
    )
    .with_max_steps(3)
}

#[tokio::test]
async fn the_same_contract_verifies_under_every_provider_and_records_which_ran() {
    for provider_name in ["openrouter", "anthropic", "openai"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.rs");
        let store = Store::memory().unwrap();
        let provider = NamedMock { name: provider_name };

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
