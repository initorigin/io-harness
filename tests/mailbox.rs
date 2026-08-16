//! 0.60.0 — the agent mailbox. The first horizontal edge in a tree.
//!
//! A tree could already nest, share one ledger, queue past its concurrency cap and
//! hand a child's report up. Every one of those is a *vertical* edge. Two children
//! investigating two subsystems had no way to tell each other what they found, and
//! a coordinator could not wait on one named child — only spawn and read whatever
//! came back.
//!
//! The tests here are in two layers, and the split is deliberate. The store layer
//! proves the rows: ordering, exactly-once delivery, and that a session delete
//! accounts for the new table. The tree layer proves the address: that a name
//! identifies one agent rather than a role, and that a name resolves inside one
//! tree and nowhere else.
//!
//! Stores are on disk rather than [`Store::memory`](io_harness::Store::memory)
//! wherever a claim is about surviving a process, because an in-memory database
//! cannot be reopened and the resume claim is the one most likely to be wrong.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_tree, AgentDef, Agents, ApproveAll, Containment, Policy, Provider, Store, TaskContract,
    Verification,
};
use serde_json::json;

/// A store on disk, and the directory that keeps it alive for the test.
fn on_disk() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("store.db");
    let store = Store::open(&path).expect("a store");
    (dir, store, path)
}

// ---------------------------------------------------------------- scaffolding

/// One scripted step: a fixed list of calls, or one computed from what the agent
/// was actually shown.
///
/// The computed arm is not a convenience. F1's whole claim is that the author
/// edits a line it could only have learnt from the scout, and a fixed script
/// cannot express "write down what you were told" — it would write the right
/// answer whether or not the message arrived, which is the one thing the test
/// exists to rule out.
type Step = Box<dyn Fn(&CompletionRequest) -> CompletionResponse + Send + Sync>;

fn fixed(calls: Vec<ToolCall>) -> Step {
    Box::new(move |_| CompletionResponse {
        tool_calls: calls.clone(),
        ..Default::default()
    })
}

/// A step that SAYS something and calls nothing.
///
/// The crate records a completion's prose as an `AgentEvent::said` row, and the
/// last of those is what a parent composes as its child's conclusion. A child
/// that never speaks leaves no such row — which is why a test asserting "the
/// terminal post is the short line and not the report" proves nothing unless its
/// child has a report to confuse it with.
fn says(text: &'static str) -> Step {
    Box::new(move |_| CompletionResponse {
        text: Some(text.to_string()),
        ..Default::default()
    })
}

/// Plays one script per agent, keyed by the goal that agent was given.
///
/// A tree needs this rather than the flat step-indexed mock the older suites use:
/// children run concurrently, so "the fourth completion in this process" names a
/// different agent on different runs, and every claim here is about which agent
/// said what.
struct ByGoal {
    scripts: std::collections::HashMap<String, Vec<Step>>,
    at: Mutex<std::collections::HashMap<String, usize>>,
    calls: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ByGoal {
    fn new(scripts: Vec<(&str, Vec<Step>)>) -> Self {
        Self {
            scripts: scripts
                .into_iter()
                .map(|(g, s)| (g.to_string(), s))
                .collect(),
            at: Mutex::new(std::collections::HashMap::new()),
            calls: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for ByGoal {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Keyed on the goal rather than on a call counter, because children run
        // concurrently: "the fourth completion in this process" names a different
        // agent on different runs, and every claim here is about which agent said
        // what.
        //
        // Anchored on the prompt's own `Goal:` line, and that is not a detail: a
        // parent's observations quote its children's goals back at it
        // (`[child scout (run 7) "locate the symbol" detached]`), so a `contains`
        // hands the parent its child's script from the step after it spawns —
        // which is exactly what happened when this was first written.
        let key = self
            .scripts
            .keys()
            .find(|k| req.user.starts_with(&format!("Goal: {k}\n")))
            .cloned();
        let Some(key) = key else {
            self.seen.lock().unwrap().push(req);
            return Ok(CompletionResponse::default());
        };
        let i = {
            let mut at = self.at.lock().unwrap();
            let i = at.entry(key.clone()).or_insert(0);
            let was = *i;
            *i += 1;
            was
        };
        let response = self.scripts[&key]
            .get(i)
            .map(|f| f(&req))
            .unwrap_or_default();
        self.seen.lock().unwrap().push(req);
        Ok(response)
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn send(to: &str, body: &str) -> ToolCall {
    call("send_message", json!({ "to": to, "body": body }))
}

fn read(args: serde_json::Value) -> ToolCall {
    call("read_messages", args)
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// A spawn with an explicit address.
fn spawn(goal: &str, address: &str, extra: serde_json::Value) -> ToolCall {
    let mut args = json!({
        "goal": goal,
        "as": address,
        "verify_file": "never.txt",
        "verify_contains": "never",
        "max_steps": 6,
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        args[k] = v;
    }
    call("spawn_agent", args)
}

/// A root contract over a fresh workspace, with no gate: the root ends when it
/// calls no tool, which is what lets a script decide the shape of a run.
fn root_contract(dir: &std::path::Path, goal: &str) -> TaskContract {
    TaskContract::workspace(goal, dir)
        .with_verification(Verification::None)
        .with_max_steps(8)
}

async fn drive(contract: &TaskContract, provider: &ByGoal, store: &Store) -> io_harness::RunResult {
    run_tree(
        contract,
        provider,
        store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .expect("the tree runs")
}

/// Every observation the whole tree recorded, as one string. What an assertion
/// about "the model was told" reads.
fn transcript(store: &Store, root: i64) -> String {
    let mut out = String::new();
    let mut runs = vec![root];
    runs.extend(store.children(root).unwrap());
    for r in runs {
        for s in store.steps(r).unwrap() {
            out.push_str(&s.result);
            out.push('\n');
            out.push_str(&s.decision);
            out.push('\n');
        }
    }
    out
}

// ------------------------------------------------ the address and the channel

/// **F1 — a child tells a sibling what it found, and the sibling could not have
/// found it otherwise.**
///
/// The author's script does not contain the answer. It writes whatever follows
/// `FOUND ` in its own prompt, so the file it produces is evidence the message
/// arrived — if nothing was delivered it writes `nothing`, which is the negative
/// half of the criterion built into the same run rather than asserted separately.
#[tokio::test]
async fn a_child_tells_a_sibling_what_it_found() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![
        (
            "coordinate the fix",
            vec![
                fixed(vec![
                    spawn("locate the symbol", "scout", json!({ "wait": false })),
                    spawn("make the edit", "author", json!({ "wait": false })),
                ]),
                fixed(vec![read(json!({ "wait_secs": 20 }))]),
                fixed(vec![read(json!({}))]),
                fixed(vec![]),
            ],
        ),
        (
            "locate the symbol",
            vec![
                fixed(vec![send("author", "FOUND src/auth.rs:210")]),
                fixed(vec![]),
            ],
        ),
        (
            "make the edit",
            vec![
                fixed(vec![read(json!({ "from": "scout", "wait_secs": 20 }))]),
                // The whole point: what it writes comes out of what it was shown.
                Box::new(|req: &CompletionRequest| {
                    let line = req
                        .user
                        .split("FOUND ")
                        .nth(1)
                        .map(|rest| rest.split_whitespace().next().unwrap_or("nothing"))
                        .unwrap_or("nothing");
                    CompletionResponse {
                        tool_calls: vec![call(
                            "write_file",
                            json!({ "path": "edited.txt", "content": line }),
                        )],
                        ..Default::default()
                    }
                }),
                fixed(vec![]),
            ],
        ),
    ]);
    let store = Store::memory().unwrap();
    drive(
        &root_contract(dir.path(), "coordinate the fix"),
        &provider,
        &store,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join("edited.txt"))
        .expect("the author wrote the file it was asked to");
    assert_eq!(
        written, "src/auth.rs:210",
        "the author edited the line only the scout knew; it wrote {written:?}"
    );
}

/// **F2 — a name addresses one agent, not a role.**
///
/// Two children of the *same* roster definition, spawned in the same step. Under
/// a resolution that went by the definition's name they are one address, and both
/// would read both messages.
#[tokio::test]
async fn two_children_of_one_definition_are_two_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let mine = |name: &'static str| -> Vec<Step> {
        vec![
            fixed(vec![read(json!({ "wait_secs": 20 }))]),
            Box::new(move |req: &CompletionRequest| {
                let saw: Vec<&str> = ["for a only", "for b only"]
                    .into_iter()
                    .filter(|needle| req.user.contains(needle))
                    .collect();
                CompletionResponse {
                    tool_calls: vec![call(
                        "write_file",
                        json!({ "path": format!("{name}.txt"), "content": saw.join("+") }),
                    )],
                    ..Default::default()
                }
            }),
            fixed(vec![]),
        ]
    };
    let provider = ByGoal::new(vec![
        (
            "fan out to two workers",
            vec![
                fixed(vec![
                    spawn("task a", "a", json!({ "agent": "worker", "wait": false })),
                    spawn("task b", "b", json!({ "agent": "worker", "wait": false })),
                ]),
                fixed(vec![send("a", "for a only"), send("b", "for b only")]),
                fixed(vec![read(json!({ "wait_secs": 20 }))]),
                fixed(vec![read(json!({}))]),
                fixed(vec![]),
            ],
        ),
        ("task a", mine("a")),
        ("task b", mine("b")),
    ]);
    let store = Store::memory().unwrap();
    let contract = root_contract(dir.path(), "fan out to two workers")
        .with_agents(Agents::new().with(AgentDef::new("worker")));
    drive(&contract, &provider, &store).await;

    let a = std::fs::read_to_string(dir.path().join("a.txt")).expect("a wrote");
    let b = std::fs::read_to_string(dir.path().join("b.txt")).expect("b wrote");
    assert_eq!(a, "for a only", "`a` read only what was addressed to it");
    assert_eq!(b, "for b only", "`b` read only what was addressed to it");
}

/// **F3 — a duplicate address is refused before anything is allocated.**
///
/// The three numbers are compared against a control run identical in every way
/// except that the second spawn asks for a free address. Asserting them against
/// themselves would pass for a refusal that allocated and then rolled back, which
/// is not what the criterion claims.
#[tokio::test]
async fn a_duplicate_address_costs_no_run_no_slot_and_no_queue_place() {
    let run = |second: &'static str| async move {
        let dir = tempfile::tempdir().unwrap();
        let provider = ByGoal::new(vec![
            (
                "spawn twice",
                vec![
                    fixed(vec![spawn("one", "dup", json!({}))]),
                    fixed(vec![spawn("two", second, json!({}))]),
                    fixed(vec![]),
                ],
            ),
            ("one", vec![fixed(vec![])]),
            ("two", vec![fixed(vec![])]),
        ]);
        let store = Store::memory().unwrap();
        let result = drive(&root_contract(dir.path(), "spawn twice"), &provider, &store).await;
        let root = result.run_id;
        (
            store.children(root).unwrap().len(),
            store.agent_count_tree(root).unwrap(),
            store.queued_agents(root).unwrap().len(),
            transcript(&store, root),
        )
    };

    let (refused_children, refused_agents, refused_queue, said) = run("dup").await;
    let (ok_children, ok_agents, ok_queue, _) = run("free").await;

    assert!(
        said.contains("is already the address of an agent in this tree"),
        "the refusal must name the rule it broke: {said}"
    );
    assert_eq!(
        (refused_children, refused_agents, refused_queue),
        (1, 2, 0),
        "a refused address costs no run row, no agent against the cap, no queue place"
    );
    assert_eq!(
        (ok_children, ok_agents, ok_queue),
        (2, 3, 0),
        "the control spawns its second child, so the numbers above are a difference"
    );
}

/// **F4 — `root` is reserved.**
#[tokio::test]
async fn a_child_cannot_take_the_roots_address() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![(
        "try to shadow the root",
        vec![
            fixed(vec![spawn("shadow", "root", json!({}))]),
            fixed(vec![]),
        ],
    )]);
    let store = Store::memory().unwrap();
    let result = drive(
        &root_contract(dir.path(), "try to shadow the root"),
        &provider,
        &store,
    )
    .await;

    let said = transcript(&store, result.run_id);
    assert!(
        said.contains("is the address of the agent at the top of this tree"),
        "the refusal names the reason: {said}"
    );
    assert!(
        store.children(result.run_id).unwrap().is_empty(),
        "and no child was made"
    );
}

/// **F10 — an unknown address is refused with the names that are addressable.**
#[tokio::test]
async fn an_unknown_address_is_refused_with_what_is_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![
        (
            "send into the void",
            vec![
                fixed(vec![spawn("be there", "scout", json!({}))]),
                fixed(vec![send("nobody", "hello?")]),
                fixed(vec![]),
            ],
        ),
        ("be there", vec![fixed(vec![])]),
    ]);
    let store = Store::memory().unwrap();
    let result = drive(
        &root_contract(dir.path(), "send into the void"),
        &provider,
        &store,
    )
    .await;

    let said = transcript(&store, result.run_id);
    assert!(
        said.contains("no agent in this tree is addressed `nobody`"),
        "the refusal names the address: {said}"
    );
    assert!(
        said.contains("Reachable from here: root, scout"),
        "and lists what is, so the model recovers in one step: {said}"
    );
}

/// **F11 — an address resolves inside this tree and nowhere else.**
///
/// Two trees over ONE store. The refusal must read like any other unknown name:
/// saying "that agent is in another tree" would leak the other tree's shape to an
/// agent that cannot reach it.
#[tokio::test]
async fn an_address_does_not_reach_out_of_its_own_tree() {
    let store = Store::memory().unwrap();

    let first_dir = tempfile::tempdir().unwrap();
    let first = ByGoal::new(vec![
        (
            "the first tree",
            vec![
                fixed(vec![spawn("first child", "alpha", json!({}))]),
                fixed(vec![]),
            ],
        ),
        ("first child", vec![fixed(vec![])]),
    ]);
    drive(
        &root_contract(first_dir.path(), "the first tree"),
        &first,
        &store,
    )
    .await;

    let second_dir = tempfile::tempdir().unwrap();
    let second = ByGoal::new(vec![
        (
            "the second tree",
            vec![
                fixed(vec![spawn("second child", "beta", json!({}))]),
                fixed(vec![send("alpha", "reaching across")]),
                fixed(vec![]),
            ],
        ),
        ("second child", vec![fixed(vec![])]),
    ]);
    let result = drive(
        &root_contract(second_dir.path(), "the second tree"),
        &second,
        &store,
    )
    .await;

    let said = transcript(&store, result.run_id);
    assert!(
        said.contains("no agent in this tree is addressed `alpha`"),
        "the other tree's agent is simply not there: {said}"
    );
    assert!(
        said.contains("Reachable from here: beta, root"),
        "and the listing does not mention it either: {said}"
    );
}

/// **F15 — a flat run has neither tool.**
///
/// Read off the tool schemas the provider was actually sent, not off
/// `workspace_tools`: the claim is about what reaches the wire.
#[tokio::test]
async fn a_flat_run_is_offered_no_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![("a plain run", vec![fixed(vec![])])]);
    let store = Store::memory().unwrap();
    io_harness::run_with(
        &TaskContract::workspace("a plain run", dir.path()).with_verification(Verification::None),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let offered: Vec<String> = provider.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(
        !offered
            .iter()
            .any(|n| n == "send_message" || n == "read_messages"),
        "a run with nobody to talk to is not offered a way to talk: {offered:?}"
    );
    assert!(
        !offered.iter().any(|n| n == "spawn_agent"),
        "the control: a flat run has never had the spawn tool either"
    );
}

// --------------------------------------------------------------- the waiting

/// **F6 — a bounded wait returns on the message, not on the clock.**
///
/// Asserted as a comparison against the clock the test itself supplied, never as
/// an absolute duration: the Windows leg of this matrix is roughly three times
/// slower and nine CI failures in this repository's history were a test gating on
/// a number of seconds.
///
/// This is the release's central guard. The first implementation slept rather
/// than driving its own in-flight children, so every one of these waits ran to its
/// full twenty seconds and then succeeded on the step after — a suite that was
/// green while the feature did not work. What that costs is visible only here,
/// which is why the elapsed time is asserted rather than the outcome alone.
#[tokio::test]
async fn a_wait_returns_when_the_message_arrives() {
    const CLOCK: u64 = 20;
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![
        (
            "wait for the scout",
            vec![
                fixed(vec![
                    spawn("go and look", "scout", json!({ "wait": false })),
                    spawn("wait for it", "waiter", json!({ "wait": false })),
                ]),
                fixed(vec![read(json!({ "wait_secs": CLOCK }))]),
                fixed(vec![]),
            ],
        ),
        (
            "go and look",
            vec![fixed(vec![send("waiter", "here it is")]), fixed(vec![])],
        ),
        (
            "wait for it",
            vec![
                fixed(vec![read(json!({ "from": "scout", "wait_secs": CLOCK }))]),
                fixed(vec![]),
            ],
        ),
    ]);
    let store = Store::memory().unwrap();
    let began = std::time::Instant::now();
    let result = drive(
        &root_contract(dir.path(), "wait for the scout"),
        &provider,
        &store,
    )
    .await;
    let took = began.elapsed();

    assert!(
        transcript(&store, result.run_id).contains("here it is"),
        "the waiter was given the message"
    );
    assert!(
        took < std::time::Duration::from_secs(CLOCK),
        "a wait that returns on the message cannot take the whole clock; it took {took:?} of {CLOCK}s"
    );
}

/// **F7 — a bounded wait returns at the clock when nothing arrives, and says so.**
///
/// The two sentences are compared against each other in one run. "Nothing was
/// sent" and "nothing was sent yet and I stopped waiting" are different facts, and
/// an agent that reads the same words for both cannot decide whether to wait
/// again.
#[tokio::test]
async fn waiting_out_the_clock_reads_differently_from_finding_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![(
        "look twice",
        vec![
            fixed(vec![read(json!({}))]),
            fixed(vec![read(json!({ "wait_secs": 1 }))]),
            fixed(vec![]),
        ],
    )]);
    let store = Store::memory().unwrap();
    let result = drive(&root_contract(dir.path(), "look twice"), &provider, &store).await;

    let said = transcript(&store, result.run_id);
    assert!(
        said.contains("[messages] nothing waiting"),
        "the drain says there was nothing: {said}"
    );
    assert!(
        said.contains("nothing arrived in 1s"),
        "and the wait says it stopped waiting, naming the clock it was given: {said}"
    );
}

/// **F8 — a wait on an agent that has already finished and said nothing returns
/// immediately.**
///
/// Sibling to sibling, and it has to be: a *parent* waiting on its own child never
/// reaches this path, because F9's terminal post means a finished child has always
/// said exactly one thing to its parent. The case the early return exists for is
/// the one where nobody posted — which is every pair of siblings.
#[tokio::test]
async fn waiting_on_an_agent_that_has_finished_returns_at_once() {
    const CLOCK: u64 = 30;
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![
        (
            "one speaks, one waits",
            vec![
                // Blocking, so the quiet one is finished before the waiter starts.
                fixed(vec![spawn("say nothing", "quiet", json!({}))]),
                fixed(vec![spawn("wait on the quiet one", "waiter", json!({}))]),
                fixed(vec![]),
            ],
        ),
        ("say nothing", vec![fixed(vec![])]),
        (
            "wait on the quiet one",
            vec![
                fixed(vec![read(json!({ "from": "quiet", "wait_secs": CLOCK }))]),
                fixed(vec![]),
            ],
        ),
    ]);
    let store = Store::memory().unwrap();
    let began = std::time::Instant::now();
    let result = drive(
        &root_contract(dir.path(), "one speaks, one waits"),
        &provider,
        &store,
    )
    .await;
    let took = began.elapsed();

    let said = transcript(&store, result.run_id);
    assert!(
        said.contains("quiet has finished and sent you nothing"),
        "the answer names why waiting again will not help: {said}"
    );
    assert!(
        took < std::time::Duration::from_secs(CLOCK),
        "it must not spend the clock on an agent that ended before it started; took {took:?}"
    );
}

/// **F13 — the operator's cap narrows the agent's request, and the agent is told.**
///
/// The boundary pair is asserted exactly — a request of 5 against a cap of 5 is
/// not narrowed and a request of 6 is — so `>` in place of `>=` fails. 0.57.0
/// shipped a criterion whose three fixtures all sat away from its threshold and a
/// comparison sabotage survived every one of them.
#[tokio::test]
async fn the_operators_cap_narrows_the_request_and_says_so() {
    // A message is already waiting when the read happens — the child's own
    // terminal post — so no arm of this test spends a clock. The narrowing is
    // decided from the request and the cap before anything is read, which is what
    // makes that sound rather than convenient: what is being asserted is the
    // notice, and F6 and F7 are what assert the waiting.
    let at = |asked: u64| async move {
        let dir = tempfile::tempdir().unwrap();
        let provider = ByGoal::new(vec![
            (
                "ask for a long wait",
                vec![
                    fixed(vec![spawn("be brief", "brief", json!({}))]),
                    fixed(vec![read(json!({ "wait_secs": asked }))]),
                    fixed(vec![]),
                ],
            ),
            ("be brief", vec![fixed(vec![])]),
        ]);
        let store = Store::memory().unwrap();
        let contract = root_contract(dir.path(), "ask for a long wait").with_max_wait_secs(5);
        let result = drive(&contract, &provider, &store).await;
        transcript(&store, result.run_id)
    };

    let long = at(60).await;
    assert!(
        long.contains("[wait narrowed] this run allows a wait of at most 5s"),
        "a request over the cap is narrowed and said: {long}"
    );

    let exactly = at(5).await;
    assert!(
        !exactly.contains("[wait narrowed]"),
        "a request EQUAL to the cap is not narrowed: {exactly}"
    );
    let over_by_one = at(6).await;
    assert!(
        over_by_one.contains("[wait narrowed]"),
        "and one second over it is: {over_by_one}"
    );
}

/// **F9 — a terminating agent posts to its parent, and the report is not
/// duplicated.**
#[tokio::test]
async fn a_finished_child_posts_once_and_the_report_still_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let provider = ByGoal::new(vec![
        (
            "one child",
            vec![
                fixed(vec![spawn("do a thing", "worker", json!({}))]),
                fixed(vec![]),
            ],
        ),
        (
            "do a thing",
            vec![says(
                "I read every handler and the shared one is src/session.rs:88, which is \
                 the report a parent would confuse the terminal line with",
            )],
        ),
    ]);
    let store = Store::memory().unwrap();
    let result = drive(&root_contract(dir.path(), "one child"), &provider, &store).await;
    let root = result.run_id;

    let inbox = store.messages_for(root).unwrap();
    assert_eq!(inbox.len(), 1, "exactly one row from the child: {inbox:?}");
    assert_eq!(inbox[0].from_name, "worker");
    assert!(
        inbox[0].body.starts_with("[finished] "),
        "the terminal line, not the report: {:?}",
        inbox[0].body
    );
    assert!(
        !inbox[0].body.contains("src/session.rs:88"),
        "the child HAS a conclusion and this is not it: {:?}",
        inbox[0].body
    );
    assert!(
        inbox[0].body.len() < 120,
        "a report in the body would deliver it twice; got {} bytes",
        inbox[0].body.len()
    );
    // And 0.50.0's path is untouched: the composed report still reaches the parent.
    assert!(
        transcript(&store, root).contains("[child "),
        "the composed report still arrives the way it always has"
    );
}

/// **F12 — a resumed tree reads the same messages in the same order, once.**
///
/// The store is closed and reopened between the two reads, so a delivery marked
/// anywhere but the row does not survive. Keyed on `read_at` rather than on a set
/// in memory is the whole claim, and an in-process set passes every other test in
/// this file.
#[test]
fn a_reopened_store_delivers_what_is_left_and_nothing_twice() {
    let (_dir, store, path) = on_disk();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    for i in 0..3 {
        store
            .send_message(scout, me, "scout", i + 1, &format!("finding {i}"))
            .unwrap();
    }
    let first = store.read_messages(me, None).unwrap();
    assert_eq!(first.len(), 3);
    // Two more arrive after that read, from a run that is still going.
    for i in 3..5 {
        store
            .send_message(scout, me, "scout", i + 1, &format!("finding {i}"))
            .unwrap();
    }
    drop(store);

    let resumed = Store::open(&path).unwrap();
    let second = resumed.read_messages(me, None).unwrap();
    assert_eq!(
        second.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["finding 3", "finding 4"],
        "the resume delivers exactly what was left, in the same order"
    );
    assert!(
        resumed.read_messages(me, None).unwrap().is_empty(),
        "and nothing a third time"
    );
    // The three already delivered are still marked, so nothing was re-delivered
    // rather than merely not re-returned.
    let audit = resumed.messages_for(me).unwrap();
    assert_eq!(audit.len(), 5);
    assert!(audit.iter().all(|m| m.read_at.is_some()));
}

/// **F5, first half — ten messages from three senders come back in row-id order.**
///
/// The senders interleave, which is the point: a per-sender order would pass a
/// fixture where each sender's messages are contiguous and reorder a real tree.
#[test]
fn messages_are_delivered_oldest_first_across_interleaved_senders() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let senders: Vec<(i64, &str)> = ["scout", "critic", "author"]
        .iter()
        .map(|n| (store.start_run(n, "/repo").unwrap(), *n))
        .collect();

    // Ten messages, round-robin over three senders, so no sender's are adjacent.
    let mut sent = Vec::new();
    for i in 0..10u32 {
        let (run, name) = senders[i as usize % senders.len()];
        let body = format!("finding {i}");
        store.send_message(run, me, name, i + 1, &body).unwrap();
        sent.push((name.to_string(), body));
    }

    let inbox = store.read_messages(me, None).unwrap();
    let got: Vec<(String, String)> = inbox
        .iter()
        .map(|m| (m.from_name.clone(), m.body.clone()))
        .collect();
    assert_eq!(got, sent, "delivery order is the order they were sent");
    assert!(
        inbox.iter().all(|m| m.read_at.is_some()),
        "a delivered message carries the mark this read stamped"
    );
}

/// **F5, second half — a second read returns nothing.**
///
/// Exactly-once within one process. The cross-process half is F12.
#[test]
fn a_message_is_delivered_exactly_once() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "src/auth.rs:210")
        .unwrap();

    assert_eq!(store.read_messages(me, None).unwrap().len(), 1);
    assert!(
        store.read_messages(me, None).unwrap().is_empty(),
        "a delivered message is not delivered again"
    );
    // And the audit read still sees it, marked.
    let audit = store.messages_for(me).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].read_at.is_some());
}

/// An audit read delivers nothing, which is the whole difference between the two
/// calls. An operator asking what an agent was told must not consume what that
/// agent has not read yet.
#[test]
fn an_audit_read_does_not_deliver() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "waiting")
        .unwrap();

    let audit = store.messages_for(me).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].read_at.is_none(), "still waiting");
    assert_eq!(
        store.read_messages(me, None).unwrap().len(),
        1,
        "the audit did not consume it"
    );
}

/// A `from` filter narrows to one sender and leaves the rest waiting — it is a
/// filter on delivery, not a view over it.
#[test]
fn a_from_filter_delivers_only_that_senders_messages() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    let critic = store.start_run("critic", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "found it")
        .unwrap();
    store
        .send_message(critic, me, "critic", 1, "it is wrong")
        .unwrap();
    store
        .send_message(scout, me, "scout", 2, "and again")
        .unwrap();

    let from_scout = store.read_messages(me, Some("scout")).unwrap();
    assert_eq!(
        from_scout
            .iter()
            .map(|m| m.body.as_str())
            .collect::<Vec<_>>(),
        vec!["found it", "and again"]
    );
    let rest = store.read_messages(me, None).unwrap();
    assert_eq!(rest.len(), 1, "the critic's was left where it was");
    assert_eq!(rest[0].from_name, "critic");
}

/// **N3 — a deleted session leaves no message at either end.**
///
/// The table is in `RUN_TABLES` keyed by the recipient, and the argument that the
/// sender end is covered too — a mailbox lives inside one tree, and a session's run
/// list is that whole tree — is an argument rather than a guarantee. So this counts
/// the table directly after the delete instead of enumerating `sqlite_master`,
/// which 0.58.0 proved cannot fail for a table the fixture never wrote to.
#[test]
fn a_deleted_session_leaves_no_message_at_either_end() {
    let (_dir, store, path) = on_disk();
    let session = store.create_session("/repo").unwrap();
    let parent = store.start_run("coordinate", "/repo").unwrap();
    store.record_turn(session, None, parent, "go").unwrap();
    let child = store.start_run("scout", "/repo").unwrap();
    store.record_turn(session, None, child, "look").unwrap();

    // Both directions, so a cascade that covered only one end still fails.
    store.send_message(child, parent, "scout", 1, "up").unwrap();
    store
        .send_message(parent, child, "root", 1, "down")
        .unwrap();
    assert_eq!(store.messages_for(parent).unwrap().len(), 1);
    assert_eq!(store.messages_for(child).unwrap().len(), 1);

    store.delete_session(session).unwrap();

    // Counted straight off the table rather than through either run id, because a
    // row orphaned by a missed cascade is exactly a row no run id reaches.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "the mailbox is accounted for by the cascade");
}

/// **N5, the measurement half — what a drain costs at 1, 100 and 1,000 unread.**
///
/// The plan assertion lives in `src/state.rs`, where the connection is reachable;
/// this is the number that goes in the record. Ignored by default because it is a
/// measurement and not an assertion, in the shape 0.47.0 and 0.48.0 already use:
/// run it with `-- --ignored --nocapture`.
///
/// On disk, never in memory: the read commits a transaction, and an in-memory
/// database would measure SQLite's page cache instead of the WAL commit an agent
/// actually pays for.
#[test]
#[ignore = "measurement, not an assertion: run with --ignored --nocapture"]
fn n5_what_a_drain_costs() {
    const SAMPLES: usize = 20;
    for unread in [1usize, 100, 1_000] {
        let mut medians = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (_dir, store, _path) = on_disk();
            let me = store.start_run("coordinate", "/repo").unwrap();
            let scout = store.start_run("scout", "/repo").unwrap();
            for i in 0..unread {
                store
                    .send_message(
                        scout,
                        me,
                        "scout",
                        i as u32 + 1,
                        "a finding of ordinary length",
                    )
                    .unwrap();
            }
            let t = std::time::Instant::now();
            let got = store.read_messages(me, None).unwrap();
            medians.push(t.elapsed().as_secs_f64() * 1_000.0);
            assert_eq!(got.len(), unread, "the measurement read what it seeded");
        }
        medians.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "mailbox drain unread={unread} median_ms={:.3}",
            medians[SAMPLES / 2]
        );
    }
}

/// A body is text and nothing more, so it survives being text: newlines, quotes and
/// non-ASCII come back byte for byte. Cheap, and it is the column an embedder will
/// put a JSON document in on the first day.
#[test]
fn a_body_survives_being_arbitrary_text() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    let body = "line one\nline \"two\"\n\tthird — ünïcode 漢字\n{\"json\": [1, 2]}";
    store.send_message(scout, me, "scout", 1, body).unwrap();
    assert_eq!(store.read_messages(me, None).unwrap()[0].body, body);
}
