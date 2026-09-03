//! The live evidence for 0.77.0's structured output: does a real vendor accept the
//! `response_format` this crate now sends, and does the local gate hold when it does
//! not?
//!
//! ```text
//! set -a; . ./.env; set +a
//! cargo run --example structured_output_live
//! ```
//!
//! # What this is evidence for, and what it is not
//!
//! An offline body test proves what this crate *sent* — `openai_wire`'s key-set
//! assertions do that, exhaustively. What no offline test can prove is that a real
//! endpoint accepts the key rather than rejecting the request outright, which is the
//! failure mode a new vendor key has: a `400` on every completion, invisible until
//! somebody points the crate at the real thing.
//!
//! **Two arms, and the second is the one that matters.**
//!
//! - The **accepted** arm declares a schema the crate supports and asks a real model
//!   for an answer in that shape. It proves the endpoint took the key and that the
//!   local validator agrees with what came back.
//! - The **authority** arm declares a schema and asks — through the same endpoint —
//!   a question whose natural answer is prose. If the vendor honours the declaration
//!   the reply is JSON anyway; if it ignores it the reply is prose and the local gate
//!   refuses the run. **Either outcome is a pass**, and that is the point being
//!   demonstrated: the vendor key is a hint that reduces attempts, and the gate is
//!   local. A release that could only show the happy path would be claiming the
//!   vendor is the guarantee.
//!
//! **What it is not.** One provider, one route, one model, one moment. It says the
//! key reaches a vendor that accepts it and that the crate's own gate is what decides.
//! It says nothing about whether a *different* endpoint behind an OpenAI-compatible
//! base URL will accept it — `docs/CONTRACT.md` records that reasoning separately —
//! and nothing about Anthropic's native wire, which deliberately carries no
//! `response_format` at all.
//!
//! Requires `OPENROUTER_API_KEY` and `OPENROUTER_MODEL` in the environment.

use io_harness::provider::CompletionRequest;
use io_harness::{OpenRouter, OutputSchema, Provider};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY is not set — run `set -a; . ./.env; set +a` first")?;
    let model = std::env::var("OPENROUTER_MODEL")
        .map_err(|_| "OPENROUTER_MODEL is not set — run `set -a; . ./.env; set +a` first")?;

    let provider = OpenRouter::new(key, &model);

    let schema = OutputSchema::new(json!({
        "type": "object",
        "properties": {
            "language": { "type": "string" },
            "year": { "type": "integer", "minimum": 1950, "maximum": 2100 }
        },
        "required": ["language", "year"],
        "additionalProperties": false
    }))?;

    println!("model: {model}\n");

    // ---- arm one: a question whose answer fits the shape ------------------
    println!("== accepted ==");
    let asked = CompletionRequest {
        system: "You answer with a JSON object and nothing else.".into(),
        user: "In what year was the Rust programming language's 1.0 released, and what \
               is the language called? Answer as JSON."
            .into(),
        output_schema: Some(schema.clone()),
        ..Default::default()
    };
    let reply = provider.complete(asked).await?;
    let text = reply.text.clone().unwrap_or_default();
    println!("reply: {text}");
    match schema.validate_text(&text) {
        Ok(value) => println!("  the endpoint accepted the key and the reply conforms: {value}"),
        Err(errors) => {
            // Not a failure of the release — a failure of this model on this day. The
            // gate held, which is the property under test; say so rather than
            // pretending the arm proved the vendor honoured the key.
            println!("  the reply did not conform, and the local gate caught it:");
            for e in &errors {
                println!("    - {e}");
            }
        }
    }

    // ---- arm two: the gate is local, whatever the vendor did --------------
    println!("\n== authority ==");
    let prose = CompletionRequest {
        system: "You are terse.".into(),
        user: "Write one sentence about the sea.".into(),
        output_schema: Some(schema.clone()),
        ..Default::default()
    };
    let reply = provider.complete(prose).await?;
    let text = reply.text.clone().unwrap_or_default();
    println!("reply: {text}");
    match schema.validate_text(&text) {
        Ok(value) => {
            println!("  the vendor honoured the declaration and answered in shape anyway: {value}")
        }
        Err(errors) => {
            println!(
                "  the vendor did not constrain this reply, and the LOCAL gate refused it — \
                 which is the release's actual claim:"
            );
            for e in &errors {
                println!("    - {e}");
            }
        }
    }

    println!("\nBoth arms completed without a transport error, which is the thing an offline");
    println!("body assertion cannot establish: the endpoint accepts `response_format`.");
    Ok(())
}
