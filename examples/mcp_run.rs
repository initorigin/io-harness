//! A live run against a real MCP server and a real model, under a policy whose
//! network default is **deny** — and then the same task with the server's host
//! denied, to show the boundary holding while the capability works.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example mcp_run
//! ```
//!
//! The MCP server is this repo's own fixture (`examples/mcp_fixture_server.rs`),
//! spawned over stdio, so the example needs no third-party server installed.
//! Build it first — `cargo build --example mcp_fixture_server` — or run
//! `cargo build --examples`.
//!
//! Two runs, and the pair is the point:
//!
//! 1. **The capability.** Network denies by default. The model still reaches
//!    OpenRouter, because the harness contributes that one host as a named
//!    `provider` layer. The agent calls the server's `echo` tool and finishes the
//!    task with the built-in `write_file`.
//! 2. **The boundary.** The same task, with an HTTP MCP server whose host no rule
//!    allows. The run refuses before a socket exists.
//!
//! A run that proved only the first would show a feature. Only the second would
//! show a wall. Together they show a governed capability.

use std::path::PathBuf;

use io_harness::{
    run_with, ApproveAll, Error, McpServer, OpenRouter, Policy, RunOutcome, Store, TaskContract,
    Verification,
};

/// The fixture server binary `cargo build --examples` produces.
///
/// This binary is itself an example, so it already lives in `.../examples/` —
/// its sibling is right there. Under `cargo test`-style invocation it would sit
/// in `deps/` instead, so both layouts are handled rather than assumed.
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("this example has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    if !dir.ends_with("examples") {
        dir.push("examples");
    }
    dir.join(format!(
        "mcp_fixture_server{}",
        std::env::consts::EXE_SUFFIX
    ))
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let server_bin = fixture_server();
    if !server_bin.exists() {
        eprintln!(
            "fixture server not built at {}.\nRun: cargo build --example mcp_fixture_server",
            server_bin.display()
        );
        std::process::exit(1);
    }

    let provider = OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;

    let goal = "Call the mcp__fix__echo tool with the text `hello from mcp`, then write \
                its exact reply into src/note.txt using write_file.";
    let verify = Verification::WorkspaceFileContains {
        file: "src/note.txt".into(),
        needle: "echo:".into(),
    };

    // ---- 1. The capability, under deny-by-default egress -------------------

    let contract = TaskContract::workspace(goal, dir.path())
        .with_verification(verify.clone())
        .with_max_steps(6)
        .with_mcp([McpServer::stdio("fix", server_bin.display().to_string())]);

    // Nothing is allowed onto the network by name. The provider host is covered
    // by the harness's own `provider` layer; the fixture server is spawned, not
    // dialled, so it needs an exec allowance rather than a net one.
    //
    // Note the TWO exec allowances. Starting the server and calling one of its
    // tools are separate checks — the first on the binary, the second on the
    // namespaced tool name — which is what lets a policy admit a server and
    // still refuse one of its tools. Allow only the binary, as an earlier draft
    // of this example did, and the server connects, its tools are discovered and
    // offered, and every call is then refused.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec(server_bin.display().to_string())
        .allow_exec("mcp__fix__*");

    let store = Store::open(dir.path().join("runs.db"))?;
    println!("run 1: network denies by default; the agent uses an MCP tool anyway");
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("  outcome: {:?}", result.outcome);

    for e in store.mcp_events(result.run_id)? {
        println!(
            "  mcp: {} {} {}{}",
            e.kind,
            e.server,
            e.tool.unwrap_or_default(),
            e.millis.map(|m| format!(" ({m}ms)")).unwrap_or_default()
        );
    }
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
    for e in store
        .events(result.run_id)?
        .iter()
        .filter(|e| e.act == "net")
    {
        println!(
            "  net: {} {} (layer {})",
            e.kind,
            e.target,
            e.layer.clone().unwrap_or_else(|| "-".into())
        );
    }
    for step in store.steps(result.run_id)? {
        println!("  step {}: {}", step.step, step.decision);
    }
    if let RunOutcome::Success { .. } = result.outcome {
        println!(
            "  wrote: {}",
            std::fs::read_to_string(dir.path().join("src/note.txt"))?.trim()
        );
    }

    // ---- 2. The boundary ---------------------------------------------------

    println!("\nrun 2: the same task against an MCP host no rule allows");
    let denied = TaskContract::workspace(goal, dir.path())
        .with_verification(verify)
        .with_max_steps(6)
        .with_mcp([McpServer::http("remote", "https://mcp.example.com/mcp")]);

    let store2 = Store::open(dir.path().join("runs2.db"))?;
    match run_with(&denied, &provider, &store2, &policy, &ApproveAll).await {
        Err(Error::Refused {
            act, target, layer, ..
        }) => {
            println!(
                "  refused before dialling: {act} {target} (layer {})",
                layer.unwrap_or_else(|| "default".into())
            );
        }
        Err(e) => println!("  unexpected error: {e}"),
        Ok(r) => println!("  UNEXPECTED: the run proceeded ({:?})", r.outcome),
    }

    Ok(())
}
