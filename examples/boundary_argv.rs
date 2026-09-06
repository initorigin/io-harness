//! 0.83.0 O3 — the release's two claims, driven from a command line.
//!
//! Nothing under `tests/` links a binary, so every assertion this crate makes is
//! made from inside the library. This example is the other door: a real process,
//! started from a shell, running a real contained command through the real loop.
//! Three releases running, that seam has caught something the suite could not.
//!
//! ```text
//! export OPENROUTER_API_KEY=whatever-you-like
//! cargo run --example boundary_argv
//! ```
//!
//! **No vendor is called.** The provider below is scripted and answers from
//! memory — but it *names an endpoint*, which is what makes the run proxied the
//! way a real one is, and being proxied is exactly the condition under which both
//! defects this release fixes were invisible. The address is TEST-NET-1
//! (RFC 5737): no DNS, nothing dialled.
//!
//! It checks two things a library test cannot check together:
//!
//! 1. **A widened sandbox lets a command bind a port.** `sandbox.allow_network =
//!    true` on a proxied run. Through 0.82.0 this was refused on both native
//!    backends — the macOS profile discarded the flag and the Landlock rung took
//!    control of TCP the moment a proxy existed.
//! 2. **A narrow run's command cannot read the harness's provider key**, while
//!    still having a `PATH` to run anything with.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::sandbox::SandboxConfig;
use io_harness::{run_with, ApproveAll, Policy, Provider, Store, TaskContract};
use serde_json::json;

struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    /// What makes this run proxied. Without it `authorize_provider` returns
    /// early, no provider layer is merged, and the whole proxied path — the one
    /// every real run takes — is never entered.
    fn endpoint(&self) -> Option<&str> {
        Some("https://192.0.2.10/v1")
    }
}

fn exec(argv: &[&str]) -> ToolCall {
    ToolCall {
        name: "exec".into(),
        arguments: json!({ "argv": argv }),
    }
}

/// Binds a loopback port and says so. Written as one `python3 -c` because a bind
/// is not something `sh` can do on its own.
const BIND: &str = "import socket\n\
     s = socket.socket()\n\
     s.bind(('127.0.0.1', 0))\n\
     s.listen(1)\n\
     open('bound.txt', 'w').write('bound on %d' % s.getsockname()[1])\n";

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    if std::env::var_os("OPENROUTER_API_KEY").is_none() {
        eprintln!(
            "set OPENROUTER_API_KEY to any value — the second check is about \
             whether a contained command can read it, so it has to be set"
        );
        std::process::exit(2);
    }

    // 1 — a widened proxied run may bind.
    let widened = tempfile::tempdir().unwrap();
    let store = Store::memory()?;
    let provider = Script {
        steps: vec![vec![exec(&["python3", "-c", BIND])]],
        at: AtomicUsize::new(0),
    };
    run_with(
        &TaskContract::workspace("serve a port", widened.path())
            .with_max_steps(3)
            .with_contained_exec(SandboxConfig {
                allow_network: true,
                ..SandboxConfig::new()
            }),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await?;
    let bound = std::fs::read_to_string(widened.path().join("bound.txt")).unwrap_or_default();
    println!(
        "widened + proxied, may bind: {}",
        if bound.trim().is_empty() {
            "NO — the command could not listen".to_string()
        } else {
            format!("yes — {}", bound.trim())
        }
    );

    // 2 — a narrow run's command sees no provider key, and still has a PATH.
    let narrow = tempfile::tempdir().unwrap();
    let store = Store::memory()?;
    let provider = Script {
        steps: vec![vec![exec(&[
            "sh",
            "-c",
            "printenv OPENROUTER_API_KEY > key.txt; printenv PATH > path.txt",
        ])]],
        at: AtomicUsize::new(0),
    };
    run_with(
        &TaskContract::workspace("read the environment", narrow.path())
            .with_max_steps(3)
            .with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await?;
    let key = std::fs::read_to_string(narrow.path().join("key.txt")).unwrap_or_default();
    let path = std::fs::read_to_string(narrow.path().join("path.txt")).unwrap_or_default();
    println!(
        "narrow, provider key visible to the command: {}",
        if key.trim().is_empty() {
            "no".to_string()
        } else {
            format!("YES — {:?}", key.trim())
        }
    );
    println!(
        "narrow, command still has a PATH: {}",
        if path.trim().is_empty() { "NO" } else { "yes" }
    );

    Ok(())
}
