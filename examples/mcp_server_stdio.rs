//! This crate's own tools, served over MCP on stdio.
//!
//! An example rather than a test helper because it has to be a real process.
//! The claim under test is that another harness can spawn this one, speak MCP
//! to it and get this crate's boundary with the tools it calls — and an
//! in-process double would prove the crate talks to itself. It is the mirror of
//! `mcp_fixture_server`, which is the deterministic server this crate's *client*
//! is driven against.
//!
//! Two things it is shaped by, both of which a test needs:
//!
//! - **Configuration arrives in the environment, not in argv.** A child process
//!   started by a test sets variables more easily than it composes a command
//!   line, and nothing here has to parse anything. `IO_MCP_ROOT` is the
//!   workspace root, `IO_MCP_STORE` the store to journal to, and
//!   `IO_MCP_POSTURE` picks the policy.
//! - **Nothing is printed.** Not a banner, not a ready line, not a diagnostic.
//!   Stdout is the protocol stream, and a stray line corrupts it in a way that
//!   surfaces as a client failing to parse rather than as an error anyone
//!   raises. Anything worth saying goes to stderr, which is where `tracing`
//!   already sends it.
//!
//! Run it by hand with `cargo run --features mcp-server --example
//! mcp_server_stdio` and it will sit on stdio waiting for a client.

use std::path::PathBuf;

use io_harness::{serve_mcp, McpServerConfig, Policy};

/// The workspace root every served path resolves against.
const ROOT: &str = "IO_MCP_ROOT";

/// The store a served session journals to. It is created if it is absent, the
/// way any other run's store is.
const STORE: &str = "IO_MCP_STORE";

/// Which policy to serve under. Absent, it is the tiered default — reads
/// allowed, writes and execs asking, egress denied — which under the default
/// `DenyAll` approver means reads work and every mutation refuses.
const POSTURE: &str = "IO_MCP_POSTURE";

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::var(ROOT).map(PathBuf::from).unwrap_or_else(|_| {
        std::env::current_dir().expect("a working directory to serve tools over")
    });
    let store = std::env::var(STORE)
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("runs.db"));

    // Three postures, and the names say what they are for. `tiered` is the
    // default and is what an operator should reach for; `reads` proves a
    // narrower policy is honoured; `ask-write` exists so a test can drive an
    // `Effect::Ask` rule and see it refuse rather than wait, which is the
    // behaviour that has no human behind it.
    let policy = match std::env::var(POSTURE).as_deref() {
        Ok("reads") => Policy::default().allow_read("**"),
        Ok("ask-write") => Policy::default().ask_write("**"),
        _ => Policy::default(),
    };

    serve_mcp(McpServerConfig::new(root, store).with_policy(policy)).await
}
