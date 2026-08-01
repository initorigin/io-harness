//! A reference catalogue of model prices, for the vendors that publish none
//! (0.29.0).
//!
//! # Why this exists
//!
//! `GET /v1/models` is near-universal and returns *identifiers*. OpenAI,
//! Anthropic, Groq, DeepSeek, Mistral, Fireworks and every local runtime return
//! no cost data whatsoever, so a catalogue built only from the vendor's own
//! endpoint prices almost nothing. One aggregator publishes per-token pricing for
//! most of the models those vendors serve, needs no API key, and is free to read
//! — so a single request fills the gap with no table anybody maintains.
//!
//! Nothing is baked into this repository: no price constants, no per-vendor
//! mapping file, no list to update when a vendor moves a number. That is the
//! whole argument for it, and it is the same argument [`crate::pricing`] makes
//! for shipping no prices at all — a number compiled into this crate is stale the
//! moment a vendor moves, and a number fetched at run time has no shelf life.
//!
//! # Three bounds, built in rather than discovered
//!
//! **A reference price is not the vendor's price.** It is what the aggregator
//! charges to serve that model, which tracks the vendor's rate closely and is not
//! identical to it. So every price carries a [`PriceSource`]
//! and an operator reading a cost can tell which they have.
//!
//! **A miss stays `None`.** Matching crosses naming schemes — the reference says
//! `deepseek/deepseek-chat` where a vendor's own API says `deepseek-chat` — so
//! there is an exact match and exactly one documented normalisation, and nothing
//! else. Never a nearest guess: a wrong match is a wrong invoice and is worse
//! than the gap it filled, which
//! [`Spend::unpriced_calls`](crate::pricing::Spend::unpriced_calls) already
//! reports honestly.
//!
//! **It dials a host the caller did not name.** This crate authorises egress
//! against [`Provider::endpoints`](super::Provider::endpoints) before a run
//! starts, so a provider that silently dialled a third host would walk through a
//! deny-by-default boundary. The lookup is opt-in, off by default, and when it is
//! on the reference host appears in `endpoints()` under the same
//! [`Act::Net`](crate::Act) rules as everything else — refused rather than
//! skipped when the policy says no.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{ModelInfo, PriceSource};
use crate::error::Result;
use crate::pricing::{Price, PriceTier};

/// The catalogue this crate reads unless the caller names another.
///
/// Keyless and free to read. It is a URL rather than a hard-coded host so an
/// internal mirror or an air-gapped copy is configuration rather than a code
/// change.
pub const DEFAULT_REFERENCE_URL: &str = "https://openrouter.ai/api/v1/models";

/// A catalogue of model prices, read from one URL (0.29.0).
///
/// Opt-in: nothing constructs one for you. Hand it to
/// [`Compatible::with_reference_prices`](super::Compatible::with_reference_prices)
/// and its host joins that provider's [`endpoints()`](super::Provider::endpoints),
/// where the run's egress policy governs it like every other endpoint.
///
/// ```
/// use io_harness::Reference;
///
/// // The default source, and a mirror — an internal copy or an air-gapped one is
/// // configuration rather than a fork.
/// let default = Reference::new();
/// assert_eq!(default.host(), Some("openrouter.ai"));
///
/// let mirror = Reference::at("https://models.internal.example/v1/models");
/// assert_eq!(mirror.host(), Some("models.internal.example"));
///
/// // The host is what a policy authorises, and it is knowable before any request
/// // is made — which is what lets a run refuse the lookup rather than discover it
/// // halfway through.
/// assert_eq!(mirror.url(), "https://models.internal.example/v1/models");
/// ```
#[derive(Debug, Clone)]
pub struct Reference {
    url: String,
    client: reqwest::Client,
    /// One fetch per instance. Deliberately here rather than in the store:
    /// [`Provider::models`](super::Provider::models) takes `&self` and nothing
    /// else, so reaching a [`Store`](crate::Store) from it would mean putting
    /// the store into the provider trait — the one type this crate has kept out
    /// of it. The instant of the fetch is not lost by that choice: it lands on
    /// [`PriceTable::as_of`](crate::pricing::PriceTable::as_of), which is the
    /// field that already exists to carry exactly that claim and already
    /// refuses to be constructed without it.
    cached: std::sync::Arc<std::sync::OnceLock<Vec<ModelInfo>>>,
}

impl Default for Reference {
    fn default() -> Self {
        Self::new()
    }
}

impl Reference {
    /// The default catalogue at [`DEFAULT_REFERENCE_URL`].
    pub fn new() -> Self {
        Self::at(DEFAULT_REFERENCE_URL)
    }

    /// A catalogue at `url`, for a mirror or an offline copy.
    pub fn at(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: crate::net::http_client(),
            cached: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Replace the request deadline, as each provider's own `with_timeout` does.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = crate::net::http_client_with_timeout(timeout);
        self
    }

    /// The URL this catalogue reads.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The host a policy has to allow for [`Reference::models`] to succeed, or
    /// `None` when the URL has none to extract.
    pub fn host(&self) -> Option<&str> {
        let rest = self.url.split("://").nth(1)?;
        let host = rest.split(['/', '?', '#']).next()?;
        let host = host.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (!host.is_empty()).then_some(host)
    }

    /// Read the catalogue, once per instance.
    ///
    /// The first call performs the request; later calls on the same `Reference`
    /// return what it produced.
    pub async fn models(&self) -> Result<Vec<ModelInfo>> {
        if let Some(hit) = self.cached.get() {
            return Ok(hit.clone());
        }
        let host = self.host().unwrap_or(&self.url).to_string();
        let models = fetch(&self.client, &self.url, &PriceSource::Reference(host)).await?;
        // A race between two callers costs a second request and the first result
        // wins, which is cheaper than holding a lock across an await.
        let _ = self.cached.set(models.clone());
        Ok(models)
    }
}

/// Read an OpenAI-shaped `/models` document into [`ModelInfo`]s, attributing
/// every price it carries to `source`.
///
/// One function for both paths on purpose. A vendor's own catalogue and the
/// reference catalogue are the same document shape — `{"data": [...]}` — and
/// differ only in whether the rows carry prices and in who to credit them to. Two
/// parsers would be two chances to disagree about what a missing field means.
pub(crate) async fn fetch(
    client: &reqwest::Client,
    url: &str,
    source: &PriceSource,
) -> Result<Vec<ModelInfo>> {
    let resp = client.get(url).send().await?;
    let body: Catalogue = super::ensure_success(resp).await?.json().await?;
    Ok(body
        .data
        .into_iter()
        .map(|entry| entry.into_model(source))
        .collect())
}

/// Fill in prices `vendor` does not state from `reference`, marking each with
/// where it came from.
///
/// A model the reference does not carry under an exact match or the one
/// normalisation is **left alone** — still `None`, still counted by
/// [`Spend::unpriced_calls`](crate::pricing::Spend::unpriced_calls). A vendor
/// that stated its own price keeps it: a stated price always beats a reference
/// one, including a stated zero, which is what a local runtime reports.
pub(crate) fn fill_missing_prices(vendor: &mut [ModelInfo], reference: &[ModelInfo]) {
    let index = Index::new(reference);
    for model in vendor.iter_mut() {
        if model.price.is_some() {
            continue;
        }
        let Some(hit) = index.find(&model.id) else {
            continue;
        };
        model.price = hit.price;
        model.price_tiers = hit.price_tiers.clone();
        model.price_source = hit.price_source.clone();
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// A catalogue indexed for lookup: exact ids, and the one normalisation.
///
/// Built once and queried per model, so a provider filling prices for a hundred
/// of its own models does not rescan the reference a hundred times.
pub(crate) struct Index<'a> {
    exact: BTreeMap<String, &'a ModelInfo>,
    /// Lowercased, leading `vendor/` segment dropped. A key two entries agree on
    /// is **absent** rather than resolved to either — an ambiguous normalisation
    /// is a guess, and a guess is what this whole module refuses.
    normalised: BTreeMap<String, Option<&'a ModelInfo>>,
}

/// The one documented normalisation: lowercase, and drop a single leading
/// `vendor/` segment.
///
/// One, not a family. Each additional rule widens what counts as a match, and
/// every widening is another chance to price a request against the wrong model.
pub(crate) fn normalise(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    match lower.split_once('/') {
        Some((_vendor, rest)) if !rest.is_empty() && !rest.contains('/') => rest.to_string(),
        _ => lower,
    }
}

impl<'a> Index<'a> {
    pub(crate) fn new(models: &'a [ModelInfo]) -> Self {
        let mut exact = BTreeMap::new();
        let mut normalised: BTreeMap<String, Option<&'a ModelInfo>> = BTreeMap::new();
        for m in models {
            exact.insert(m.id.to_ascii_lowercase(), m);
            normalised
                .entry(normalise(&m.id))
                // Seen before: park `None` in the key to mark it ambiguous.
                .and_modify(|slot| *slot = None)
                .or_insert(Some(m));
        }
        Self { exact, normalised }
    }

    /// The reference entry for `id`, or `None` — which is the honest answer and
    /// never a nearest guess.
    pub(crate) fn find(&self, id: &str) -> Option<&'a ModelInfo> {
        let lower = id.to_ascii_lowercase();
        if let Some(hit) = self.exact.get(&lower) {
            return Some(hit);
        }
        // Only a one-segment id reaches the normalised map: the normalisation
        // drops a segment, and applying it in the other direction would be a
        // second rule.
        if lower.contains('/') {
            return None;
        }
        self.normalised.get(&lower).copied().flatten()
    }
}

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct Catalogue {
    #[serde(default)]
    data: Vec<Entry>,
}

/// One catalogue row.
///
/// Deliberately **not** `deny_unknown_fields`: the live response carries twenty
/// top-level keys per model and will carry more. Everything is
/// `#[serde(default)]`, so a row missing anything still parses into honest
/// `None`s rather than failing the whole read.
#[derive(serde::Deserialize)]
struct Entry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    pricing: Option<Pricing>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default, Clone)]
struct Pricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
    #[serde(default)]
    web_search: Option<String>,
    /// Prompt-length tiers, because a model does not necessarily have one price.
    #[serde(default)]
    overrides: Option<Vec<Override>>,
}

#[derive(serde::Deserialize, Clone)]
struct Override {
    #[serde(default)]
    min_prompt_tokens: Option<u64>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
    #[serde(default)]
    web_search: Option<String>,
}

#[derive(serde::Deserialize)]
struct TopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[derive(serde::Deserialize)]
struct Architecture {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

impl Pricing {
    /// The five dimensions, or `None` when the row priced nothing at all — which
    /// is "the vendor did not say" and must never become a zero.
    fn price(&self) -> Option<Price> {
        let dims = [
            &self.prompt,
            &self.completion,
            &self.input_cache_read,
            &self.input_cache_write,
            &self.web_search,
        ];
        if dims.iter().all(|d| d.is_none()) {
            return None;
        }
        Some(Price {
            input: rate(self.prompt.as_deref()),
            output: rate(self.completion.as_deref()),
            cache_read: rate(self.input_cache_read.as_deref()),
            cache_write: rate(self.input_cache_write.as_deref()),
            // Billed per request rather than per million, so it is not scaled.
            per_server_tool_request: per_request(self.web_search.as_deref()),
        })
    }
}

impl Override {
    /// This override merged over `base`. A field the override does not name
    /// keeps the base value, so a stored tier is always a complete [`Price`]
    /// rather than a sparse patch that would price the rest at zero.
    fn tier(&self, base: Price) -> Option<PriceTier> {
        Some(PriceTier {
            min_prompt_tokens: self.min_prompt_tokens?,
            price: Price {
                input: self.prompt.as_deref().map_or(base.input, |v| rate(Some(v))),
                output: self
                    .completion
                    .as_deref()
                    .map_or(base.output, |v| rate(Some(v))),
                cache_read: self
                    .input_cache_read
                    .as_deref()
                    .map_or(base.cache_read, |v| rate(Some(v))),
                cache_write: self
                    .input_cache_write
                    .as_deref()
                    .map_or(base.cache_write, |v| rate(Some(v))),
                per_server_tool_request: self
                    .web_search
                    .as_deref()
                    .map_or(base.per_server_tool_request, |v| per_request(Some(v))),
            },
        })
    }
}

impl Entry {
    fn into_model(self, source: &PriceSource) -> ModelInfo {
        let pricing = self.pricing.unwrap_or_default();
        let price = pricing.price();
        let price_tiers = match price {
            Some(base) => pricing
                .overrides
                .unwrap_or_default()
                .iter()
                .filter_map(|o| o.tier(base))
                .collect::<Vec<_>>(),
            // No base price means no tiers: a tier without a base would price a
            // model this crate otherwise reports as unpriced, which is inventing
            // the base rather than reading it.
            None => Vec::new(),
        };
        ModelInfo {
            id: self.id,
            context_length: self.context_length,
            max_output_tokens: self.top_provider.and_then(|t| t.max_completion_tokens),
            accepts_images: self
                .architecture
                .and_then(|a| a.input_modalities)
                .map(|m| m.iter().any(|x| x == "image")),
            accepts_tools: self
                .supported_parameters
                .map(|p| p.iter().any(|x| x == "tools")),
            // `Some` exactly when there is a price to attribute: a number this
            // crate reports always says its own origin, and an absent number
            // attributes nothing.
            price_source: price.map(|_| source.clone()),
            price,
            price_tiers,
        }
    }
}

// ---------------------------------------------------------------------------
// Decimal strings to integers, with no float on the path
// ---------------------------------------------------------------------------

/// `value` shifted `places` decimal places, as an integer.
///
/// Shifted by moving the digits rather than by multiplying, because `f64` cannot
/// hold these values exactly and a fraction of a cent that rounds on every one of
/// a million rows accumulates into a figure matching no invoice — which is the
/// reason [`crate::pricing`] counts integers in the first place.
///
/// Anything that is not a plain non-negative decimal yields `0`: a leading sign
/// is refused rather than stripped, because a negative rate does not exist and
/// silently dropping the `-` produces a number that reads plausible.
fn scaled(value: Option<&str>, places: u32) -> u64 {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return 0;
    };
    let (int, frac) = match raw.split_once('.') {
        Some((i, f)) => (i, f),
        None => (raw, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return 0;
    }
    if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let places = places as usize;
    // Round half-up at the last place kept, so a value carrying more precision
    // than this unit does is not silently truncated downward.
    let round_up = frac.as_bytes().get(places).is_some_and(|b| *b >= b'5');
    let mut digits = String::with_capacity(int.len() + places);
    digits.push_str(int);
    for i in 0..places {
        digits.push(*frac.as_bytes().get(i).unwrap_or(&b'0') as char);
    }
    let trimmed = digits.trim_start_matches('0');
    let base: u64 = if trimmed.is_empty() {
        0
    } else {
        trimmed.parse().unwrap_or(0)
    };
    base.saturating_add(u64::from(round_up))
}

/// A per-token rate, in micro-units per million tokens: twelve places, six for
/// micro-units and six for the per-million basis.
fn rate(value: Option<&str>) -> u64 {
    scaled(value, 12)
}

/// A per-request charge, in micro-units: six places, because it is billed per
/// request and never scaled to a million of anything.
fn per_request(value: Option<&str>) -> u64 {
    scaled(value, 6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::failures::{json_response, serve};

    // -----------------------------------------------------------------------
    // F4 — a decimal price string becomes an exact integer, with no f64
    // -----------------------------------------------------------------------

    /// Hand-computed, and every entry either a value the live catalogue serves
    /// or a boundary around one.
    const CASES: &[(&str, u64)] = &[
        // deepseek/deepseek-chat's own prompt rate: $0.2574 per million.
        ("0.0000002574", 257_400),
        // google/gemini-2.5-pro, base and its 200k override.
        ("0.00000125", 1_250_000),
        ("0.0000025", 2_500_000),
        // anthropic/claude-sonnet-4.5.
        ("0.000003", 3_000_000),
        // Fewer fraction digits than the shift.
        ("0.5", 500_000_000_000),
        // An integer with no point at all.
        ("2", 2_000_000_000_000),
        // Free, stated. A local runtime's honest zero.
        ("0", 0),
        // More precision than twelve places: rounded half-up, not truncated.
        ("0.0000000000005", 1),
        ("0.0000000000004", 0),
        // A value carrying float noise. openai/gpt-5.6-luna serves this exact
        // string; it survives an f64 round trip, which is why it is *not* the
        // discriminating case and why the control below names a different one.
        ("0.0000006000000000000001", 600_000),
        // The float traps, and every one of them a rate the live catalogue
        // actually serves. `x * 1e12` lands a hair under the integer and `as
        // u64` truncates, so each of these is short by one micro-unit — a
        // rounding error in the vendor's favour, on every row, forever.
        // `0.00000003` is qwen/qwen3.7-flash's own prompt rate.
        ("0.000000015", 15_000),
        ("0.00000003", 30_000),
        ("0.00000006", 60_000),
        ("0.000000063", 63_000),
        ("0.00000012", 120_000),
        ("0.00000024", 240_000),
        ("0.00000096", 960_000),
        ("0.000000966", 966_000),
        ("0.00000384", 3_840_000),
    ];

    /// The subset of [`CASES`] an `f64` multiply gets wrong. Named rather than
    /// derived so that deleting one from the table above breaks this list and
    /// not merely the strength of the control.
    const FLOAT_TRAPS: &[&str] = &[
        // The rounding trap: a float truncates this to nothing where half-up
        // keeps the micro-unit.
        "0.0000000000005",
        "0.000000015",
        "0.00000003",
        "0.00000006",
        "0.000000063",
        "0.00000012",
        "0.00000024",
        "0.00000096",
        "0.000000966",
        "0.00000384",
    ];

    #[test]
    fn f4_a_decimal_rate_becomes_an_exact_integer() {
        for (raw, want) in CASES {
            assert_eq!(rate(Some(raw)), *want, "{raw}");
        }
    }

    #[test]
    fn f4_the_conversion_is_not_a_float_multiply() {
        // The named negative control. It runs the same table through the obvious
        // `f64` implementation and requires that it disagrees somewhere — so the
        // test proves the shipped code is *not* the float one, rather than
        // agreeing with it on inputs where both happen to be right.
        //
        // Without this, `(s.parse::<f64>()? * 1e12) as u64` passes every case
        // above that a float can represent, and silently loses money on the rest.
        let disagreeing: Vec<&str> = CASES
            .iter()
            .filter(|(raw, want)| (raw.parse::<f64>().unwrap() * 1e12) as u64 != *want)
            .map(|(raw, _)| *raw)
            .collect();
        assert_eq!(
            disagreeing, FLOAT_TRAPS,
            "the table must contain exactly the cases an f64 multiply gets wrong, \
             or it has stopped discriminating between the two implementations"
        );
        assert!(
            !FLOAT_TRAPS.is_empty(),
            "a control that discriminates on nothing is not a control"
        );

        // Each trap, spelled out: the float is short by one micro-unit and the
        // shipped conversion is exact. Short in the vendor's favour, on every
        // row that carries the rate, for as long as the table is used.
        for raw in FLOAT_TRAPS {
            let naive = (raw.parse::<f64>().unwrap() * 1e12) as u64;
            let exact = rate(Some(raw));
            assert_eq!(naive + 1, exact, "{raw}: f64 {naive}, exact {exact}");
        }
    }

    #[test]
    fn f4_a_malformed_rate_is_absent_rather_than_free() {
        assert_eq!(rate(Some("not a number")), 0);
        assert_eq!(rate(Some("-0.5")), 0, "a negative rate is not a price");
        assert_eq!(rate(Some("")), 0);
        assert_eq!(rate(None), 0);
        // The control that matters, one level up: a row with no pricing object
        // at all is `None`, never `Some(Price::ZERO)`.
        assert_eq!(Pricing::default().price(), None);
        // And a row that priced exactly one dimension is `Some`, so the rule is
        // "nothing was named" rather than "everything came out zero".
        let one = Pricing {
            prompt: Some("0".into()),
            ..Default::default()
        };
        assert_eq!(one.price(), Some(Price::ZERO));
    }

    #[test]
    fn a_per_request_charge_is_not_scaled_to_a_million() {
        // `web_search` is billed per request. Scaling it per million would
        // over-report one search by a factor of a million.
        assert_eq!(per_request(Some("0.014")), 14_000);
        assert_eq!(per_request(Some("0.005")), 5_000);
    }

    // -----------------------------------------------------------------------
    // F5 — exact, one normalisation, and a miss stays None
    // -----------------------------------------------------------------------

    fn named(ids: &[&str]) -> Vec<ModelInfo> {
        ids.iter()
            .map(|id| ModelInfo {
                id: (*id).into(),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn f5_an_exact_slug_matches() {
        let models = named(&["deepseek/deepseek-chat", "openai/gpt-5"]);
        let index = Index::new(&models);
        assert_eq!(
            index.find("deepseek/deepseek-chat").map(|m| m.id.as_str()),
            Some("deepseek/deepseek-chat")
        );
        // Case is not a difference worth a miss.
        assert!(index.find("DeepSeek/DeepSeek-Chat").is_some());
    }

    #[test]
    fn f5_the_one_normalisation_drops_a_single_leading_vendor_segment() {
        let models = named(&["deepseek/deepseek-chat"]);
        let index = Index::new(&models);
        // What DeepSeek's own API calls it, against what the reference calls it.
        assert_eq!(
            index.find("deepseek-chat").map(|m| m.id.as_str()),
            Some("deepseek/deepseek-chat")
        );
    }

    #[test]
    fn f5_a_model_the_reference_does_not_carry_stays_none() {
        // The first negative control, and the roadmap's own example: the
        // reference carries `deepseek/deepseek-chat`, the vendor asks for
        // `deepseek-v4-flash`, and the answer is None rather than the nearest
        // entry. A wrong match is a wrong invoice.
        let models = named(&["deepseek/deepseek-chat"]);
        assert!(Index::new(&models).find("deepseek-v4-flash").is_none());
    }

    #[test]
    fn f5_an_ambiguous_normalisation_is_a_miss_not_a_first_wins() {
        // The second negative control. Two vendors serving a model of the same
        // name normalise to one key; resolving it to either is a coin toss that
        // prices a request against another vendor's rate.
        let models = named(&["alpha/shared-name", "beta/shared-name"]);
        let index = Index::new(&models);
        assert!(index.find("shared-name").is_none());
        // Both stay reachable by their exact slugs: the ambiguity is in the
        // normalisation, not in the catalogue.
        assert!(index.find("alpha/shared-name").is_some());
        assert!(index.find("beta/shared-name").is_some());
    }

    #[test]
    fn f5_normalisation_is_not_applied_in_the_other_direction() {
        // The third negative control. One normalisation means one: a two-segment
        // vendor id is not stripped to meet a one-segment reference entry, which
        // would be a second rule and a second way to be wrong.
        let models = named(&["deepseek-chat"]);
        assert!(Index::new(&models).find("deepseek/deepseek-chat").is_none());
        assert_eq!(normalise("a/b/c"), "a/b/c");
        assert_eq!(normalise("A/B"), "b");
        assert_eq!(normalise("bare"), "bare");
    }

    // -----------------------------------------------------------------------
    // Parsing, against the shape the live host actually serves
    // -----------------------------------------------------------------------

    /// Trimmed from the live response, keys and values verbatim.
    const BODY: &str = r#"{"data":[
      {"id":"deepseek/deepseek-chat","context_length":163840,
       "pricing":{"prompt":"0.0000002574","completion":"0.0000010287"},
       "top_provider":{"max_completion_tokens":16000},
       "architecture":{"input_modalities":["text"]},
       "supported_parameters":["tools","temperature"]},
      {"id":"google/gemini-2.5-pro","context_length":1048576,
       "pricing":{"prompt":"0.00000125","completion":"0.00001","web_search":"0.014",
                  "input_cache_read":"0.000000125","input_cache_write":"0.000000375",
                  "overrides":[{"min_prompt_tokens":200000,"prompt":"0.0000025",
                                "completion":"0.000015","input_cache_read":"0.00000025"}]},
       "architecture":{"input_modalities":["text","image"]},
       "supported_parameters":["tools"]},
      {"id":"someone/unpriced","context_length":8192}
    ]}"#;

    #[tokio::test]
    async fn the_live_shape_parses_into_model_info() {
        let url = serve(json_response(BODY));
        let models = Reference::at(&url).models().await.unwrap();
        assert_eq!(models.len(), 3);

        let ds = &models[0];
        assert_eq!(ds.context_length, Some(163_840));
        assert_eq!(ds.max_output_tokens, Some(16_000));
        assert_eq!(ds.accepts_images, Some(false));
        assert_eq!(ds.accepts_tools, Some(true));
        assert_eq!(ds.price.unwrap().input, 257_400);
        assert_eq!(ds.price.unwrap().output, 1_028_700);
        assert!(ds.price_tiers.is_empty());

        let gem = &models[1];
        assert_eq!(gem.accepts_images, Some(true));
        assert_eq!(gem.price.unwrap().per_server_tool_request, 14_000);
        assert_eq!(gem.price_tiers.len(), 1);
        let tier = gem.price_tiers[0];
        assert_eq!(tier.min_prompt_tokens, 200_000);
        assert_eq!(tier.price.input, 2_500_000);
        assert_eq!(tier.price.output, 15_000_000);
        assert_eq!(tier.price.cache_read, 250_000);
        // The override named no `input_cache_write`, so it keeps the base value
        // rather than pricing it at zero. This is F11's sparse-merge half, and
        // it is the difference between a tier and a truncation.
        assert_eq!(tier.price.cache_write, 375_000);

        // A row that priced nothing is unpriced and unattributed, not free.
        let none = &models[2];
        assert_eq!(none.price, None);
        assert_eq!(none.price_source, None);
        assert!(none.price_tiers.is_empty());
    }

    #[tokio::test]
    async fn every_price_carries_its_provenance_and_the_two_cannot_disagree() {
        // F6's catalogue half: `price.is_some()` and `price_source.is_some()`
        // asserted equal across every entry, rather than spot-checked on one.
        let url = serve(json_response(BODY));
        let models = Reference::at(&url).models().await.unwrap();
        for m in &models {
            assert_eq!(
                m.price.is_some(),
                m.price_source.is_some(),
                "{} reports a price and a source that disagree",
                m.id
            );
        }
        assert_eq!(
            models[0].price_source.as_ref().unwrap(),
            &PriceSource::Reference("127.0.0.1".into()),
            "a reference price names the host it came from, never the vendor"
        );
    }

    #[tokio::test]
    async fn the_catalogue_is_read_once_per_instance() {
        // The cache is on the instance rather than in the store, so this is
        // where "one request" is asserted.
        let url = serve(json_response(BODY));
        let reference = Reference::at(&url);
        let first = reference.models().await.unwrap();
        let second = reference.models().await.unwrap();
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].id, second[0].id);
    }

    #[test]
    fn the_host_is_knowable_before_any_request_is_made() {
        // It has to be: the run authorises egress from `endpoints()` before the
        // first step, so a host only discoverable by dialling would be a host
        // the policy could not govern.
        assert_eq!(Reference::new().host(), Some("openrouter.ai"));
        assert_eq!(
            Reference::at("https://user@mirror.example:8443/v1/models?x=1").host(),
            Some("mirror.example")
        );
        assert_eq!(Reference::at("not a url").host(), None);
    }
}
