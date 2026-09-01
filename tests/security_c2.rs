//! C2 — the post-edit reflex checker is an `Act::Exec`, and a policy that does
//! not allow it skips it without failing the write.
//!
//! The class: a checker chosen by a marker file in the workspace is a program the
//! *workspace* picked, and for a cargo project that program compiles — which runs
//! the build script and the procedural macros the workspace also contains. Until
//! 0.74.0 the reflex after a successful write spawned that on the host without
//! asking the policy and without the run's containment, so an agent that wrote a
//! manifest naming a build script and then wrote the build script reached host
//! execution through two calls the approver saw as writes. The sentinel here is a
//! file the build script creates and nothing else: what is being asserted is that
//! the build script ran at all, not what it could have done once running.
//!
//! **The marker file has to exist before the run starts.** `src/run/step.rs`
//! detects the toolchain once, before the first turn, so a manifest a run creates
//! is not the manifest that run checks against — it is the next run's. That is
//! why these tests start from a workspace that is already a cargo project, which
//! is also the shape the threat model names: a hostile repository that has been
//! cloned, not one invented from nothing.
//!
//! Both tests run the same script over the same workspace and differ only in the
//! policy, because a single-armed version of the first one passes against a build
//! that has simply stopped checking anything.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

/// Plays a fixed script of tool calls. The same shape `tests/exec_contained.rs`
/// and `tests/handles.rs` use.
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

/// A path outside the workspace that only a build script running on the host can
/// create. Under `target/` rather than a second temp directory for the reason
/// `tests/exec_contained.rs` documents at length: the macOS profile blanket-allows
/// `/private/var/folders`, so a sentinel in a temp directory would prove nothing
/// on the platform this was written on.
///
/// Unique per test, and removed on drop including when an assertion fails.
struct Sentinel(PathBuf);

impl Sentinel {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("security-c2")
            .join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        let this = Self(dir.join("build-script-ran.txt"));
        // A leftover from a previous run would make the first assertion below a
        // lie in either direction.
        let _ = std::fs::remove_file(this.path());
        this
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn reached(&self) -> bool {
        self.path().exists()
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.path().parent().unwrap());
    }
}

/// A workspace that is already a cargo project, so the run detects an ecosystem
/// and the reflex has a checker to choose. The manifest names no build script
/// yet — the run writes the one that does.
fn cloned_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn x() {}\n").unwrap();
    dir
}

/// The two writes: a manifest that declares a build script, then the build
/// script. Neither call is an `exec`, and that is the whole point of the finding.
fn two_writes(sentinel: &Path) -> Vec<Vec<ToolCall>> {
    let write = |path: &str, content: String| ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    };
    vec![
        vec![write(
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             build = \"build.rs\"\n"
                .to_string(),
        )],
        vec![write(
            "build.rs",
            // `{:?}` on the path writes it as an escaped Rust string literal,
            // which is what makes this work on Windows too.
            format!(
                "fn main() {{ std::fs::write({:?}, \"reached\").ok(); }}\n",
                sentinel.to_str().unwrap()
            ),
        )],
    ]
}

fn transcript(store: &Store) -> String {
    let run_id = store.runs().unwrap()[0];
    store
        .steps(run_id)
        .unwrap()
        .iter()
        .map(|s| format!("{}\n{}", s.decision, s.prompt))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The same run in both tests but for the exec timeout, which is the reflex's
/// timeout and has to be a different number in each.
///
/// The first test gives the checker room to finish, because a zero there would
/// hide the finding rather than test it: a `cargo check` killed at spawn compiles
/// nothing, so the sentinel would be absent on 0.73.0 too and the assertion would
/// pass against the vulnerable build. The control uses zero for the opposite
/// reason — it only needs to prove the spawn happened, and a deadline already
/// past when the child is polled is the one bound that is a fact rather than a
/// race on a loaded runner.
fn contract(root: &Path, exec_timeout: Duration) -> TaskContract {
    TaskContract::workspace("write two files", root)
        .with_max_steps(4)
        .with_exec_timeout(exec_timeout)
}

#[tokio::test]
async fn c2_a_write_cannot_run_the_projects_checker_when_the_policy_forbids_it() {
    let dir = cloned_project();
    let sentinel = Sentinel::new("denied");
    let store = Store::memory().unwrap();
    let provider = MockScript::new(two_writes(sentinel.path()));

    run_with(
        // Two minutes, which a build script compiled from a crate with no
        // dependencies does not need a tenth of. Nothing is spawned once the
        // policy is asked, so this bound costs a passing run nothing at all.
        &contract(dir.path(), Duration::from_secs(120)),
        &provider,
        &store,
        &Policy::permissive().deny_exec("cargo"),
        &ApproveAll,
    )
    .await
    .expect("a refused check is not an error: the run finishes");

    assert!(
        !sentinel.reached(),
        "the workspace's own build script ran on the host after a write, which is \
         arbitrary execution behind two `Act::Write` prompts"
    );

    // The other half, and the property the fix may not trade away: both writes
    // still happened, and the model was told so.
    let text = transcript(&store);
    for path in ["Cargo.toml", "build.rs"] {
        assert!(
            dir.path().join(path).exists(),
            "a refused check turned a successful write into something else: {path} is \
             not on disk"
        );
        assert!(
            text.contains(&format!("[wrote {path}]")),
            "the write is reported to the model as a write:\n{text}"
        );
    }
    assert!(
        !text.contains("[check did not run]"),
        "the skip is silent — a note here is a refusal the model has to spend \
         context reading and cannot act on:\n{text}"
    );
    assert!(
        !text.contains("Diagnostics from"),
        "a checker the policy forbids produced findings, so it ran:\n{text}"
    );
}

/// The control. Same workspace, same script, and the policy is the only thing
/// that changed: the reflex reaches the spawn.
///
/// Without this, the test above would pass against a build whose post-edit check
/// had been deleted, or one whose ecosystem detection never fires in a temporary
/// directory. The sentinel stays absent here too, because a checker killed at
/// spawn compiles nothing.
#[tokio::test]
async fn c2_the_same_write_still_reaches_the_checker_when_the_policy_allows_it() {
    let dir = cloned_project();
    let sentinel = Sentinel::new("allowed");
    let store = Store::memory().unwrap();
    let provider = MockScript::new(two_writes(sentinel.path()));

    run_with(
        &contract(dir.path(), Duration::ZERO),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let text = transcript(&store);
    assert!(
        text.contains("[check did not run]"),
        "the reflex was never wired up in this workspace, so the test above proves \
         nothing about the policy:\n{text}"
    );
    assert!(
        text.contains("cargo check"),
        "and the checker it chose is the one the finding is about:\n{text}"
    );
}
