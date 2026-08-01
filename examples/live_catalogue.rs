//! F10 — the live reference host serves the shape the parser was written against.
//!
//! Run by hand at release time, never by CI: a network test in the suite is a
//! flaky suite. What it proves is the one thing a fixture cannot — that the
//! shape the shipped parser assumes is the shape the host actually serves.
use io_harness::{PriceSource, Reference};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let reference = Reference::new();
    println!("GET {}", reference.url());
    let models = reference.models().await?;

    let priced = models.iter().filter(|m| m.price.is_some()).count();
    let tiered = models.iter().filter(|m| !m.price_tiers.is_empty()).count();
    println!("models: {}", models.len());
    println!("priced: {priced}");
    println!("tiered: {tiered}");

    // Every one of the five dimensions the mapping claims, seen on a real row.
    let full = models.iter().find(|m| {
        m.price.is_some_and(|p| {
            p.input > 0
                && p.output > 0
                && p.cache_read > 0
                && p.cache_write > 0
                && p.per_server_tool_request > 0
        })
    });
    match full {
        Some(m) => println!("all five dimensions: {} {:?}", m.id, m.price.unwrap()),
        None => println!("all five dimensions: NONE FOUND"),
    }

    // A tier, with the base it re-rates.
    if let Some(m) = models.iter().find(|m| !m.price_tiers.is_empty()) {
        println!(
            "tier sample: {} base input {} -> {} at >= {} prompt tokens",
            m.id,
            m.price.unwrap().input,
            m.price_tiers[0].price.input,
            m.price_tiers[0].min_prompt_tokens
        );
    }

    // Provenance, and the invariant.
    let disagreeing = models
        .iter()
        .filter(|m| m.price.is_some() != m.price_source.is_some())
        .count();
    println!("price/source disagreements: {disagreeing}");
    let referenced = models
        .iter()
        .filter(|m| matches!(m.price_source, Some(PriceSource::Reference(_))))
        .count();
    println!("attributed to the reference host: {referenced}");

    assert!(!models.is_empty(), "the catalogue must not be empty");
    assert!(
        full.is_some(),
        "at least one model prices all five dimensions"
    );
    assert!(tiered > 0, "at least one model carries a tier");
    assert_eq!(disagreeing, 0, "price and provenance must agree everywhere");
    println!("\nF10 OK");
    Ok(())
}
