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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
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
fn tick_binary() -> std::path::PathBuf {
    example_binary("tick")
}

/// One of this crate's example fixtures, as an absolute path.
///
/// Integration test binaries live in `target/<profile>/deps/`, so the examples
/// built alongside them are one directory over. Located rather than hard-coded
/// so this works under any profile and any `CARGO_TARGET_DIR`.
fn example_binary(name: &str) -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary knows where it is");
    dir.pop(); // deps/
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = dir
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        // A full `cargo test` builds every example, so this is already there.
        // `cargo test --test handles` does not, and a test that only passes
        // under one invocation is a test that will be reported as broken by
        // whoever runs the other one. Building it here costs nothing when it is
        // already built and removes the footgun entirely.
        let built = std::process::Command::new(env!("CARGO"))
            .args(["build", "--example", name])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status();
        assert!(
            matches!(built, Ok(s) if s.success()),
            "could not build the {name} fixture: {built:?}"
        );
    }
    assert!(
        exe.exists(),
        "the {name} fixture is missing at {}; it is an example and `cargo test` builds it",
        exe.display()
    );
    exe
}

/// A fixture path as a shell word the parser will hand back unchanged.
///
/// Single-quoted, and this matters only on Windows — where it matters a lot. The
/// shell grammar this crate parses treats `\` as an escape, exactly as POSIX
/// does, so an unquoted `D:\a\repo\tick.exe` lexes to `D:arepotick.exe` and the
/// spawn fails with a program nobody named. Quoting is the same answer a real
/// shell gives, and writing it here rather than special-casing the parser keeps
/// the tests honest about what a caller has to do.
fn word(path: &std::path::Path) -> String {
    format!("'{}'", path.display())
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
            vec![start(&word(&tick))],
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
            vec![start(&word(&tick))],
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
    let provider = MockScript::new(vec![vec![start(&word(&tick))]]);
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

#[tokio::test]
async fn a_handle_that_exits_on_its_own_stops_reading_as_running() {
    let tick = tick_binary();
    // `tick 2` prints twice and returns. By the time the run ends it is gone of
    // its own accord, with nobody having killed it.
    let (store, _dir) = run(
        vec![
            vec![start(&format!("{} 2", word(&tick)))],
            vec![poll(1)],
        ],
        allow_tick(),
    )
    .await;
    let id = run_id(&store);
    let handles = store.process_handles(id).expect("recorded");
    assert_eq!(
        handles[0].state, "exited",
        "a handle that ended by itself still reads as {:?} in the trace; the ending is \
         noticed by a task that cannot write to the store, so something on the run loop's \
         thread has to carry it to disk: {handles:?}",
        handles[0].state
    );
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
    let line = format!("{} > made.txt | denied-program", word(&tick));
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
        .map(|i| vec![start(&format!("{} {}", word(&tick), 900 + i))])
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

// ---------------------------------------------------------------------------
// F2 — a kill takes the whole tree, including a grandchild whose parent is gone
// ---------------------------------------------------------------------------

/// Wait for the `orphan` fixture's leaf to announce its pid, and return it.
///
/// The file appearing is the fixture's signal that the leaf has been reparented
/// — that the middle process is gone and the parent/child link from the handle
/// to the leaf no longer exists. So this is not only how the test learns the
/// pid, it is how the test knows the scenario it wanted has actually happened
/// before it kills anything.
#[cfg(unix)]
async fn leaf_pid(pidfile: &std::path::Path) -> u32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(pidfile) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "the orphan fixture never wrote {}; without a grandchild pid this test proves nothing",
        pidfile.display()
    );
}

/// Give a signal time to be delivered before asking whether it worked.
///
/// `SIGKILL` is not synchronous with the `kill` that sent it, and a test that
/// checks immediately is a test that fails on a loaded runner for a reason that
/// has nothing to do with the code. Polls rather than sleeps a fixed time, so
/// the usual case costs one poll.
#[cfg(unix)]
async fn gone_within(pid: u32, tries: u32) -> bool {
    for _ in 0..tries {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
#[tokio::test]
async fn killing_a_handle_kills_a_grandchild_whose_parent_already_exited() {
    let orphan = example_binary("orphan");
    // Outside the workspace on purpose: the fixture writes this file itself,
    // and putting it in the run's own directory would make the test also a test
    // of what a payload may write where.
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaf.pid");
    let line = format!("{} {}", word(&orphan), word(&pidfile));

    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // Start, then kill. The mock's pause between turns is far longer than the
    // fixture needs to build its chain, so the kill lands on a tree that has
    // already lost its middle.
    let provider = MockScript::new(vec![vec![start(&line)], vec![kill(1)]]);
    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &allow_tick(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // Read after the run rather than during it: the file outlives the process
    // that wrote it, and reading it here keeps the test out of the run loop's
    // way. If it is missing the fixture never got as far as a grandchild, which
    // is a failure of the test rather than a pass.
    let leaf = leaf_pid(&pidfile).await;
    assert!(
        gone_within(leaf, 100).await,
        "the grandchild {leaf} survived the kill; its parent had already exited, so a \
         parent/child walk could not reach it and only the process group can"
    );

    let handles = store
        .process_handles(run_id(&store))
        .expect("the run recorded its handle");
    let top = *handles
        .first()
        .and_then(|h| h.pids.first())
        .expect("the handle recorded the process it started");
    assert!(
        gone_within(top, 100).await,
        "the handle's own process {top} survived the kill"
    );
}

/// The negative control, without which the test above proves nothing.
///
/// It runs the identical fixture with the containment switched off — spawned
/// straight from here, so nothing puts it in a process group of its own — and
/// kills it the way the crate killed handles before this release: the recorded
/// pid, plus whatever the process table still says descends from it. The middle
/// is already gone by the time the pid file exists, so that walk finds nothing
/// and the grandchild lives. That is the gap, demonstrated rather than asserted.
#[cfg(unix)]
#[tokio::test]
async fn without_a_process_group_the_grandchild_survives() {
    let orphan = example_binary("orphan");
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaf.pid");

    let mut top = std::process::Command::new(&orphan)
        .arg(&pidfile)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the orphan fixture starts");
    let leaf = leaf_pid(&pidfile).await;

    // The old kill, in full. `kill_tree` walks `ps` for descendants and then
    // signals the root; with the middle already exited the walk returns nothing
    // for this tree, so signalling the root is all of it. Reproduced here rather
    // than called because it is crate-private, and it is faithful because the
    // fixture guarantees the walk has nothing left to find.
    //
    // SAFETY: `kill` takes a pid and a signal by value and dereferences
    // nothing. The pid is this test's own child, which has not been reaped, so
    // it cannot have been reused by an unrelated process.
    unsafe { libc::kill(top.id() as i32, libc::SIGKILL) };
    let _ = top.wait();

    assert!(
        !gone_within(leaf, 6).await,
        "the grandchild {leaf} died without any containment, so the test above would \
         pass even with the process group removed and is not evidence of anything"
    );

    // Do not leave the thing this test just proved is unkillable-by-pid running.
    // SAFETY: as above; the leaf pid was published by the fixture moments ago
    // and was still alive at the assertion.
    unsafe { libc::kill(leaf as i32, libc::SIGKILL) };
}
