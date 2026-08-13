//! 0.52.0's live run: a real `rust-analyzer`, over a real crate, through the real
//! loop.
//!
//! The suite in `tests/lsp.rs` proves the client against a fixture server that can
//! be wrong on purpose — out of order, mid-handshake, missing a capability — which
//! is the only way those paths are reachable at all. What a fixture cannot prove is
//! that the protocol was *understood*: that the handshake this crate sends is one a
//! real server accepts, that its `didOpen` is one a real server indexes, and that
//! the positions it computes name what a real server thinks they name.
//!
//! That is this example's whole job, and it is a `cargo run` rather than a test on
//! purpose. A real server's index is minutes on a real repository, and a gate that
//! waits minutes is a gate someone eventually deletes.
//!
//! ```sh
//! cargo run --example lsp_live
//! ```
//!
//! Needs `rust-analyzer` on `PATH` (`rustup component add rust-analyzer`). It
//! writes a three-file crate into a temporary directory, so it indexes in seconds
//! rather than minutes, and prints what came back.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, LspServer, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

/// Plays a fixed script of tool calls and keeps every prompt it was handed, so the
/// observations the loop produced can be printed at the end.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: std::sync::Mutex<Vec<String>>,
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(request.user.clone());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"live\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // `Ledger` is defined at line 1 column 12, and used in two other places.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct Ledger {\n    pub spent: u64,\n}\n\nmod uses;\n\npub fn total(l: &Ledger) -> u64 {\n    l.spent\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/uses.rs"),
        "use crate::Ledger;\n\npub fn draw(l: &Ledger) -> u64 {\n    l.spent\n}\n",
    )
    .unwrap();

    let server = LspServer::new("rust-analyzer", "rust-analyzer")
        .with_extensions([".rs"])
        // A real index. Generous, because the point here is the answer and not
        // the clock.
        .with_timeout(std::time::Duration::from_secs(120));

    let provider = Script {
        steps: vec![
            // Where is the `Ledger` in `total`'s signature defined?
            vec![call(
                "lsp_definition",
                json!({"path": "src/lib.rs", "line": 7, "column": 19}),
            )],
            // Who uses it?
            vec![call(
                "lsp_references",
                json!({"path": "src/lib.rs", "line": 1, "column": 12}),
            )],
            // What is in this file?
            vec![call("lsp_symbols", json!({"path": "src/lib.rs"}))],
            // Where in the workspace is a symbol called `Ledger`?
            vec![call("lsp_symbols", json!({"query": "Ledger"}))],
            // What is it?
            vec![call(
                "lsp_hover",
                json!({"path": "src/lib.rs", "line": 1, "column": 12}),
            )],
            // And what would renaming it look like? Nothing is written.
            vec![call(
                "lsp_rename",
                json!({"path": "src/lib.rs", "line": 1, "column": 12, "new_name": "Tally"}),
            )],
            vec![call(
                "write_file",
                json!({"path": "done.txt", "content": "ok"}),
            )],
        ],
        at: AtomicUsize::new(0),
        seen: std::sync::Mutex::new(Vec::new()),
    };

    let contract = TaskContract::workspace("navigate this crate", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(10)
        .with_lsp([server]);
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*");

    let result = run_with(
        &contract,
        &provider,
        &Store::memory()?,
        &policy,
        &ApproveAll,
    )
    .await?;
    println!("outcome: {:?}\n", result.outcome);

    // The last prompt carries every observation the run produced, which is what a
    // reader of this example wants to see.
    if let Some(last) = provider.seen.lock().unwrap().last() {
        println!("{last}");
    }

    // The rename must have written nothing.
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    println!(
        "\nsrc/lib.rs still says `struct Ledger`: {}",
        lib.contains("pub struct Ledger")
    );
    Ok(())
}
