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

use io_harness::{
    Auth, Compatible, CompletionRequest, Message, OpenRouter, Provider, ToolCall, ToolResult,
    Usage,
};

/// An Anthropic slug, because Anthropic is the vendor whose caching is
/// request-side. Override with `CACHE_LIVE_MODEL` to measure another one.
///
/// Haiku rather than a larger sibling on purpose. The OpenAI wire sends no
/// `max_tokens`, so the vendor applies the *model's* own default maximum output — 64k
/// on `claude-sonnet-4.5` — and a credit-limited account is refused with an HTTP 402
/// before a single token is generated. That is not a caching failure and reads like
/// one. The measurement does not care which model answers, only that its vendor caches
/// on request, so the cheap one is the right default.
const DEFAULT_MODEL: &str = "anthropic/claude-haiku-4.5";

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
                u.prompt_tokens, u.cache_read_tokens, u.cache_write_tokens, u.completion_tokens,
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

/// 0.44.0 — a user turn whose leading `at` bytes the caller says are byte-stable.
fn request_marked_at(system: &str, user: &str, at: Option<usize>) -> CompletionRequest {
    CompletionRequest {
        cache_boundary: at,
        ..request(system, user)
    }
}

/// The frozen half of a transcript: what 0.43.0's compaction leaves unchanging ahead
/// of the summary, standing in here for a real run's prompt header, memory block and
/// folded paragraph.
///
/// Long enough on its own to clear a vendor's minimum cacheable length, so that the
/// second breakpoint is measurable independently of the first rather than riding on
/// it.
fn frozen_transcript() -> String {
    let mut s = String::from("Goal: port the tokenizer\n\nObservations so far:\n");
    for i in 0..PARAGRAPHS {
        s.push_str(&format!(
            "[read src/lex{i}.rs] The lexer walks the input by byte and emits a Token for each \
             run it recognises, holding the span so a later pass can report a position without \
             re-scanning. Earlier work, summarised: the enum was kept and the span widened.\n"
        ));
    }
    s
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
    let first = marked
        .complete(request(&system, "In one word: yes?"))
        .await?;
    let a1 = report("call 1 (writes the cache)", &first.model, first.usage);
    let second = marked
        .complete(request(&system, "In one word: still yes?"))
        .await?;
    let a2 = report("call 2 (should read it)", &second.model, second.usage);

    // ---- the control: the same endpoint and model, with no marker -------------
    // `Compatible` builds with `WebFlavor::OpenAi`, so no `cache_control` is sent.
    println!("\ncontrol (same endpoint and model, no cache_control):");
    let plain = Compatible::new("https://openrouter.ai/api/v1", Auth::Bearer, &key, &model)
        .with_name("openrouter-unmarked");
    let third = plain
        .complete(request(&system, "In one word: yes?"))
        .await?;
    let b1 = report("call 1", &third.model, third.usage);
    let fourth = plain
        .complete(request(&system, "In one word: still yes?"))
        .await?;
    let b2 = report("call 2", &fourth.model, fourth.usage);

    println!("\n--- verdict: one breakpoint (0.38.0) ---");
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

    // ---- 0.44.0: the second breakpoint, at the compaction boundary -------------
    //
    // The question this half answers is narrower than the one above, and needs its
    // own baseline rather than the unmarked control: OpenRouter *always* sends the
    // system breakpoint now, so "did anything get cached" cannot tell the two
    // breakpoints apart. The comparison is therefore between two marked arms —
    // system-only, and system plus the frozen transcript prefix — and what the
    // second breakpoint is worth is the difference between them.
    let frozen = frozen_transcript();
    let user = format!("{frozen}\n[read src/parse.rs] the volatile tail, different each turn.\n");
    println!(
        "\nfrozen transcript prefix: {} chars, ~{} estimated tokens",
        frozen.len(),
        frozen.len() / 4,
    );

    println!("\nsystem breakpoint only (0.43.0's request, no cache_boundary):");
    // One call, not two: the system prefix is already in the vendor's cache from the
    // arms above, so this reads it and the number is the baseline the second
    // breakpoint has to beat.
    let sys_only = marked
        .complete(request_marked_at(&system, &user, None))
        .await?;
    let c1 = report("call 1", &sys_only.model, sys_only.usage);

    println!("\nboth breakpoints (cache_boundary at the end of the frozen prefix):");
    let at = Some(frozen.len());
    let m1 = marked
        .complete(request_marked_at(&system, &user, at))
        .await?;
    let d1 = report("call 1 (writes)", &m1.model, m1.usage);
    let m2 = marked
        .complete(request_marked_at(&system, &user, at))
        .await?;
    let d2 = report("call 2 (should read)", &m2.model, m2.usage);

    println!("\n--- verdict: two breakpoints (0.44.0) ---");
    println!("system only:      cache_read={c1}");
    println!("both breakpoints: call 1 cache_read={d1}, call 2 cache_read={d2}");
    if d2 > c1 {
        println!(
            "PASS — the transcript breakpoint reads {} tokens beyond what the system block \
             alone accounts for.",
            d2 - c1
        );
    } else {
        println!(
            "FAIL — marking the transcript prefix bought nothing over the system breakpoint. \
             Check that the frozen prefix clears the vendor's minimum cacheable length."
        );
    }
    // ---- 0.49.0: the same breakpoint, on a request that carries a transcript ----
    //
    // The reshape moved the marker from a byte offset into `user` to a count of
    // messages, and a wire that quietly stopped marking would fail no test and cost
    // money on every step. So the arm is measured, not reasoned about — and against
    // the same-shaped baseline the 0.44.0 half uses: an identical transcript sent
    // with no `cache_through`, so the difference is the one field.
    let transcript = vec![
        Message::User(frozen.clone()),
        Message::Assistant {
            text: None,
            calls: vec![ToolCall {
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "src/parse.rs" }),
            }],
        },
        Message::Results(vec![ToolResult {
            call: 0,
            content: "[read src/parse.rs] the volatile tail, different each turn.\n".into(),
        }]),
    ];
    let conversational = |through: Option<usize>| CompletionRequest {
        messages: transcript.clone(),
        cache_through: through,
        ..request(&system, &user)
    };

    println!("\ntranscript, unmarked (system breakpoint only):");
    let t0 = marked.complete(conversational(None)).await?;
    let e1 = report("call 1", &t0.model, t0.usage);

    println!("\ntranscript, cache_through = 1 (the frozen user turn):");
    let t1 = marked.complete(conversational(Some(1))).await?;
    let f1 = report("call 1 (writes)", &t1.model, t1.usage);
    let t2 = marked.complete(conversational(Some(1))).await?;
    let f2 = report("call 2 (should read)", &t2.model, t2.usage);

    println!("\n--- verdict: the transcript breakpoint (0.49.0) ---");
    println!("transcript unmarked: cache_read={e1}");
    println!("transcript marked:   call 1 cache_read={f1}, call 2 cache_read={f2}");
    if f2 > e1 {
        println!(
            "PASS — marking a message boundary reads {} tokens beyond the system block alone, \
             so the reshape did not lose 0.44.0's saving.",
            f2 - e1
        );
    } else {
        println!(
            "FAIL — a request carrying a transcript cached no more than its system block. The \
             marker is not reaching the vendor."
        );
    }

    println!(
        "\nNote: OpenRouter reports no cache-write counter, so every `cache_write` above is \
         zero by construction on this wire and the writing call's cost is unreported rather \
         than measured. Do not infer it from the prompt length."
    );
    Ok(())
}
