//! A live run whose answer is not on disk: the agent can finish only by calling
//! a tool the embedding program supplied itself.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example custom_tool
//! ```
//!
//! The workspace holds a manifest of shipment ids and nothing else — no status,
//! no port, no date. Those live in `Shipments`, an in-memory table registered
//! with [`TaskContract::with_tools`]. `grep` and `find` can locate the id; only
//! the tool can say what became of it, so a run that produces the right file is
//! a run that called it. That is the whole demonstration: the datum in
//! `status.txt` at the end appears nowhere in the workspace and nowhere in the
//! prompt.
//!
//! Registering a tool makes it available; it does not authorize it. Calling one
//! is an exec check on its name, which is why the policy below names the tool
//! beside the paths — an operator can hand over a toolbox and still refuse one
//! tool in it.

use std::sync::{Arc, Mutex};

use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Policy, RunOutcome, Store, TaskContract, ToolSpec, Verification,
};
use serde_json::{json, Value};

/// The id the manifest lists, the line only the tool knows, and the word inside
/// it that exists in neither the workspace nor the prompt. The last one is what
/// makes the check at the end of this example mean something: the verify can
/// only assert a shape the model was told, this asserts a fact it was not.
const ID: &str = "SHP-4417";
const STATUS: &str = "SHP-4417: held at Rotterdam, cleared 2026-08-01, next hop Felixstowe";
const DATUM: &str = "Rotterdam";

/// A caller's own capability: a lookup the filesystem cannot answer.
///
/// It also records what it was asked, which is how the run below can show the
/// arguments the model actually sent rather than infer them from the output.
struct Shipments {
    asked: Arc<Mutex<Vec<String>>>,
}

impl Tool for Shipments {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lookup_shipment".into(),
            description: "Look up the current status of one shipment by its id.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A shipment id, e.g. SHP-4417." }
                },
                "required": ["id"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a Value) -> ToolFuture<'a> {
        // Read defensively: the arguments are what the model sent, not what the
        // schema promised, and a missing field is an observation to hand back
        // rather than a panic to end the run with.
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Box::pin(async move {
            self.asked
                .lock()
                .expect("no panic holds this lock")
                .push(id.clone());
            if id == ID {
                return Ok(STATUS.to_string());
            }
            // Not a run failure. The text becomes an observation, and only the
            // model can decide whether an unknown id means "try another" or
            // "give up on this approach".
            Err(io_harness::Error::Config(format!(
                "no shipment {id:?}; this table knows {ID}"
            )))
        })
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;

    // The only thing on disk is the id. Everything the task asks for is behind
    // the tool.
    std::fs::write(
        dir.path().join("manifest.txt"),
        format!("shipments we track:\n{ID}\nSHP-9002\n"),
    )?;

    // A handle the example keeps, so it can print what the tool was asked.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let toolbox = Toolbox::new().with(Shipments {
        asked: Arc::clone(&asked),
    });

    let contract = TaskContract::workspace(
        "manifest.txt lists the shipments we track. Use the lookup_shipment \
         tool to get the current status of SHP-4417, then write the line it \
         returns, verbatim, into status.txt with write_file.",
        dir.path(),
        // The needle is only the shape: it is in the prompt, so it proves
        // nothing on its own. The datum check after the run is the real one.
        Verification::WorkspaceFileContains {
            file: "status.txt".into(),
            needle: ID.into(),
        },
    )
    .with_tools(toolbox)
    .with_max_steps(6);

    // Reads and writes by path, plus the one exec allowance the registered tool
    // needs. Drop the last line and the tool is still offered to the model and
    // still refused on every call — availability and authority are separate.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("lookup_shipment");

    let store = Store::open(dir.path().join("runs.db"))?;
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

    println!("outcome: {:?}", result.outcome);
    println!("\ntool calls the trace recorded:");
    for step in store.steps(result.run_id)? {
        if step.tool_call.contains("lookup_shipment") {
            println!("  step {}: {}", step.step, step.tool_call);
        }
    }
    println!("  asked for: {:?}", asked.lock().expect("no poisoning"));

    for e in store
        .events(result.run_id)?
        .iter()
        .filter(|e| e.kind == "refusal")
    {
        println!(
            "  refused: {} {} (rule {})",
            e.act,
            e.target,
            e.rule.clone().unwrap_or_else(|| "-".into())
        );
    }

    if matches!(result.outcome, RunOutcome::Success { .. }) {
        let written = std::fs::read_to_string(dir.path().join("status.txt"))?;
        println!("\nstatus.txt: {}", written.trim());
        // The point of the example, stated as a check rather than as prose: the
        // word is in no file the agent could read and in no sentence it was
        // sent, so it can only have come back through `invoke`.
        println!(
            "  carries {DATUM:?}, which was on no disk the agent could reach: {}",
            written.contains(DATUM)
        );
    } else {
        eprintln!("note: the run did not reach success; see the trace above");
    }
    Ok(())
}
