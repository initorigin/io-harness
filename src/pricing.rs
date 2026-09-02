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

/// A rate that replaces a model's base [`Price`] once the prompt is long enough
/// (0.29.0).
///
/// A model does not necessarily have one price. Many vendors charge more per
/// token once a request passes a prompt-length threshold, and the step is
/// usually a doubling rather than a nudge — so a long agentic run priced against
/// the base row reports about half of what it cost.
///
/// The threshold is compared against [`Usage::prompt_tokens`], the highest one
/// at or below it wins, and that row prices the **whole** request. That is how
/// the vendors bill it: crossing the line re-rates everything, rather than
/// charging the first tranche at the old rate and the remainder at the new one.
///
/// `price` is a complete [`Price`], never a patch over the base — a tier that
/// named only the dimensions it changed would silently price the others at zero.
///
/// ```
/// use io_harness::pricing::{Price, PriceTable, PriceTier};
/// use io_harness::Usage;
///
/// // $1.25 per million input tokens, doubling to $2.50 once the prompt reaches
/// // 200k — the shape a long-context model is usually sold at.
/// let base = Price { input: 1_250_000, output: 10_000_000, ..Price::ZERO };
/// let prices = PriceTable::new("2026-08-01")
///     .with("some-vendor/long-context", base)
///     .with_tiers(
///         "some-vendor/long-context",
///         vec![PriceTier {
///             min_prompt_tokens: 200_000,
///             price: Price { input: 2_500_000, output: 15_000_000, ..Price::ZERO },
///         }],
///     );
///
/// let short = Usage { prompt_tokens: 100_000, total_tokens: 100_000, ..Default::default() };
/// let long = Usage { prompt_tokens: 400_000, total_tokens: 400_000, ..Default::default() };
///
/// // 100k at $1.25/M is $0.125; 400k at $2.50/M is $1.00 — and it is the whole
/// // 400k at the higher rate, not 200k at each.
/// assert_eq!(prices.cost_micros("some-vendor/long-context", &short), Some(125_000));
/// assert_eq!(prices.cost_micros("some-vendor/long-context", &long), Some(1_000_000));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTier {
    /// The prompt length, in tokens, at which this rate takes over. A request
    /// whose [`Usage::prompt_tokens`] is greater than or equal to this is priced
    /// by [`PriceTier::price`] unless a higher tier also applies.
    pub min_prompt_tokens: u64,
    /// The rate for a request that reaches the threshold, in the same
    /// micro-units per million tokens as every other [`Price`] here.
    pub price: Price,
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
    /// (0.29.0) Prompt-length tiers per model, held beside the base prices
    /// rather than inside [`Price`]. A `Vec` in that type would cost it its
    /// `Copy` impl, and `..Price::ZERO` and [`PriceTable::price`] both rest on
    /// it — a public break bought for a data shape.
    ///
    /// Empty for every model that prices flat, which is most of them, and
    /// `#[serde(default)]` so a table serialized by 0.28.0 still deserializes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tiers: BTreeMap<String, Vec<PriceTier>>,
}

impl PriceTable {
    /// An empty table, accurate as of `as_of` (any format the operator reads —
    /// it is recorded and shown, never parsed).
    pub fn new(as_of: impl Into<String>) -> Self {
        Self {
            as_of: as_of.into(),
            prices: BTreeMap::new(),
            tiers: BTreeMap::new(),
        }
    }

    /// Add or replace one model's price, for building a table in one expression.
    #[must_use]
    pub fn with(mut self, model: impl Into<String>, price: Price) -> Self {
        self.prices.insert(model.into(), price);
        self
    }

    /// Add or replace one model's prompt-length tiers (0.29.0).
    ///
    /// Sorted here rather than trusted from the caller, so the order they were
    /// written in cannot decide which tier applies. An empty list removes the
    /// model's tiers, which is how a table says "this one prices flat".
    #[must_use]
    pub fn with_tiers(mut self, model: impl Into<String>, mut tiers: Vec<PriceTier>) -> Self {
        let model = model.into();
        if tiers.is_empty() {
            self.tiers.remove(&model);
            return self;
        }
        tiers.sort_by_key(|t| t.min_prompt_tokens);
        self.tiers.insert(model, tiers);
        self
    }

    /// When the operator says these prices were accurate.
    pub fn as_of(&self) -> &str {
        &self.as_of
    }

    /// The base price entered for `model`, or `None` if none was.
    ///
    /// The *base* — a model with tiers costs this only while its prompt stays
    /// under the lowest threshold. Ask [`PriceTable::cost_micros`] what a
    /// particular request costs; this answers what the table holds.
    pub fn price(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied()
    }

    /// The prompt-length tiers entered for `model`, lowest threshold first, or
    /// an empty slice when it prices flat (0.29.0).
    pub fn tiers(&self, model: &str) -> &[PriceTier] {
        self.tiers.get(model).map_or(&[], |t| t.as_slice())
    }

    /// Every model this table can price, sorted (0.71.0).
    ///
    /// The models [`PriceTable::price`] and [`PriceTable::cost_micros`] will
    /// answer for, which is not every model the table mentions: [`PriceTier`]s
    /// are keyed separately, and a model given tiers but no base price is
    /// unpriced — `cost_micros` returns `None` for it. Listing it here would
    /// promise a cost the table cannot produce.
    ///
    /// ```
    /// use io_harness::pricing::{Price, PriceTable, PriceTier};
    ///
    /// let prices = PriceTable::new("2026-07-29")
    ///     .with("some-vendor/zeta", Price { input: 1_000_000, ..Price::ZERO })
    ///     .with("some-vendor/alpha", Price { input: 2_000_000, ..Price::ZERO })
    ///     .with_tiers(
    ///         "some-vendor/tiers-only",
    ///         vec![PriceTier { min_prompt_tokens: 200_000, price: Price::ZERO }],
    ///     );
    ///
    /// assert_eq!(prices.models(), vec!["some-vendor/alpha", "some-vendor/zeta"]);
    /// assert!(
    ///     !prices.models().contains(&"some-vendor/tiers-only"),
    ///     "tiers with no base price cannot be priced, so the model is not listed",
    /// );
    /// ```
    pub fn models(&self) -> Vec<&str> {
        self.prices.keys().map(String::as_str).collect()
    }

    /// How many models this table can price (0.71.0).
    ///
    /// ```
    /// use io_harness::pricing::{Price, PriceTable};
    ///
    /// assert_eq!(PriceTable::new("2026-07-29").with("a", Price::ZERO).len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// Whether the table prices nothing (0.71.0).
    ///
    /// ```
    /// use io_harness::pricing::PriceTable;
    ///
    /// assert!(PriceTable::new("2026-07-29").is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// The rate that prices a prompt of `prompt_tokens` on `model`: the highest
    /// tier at or below it, or the base price when it reaches none.
    fn rate(&self, model: &str, prompt_tokens: u64) -> Option<Price> {
        let base = self.price(model)?;
        Some(
            self.tiers(model)
                .iter()
                .rfind(|t| prompt_tokens >= t.min_prompt_tokens)
                .map_or(base, |t| t.price),
        )
    }

    /// What `usage` on `model` cost, in micro-units, or `None` when no price was
    /// entered for that model.
    ///
    /// The cache counters are treated as a breakdown of
    /// [`Usage::prompt_tokens`]: fresh input is the prompt minus what was read
    /// from and written to cache, so nothing is billed twice. A provider that
    /// reports cache figures larger than its own prompt total — which would be a
    /// vendor bug — saturates to zero fresh input rather than underflowing.
    ///
    /// (0.29.0) When the model has [`PriceTier`]s, the rate is the highest one
    /// whose threshold [`Usage::prompt_tokens`] reaches, and it prices the whole
    /// request. A model with no tiers is priced exactly as it was before they
    /// existed.
    pub fn cost_micros(&self, model: &str, usage: &Usage) -> Option<u64> {
        let p = self.rate(model, usage.prompt_tokens)?;
        // (0.75.0) An unreported cache write bills as fresh input, which is what
        // it was before the counter became `Option` and what the invoice from an
        // OpenAI-shaped endpoint actually says: that wire writes the cache
        // implicitly and charges a normal prompt token for it.
        let cache_write = usage.cache_write_tokens.unwrap_or(0);
        let fresh_input = usage
            .prompt_tokens
            .saturating_sub(usage.cache_read_tokens)
            .saturating_sub(cache_write);
        // Summed exactly in u128 and rounded once at the end. Rounding each line
        // and adding would drift by up to a half-unit per dimension per call,
        // which over a long trace is a figure that matches no invoice.
        let per_million = |tokens: u64, price: u64| tokens as u128 * price as u128;
        let mtok = per_million(fresh_input, p.input)
            + per_million(usage.completion_tokens, p.output)
            + per_million(usage.cache_read_tokens, p.cache_read)
            + per_million(cache_write, p.cache_write);
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
    /// (0.75.0) How many calls in this group came off a wire that reports no
    /// cache-write counter. Their write cost is unknown, not zero, so a group
    /// with calls here is a floor on the cache write exactly as
    /// [`Spend::unpriced_calls`] makes it a floor on the money — and for the
    /// same reason it is a field rather than a silence.
    pub unreported_cache_writes: u64,
}

impl Spend {
    /// (0.75.0) The share of this group's prompt tokens that were served from
    /// the provider's cache, or `None` for a group that summed no usage at all.
    ///
    /// A group whose calls all reported zero cached tokens answers `Some(0.0)`:
    /// a cache that never hit is a measurement, and an absent measurement is
    /// not. `None` means the group summed no prompt tokens at all — an empty
    /// group, or one whose every call reported no usage.
    ///
    /// **Pricing does not enter into it.** Usage is summed before a price is
    /// looked for, so a group whose every call is unpriced still rates; with no
    /// vendor prices shipped, that is the ordinary case for a partial
    /// [`PriceTable`]. [`Spend::unpriced_calls`] is what says the money is a
    /// floor.
    ///
    /// Read it beside [`Spend::unreported_cache_writes`]: a high rate over a
    /// group of calls whose writes were never reported is a read rate, not a
    /// complete accounting.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        self.usage.cache_hit_rate()
    }
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
        // (0.75.0) A group sums the writes it was told about and counts the
        // calls it was not. `None` survives only while NO call in the group
        // reported one, so a mixed group reports the reported half and says how
        // many calls are missing from it rather than quietly rounding them to
        // zero.
        match usage.cache_write_tokens {
            Some(n) => {
                *spend.usage.cache_write_tokens.get_or_insert(0) += n;
            }
            None => spend.unreported_cache_writes += 1,
        }
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
            cache_write_tokens: Some(100_000),
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

    // -----------------------------------------------------------------------
    // F11 — a tiered price is billed at the tier the prompt actually reached
    // -----------------------------------------------------------------------

    /// $1.25/M input doubling to $2.50/M at 200k — `google/gemini-2.5-pro`'s own
    /// shape, and `anthropic/claude-sonnet-4.5`'s at the same floor.
    fn tiered() -> PriceTable {
        PriceTable::new("2026-08-01")
            .with(
                "long",
                Price {
                    input: 1_250_000,
                    ..Price::ZERO
                },
            )
            .with_tiers(
                "long",
                vec![PriceTier {
                    min_prompt_tokens: 200_000,
                    price: Price {
                        input: 2_500_000,
                        ..Price::ZERO
                    },
                }],
            )
    }

    fn prompt(tokens: u64) -> Usage {
        Usage {
            prompt_tokens: tokens,
            total_tokens: tokens,
            ..Default::default()
        }
    }

    #[test]
    fn the_tier_boundary_is_exact_on_both_sides() {
        let t = tiered();
        // One token below the floor is still the base rate; the floor itself is
        // already the tier. An off-by-one here under-reports every request that
        // lands exactly on the line, which is why both sides are asserted rather
        // than one value comfortably past it.
        assert_eq!(t.cost_micros("long", &prompt(199_999)), Some(249_999));
        assert_eq!(t.cost_micros("long", &prompt(200_000)), Some(500_000));
        // And it re-rates the *whole* request rather than splitting it: 400k at
        // $2.50/M is $1.00, not 200k at each rate.
        assert_eq!(t.cost_micros("long", &prompt(400_000)), Some(1_000_000));
    }

    #[test]
    fn a_prompt_below_every_floor_gets_the_base_row_not_the_lowest_tier() {
        // The negative control for the test above. Without it, an implementation
        // that always took the first tier would pass every assertion that only
        // looked at long prompts.
        assert_eq!(tiered().cost_micros("long", &prompt(1_000)), Some(1_250));
    }

    #[test]
    fn the_highest_reached_floor_wins_whatever_order_the_tiers_were_written_in() {
        // `qwen/qwen3-max`'s shape: two steps, at 32k and 128k. Registered
        // highest-first on purpose, so the answer cannot come from input order.
        let t = PriceTable::new("x")
            .with(
                "two",
                Price {
                    input: 1_000_000,
                    ..Price::ZERO
                },
            )
            .with_tiers(
                "two",
                vec![
                    PriceTier {
                        min_prompt_tokens: 128_000,
                        price: Price {
                            input: 3_000_000,
                            ..Price::ZERO
                        },
                    },
                    PriceTier {
                        min_prompt_tokens: 32_000,
                        price: Price {
                            input: 2_000_000,
                            ..Price::ZERO
                        },
                    },
                ],
            );
        assert_eq!(t.cost_micros("two", &prompt(1_000_000)), Some(3_000_000));
        assert_eq!(t.cost_micros("two", &prompt(64_000)), Some(128_000));
        assert_eq!(t.cost_micros("two", &prompt(1_000)), Some(1_000));
        assert_eq!(
            t.tiers("two")
                .iter()
                .map(|x| x.min_prompt_tokens)
                .collect::<Vec<_>>(),
            vec![32_000, 128_000],
            "stored lowest-first however they arrived"
        );
    }

    #[test]
    fn the_same_usage_costs_strictly_less_once_the_tiers_are_removed() {
        // The control that the tiers are doing the work. Without it every
        // assertion above would also pass against an implementation that ignored
        // `with_tiers` entirely and priced everything at the base row.
        let long = prompt(400_000);
        let with = tiered().cost_micros("long", &long).unwrap();
        let without = tiered()
            .with_tiers("long", Vec::new())
            .cost_micros("long", &long)
            .unwrap();
        assert!(
            without < with,
            "removing the tier must make the same request cheaper: {without} vs {with}"
        );
        assert_eq!(without, 500_000);
    }

    #[test]
    fn a_table_with_no_tiers_prices_exactly_as_it_did_before_they_existed() {
        // NF5's arithmetic half, asserted rather than argued: every figure in
        // `a_hand_computed_million_token_figure_comes_out_exact` is produced by a
        // table that registers no tier at all, through the same `cost_micros`.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            cache_read_tokens: 500_000,
            cache_write_tokens: Some(100_000),
            reasoning_tokens: 200_000,
            server_tool_requests: 3,
        };
        assert!(table().tiers("m").is_empty());
        assert_eq!(
            table().cost_micros("m", &usage),
            Some(1_200_000 + 15_000_000 + 150_000 + 375_000 + 30_000)
        );
    }

    #[test]
    fn a_tier_on_a_model_with_no_base_price_is_still_unpriced() {
        // Tiers do not make a model priced. `cost_micros` reads the base first,
        // so a table carrying a tier and no price answers None rather than
        // pricing an unknown model from its tier.
        let t = PriceTable::new("x").with_tiers(
            "orphan",
            vec![PriceTier {
                min_prompt_tokens: 1,
                price: Price {
                    input: 9_000_000,
                    ..Price::ZERO
                },
            }],
        );
        assert_eq!(t.cost_micros("orphan", &prompt(1_000)), None);
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
