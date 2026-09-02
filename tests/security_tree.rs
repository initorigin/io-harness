//! The tree loop's three short-circuits, and what they were skipping (0.74.0).
//!
//! `spawn_agent`, `send_message` and `read_messages` are handled by the tree loop
//! itself and never reach `dispatch`. Everything `dispatch` does on the way in was
//! therefore not done for them: the operator's `before_tool` checks never ran, and
//! the plan phase — which is enforced as a policy layer, and a spawn never reaches
//! the policy — did not exist for a spawn at all.
//!
//! What is asserted here is almost always an absence: a marker file a child would
//! have written, a file a child was told to write and may not, an approver's
//! counter that did not move. A refusal that arrives after the child has run
//! produces the same outcome variant, the same events and the same composed report,
//! and only the absent file tells the two apart. Every one of them is paired with a
//! control running the identical script with the gate lifted, because a file that
//! is absent because the whole tree stopped working proves nothing.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use io_harness::approve::DecisionFuture;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_tree, AcceptPlan, Act, ApproveAll, Approver, Config, Containment, Decision, Effect, Plan,
    PlanGate, PlanGateNone, PlanReview, PlanVerdict, Policy, Provider, Request, Rule, Store,
    TaskContract, PROPOSE_PLAN_TOOL, READ_MESSAGES_TOOL, SEND_MESSAGE_TOOL, SPAWN_TOOL,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// A word only the root agent's prompt can contain. Its goal carries it; a child's
/// goal is written by the spawn call and never does, which is what lets one
/// provider answer a parent from a script and every child with the same turn.
const ROOT: &str = "T09-ROOT";

/// What a child writes if it ever runs. Absent means no child ran.
const MARKER: &str = "child-ran.txt";

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

/// A spawn whose child writes `file`, so "did a child run" is a fact on disk.
fn spawn(file: &str) -> ToolCall {
    call(
        SPAWN_TOOL,
        json!({
            "goal": format!("write {file}"),
            "verify_file": file,
            "verify_contains": "ran",
            "max_steps": 2
        }),
    )
}

fn propose() -> ToolCall {
    call(
        PROPOSE_PLAN_TOOL,
        json!({ "steps": [{ "intent": "hand the work to a sub-agent" }] }),
    )
}

/// Answers the root from a script and every child with one write.
///
/// The root is told apart by [`ROOT`] in its prompt rather than by a call counter:
/// a refused spawn and an honoured one make different numbers of completions, and a
/// positional script would answer a different agent in each case.
struct Tree {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    child_file: String,
}

impl Tree {
    fn new(steps: Vec<Vec<ToolCall>>, child_file: &str) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            child_file: child_file.to_string(),
        }
    }
}

impl Provider for Tree {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let usage = Some(Usage {
            total_tokens: 1,
            ..Default::default()
        });
        if !req.user.contains(ROOT) {
            return Ok(CompletionResponse {
                tool_calls: vec![write(&self.child_file, "ran")],
                usage,
                ..Default::default()
            });
        }
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            usage,
            ..Default::default()
        })
    }
}

fn root_contract(root: &Path) -> TaskContract {
    TaskContract::workspace(format!("{ROOT}: hand the work to a sub-agent"), root).with_max_steps(4)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Approves everything, counts the questions, and optionally answers the first one
/// with a rule as well.
///
/// One approver for both halves of the pair: the counter says whether "stop asking"
/// was honoured, and `remember` says what was remembered. A test that wanted only
/// the count passes `None` and gets 0.73.0's approver.
struct Counting {
    asked: AtomicUsize,
    remember: Vec<Rule>,
}

impl Counting {
    fn new(remember: Vec<Rule>) -> Self {
        Self {
            asked: AtomicUsize::new(0),
            remember,
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Approver for Counting {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        let first = self.asked.fetch_add(1, Ordering::SeqCst) == 0;
        let remember = match first {
            true => self.remember.clone(),
            false => Vec::new(),
        };
        Box::pin(async move {
            Decision::Approve {
                modified: None,
                remember,
            }
        })
    }
}

fn deny_write(pattern: &str) -> Rule {
    Rule {
        act: Act::Write,
        effect: Effect::Deny,
        pattern: pattern.into(),
    }
}

/// Every observation this run recorded, joined — what the model actually read.
fn said(store: &Store, run_id: i64) -> String {
    store
        .observations(run_id)
        .unwrap()
        .iter()
        .map(|o| o.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------------- H5

/// H5. A spawn while the plan is unreviewed is refused, and no child exists.
///
/// The exploit this closes is not a permission the spawn held — it is that a spawn
/// never reached the policy at all, so `plan_lock`'s `deny_write("*")` could not
/// see it. A model that emits `spawn_agent` instead of `propose_plan` had the whole
/// task done by a child under the run's full boundary with no human in the loop.
///
/// Asserted on the child's marker file, not on the outcome: a harness that ran the
/// child and then refused something would end the same way.
#[tokio::test]
async fn h5_spawn_agent_is_refused_while_a_plan_is_unapproved() {
    let dir = ws();
    let contract = root_contract(dir.path()).with_plan_gate(Arc::new(PlanGateNone));
    let store = Store::memory().unwrap();
    let provider = Tree::new(vec![vec![spawn(MARKER)], vec![]], MARKER);

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        !dir.path().join(MARKER).exists(),
        "a sub-agent ran while the plan was unapproved and did the work"
    );
    let text = said(&store, result.run_id);
    assert!(
        text.contains(&format!("[{SPAWN_TOOL} refused]")),
        "the refusal names the tool it refused: {text}"
    );
    assert!(
        text.contains(PROPOSE_PLAN_TOOL),
        "and names what to do instead: {text}"
    );
}

/// A gate that always sends the plan back with a correction, which keeps the run
/// in its planning phase without pausing it.
#[derive(Debug)]
struct Revising;

impl PlanGate for Revising {
    fn review<'a>(&'a self, _plan: &'a Plan) -> PlanReview<'a> {
        Box::pin(async {
            Some(PlanVerdict::Revise {
                correction: "narrow it to one file".into(),
            })
        })
    }
}

/// H5, on the one path where the loop itself would have fanned the child out
/// beside a live plan lock.
///
/// A `Revise` is an ordinary observation: the phase stays on, and neither of the
/// two conditions that clear the queued spawn calls — a pending plan, a cancelled
/// one — is met. So a completion carrying `propose_plan` and `spawn_agent` together
/// reached the fan-out with the phase still on, and the fan-out was handed the
/// contract's own policy rather than the one `plan_lock` had narrowed. The child
/// ran with `deny_write("*")` nowhere in sight.
///
/// Asserted as an absence, and it is the whole assertion: the child's marker.
#[tokio::test]
async fn h5_a_spawn_beside_a_plan_sent_back_for_revision_starts_no_child() {
    let dir = ws();
    let contract = root_contract(dir.path()).with_plan_gate(Arc::new(Revising));
    let store = Store::memory().unwrap();
    let provider = Tree::new(vec![vec![propose(), spawn(MARKER)], vec![]], MARKER);

    run_tree(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        !dir.path().join(MARKER).exists(),
        "a sub-agent ran beside a plan the gate had just sent back"
    );
}

/// H5's control, and the ordinary case: with no gate registered there is no phase,
/// and the identical script runs its child.
///
/// Without this the test above passes against a tree that stopped spawning at all.
#[tokio::test]
async fn h5_a_spawn_still_runs_its_child_when_no_plan_gate_is_registered() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = Tree::new(vec![vec![spawn(MARKER)], vec![]], MARKER);

    run_tree(
        &root_contract(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        dir.path().join(MARKER).exists(),
        "the ordinary spawn — no plan gate anywhere — must still run its child"
    );
}

/// H5's second control: the refusal is the phase, not the tool. The same spawn
/// after an approved plan runs its child.
#[tokio::test]
async fn h5_the_spawn_refusal_lifts_once_the_plan_is_approved() {
    let dir = ws();
    let contract = root_contract(dir.path()).with_plan_gate(Arc::new(AcceptPlan));
    let store = Store::memory().unwrap();
    let provider = Tree::new(vec![vec![propose()], vec![spawn(MARKER)], vec![]], MARKER);

    run_tree(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        dir.path().join(MARKER).exists(),
        "an approved plan must unlock the spawn it names"
    );
}

/// H5, second half. A child inherits the policy its parent is *running under*, not
/// the one the contract was started with.
///
/// The tree handed `spawn_child` the caller's own `Policy`, so every narrowing the
/// run had acquired since it started — the plan phase's lock, and any rule an
/// approver had installed — was absent from the child. A child may only narrow what
/// it is handed, so handing it the wrong thing is the one way a descendant ends up
/// with more than its parent.
///
/// The narrowing used here is an approver's remembered deny rather than the plan
/// lock, because the refusal above closes the only in-loop path on which a spawn
/// and the lock can coexist. It exercises the same one-word plumbing: what reaches
/// `Policy::contain` is the workspace's policy.
#[tokio::test]
async fn h5_a_child_inherits_the_policy_its_parent_is_running_under() {
    let dir = ws();
    let store = Store::memory().unwrap();
    // `gate.txt` asks; everything else is allowed outright, so the child's write of
    // `secret.txt` is refused only if the remembered deny reached it.
    let policy = open_policy().ask_write("gate.txt");
    let approver = Counting::new(vec![deny_write("secret.txt")]);
    let provider = Tree::new(
        vec![
            vec![write("gate.txt", "asked")],
            vec![spawn("secret.txt")],
            vec![],
        ],
        "secret.txt",
    );

    run_tree(
        &root_contract(dir.path()),
        &provider,
        &store,
        &policy,
        &approver,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        dir.path().join("gate.txt").exists(),
        "the approved write happened, so the approver really was consulted"
    );
    assert!(
        !dir.path().join("secret.txt").exists(),
        "the child wrote through a deny its parent was running under"
    );
}

/// The control for the test above: the identical run whose approver remembers
/// nothing must let the child write. Without it, an inherited-policy assertion
/// passes against a child that never ran.
#[tokio::test]
async fn h5_the_same_child_writes_when_nothing_was_remembered() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = open_policy().ask_write("gate.txt");
    let provider = Tree::new(
        vec![
            vec![write("gate.txt", "asked")],
            vec![spawn("secret.txt")],
            vec![],
        ],
        "secret.txt",
    );

    run_tree(
        &root_contract(dir.path()),
        &provider,
        &store,
        &policy,
        &Counting::new(Vec::new()),
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        dir.path().join("secret.txt").exists(),
        "with no rule remembered the child writes, so the assertion above is about the rule"
    );
}

// ------------------------------------------------------------------------- M5

/// One empty directory for the whole binary, so a configuration file on the
/// developer's own machine cannot change what the hook tests measure. Every test
/// here wants the same answer — an empty user scope — so one shared directory
/// removes the race rather than serialising around it.
static USER: OnceLock<tempfile::TempDir> = OnceLock::new();

fn empty_user_scope() {
    let dir = USER.get_or_init(|| tempfile::tempdir().unwrap());
    std::env::set_var("IO_CONFIG_HOME", dir.path());
}

// The hook argv per platform. Each platform's own programs are named rather than
// the test being skipped on Windows, as `tests/tool_hooks.rs` does.

/// Refuses, and says why on stdout in a sentence nothing else in this file writes.
#[cfg(unix)]
const REFUSES: &[&str] = &["sh", "-c", "echo 'no fan-out in this repository'; exit 1"];
#[cfg(windows)]
const REFUSES: &[&str] = &[
    "powershell",
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Write-Output 'no fan-out in this repository'; exit 1",
];

/// Allows every call it sees.
#[cfg(unix)]
const ALLOWS: &[&str] = &["true"];
#[cfg(windows)]
const ALLOWS: &[&str] = &["cmd", "/c", "exit 0"];

/// A TOML array literal for one of the argvs above.
fn argv(parts: &[&str]) -> String {
    let items: Vec<String> = parts.iter().map(|p| format!("{p:?}")).collect();
    format!("[{}]", items.join(", "))
}

/// A root contract carrying one `[[hook]]` table over the three tools, installed
/// at local scope — the only scope a lifecycle hook may be declared in.
/// The hook goes in the **user scope**, not `io.local.toml` (0.74.0, audit H2).
///
/// It used to go in `io.local.toml` beside the workspace, which is the shape the
/// hooks guide documented. That file sits at a path the run's own agent can
/// write and a clone can ship, so as of this release it is widening-checked like
/// `io.toml` and may not declare a `[[hook]]` at all. The user scope is the only
/// place left, and it is what every refusal now names.
///
/// The tempdir is dropped as soon as `discover` has read it: nothing after that
/// point reads the file, only the parsed hooks. Setting the environment without
/// a lock is safe here because nextest gives every test its own process.
fn hooked(root: &Path, run: &[&str]) -> TaskContract {
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        user.path().join("io.toml"),
        format!(
            "[[hook]]\nat = \"before_tool\"\n\
             tools = [\"{SPAWN_TOOL}\", \"{SEND_MESSAGE_TOOL}\", \"{READ_MESSAGES_TOOL}\"]\n\
             run = {}\ntimeout_ms = 20000\n",
            argv(run)
        ),
    )
    .unwrap();
    std::env::remove_var("IO_CONFIG");
    std::env::set_var("IO_CONFIG_HOME", user.path());
    let hooks = Config::discover(root).unwrap().hooks();
    assert!(
        !hooks.is_empty(),
        "the hook must actually have loaded, or every assertion below passes for the wrong reason"
    );
    root_contract(root).with_tool_hooks(Arc::new(hooks))
}

/// The three calls the tree loop handles itself, in one completion.
fn the_three() -> Vec<ToolCall> {
    vec![
        spawn(MARKER),
        call(SEND_MESSAGE_TOOL, json!({ "to": "scout", "body": "hi" })),
        call(READ_MESSAGES_TOOL, json!({})),
    ]
}

/// M5. An operator's `before_tool` hook fires for all three tools the tree loop
/// short-circuits.
///
/// A hook that loads, validates, installs and never fires is indistinguishable
/// from one that approved every call, which is the silence a check attached to a
/// tool can least afford. So the assertion is that each of the three names itself
/// in a refusal, *and* that the hook's own sentence — which nothing else in this
/// file writes — reached the model.
#[tokio::test]
async fn m5_a_before_tool_hook_fires_for_spawn_agent_send_message_and_read_messages() {
    empty_user_scope();
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = hooked(dir.path(), REFUSES);
    let provider = Tree::new(vec![the_three(), vec![]], MARKER);

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let text = said(&store, result.run_id);
    for tool in [SPAWN_TOOL, SEND_MESSAGE_TOOL, READ_MESSAGES_TOOL] {
        assert!(
            text.contains(&format!("[{tool} refused]")),
            "the hook never fired for `{tool}`: {text}"
        );
    }
    assert!(
        text.contains("no fan-out in this repository"),
        "the hook's own reason reaches the model: {text}"
    );
    assert!(
        !dir.path().join(MARKER).exists(),
        "the refused spawn started a child anyway"
    );
}

/// M5's control: the identical three calls under a hook that allows. Without it the
/// test above passes against a tree that refuses these three tools outright.
#[tokio::test]
async fn m5_the_same_three_calls_run_when_the_hook_allows() {
    empty_user_scope();
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = hooked(dir.path(), ALLOWS);
    let provider = Tree::new(vec![the_three(), vec![]], MARKER);

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let text = said(&store, result.run_id);
    assert!(
        !text.contains("refused] a local check"),
        "an allowing hook refused something: {text}"
    );
    assert!(
        dir.path().join(MARKER).exists(),
        "the allowed spawn did not run its child"
    );
}

// ------------------------------------------------------------------------ L15a

/// L15a. A rule an approver asked to remember is honoured for the rest of a
/// `run_tree`, as it already was for the rest of a flat run.
///
/// The tree read `remember` off every dispatch and dropped it at the end of the
/// step, so "approve this and stop asking" meant "approve it again next step" —
/// and an operator answering the same question on every step of a long run is how
/// a person stops reading the question.
///
/// The observable is the approver's own counter, not the files: all three writes
/// land either way, and only the number of times a human was interrupted differs.
#[tokio::test]
async fn l15a_a_remembered_rule_stops_the_approver_being_asked_again_in_a_tree() {
    let dir = ws();
    let store = Store::memory().unwrap();
    // No write rule at all: `Policy::default()` asks for every write, so a
    // remembered allow is the only thing that can stop the second question.
    let approver = Counting::new(vec![Rule {
        act: Act::Write,
        effect: Effect::Allow,
        pattern: "*".into(),
    }]);
    let provider = Tree::new(
        vec![
            vec![write("a.txt", "1")],
            vec![write("b.txt", "2")],
            vec![write("c.txt", "3")],
            vec![],
        ],
        MARKER,
    );

    run_tree(
        &root_contract(dir.path()),
        &provider,
        &store,
        &Policy::default(),
        &approver,
        &containment(),
    )
    .await
    .unwrap();

    for f in ["a.txt", "b.txt", "c.txt"] {
        assert!(
            dir.path().join(f).exists(),
            "{f} was not written, so the count below measures nothing"
        );
    }
    assert_eq!(
        approver.asked(),
        1,
        "the remembered rule was dropped and the human was asked again"
    );
}

/// L15a's control: the same three writes under an approver that remembers nothing
/// must ask three times. Without it the assertion above passes against a run that
/// stopped consulting the approver.
#[tokio::test]
async fn l15a_an_approver_that_remembers_nothing_is_asked_every_time() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let approver = Counting::new(Vec::new());
    let provider = Tree::new(
        vec![
            vec![write("a.txt", "1")],
            vec![write("b.txt", "2")],
            vec![write("c.txt", "3")],
            vec![],
        ],
        MARKER,
    );

    run_tree(
        &root_contract(dir.path()),
        &provider,
        &store,
        &Policy::default(),
        &approver,
        &containment(),
    )
    .await
    .unwrap();

    assert_eq!(
        approver.asked(),
        3,
        "one question per write is what remembering nothing means"
    );
}
