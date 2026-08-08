//! A check attached to a point in the tool lifecycle, from `io.toml` (0.42.0).
//!
//! A `[[hook]]` has been an `Observer` since 0.28.0: it names events and the
//! strongest thing it can do is cancel at the next step boundary, which is after
//! the tool it objected to has run. So "never run this command in this
//! repository" was a Rust `Approver` an operator had to compile in.
//!
//! What is asserted here is what did **not** happen: no `Edit` row, a file
//! byte-identical on disk, a hook never spawned. A refusal that arrives after the
//! write has landed produces the same event stream and the same log line, and
//! only the absent write tells the two apart.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Config, Policy, Provider, RunOutcome, Store, TaskContract, ToolSpec,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls.
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
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// 0.41.0's technique, unchanged: a read-only tool that cannot finish alone.
/// Three of these complete only if all three are in flight together, which is how
/// N4 asks whether the gate serialised the batch without measuring anything.
struct Rendezvous {
    name: String,
    barrier: Arc<tokio::sync::Barrier>,
}

impl Tool for Rendezvous {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Waits for its siblings, then reports.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.barrier.wait().await;
            Ok(format!("{} met the others", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

// The hook argv per platform. Each platform's own programs are named rather than
// the test being skipped on Windows — the lesson 0.27.0's F4 paid for.

/// Refuses, and says why on stdout. The reason is what the model must end up
/// reading, so it is deliberately a sentence no other part of this test writes.
#[cfg(unix)]
const REFUSES: &[&str] = &["sh", "-c", "echo 'out.txt is generated here'; exit 1"];
#[cfg(windows)]
const REFUSES: &[&str] = &[
    "powershell",
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Write-Output 'out.txt is generated here'; exit 1",
];

/// Allows every call it sees.
#[cfg(unix)]
const ALLOWS: &[&str] = &["true"];
#[cfg(windows)]
const ALLOWS: &[&str] = &["cmd", "/c", "exit 0"];

/// Appends one line per spawn to `spawns.txt`, so "how many times was this hook
/// run" is a fact on disk rather than an inference.
#[cfg(unix)]
const COUNTS: &[&str] = &["sh", "-c", "echo x >> spawns.txt"];
#[cfg(windows)]
const COUNTS: &[&str] = &[
    "powershell",
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Add-Content -LiteralPath 'spawns.txt' -Value 'x'",
];

/// Sleeps well past any timeout these tests set.
#[cfg(unix)]
const SLOW: &[&str] = &["sleep", "30"];
#[cfg(windows)]
const SLOW: &[&str] = &["ping", "-n", "31", "127.0.0.1"];

/// A TOML array literal for one of the argvs above.
fn argv(parts: &[&str]) -> String {
    let items: Vec<String> = parts.iter().map(|p| format!("{p:?}")).collect();
    format!("[{}]", items.join(", "))
}

/// One `[[hook]]` table at local scope, installed on a contract.
fn gated(dir: &Path, table: &str) -> TaskContract {
    std::fs::write(dir.join("io.local.toml"), table).unwrap();
    let hooks = Config::discover(dir).unwrap().hooks();
    TaskContract::workspace("write out.txt", dir).with_tool_hooks(Arc::new(hooks))
}

/// How many times a `COUNTS` hook was spawned.
fn spawns(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("spawns.txt"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Write `io.local.toml` — local scope, where a hook is permitted — and read the
/// configuration back.
fn local(dir: &Path, body: &str) -> io_harness::Result<Config> {
    std::fs::write(dir.join("io.local.toml"), body).unwrap();
    Config::discover(dir)
}

/// Write `io.toml` — project scope, the file a `git clone` delivers.
fn project(dir: &Path, body: &str) -> io_harness::Result<Config> {
    std::fs::write(dir.join("io.toml"), body).unwrap();
    Config::discover(dir)
}

const GATE: &str = r#"
[[hook]]
at = "before_tool"
tools = ["write_file"]
run = ["true"]
"#;

// ------------------------------------------------------------------------- F6

/// F6 — the trust rule is extended, never weakened.
///
/// A hook that can stop a tool is strictly more dangerous than one that appends a
/// log line, so `at` inherits the project-scope refusal rather than reopening the
/// question — including inside a `[profile]`, which is where the boundary has been
/// widened by accident before.
#[test]
fn a_lifecycle_hook_is_refused_in_a_project_scoped_file() {
    let dir = tempfile::tempdir().unwrap();
    let err = project(dir.path(), GATE).unwrap_err();
    assert!(err.to_string().contains("may not declare hooks"), "{err}");

    // The same table, one level down, reached by a different path.
    let dir = tempfile::tempdir().unwrap();
    let err = project(
        dir.path(),
        "[profile.ci]\n[[profile.ci.hook]]\nat = \"before_tool\"\nrun = [\"true\"]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("hook"), "{err}");

    // And the identical file at local scope loads.
    let dir = tempfile::tempdir().unwrap();
    let config = local(dir.path(), GATE).expect("a local-scope lifecycle hook loads");
    assert!(!config.hooks().is_empty());
}

/// F6 — a table this crate cannot honour is refused when it is read, not when it
/// would have fired.
///
/// The failure mode a lifecycle hook can least afford is silence: a misspelled
/// `at` that loads, installs and never fires looks exactly like a check that
/// approved everything.
#[test]
fn a_lifecycle_table_that_cannot_fire_is_refused_at_load() {
    let cases = [
        // An `at` value this crate does not have.
        (
            "[[hook]]\nat = \"after_tool\"\nrun = [\"true\"]\n",
            "after_tool",
        ),
        // Both kinds at once. An event hook and a lifecycle hook are different
        // things and a table claiming both is a mistake worth naming.
        (
            "[[hook]]\non = [\"stalled\"]\nat = \"before_tool\"\nrun = [\"true\"]\n",
            "hook[0]",
        ),
        // A tool filter on an event hook filters nothing.
        (
            "[[hook]]\non = [\"stalled\"]\ntools = [\"exec\"]\nappend = \"a.jsonl\"\n",
            "tools",
        ),
        // Appending a log line cannot stop a tool call, so a lifecycle hook that
        // only appends is a check that always passes.
        (
            "[[hook]]\nat = \"before_tool\"\nappend = \"a.jsonl\"\n",
            "run",
        ),
    ];
    for (body, expect) in cases {
        let dir = tempfile::tempdir().unwrap();
        let err = local(dir.path(), body).unwrap_err();
        assert!(
            err.to_string().contains(expect),
            "`{body}` must be refused naming `{expect}`, got: {err}"
        );
    }
}

// ------------------------------------------------------------------------- F5

/// F5 — a table in a config file stops a write, and the model is told why.
///
/// The assertion that matters is the absent one: no `Edit` row and a path that
/// does not exist. A hook consulted *after* the tool would produce the same
/// warning, the same event and the same log line, and only the file tells them
/// apart.
#[tokio::test]
async fn a_before_tool_hook_refuses_the_call_and_the_model_reads_why() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = gated(
        dir.path(),
        &format!(
            "[[hook]]\nat = \"before_tool\"\ntools = [\"write_file\"]\nrun = {}\n\
             timeout_ms = 20000\n",
            argv(REFUSES)
        ),
    );
    let provider = MockScript::new(vec![vec![call(
        "write_file",
        json!({"path": "out.txt", "content": "written\n"}),
    )]]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        !dir.path().join("out.txt").exists(),
        "the write did not happen"
    );
    assert!(
        store.edits(result.run_id).unwrap().is_empty(),
        "and nothing recorded it as though it had"
    );
    assert!(
        store
            .observations(result.run_id)
            .unwrap()
            .iter()
            .any(|o| o.text.contains("out.txt is generated here")),
        "the hook's own reason reaches the model: {:?}",
        store.observations(result.run_id).unwrap()
    );
    assert!(
        !matches!(result.outcome, RunOutcome::Cancelled { .. }),
        "refusing a call is not ending the run: {:?}",
        result.outcome
    );
}

// ------------------------------------------------------------------------- F7

/// F7 — `on_failure` decides the consequence, and a lifecycle hook defaults to
/// refusing.
///
/// Four arms, identical but for one key. The first is the one that would be
/// easiest to get wrong in the safe-looking direction: a table that says nothing
/// must refuse, because 0.28.0's default — continue — would let through exactly
/// the call the operator attached the check to.
#[tokio::test]
async fn on_failure_decides_what_a_failing_hook_costs() {
    for (key, writes, cancelled) in [
        ("", false, false),
        ("on_failure = \"refuse\"\n", false, false),
        ("on_failure = \"cancel\"\n", false, true),
        ("on_failure = \"continue\"\n", true, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        let contract = gated(
            dir.path(),
            &format!(
                "[[hook]]\nat = \"before_tool\"\ntools = [\"write_file\"]\nrun = {}\n\
                 timeout_ms = 20000\n{key}",
                argv(REFUSES)
            ),
        );
        let provider = MockScript::new(vec![vec![call(
            "write_file",
            json!({"path": "out.txt", "content": "written\n"}),
        )]]);

        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        assert_eq!(
            dir.path().join("out.txt").exists(),
            writes,
            "`{key}`: the write should{} have landed",
            if writes { "" } else { " not" }
        );
        assert_eq!(
            matches!(result.outcome, RunOutcome::Cancelled { .. }),
            cancelled,
            "`{key}`: outcome was {:?}",
            result.outcome
        );
    }
}

// ------------------------------------------------------------------------- N4

/// N4 — the gate does not serialise 0.41.0's batch, and a filtered hook is not
/// spawned for a call it does not want.
///
/// Both arms are structural. The first is 0.41.0's own rendezvous: three
/// read-only tools that can only finish together, under a hook that matches all
/// three. If the gate ran inside the concurrent phase — or held the batch while
/// it spawned — the barrier could not be met and the bounded wait would fail the
/// test. The second counts spawns on disk: a hook filtered to `write_file` must
/// cost a read-heavy completion nothing at all.
#[tokio::test]
async fn the_gate_is_serial_but_the_batch_is_not_and_a_filter_costs_nothing() {
    // Arm 1: three reads under a matching, allowing hook.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tools = Toolbox::new();
    for name in ["look_1", "look_2", "look_3"] {
        tools = tools.with(Rendezvous {
            name: name.into(),
            barrier: Arc::clone(&barrier),
        });
    }
    let contract = gated(
        dir.path(),
        &format!(
            "[[hook]]\nat = \"before_tool\"\nrun = {}\ntimeout_ms = 20000\n",
            argv(ALLOWS)
        ),
    )
    .with_tools(tools);
    let provider = MockScript::new(vec![vec![
        call("look_1", json!({})),
        call("look_2", json!({})),
        call("look_3", json!({})),
    ]]);

    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_with(&contract, &provider, &store, &open_policy(), &ApproveAll),
    )
    .await;
    ran.expect("the batch still runs concurrently under a gate")
        .unwrap();

    // Arm 2: twenty reads under a hook that wants writes.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    let contract = gated(
        dir.path(),
        &format!(
            "[[hook]]\nat = \"before_tool\"\ntools = [\"write_file\"]\nrun = {}\n\
             timeout_ms = 20000\n",
            argv(COUNTS)
        ),
    );
    let reads: Vec<ToolCall> = (0..20)
        .map(|_| call("read_file", json!({"path": "a.txt"})))
        .collect();
    let provider = MockScript::new(vec![reads]);
    run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    assert_eq!(
        spawns(dir.path()),
        0,
        "a hook filtered to write_file costs a read-heavy completion nothing"
    );
}

// ------------------------------------------------------------------------- N5

/// N5 — a hook cannot hang a run.
///
/// The bound is `timeout_ms`, the same one an event hook has had since 0.28.0,
/// and it is asserted rather than assumed because a hook on the tool path is
/// spawned far more often than one on an event. The wall clock here is not the
/// claim — it is the failure mode: the assertion is that the run *finished*, and
/// the outer timeout only stops a hang from taking the matrix with it.
#[tokio::test]
async fn a_hook_that_never_returns_is_killed_and_the_call_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let contract = gated(
        dir.path(),
        &format!(
            "[[hook]]\nat = \"before_tool\"\ntools = [\"write_file\"]\nrun = {}\n\
             timeout_ms = 200\n",
            argv(SLOW)
        ),
    );
    let provider = MockScript::new(vec![vec![call(
        "write_file",
        json!({"path": "out.txt", "content": "written\n"}),
    )]]);

    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_with(&contract, &provider, &store, &open_policy(), &ApproveAll),
    )
    .await;
    let result = ran.expect("the deadline, not the hook, decides").unwrap();

    assert!(!dir.path().join("out.txt").exists());
    assert!(store
        .observations(result.run_id)
        .unwrap()
        .iter()
        .any(|o| o.text.contains("refused")));
}
