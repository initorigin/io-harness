//! A price table can say which models it prices — and only those.
//!
//! `PriceTable` holds two independently keyed maps: base `prices`, and `tiers`.
//! `cost_micros` needs a base price and returns `None` without one, so a model
//! given tiers and nothing else is *unpriced*. `models()` therefore enumerates
//! `prices`, not the union: a caller that asks the table what it covers and then
//! prices every answer must not be handed a model that comes back `None`.

use io_harness::pricing::{Price, PriceTable, PriceTier};
use io_harness::{Config, Usage};

/// The two models an operator actually wrote prices for, and one they only gave
/// prompt-length tiers.
const CHEAP: &str = "some-vendor/zeta";
const DEAR: &str = "some-vendor/alpha";
const TIERS_ONLY: &str = "some-vendor/tiers-only";

/// Built the way an operator builds one: a `[prices.models]` section through
/// `Config::prices`, then the tier-only model added on top. `models_first`
/// decides which order the two priced models are written in — the table must not
/// care.
fn table(models_first: bool) -> PriceTable {
    let priced = |model: &str, input: u64| {
        format!("[prices.models.\"{model}\"]\ninput = {input}\noutput = 0\n")
    };
    let (a, b) = if models_first {
        (priced(CHEAP, 1_000_000), priced(DEAR, 9_000_000))
    } else {
        (priced(DEAR, 9_000_000), priced(CHEAP, 1_000_000))
    };
    let toml = format!("[prices]\nas_of = \"2026-08-29\"\n\n{a}\n{b}");

    Config::from_toml(&toml)
        .unwrap()
        .prices()
        .expect("the file has a [prices] section")
        .with_tiers(
            TIERS_ONLY,
            vec![PriceTier {
                min_prompt_tokens: 200_000,
                price: Price {
                    input: 2_000_000,
                    ..Price::ZERO
                },
            }],
        )
}

/// The premise: the tier-only model really is one the table cannot price. If
/// this ever stops holding, listing it would no longer be a lie and the
/// exclusion below would be the thing to revisit.
#[test]
fn a_model_with_tiers_and_no_base_price_cannot_be_priced() {
    let table = table(true);
    let usage = Usage {
        prompt_tokens: 500_000,
        ..Default::default()
    };

    assert!(
        !table.tiers(TIERS_ONLY).is_empty(),
        "the table does hold tiers for it"
    );
    assert_eq!(table.price(TIERS_ONLY), None, "but no base price");
    assert_eq!(
        table.cost_micros(TIERS_ONLY, &usage),
        None,
        "so it has no cost, however long the prompt"
    );
    assert_eq!(
        table.cost_micros(CHEAP, &usage),
        Some(500_000),
        "a model with a base price does have one"
    );
}

/// `models()` is exactly the `[prices.models]` keys — the tier-only model is not
/// among them.
#[test]
fn models_lists_what_the_table_can_price_and_nothing_else() {
    let table = table(true);

    assert_eq!(
        table.models(),
        vec![DEAR, CHEAP],
        "both priced models, in sorted order"
    );
    assert!(
        !table.models().contains(&TIERS_ONLY),
        "a model the table returns None for must not be advertised as covered"
    );
    for model in table.models() {
        assert!(
            table.price(model).is_some(),
            "{model} was listed, so it must have a price"
        );
    }
}

/// `len` and `is_empty` count the same models `models()` lists.
#[test]
fn len_and_is_empty_agree_with_models() {
    let table = table(true);
    assert_eq!(table.len(), table.models().len());
    assert_eq!(table.len(), 2, "not 3 — the tier-only model is not priced");
    assert!(!table.is_empty());

    let empty = PriceTable::new("2026-08-29").with_tiers(
        TIERS_ONLY,
        vec![PriceTier {
            min_prompt_tokens: 1,
            price: Price::ZERO,
        }],
    );
    assert_eq!(empty.models(), Vec::<&str>::new());
    assert_eq!(empty.len(), 0);
    assert!(
        empty.is_empty(),
        "tiers alone price nothing, so the table is empty"
    );
}

/// The order is the `BTreeMap`'s, not the order the file listed the models in,
/// so a caller may render the list without sorting it again.
#[test]
fn the_order_is_deterministic_across_builds() {
    assert_eq!(table(true).models(), table(false).models());
    assert_eq!(
        table(false).models(),
        vec![DEAR, CHEAP],
        "sorted, though this file wrote {DEAR} second"
    );
}
