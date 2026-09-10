//! Where an MCP server's stderr ends up (0.86.0), provable from outside.
//!
//! The claim has two halves and only one of them is observable from inside the
//! process that makes it. That a server's line was *captured* is a row in the
//! run's own trace. That it did not ALSO reach the host's stderr can only be
//! seen by whoever owns that stderr — so this example is the subject and
//! `tests/mcp_stderr.rs` is the observer: it spawns this binary with both
//! streams piped, and reads the two answers off the two streams.
//!
//! It runs a one-step task against an in-process scripted provider, so it needs
//! no key and no network. The MCP server is this repo's own fixture, spawned
//! over stdio, which prints [`BANNER`] to its stderr before it speaks the
//! protocol.
//!
//! Everything this example says about its own findings goes to **stdout**. It
//! writes to stderr exactly once, and only to report its own failure to run at
//! all — which the test tells apart from the server's banner by its text.
//!
//! ```text
//! cargo build --example mcp_fixture_server && cargo run --example mcp_stderr_probe
//! ```

use std::path::PathBuf;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, McpServer, Policy, Provider, Store, TaskContract, Verification,
};

/// One step that calls the fixture's `echo` tool, then nothing.
///
/// A scripted provider rather than a real one: the subject is where a child's
/// stderr goes, and a model in the loop would make the answer depend on what it
/// chose to call.
struct CallsEcho;

impl Provider for CallsEcho {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "mcp__fix__echo".into(),
                arguments: serde_json::json!({ "text": "hello" }),
            }],
            ..Default::default()
        })
    }
}

/// The fixture server binary `cargo build --examples` produces.
///
/// The same two layouts `examples/mcp_run.rs` handles, for the same reason: this
/// binary's sibling is in `examples/` when run as an example and in `deps/`
/// when a test spawns it.
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
            "probe could not run: fixture server not built at {}.\nRun: cargo build --example \
             mcp_fixture_server",
            server_bin.display()
        );
        std::process::exit(1);
    }

    let dir = tempfile::tempdir()?;
    let contract = TaskContract::workspace("Call the echo tool.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1)
        .with_mcp([McpServer::stdio("fix", server_bin.display().to_string())]);
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec(server_bin.display().to_string())
        .allow_exec("mcp__fix__*");

    let store = Store::memory()?;
    let result = run_with(&contract, &CallsEcho, &store, &policy, &ApproveAll).await?;

    // What the run captured, on stdout, one line per row so the observer can
    // read it without parsing anything.
    for e in store.context_events(result.run_id)? {
        if e.kind == "mcp_stderr" {
            println!(
                "captured: {}",
                e.detail.unwrap_or_default().replace('\n', " ")
            );
        }
    }
    println!("probe finished");
    Ok(())
}
