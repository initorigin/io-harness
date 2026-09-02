//! Live run: what did it cost, and which model spent it (0.18.0).
//!
//! The suite proves the mechanism against fixtures. Only a live run proves the
//! numbers are the ones a vendor actually reports — that the model id the API
//! answered with reaches the trace, that cache tokens appear on the turn after
//! the prompt was cached, that a measured TTFT is smaller than a measured
//! latency, and that a derived cost changes when the price table is corrected
//! and does not change when it is not.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example live_accounting
//! ```

use io_harness::pricing::{Price, PriceTable};
use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-accounting-example");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("src"))?;
    // Three files with a little bulk, so the prompt is worth caching and a second
    // turn has something to be served from cache.
    for name in ["alpha", "beta", "gamma"] {
        let filler: String = (0..40)
            .map(|i| format!("// {name} note {i}: context so the prompt is not trivial\n"))
            .collect();
        std::fs::write(
            root.join(format!("src/{name}.rs")),
            format!("{filler}pub fn {name}() -> u32 {{ 0 }}\n"),
        )?;
    }

    let contract = TaskContract::workspace(
        "Read every file under src/, then write NOTES.md listing each function you \
         found and the value it returns. End the file with the word done.",
        &root,
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "NOTES.md".into(),
        needle: "done".into(),
    })
    .with_max_steps(10)
    .with_token_budget(200_000);

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("outcome: {:?}\n", result.outcome);

    // ---- one row per call, which is the whole claim ------------------------
    let calls = store.provider_calls(result.run_id)?;
    println!("{} provider call(s):", calls.len());
    for c in &calls {
        let u = c.usage.unwrap_or_default();
        println!(
            "  step {} attempt {} — model {:?}\n    \
             prompt {} (cache read {}, write {:?}), completion {} (reasoning {}), total {}\n    \
             latency {}ms, ttft {:?}, finish {:?}, failure {:?}",
            c.step,
            c.attempt,
            c.model,
            u.prompt_tokens,
            u.cache_read_tokens,
            u.cache_write_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
            c.latency_ms,
            c.ttft_ms,
            c.finish_reason,
            c.failure,
        );
    }

    // ---- the checks that make the numbers more than decoration -------------
    let configured = std::env::var("OPENROUTER_MODEL").unwrap_or_default();
    let named: Vec<&str> = calls.iter().filter_map(|c| c.model.as_deref()).collect();
    println!(
        "\nmodel recorded on every call: {} (configured {configured:?})",
        !named.is_empty() && named.len() == calls.len()
    );
    for c in &calls {
        if let Some(ttft) = c.ttft_ms {
            assert!(
                ttft <= c.latency_ms,
                "ttft {ttft}ms exceeded the call's own latency {}ms",
                c.latency_ms
            );
        }
    }
    println!("every measured ttft is inside its own call's latency");

    let cached: u64 = calls
        .iter()
        .filter_map(|c| c.usage)
        .map(|u| u.cache_read_tokens)
        .sum();
    println!("cache-read tokens across the run: {cached}");

    // ---- edits -------------------------------------------------------------
    let edits = store.edits(result.run_id)?;
    println!("\n{} edit(s):", edits.len());
    for e in &edits {
        println!(
            "  step {} {} {} (+{} -{})",
            e.step, e.tool, e.path, e.lines_added, e.lines_removed
        );
    }

    // ---- cost, derived twice from one unchanged trace -----------------------
    //
    // These are illustrative numbers, not the vendor's: the crate ships no
    // prices, and this is what an operator entering their own looks like.
    let table = |input: u64| {
        PriceTable::new("2026-07-29").with(
            &configured,
            Price {
                input,
                output: input * 5,
                cache_read: input / 10,
                ..Price::ZERO
            },
        )
    };
    let at = |input: u64| -> u64 {
        store
            .spend_by_run(&table(input))
            .expect("the store is open")
            .iter()
            .map(|s| s.cost_micros)
            .sum()
    };
    println!(
        "\ncost at $3/M input: {} micro-units\ncost at $6/M input: {} micro-units",
        at(3_000_000),
        at(6_000_000)
    );
    println!("the trace did not change between those two answers — no cost is stored");

    for row in store.spend_by_model(&table(3_000_000))? {
        println!(
            "\nby model: {} — {} call(s), {} tokens, {} micro-units, {} unpriced",
            row.key, row.calls, row.usage.total_tokens, row.cost_micros, row.unpriced_calls
        );
    }
    Ok(())
}
