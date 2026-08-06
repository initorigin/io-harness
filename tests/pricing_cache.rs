//! F5 — the cached tokens are priced as cached.
//!
//! The counts here are not invented. They were measured by `examples/cache_live`
//! against `anthropic/claude-haiku-4.5` through OpenRouter on 2026-08-06, with the
//! marked arm reading the cache and an unmarked control over the identical
//! endpoint and model reading nothing:
//!
//! ```text
//! marked   call 1  prompt=7420  cache_read=0     cache_write=0  completion=5
//! marked   call 2  prompt=7421  cache_read=7408  cache_write=0  completion=5
//! control  call 1  prompt=7420  cache_read=0     cache_write=0  completion=5
//! control  call 2  prompt=7421  cache_read=0     cache_write=0  completion=5
//! ```
//!
//! Those rows go into a real [`Store`], come back out through
//! [`Store::provider_calls`], and are priced through the shipped table — which is
//! the first time the cache branch of `src/pricing.rs` has ever run against a
//! non-zero count. The test is deterministic and needs no network: the live run
//! supplied the numbers, this pins what the crate charges for them.

use io_harness::pricing::{Price, PriceTable};
use io_harness::{ProviderCall, Store, Usage};

/// The model the live run actually answered on, and the rate shape this crate
/// already ships for a tiered vendor: a cache read at a tenth of fresh input and a
/// cache write at 1.25 times it.
const MODEL: &str = "anthropic/claude-haiku-4.5";

fn priced(cache_read: u64) -> Price {
    Price {
        input: 3_000_000,
        output: 15_000_000,
        cache_read,
        cache_write: 3_750_000,
        ..Price::ZERO
    }
}

/// The two calls the marked arm made, in order.
fn measured() -> [Usage; 2] {
    [
        Usage {
            prompt_tokens: 7420,
            completion_tokens: 5,
            total_tokens: 7425,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            ..Default::default()
        },
        Usage {
            prompt_tokens: 7421,
            completion_tokens: 5,
            total_tokens: 7426,
            cache_read_tokens: 7408,
            cache_write_tokens: 0,
            ..Default::default()
        },
    ]
}

fn store_the_pair() -> (Store, i64) {
    let store = Store::memory().unwrap();
    let run_id = store
        .start_run("a cached conversation", "NOTES.md")
        .unwrap();
    for (step, usage) in measured().into_iter().enumerate() {
        store
            .record_provider_call(
                run_id,
                &ProviderCall {
                    step: step as u32 + 1,
                    provider: "openrouter".into(),
                    model: Some(MODEL.into()),
                    usage: Some(usage),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    (store, run_id)
}

/// F5 — read back from the store and priced through the shipped path, the second
/// call costs a fraction of the first, and both equal the figure computed by hand
/// from the stored counts.
#[tokio::test]
async fn a_cached_read_is_billed_at_the_cache_rate_not_the_input_rate() {
    let (store, run_id) = store_the_pair();
    let table = PriceTable::new("2026-08-06").with(MODEL, priced(300_000));

    let rows = store.provider_calls(run_id).unwrap();
    assert_eq!(rows.len(), 2, "both calls were stored");

    let cost = |row: &ProviderCall| {
        table
            .cost_micros(row.model.as_deref().unwrap(), &row.usage.unwrap())
            .expect("the model is in the table")
    };
    let first = cost(&rows[0]);
    let second = cost(&rows[1]);

    // Computed by hand from the stored counts, not read back from the code under
    // test. Fresh input is the prompt minus what was served from cache, summed in
    // micro-units and rounded once:
    //   call 1: 7420*3_000_000 + 5*15_000_000                    -> 22_335
    //   call 2:   13*3_000_000 + 5*15_000_000 + 7408*300_000     ->  2_336
    assert_eq!(first, 22_335, "the uncached call");
    assert_eq!(second, 2_336, "the cached call");
    assert!(
        second < first,
        "a call that read {} tokens from cache must cost less than one that read none: \
         {second} against {first}",
        rows[1].usage.unwrap().cache_read_tokens
    );
}

/// F5's sabotage, kept as a permanent control rather than run once by hand.
///
/// With the cache rate set equal to the input rate, the same stored rows must stop
/// being cheaper — which is what proves the assertion above reads the *cache* rate
/// rather than merely observing a smaller number of fresh tokens.
#[tokio::test]
async fn with_no_cache_discount_the_cached_call_is_not_cheaper() {
    let (store, run_id) = store_the_pair();
    let flat = PriceTable::new("2026-08-06").with(MODEL, priced(3_000_000));

    let rows = store.provider_calls(run_id).unwrap();
    let cost = |row: &ProviderCall| {
        flat.cost_micros(row.model.as_deref().unwrap(), &row.usage.unwrap())
            .unwrap()
    };
    let (first, second) = (cost(&rows[0]), cost(&rows[1]));

    assert_eq!(first, 22_335);
    assert_eq!(second, 22_338, "the same tokens, none of them discounted");
    assert!(
        second >= first,
        "without a cache discount the cached call must not come out cheaper"
    );
}

/// The counter this crate cannot see, asserted rather than left as prose (N4).
///
/// Every one of the four live calls reported `cache_write_tokens` as zero,
/// including the marked call that must have written the entry the next one read.
/// `openai_wire` sets that field to zero by construction because the OpenAI wire
/// reports no write counter, so a run cached through OpenRouter under-reports the
/// write premium. This pins the shape of that gap: with the write count zero, the
/// tokens that were written are billed as ordinary fresh input.
#[tokio::test]
async fn an_unreported_cache_write_is_billed_as_fresh_input() {
    let table = PriceTable::new("2026-08-06").with(MODEL, priced(300_000));
    let [as_reported, _] = measured();

    // What the crate charged for the writing call, from what OpenRouter told it.
    let reported = table.cost_micros(MODEL, &as_reported).unwrap();

    // What it would have charged had the write been reported, at the 1.25x premium.
    let with_write = Usage {
        cache_write_tokens: 7408,
        ..as_reported
    };
    let honest = table.cost_micros(MODEL, &with_write).unwrap();

    assert!(
        honest > reported,
        "a reported write costs more than the same tokens billed as fresh input: \
         {honest} against {reported}"
    );
}
