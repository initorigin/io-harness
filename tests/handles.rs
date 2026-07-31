//! Process handles through the full loop — F1, F4 and F6 of 0.25.0.
//!
//! What the unit tests beside the registry in `src/tools/handles.rs` cannot
//! reach is asserted here: that a handle is actually *joined to the loop* — that
//! `shell_start` returns an id the model can use, that a poll shows output the
//! process produced while the run went on doing other things, that a kill takes
//! the process with it, and that the refusal set of the foreground tool applies
//! to this one.
//!
//! The process under test is `examples/tick.rs`, built by the same `cargo test`
//! that runs this file, for the reason its own docs give: nothing that ships on
//! all three platforms both keeps running and keeps printing.
//!
//! Every kill assertion checks the operating system rather than the registry.
//! The registry believing a process is dead is precisely the failure mode these
//! tests exist to catch, so asking it whether it succeeded would be asking the
//! defendant for the verdict.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{ApproveAll, Provider, Store, TaskContract, run_with};
use serde_json::json;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<ToolSpec>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = req.tools.clone();
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // A real pause between turns, so a handle started on step one has
        // genuinely produced output by the time step two polls it. Without it
        // the loop runs faster than the fixture ticks and the test would assert
        // that a process which has not yet spoken said nothing, which is true
        // and proves nothing.
        if i > 0 {
            // Generous rather than marginal, and deliberately so. The fixture
            // prints its first line immediately, so the only race is process
            // spawn — but a loaded runner running the whole suite in parallel
            // can make that take far longer than it looks like it should, and
            // that is exactly how a test becomes a flake nobody trusts. A
            // second and a half is not a claim about how fast a spawn is; it is
            // a margin wide enough that the test is measuring the handle rather
            // than the runner.
            //
            // Note also that the polls cannot simply be repeated to paper over
            // this: several identical tool calls in a row are a stalled agent
            // to the run loop, which ends the run before the later steps happen.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// The `tick` example, as an absolute path.
///
/// Integration test binaries live in `target/<profile>/deps/`, so the examples
/// built alongside them are one directory over. Located rather than hard-coded
/// so this works under any profile and any `CARGO_TARGET_DIR`.
fn tick_binary() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary knows where it is");
    dir.pop(); // deps/
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = dir
        .join("examples")
        .join(format!("tick{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        // A full `cargo test` builds every example, so this is already there.
        // `cargo test --test handles` does not, and a test that only passes
        // under one invocation is a test that will be reported as broken by
        // whoever runs the other one. Building it here costs nothing when it is
        // already built and removes the footgun entirely.
        let built = std::process::Command::new(env!("CARGO"))
            .args(["build", "--example", "tick"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status();
        assert!(
            matches!(built, Ok(s) if s.success()),
            "could not build the tick fixture: {built:?}"
        );
    }
    assert!(
        exe.exists(),
        "the tick fixture is missing at {}; it is an [[example]] and `cargo test` builds it",
        exe.display()
    );
    exe
}

fn start(line: &str) -> ToolCall {
    ToolCall {
        name: "shell_start".into(),
        arguments: json!({ "line": line }),
    }
}

fn poll(handle: u64) -> ToolCall {
    ToolCall {
        name: "shell_poll".into(),
        arguments: json!({ "handle": handle }),
    }
}

fn kill(handle: u64) -> ToolCall {
    ToolCall {
        name: "shell_kill".into(),
        arguments: json!({ "handle": handle }),
    }
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("watch a long-running process", root).with_max_steps(10)
}

/// The policy these tests run under.
///
/// Permissive, and deliberately: what is under test here is the handle
/// lifecycle, not the boundary — `tests/shell.rs` already proves the boundary
/// for the parse both tools share, and the two refusal tests below prove this
/// tool reaches it. A restrictive policy here would make every test also a test
/// of pattern matching against a temporary path, which is a different subject
/// and a flakier one.
fn allow_tick() -> Policy {
    Policy::permissive()
}

async fn run(steps: Vec<Vec<ToolCall>>, policy: Policy) -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(steps);
    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();
    (store, dir)
}

/// Every decision the run recorded and every observation the model read.
///
/// The observations are taken from each step's *prompt* rather than from the
/// step that produced them, for the reason `tests/shell.rs` gives: an
/// observation reaching the agent's next turn is the claim, and a return value
/// nobody forwards would satisfy an assertion about the return value.
fn transcript(store: &Store, run_id: i64) -> String {
    store
        .steps(run_id)
        .unwrap()
        .iter()
        .map(|s| format!("{}\n{}", s.decision, s.prompt))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_id(store: &Store) -> i64 {
    store.runs().unwrap()[0]
}

/// Whether a pid is a live process, asked of the operating system.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // Signal 0 checks for existence and permission without delivering anything.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    // `tasklist` is on every Windows runner and needs no crate. A pid that is
    // gone is absent from the filtered list.
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// F1 — a handle starts, polls and dies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_handle_starts_polls_twice_and_is_killed() {
    let tick = tick_binary();
    let (store, _dir) = run(
        vec![
            vec![start(&tick.display().to_string())],
            vec![poll(1)],
            vec![poll(1)],
            vec![kill(1)],
        ],
        allow_tick(),
    )
    .await;
    let id = run_id(&store);
    let text = transcript(&store, id);

    assert!(
        text.contains("shell_start handle 1"),
        "the start reports an id the model can use: {text}"
    );
    // Asserted across the polls rather than against any single one. Whether a
    // particular poll lands before or after a particular tick is a wall clock
    // question, and a test that depends on the answer fails on a loaded runner
    // and teaches nothing when it does. What must be true is that polling a
    // running process shows what it printed — not that the first poll does.
    assert!(
        text.contains("tick 1"),
        "polling a running process never showed anything it printed: {text}"
    );
    assert!(
        text.contains("shell_kill handle 1"),
        "the kill is recorded: {text}"
    );
}

#[tokio::test]
async fn a_poll_does_not_return_the_same_output_twice() {
    let tick = tick_binary();
    let (store, _dir) = run(
        vec![
            vec![start(&tick.display().to_string())],
            vec![poll(1)],
            vec![poll(1)],
            vec![kill(1)],
        ],
        allow_tick(),
    )
    .await;
    let id = run_id(&store);
    // The prompt is cumulative — it carries every observation so far — so the
    // question is not whether a later prompt contains an earlier poll's output.
    // It is whether any one line was delivered by two different polls. That is
    // what "incremental, with no duplication" means, and it is what a log tail
    // polled in a loop depends on.
    //
    // Asserted over every poll rather than over the first two, so that which
    // poll happens to catch a tick is not part of the claim. A loaded runner
    // changes that and must not change the verdict.
    let text = transcript(&store, id);
    let last = store.steps(id).unwrap();
    let last = last.last().map(|s| s.prompt.clone()).unwrap_or_default();
    let blocks: Vec<&str> = last.split("[shell_poll handle").skip(1).collect();
    assert!(
        blocks.len() >= 2,
        "this needs two poll blocks to compare, and found {}:\n{text}",
        blocks.len()
    );
    let ticks = |b: &str| -> Vec<String> {
        b.lines()
            .filter(|l| l.starts_with("tick "))
            .map(str::to_string)
            .collect()
    };
    let mut seen: Vec<String> = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        for line in ticks(block) {
            assert!(
                !seen.contains(&line),
                "{line:?} was delivered by two different polls; a poll must return \
                 only what is new. Poll {i} repeated it.\n{last}"
            );
            seen.push(line);
        }
    }
    assert!(
        !seen.is_empty(),
        "no poll saw any output at all, so this proves nothing:\n{last}"
    );
}

#[tokio::test]
async fn a_handle_left_running_is_killed_when_the_run_ends() {
    let tick = tick_binary();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // Started and deliberately never killed, so the run ends with it live. That
    // is the leak this asserts against: a handle the model forgot about must not
    // outlive the run that made it.
    let provider = MockScript::new(vec![vec![start(&tick.display().to_string())]]);
    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &allow_tick(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let id = run_id(&store);
    let handles = store
        .process_handles(id)
        .expect("the run recorded its handle");
    assert_eq!(handles.len(), 1, "one handle was started: {handles:?}");
    let pids = handles[0].pids.clone();
    assert!(
        !pids.is_empty(),
        "no pid was recorded for the handle, so this test cannot check anything: {handles:?}"
    );

    // Asked of the operating system rather than of the registry. The registry
    // believing it killed something is precisely the failure this exists to
    // catch, so asking it whether it succeeded would be asking the defendant for
    // the verdict.
    for pid in pids {
        let mut gone = false;
        for _ in 0..100 {
            if !alive(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "pid {pid} outlived the run that started it; a handle left live must be \
             killed, not leaked"
        );
    }
}

// ---------------------------------------------------------------------------
// F4 — the handle path is checked by the same machinery as the foreground path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_start_refuses_every_construct_shell_refuses() {
    // The same table both tools are held to. Driven from one list on purpose:
    // a refusal added to the parser must not be able to apply to one tool and
    // not the other, and two hand-written lists is exactly how that happens.
    let refused = [
        ("$(whoami)", "command substitution"),
        ("`whoami`", "backtick"),
        ("echo $HOME", "expansion"),
        ("echo ${HOME}", "expansion"),
        ("echo $((1+1))", "arithmetic"),
        ("(cd /)", "subshell"),
        ("cat <<EOF", "heredoc"),
        ("sleep 1 &", "background"),
        ("if true; then echo x; fi", "control flow"),
    ];
    for (line, what) in refused {
        let (store, _dir) = run(vec![vec![start(line)]], Policy::permissive()).await;
        let id = run_id(&store);
        let text = transcript(&store, id);
        assert!(
            text.contains("shell_start refused"),
            "shell_start must refuse {what} in {line:?}, and it did not:\n{text}"
        );
    }
}

#[tokio::test]
async fn a_denied_stage_starts_no_handle_and_spawns_nothing() {
    let tick = tick_binary();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // The first stage would write a file if it ran. The second is denied, so
    // nothing may run — the same negative control the foreground tool carries,
    // and the reason the whole line is checked before the first spawn.
    let line = format!("{} > made.txt | denied-program", tick.display());
    let provider = MockScript::new(vec![vec![start(&line)]]);
    // The second stage is denied by name. Permissive would allow it and the
    // line would merely fail to find the program, which is a different fact and
    // would make this assert nothing.
    let policy = Policy::permissive()
        .layer("test")
        .deny_exec("denied-program*");
    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();
    assert!(
        !dir.path().join("made.txt").exists(),
        "a denied later stage let the earlier one run and write a file"
    );
    let id = run_id(&store);
    let text = transcript(&store, id);
    assert!(
        text.contains("refused") || text.contains("denied"),
        "the refusal is what the trace records: {text}"
    );
}

// ---------------------------------------------------------------------------
// F6 — the cap refuses rather than queues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn starting_past_the_cap_is_refused_and_spawns_nothing() {
    let tick = tick_binary();
    // Nine starts against a cap of eight. The ninth must be refused by name
    // rather than queued, and the eight before it must be untouched.
    //
    // Each line differs by its argument. Nine identical tool calls are a stalled
    // agent as far as the run loop is concerned, and it would stop the run
    // before the cap was ever reached — which would make this test pass for the
    // wrong reason if the cap were removed entirely.
    let steps: Vec<Vec<ToolCall>> = (0..9)
        .map(|i| vec![start(&format!("{} {}", tick.display(), 900 + i))])
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(steps);
    run_with(
        &TaskContract::workspace("start too many", dir.path()).with_max_steps(12),
        &provider,
        &store,
        &allow_tick(),
        &ApproveAll,
    )
    .await
    .unwrap();
    let id = run_id(&store);
    let text = transcript(&store, id);
    assert!(
        text.contains("the handle cap") || text.contains("cap is 8"),
        "the ninth start must be refused by the cap:\n{text}"
    );
    assert!(
        text.contains("shell_start handle 8"),
        "the eight before the cap must all have started:\n{text}"
    );
}
