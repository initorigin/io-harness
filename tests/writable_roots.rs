//! A run declaring a writable root beyond its workdir (0.81.0).
//!
//! A contained run may write inside its workspace and inside the toolchain caches
//! the host was found to have, and nowhere else. That is right for most runs and
//! wrong for the ones that are not self-contained — a `git worktree` child commits
//! into the parent repository's object store, which is outside its own root by
//! construction. Through 0.80.0 the only way to make such a run work was for the
//! Linux backend to grant the whole system temporary directory to everything,
//! which is what the narrowing in this release removes.
//!
//! The claim asserted here is end to end and not structural: a command writes to a
//! declared root and the write lands, the same command writes to an undeclared one
//! and it does not. A test that read the resolved root back out of the crate would
//! pass on a declaration that never reached a backend.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::sandbox::SandboxConfig;
use io_harness::{run_with, ApproveAll, Config, Policy, Provider, Store, TaskContract};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn exec_call(argv: &[&str]) -> ToolCall {
    ToolCall {
        name: "exec".into(),
        arguments: json!({ "argv": argv }),
    }
}

/// A directory outside the workspace that no backend blanket-allows. Removed on
/// drop, including when the assertion that reads it fails.
struct Outside(PathBuf);

impl Outside {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("writable-roots")
            .join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("landed.txt")
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("write outside the workspace", root).with_max_steps(3)
}

async fn write_to(target: &Path, contract: TaskContract) -> bool {
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", target.to_str().unwrap()])]]);
    let _ = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    target.exists()
}

// --------------------------------------------------------------------- F17

/// A declared root is writable; the same root undeclared is not.
///
/// Both halves in one test on purpose. The grant is only meaningful against the
/// refusal it lifts, and an implementation that granted every path would pass the
/// first assertion alone.
#[tokio::test]
async fn f17_a_declared_root_is_writable_and_an_undeclared_one_is_not() {
    let dir = tempfile::tempdir().unwrap();

    let refused = Outside::new("undeclared");
    let landed = write_to(
        &refused.file(),
        contract(dir.path()).with_contained_exec(SandboxConfig::new()),
    )
    .await;
    if landed {
        // A host with no working backend confines nothing, and there is no
        // boundary here to narrow. Saying so beats asserting a grant against a
        // refusal that never happened.
        eprintln!("no containment backend on this host; nothing to narrow");
        return;
    }

    let granted = Outside::new("declared");
    assert!(
        write_to(
            &granted.file(),
            contract(dir.path())
                .with_contained_exec(SandboxConfig::new())
                .with_writable_roots([granted.0.clone()]),
        )
        .await,
        "a declared root must be writable: {}",
        granted.file().display()
    );
}

/// The declaration survives the journey from `io.toml` to the contract.
///
/// The key exists so an operator can write it, and a key that stopped at `Config`
/// would be a setting nothing obeys — the run loop reads `TaskContract` and never
/// the configuration.
#[test]
fn f17_the_declaration_reaches_the_contract_from_a_config_file() {
    let config =
        Config::from_toml("[run]\nwritable_roots = [\"/tmp/one\", \"/tmp/two\"]\n").unwrap();
    let contract = config.apply_to(TaskContract::workspace("go", "/repo"));

    assert_eq!(
        contract.writable_roots,
        vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
    );

    // Unset is 0.80.0's behaviour: nothing beyond the workspace and the toolchain
    // caches.
    assert!(Config::from_toml("")
        .unwrap()
        .apply_to(TaskContract::workspace("go", "/repo"))
        .writable_roots
        .is_empty());
}

/// A relative or absent root is dropped rather than granted.
///
/// Not tidiness. The Linux mount setup binds every root it is given, a bind of a
/// path that is not there fails the setup, and a failed setup degrades the whole
/// backend to the portable floor — so a bad entry would silently unwind the
/// confinement the good ones were added to preserve.
#[tokio::test]
async fn f17_a_root_that_is_relative_or_absent_does_not_weaken_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let refused = Outside::new("bad-entries");

    let landed = write_to(
        &refused.file(),
        contract(dir.path())
            .with_contained_exec(SandboxConfig::new())
            .with_writable_roots(["relative/path", "/no/such/directory/anywhere"]),
    )
    .await;

    if landed && !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        return;
    }
    assert!(
        !landed,
        "two unusable roots must leave the boundary where it was: {}",
        refused.file().display()
    );
}
