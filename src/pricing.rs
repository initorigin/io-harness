//! Turning recorded tokens into money (0.18.0).
//!
//! # Cost is derived, never stored
//!
//! The trace records what the provider reported — tokens, per call, per model —
//! and nothing else. Money is computed here, at read time, from a price table
//! the operator supplies. That is deliberate and it is the whole design: a price
//! written into the database is wrong the moment a vendor changes it or the
//! moment it was entered wrong, and repairing it means rewriting history. A
//! price kept outside the database is corrected once, and every past run reads
//! true.
//!
//! # The crate ships no prices
//!
//! [`PriceTable::new`] takes the date its prices were accurate as of, and starts
//! empty. There is no built-in list of vendor prices, because this crate cannot
//! keep one accurate: it publishes on its own schedule and vendors change prices
//! on theirs, so a shipped number would be a confident wrong answer for whoever
//! upgraded late — which is worse than an explicit unknown. An unpriced model
//! reports [`Spend::unpriced_calls`] rather than a zero.
//!
//! # The unit
//!
//! Everything here is an integer count of **micro-units**: millionths of one
//! unit of whatever currency the operator's prices are in. `f64` was not used
//! because a fraction of a cent that rounds on every one of a million rows
//! accumulates into a figure that does not match an invoice.
//!
//! ```
//! use io_harness::pricing::{Price, PriceTable};
//! use io_harness::Usage;
//!
//! // $3.00 per million input tokens, $15.00 per million output. Prices are
//! // micro-units per MILLION tokens, so $3.00 is 3_000_000.
//! let prices = PriceTable::new("2026-07-29").with(
//!     "some-vendor/some-model",
//!     Price { input: 3_000_000, output: 15_000_000, cache_read: 300_000, ..Price::ZERO },
//! );
//!
//! let usage = Usage {
//!     prompt_tokens: 1_000_000,
//!     completion_tokens: 100_000,
//!     total_tokens: 1_100_000,
//!     // 900k of that prompt was served from cache at a tenth the price, which
//!     // is the difference between $3.00 and $0.57 for the input alone.
//!     cache_read_tokens: 900_000,
//!     ..Default::default()
//! };
//!
//! // 100k fresh input at $3/M = $0.30, 900k cached at $0.30/M = $0.27,
//! // 100k output at $15/M = $1.50.
//! assert_eq!(prices.cost_micros("some-vendor/some-model", &usage), Some(2_070_000));
//!
//! // A model nobody entered a price for is unknown, not free.
//! assert_eq!(prices.cost_micros("some-other-model", &usage), None);
//! ```

use std::collections::BTreeMap;

use crate::provider::Usage;
use crate::state::ProviderCall;

/// Micro-units in one currency unit — the divisor between a price and a
/// human-facing figure.
pub const MICROS_PER_UNIT: u64 = 1_000_000;

/// What one model costs, in micro-units per million tokens.
///
/// Construct with `..Price::ZERO` and set only the dimensions the vendor
/// actually charges for, so a dimension nobody priced stays an explicit zero
/// rather than an accident.
///
/// Every field is `#[serde(default)]` for the same reason `..Price::ZERO` exists:
/// a config file that prices input and output should not have to write three
/// zeros to say the vendor charges nothing for cache and search.
///
/// ```
/// use io_harness::pricing::Price;
///
/// let flat: Price = serde_json::from_str(r#"{"input": 3000000}"#).unwrap();
/// assert_eq!(flat.input, 3_000_000);
/// assert_eq!(flat.cache_read, 0, "an unnamed dimension is an explicit zero");
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Price {
    /// Input tokens read fresh — the prompt minus anything served from or
    /// written to cache.
    pub input: u64,
    /// Tokens the model generated, reasoning tokens included: a vendor that
    /// reports reasoning separately still bills it as output.
    pub output: u64,
    /// Input tokens served from the provider's cache, usually a small fraction
    /// of [`Price::input`].
    pub cache_read: u64,
    /// Input tokens written into the provider's cache, usually above
    /// [`Price::input`] rather than below it.
    pub cache_write: u64,
    /// Per provider-executed tool request — a server-side search — priced per
    /// request rather than per million, because that is how it is billed.
    pub per_server_tool_request: u64,
}

impl Price {
    /// Every dimension free. The base for `..Price::ZERO`.
    pub const ZERO: Self = Self {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        per_server_tool_request: 0,
    };
}

/// Prices by model id, and the date they were accurate as of.
///
/// The date is required at construction because a price list with no date is a
/// claim with no expiry, and this one *will* go stale — the operator is the only
/// one who can know when it did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PriceTable {
    as_of: String,
    prices: BTreeMap<String, Price>,
}

impl PriceTable {
    /// An empty table, accurate as of `as_of` (any format the operator reads —
    /// it is recorded and shown, never parsed).
    pub fn new(as_of: impl Into<String>) -> Self {
        Self {
            as_of: as_of.into(),
            prices: BTreeMap::new(),
        }
    }

    /// Add or replace one model's price, for building a table in one expression.
    #[must_use]
    pub fn with(mut self, model: impl Into<String>, price: Price) -> Self {
        self.prices.insert(model.into(), price);
        self
    }

    /// When the operator says these prices were accurate.
    pub fn as_of(&self) -> &str {
        &self.as_of
    }

    /// The price entered for `model`, or `None` if none was.
    pub fn price(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied()
    }

    /// What `usage` on `model` cost, in micro-units, or `None` when no price was
    /// entered for that model.
    ///
    /// The cache counters are treated as a breakdown of
    /// [`Usage::prompt_tokens`]: fresh input is the prompt minus what was read
    /// from and written to cache, so nothing is billed twice. A provider that
    /// reports cache figures larger than its own prompt total — which would be a
    /// vendor bug — saturates to zero fresh input rather than underflowing.
    pub fn cost_micros(&self, model: &str, usage: &Usage) -> Option<u64> {
        let p = self.price(model)?;
        let fresh_input = usage
            .prompt_tokens
            .saturating_sub(usage.cache_read_tokens)
            .saturating_sub(usage.cache_write_tokens);
        // Summed exactly in u128 and rounded once at the end. Rounding each line
        // and adding would drift by up to a half-unit per dimension per call,
        // which over a long trace is a figure that matches no invoice.
        let per_million = |tokens: u64, price: u64| tokens as u128 * price as u128;
        let mtok = per_million(fresh_input, p.input)
            + per_million(usage.completion_tokens, p.output)
            + per_million(usage.cache_read_tokens, p.cache_read)
            + per_million(usage.cache_write_tokens, p.cache_write);
        let requests = usage.server_tool_requests as u128 * p.per_server_tool_request as u128;
        let micros = (mtok + 500_000) / 1_000_000 + requests;
        Some(micros.min(u64::MAX as u128) as u64)
    }
}

/// One group of provider calls: the raw sums, and what they cost.
///
/// Raw rows are where the crate stops. Streaks, leaderboards, per-day charts and
/// every other rendering are the consuming application's decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spend {
    /// What this group is keyed by — a model id, a day (`YYYY-MM-DD`), or a run
    /// id as a string, depending on which grouping produced it.
    pub key: String,
    /// Calls in the group, failed attempts included.
    pub calls: u64,
    /// Summed token counts across the group.
    pub usage: Usage,
    /// What the priced calls in this group cost, in micro-units.
    pub cost_micros: u64,
    /// How many calls could not be priced — no model recorded, or no price
    /// entered for it. A group with calls here is reporting a floor, not a
    /// total, and a renderer that hides this number is lying by omission.
    pub unpriced_calls: u64,
}

/// Sum `calls` into one group under `key`, pricing what can be priced.
pub(crate) fn group(key: impl Into<String>, calls: &[&ProviderCall], prices: &PriceTable) -> Spend {
    let mut spend = Spend {
        key: key.into(),
        calls: calls.len() as u64,
        ..Default::default()
    };
    for call in calls {
        let Some(usage) = call.usage else {
            // The provider reported nothing at all: it cannot be summed and it
            // cannot be priced. Counting it as unpriced is what keeps the group
            // honest about being a floor.
            spend.unpriced_calls += 1;
            continue;
        };
        spend.usage.prompt_tokens += usage.prompt_tokens;
        spend.usage.completion_tokens += usage.completion_tokens;
        spend.usage.total_tokens += usage.total_tokens;
        spend.usage.cache_read_tokens += usage.cache_read_tokens;
        spend.usage.cache_write_tokens += usage.cache_write_tokens;
        spend.usage.reasoning_tokens += usage.reasoning_tokens;
        spend.usage.server_tool_requests += usage.server_tool_requests;
        match call
            .model
            .as_deref()
            .and_then(|m| prices.cost_micros(m, &usage))
        {
            Some(micros) => spend.cost_micros += micros,
            None => spend.unpriced_calls += 1,
        }
    }
    spend
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PriceTable {
        PriceTable::new("2026-07-29").with(
            "m",
            Price {
                input: 3_000_000,
                output: 15_000_000,
                cache_read: 300_000,
                cache_write: 3_750_000,
                per_server_tool_request: 10_000,
            },
        )
    }

    #[test]
    fn a_hand_computed_million_token_figure_comes_out_exact() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            cache_read_tokens: 500_000,
            cache_write_tokens: 100_000,
            reasoning_tokens: 200_000,
            server_tool_requests: 3,
        };
        // 400k fresh @3/M = 1_200_000; 1M output @15/M = 15_000_000;
        // 500k cache-read @0.30/M = 150_000; 100k cache-write @3.75/M = 375_000;
        // 3 requests @0.01 = 30_000. Reasoning is inside output and is not
        // charged twice.
        assert_eq!(
            table().cost_micros("m", &usage),
            Some(1_200_000 + 15_000_000 + 150_000 + 375_000 + 30_000)
        );
    }

    #[test]
    fn an_unpriced_model_is_unknown_rather_than_free() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 10,
            total_tokens: 20,
            ..Default::default()
        };
        // The negative control for the assertion above: without it, a table that
        // returned Some(0) for everything would pass every cost test here.
        assert_eq!(table().cost_micros("not-in-the-table", &usage), None);
        assert_eq!(table().cost_micros("m", &usage), Some(180));
    }

    #[test]
    fn cache_figures_larger_than_the_prompt_saturate_rather_than_underflow() {
        // A vendor bug, or a provider whose cache counters are additive rather
        // than a breakdown. Either way the fresh-input line is zero, not a
        // wrapped u64 costing several billion.
        let usage = Usage {
            prompt_tokens: 10,
            cache_read_tokens: 900,
            total_tokens: 10,
            ..Default::default()
        };
        assert_eq!(table().cost_micros("m", &usage), Some(270));
    }

    #[test]
    fn rounding_is_once_at_the_end_and_half_up() {
        let half = PriceTable::new("x").with(
            "m",
            Price {
                input: 1,
                ..Price::ZERO
            },
        );
        // 500_000 tokens at 1 micro/million is exactly half a micro-unit.
        let usage = Usage {
            prompt_tokens: 500_000,
            total_tokens: 500_000,
            ..Default::default()
        };
        assert_eq!(half.cost_micros("m", &usage), Some(1));
        let under = Usage {
            prompt_tokens: 499_999,
            total_tokens: 499_999,
            ..Default::default()
        };
        assert_eq!(half.cost_micros("m", &under), Some(0));
    }
}
