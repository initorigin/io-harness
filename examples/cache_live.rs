//! The live evidence for 0.38.0's prompt caching: does the vendor actually serve
//! the marked prefix from its cache, and can this crate see that it did?
//!
//! ```text
//! set -a; . ./.env; set +a
//! cargo run --example cache_live
//! ```
//!
//! # What this is evidence for, and what it is not
//!
//! A body test proves what this crate *sent*. It cannot prove what a vendor did
//! with it, and the difference between "the marker was honoured" and "the marker
//! was accepted and ignored" is invisible in every observable except
//! [`Usage::cache_read_tokens`]. That is what this example measures.
//!
//! **The arms.** Both call the same endpoint, with the same model, the same system
//! block and the same question. They differ in one thing:
//!
//! - The **marked** arm is [`OpenRouter`], which since 0.38.0 sends the system
//!   message as a content part carrying `cache_control`.
//! - The **control** arm is [`Compatible`] pointed at OpenRouter's own base URL.
//!   `Compatible` builds its body with `WebFlavor::OpenAi`, which deliberately
//!   sends no marker — so the control is not a different vendor or a different
//!   route, it is the identical request minus the one key this release adds.
//!
//! Without that control the measurement means nothing: an OpenAI-family model
//! caches a repeated prefix by itself, so a marked pair against one would report
//! cached tokens that owe nothing to this release. **This is why the model must be
//! an Anthropic slug** and why `OPENROUTER_MODEL` is deliberately *not* used —
//! that variable holds an OpenAI-family model in this checkout, which would have
//! produced a confident false pass.
//!
//! **The prefix.** The system block is padded well past the minimum length a
//! vendor requires before it will cache anything, because below that minimum the
//! marker is accepted and silently does nothing. A short prefix is the most likely
//! reason for a zero here, and it is a property of the request rather than of the
//! implementation.
//!
//! **What it is not.** One provider, one route, one model, one moment. It says the
//! marker reaches a vendor that honours it and that the counters come back. It
//! says nothing about any other vendor, and the second call's saving depends on
//! the cache entry still being alive, which is the vendor's clock and not ours.

use io_harness::{Auth, Compatible, CompletionRequest, OpenRouter, Provider, Usage};

/// An Anthropic slug, because Anthropic is the vendor whose caching is
/// request-side. Override to measure another one.
const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4.5";

/// Enough repetitions to clear any vendor's minimum cacheable prefix with room to
/// spare. Below that minimum a marker is accepted and does nothing, which reads
/// from the outside exactly like an unimplemented release.
const PARAGRAPHS: usize = 90;

fn system_block() -> String {
    let mut s = String::from(
        "You are a careful assistant answering questions about a Rust crate. \
         The following notes are reference material. Answer only from them.\n\n",
    );
    for i in 0..PARAGRAPHS {
        s.push_str(&format!(
            "Note {i}: the harness records every provider call with its model, its latency, its \
             time to first token, and the tokens it consumed, split into prompt, completion, \
             cache-read, cache-write and reasoning counts. Raw counts are stored and cost is \
             derived on read against a pricing table held as data, so a price correction repairs \
             the whole history rather than only what follows it.\n"
        ));
    }
    s
}

fn report(label: &str, model: &Option<String>, usage: Option<Usage>) -> u64 {
    let model = model.as_deref().unwrap_or("(not reported)");
    match usage {
        Some(u) => {
            println!(
                "  {label:<28} model={model:<32} prompt={:<7} cache_read={:<7} cache_write={:<7} \
                 completion={}",
                u.prompt_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.completion_tokens,
            );
            u.cache_read_tokens
        }
        None => {
            println!("  {label:<28} model={model:<32} (the provider reported no usage)");
            0
        }
    }
}

fn request(system: &str, user: &str) -> CompletionRequest {
    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    CompletionRequest {
        system: system.to_string(),
        user: user.to_string(),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY must be set; source ./.env first");
    let model = std::env::var("CACHE_LIVE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    let system = system_block();
    println!(
        "system block: {} chars, ~{} estimated tokens, model {model}\n",
        system.len(),
        system.len() / 4,
    );

    // ---- the marked arm: OpenRouter, which since 0.38.0 sends the breakpoint ---
    println!("marked (OpenRouter, cache_control sent):");
    let marked = OpenRouter::new(&key, &model);
    let first = marked.complete(request(&system, "In one word: yes?")).await?;
    let a1 = report("call 1 (writes the cache)", &first.model, first.usage);
    let second = marked
        .complete(request(&system, "In one word: still yes?"))
        .await?;
    let a2 = report("call 2 (should read it)", &second.model, second.usage);

    // ---- the control: the same endpoint and model, with no marker -------------
    // `Compatible` builds with `WebFlavor::OpenAi`, so no `cache_control` is sent.
    println!("\ncontrol (same endpoint and model, no cache_control):");
    let plain = Compatible::new(
        "https://openrouter.ai/api/v1",
        Auth::Bearer,
        &key,
        &model,
    )
    .with_name("openrouter-unmarked");
    let third = plain.complete(request(&system, "In one word: yes?")).await?;
    let b1 = report("call 1", &third.model, third.usage);
    let fourth = plain
        .complete(request(&system, "In one word: still yes?"))
        .await?;
    let b2 = report("call 2", &fourth.model, fourth.usage);

    println!("\n--- verdict ---");
    println!("marked:  call 1 cache_read={a1}, call 2 cache_read={a2}");
    println!("control: call 1 cache_read={b1}, call 2 cache_read={b2}");
    if a2 > 0 && b2 == 0 {
        println!("PASS — the second marked call read the cache and the unmarked one did not.");
    } else if a2 > 0 && b2 > 0 {
        println!(
            "INCONCLUSIVE — both arms report cached tokens, so this route caches without being \
             asked and cannot tell this release apart. Re-aim at a vendor whose caching is \
             request-side."
        );
    } else {
        println!(
            "FAIL — the marked pair reported no cached read. Check that the model is one whose \
             vendor caches on request, and that the prefix clears its minimum length."
        );
    }
    Ok(())
}
