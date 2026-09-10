//! The prefix is append-only within a run (0.85.0).
//!
//! Prefix caching serves a request only up to the first byte that differs from
//! the request before it, so anything this crate rewrites *before* the newest
//! message throws away the cache from that point on for the rest of the run.
//! Through 0.84.0 six things in assembly did exactly that. These tests assert the
//! property directly — step N's rendered text is a byte prefix of step N+1's,
//! except across a fold — rather than asserting the absence of any one of them,
//! because the property is what the vendor actually charges on.
//!
//! The assertions are on `Assembled::text` and on recorded request bodies, not on
//! a counter: a counter can be right while the bytes move.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::context::{
    assemble, Assembly, Collapse, ContextBudget, Ladder, Ledger, ObsKind, Observation, Origin,
};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::Workspace;
use io_harness::{
    run_with, run_with_observed, ApproveAll, EventKind, Flow, Observer, Policy, PrefixBreak,
    Provider, RunEvent, Store, TaskContract, Verification,
};
use serde_json::json;

/// A workspace with an open read policy, and the policy beside it.
fn ws(dir: &std::path::Path) -> (Workspace, Policy) {
    let policy = Policy::default().layer("test").allow_read("*");
    (Workspace::with_policy(dir, policy.clone()), policy)
}

/// One assembly at `step`, over a ledger the call may extend, with the prefix
/// treated as having been built at `since`.
async fn at_since(
    ledger: &mut Ledger,
    store: &Store,
    workspace: &Workspace,
    policy: &Policy,
    step: u32,
    since: u32,
) -> String {
    assemble(
        ledger,
        24_000,
        &[],
        &[],
        Assembly {
            ws: Some(workspace),
            policy,
            store,
            run_id: 1,
            step,
            collapse: Collapse::default(),
            ladder: Ladder::default(),
            since,
            // A fold is the step that rebuilt the prefix, and it is the only step
            // allowed to elide what it has already shown.
            folding: since == step,
        },
    )
    .await
    .unwrap()
    .text
}

/// One assembly at `step`, over a ledger the call may extend.
async fn at_step(
    ledger: &mut Ledger,
    store: &Store,
    workspace: &Workspace,
    policy: &Policy,
    step: u32,
) -> String {
    assemble(
        ledger,
        24_000,
        &[],
        &[],
        Assembly {
            ws: Some(workspace),
            policy,
            store,
            run_id: 1,
            step,
            collapse: Collapse::default(),
            ladder: Ladder::default(),
            // The ordinary step of a run that has not folded: the prefix was built
            // at the run's first step, and nothing this step renders may change
            // from what the step before it rendered.
            since: 1,
            folding: false,
        },
    )
    .await
    .unwrap()
    .text
}

// ------------------------------------------------------------ the loop harness

/// Plays a fixed script of tool calls and keeps every request it was sent. The
/// requests are the whole of what this file asserts on: the property is about what
/// a vendor receives, and nothing else can see it.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request the run made, the fold's own summarisation call included.
    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// The steps' own requests.
    ///
    /// A fold buys its summary through the same provider, and (0.85.0) it does so
    /// by *extending the step's own request* — so it carries the workspace framing
    /// too and the frame alone no longer tells them apart. The instruction does.
    /// Reading a fold as a step would compare a step's prompt against a
    /// summariser's and call the difference a broken prefix.
    fn steps(&self) -> Vec<CompletionRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.user.contains(FRAME) && !r.user.contains(SUMMARISER))
            .collect()
    }
}

impl Provider for Script {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        // A fold's summarisation request carries no tools, and it must be answered
        // with prose: a summariser that says nothing is not allowed to replace the
        // notes with nothing, so an empty answer aborts the fold and the run
        // silently goes on never folding. It also takes no slot in the script —
        // the script is the agent's turns, and this is not one of them.
        if req.user.contains(SUMMARISER) {
            self.seen.lock().unwrap().push(req);
            return Ok(CompletionResponse {
                text: Some("the run read some files and grepped for some names".into()),
                ..Default::default()
            });
        }
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// What a run said about its own prefix: the steps it folded on, which are the
/// steps allowed to break the property, and the breaks it announced.
#[derive(Default)]
struct Watched {
    folds: Arc<Mutex<Vec<u32>>>,
    breaks: Arc<Mutex<Vec<(u32, PrefixBreak)>>>,
}

impl Observer for Watched {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::Compacted { through_step, .. } => {
                self.folds.lock().unwrap().push(*through_step);
            }
            EventKind::PrefixBroke { step, reason, .. } => {
                self.breaks.lock().unwrap().push((*step, *reason));
            }
            _ => {}
        }
        Flow::Continue
    }
}

impl Watched {
    /// The fold steps as indices into the step requests, which are one-based
    /// steps in order.
    fn indices(&self) -> Vec<usize> {
        self.folds
            .lock()
            .unwrap()
            .iter()
            .map(|s| *s as usize - 1)
            .collect()
    }

    fn breaks(&self) -> Vec<(u32, PrefixBreak)> {
        self.breaks.lock().unwrap().clone()
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
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

/// A contract that can never be satisfied, so the loop runs its whole step budget.
fn never_passes(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace("exercise the assembler", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

/// The prompt's three parts. `user` is `head + section + tail`, and the section is
/// what assembly produced — so the property is asserted over the section, with the
/// framing either side asserted equal rather than assumed to be.
fn split(user: &str) -> (String, String, String) {
    let head = FRAME;
    let from = user.find(head).expect("the workspace prompt frame") + head.len();
    let rest = &user[from..];
    // The tail starts at whichever comes first: the withheld sentence, when a mask
    // is on, or the closing instruction. Cutting only at the instruction would put
    // the withheld sentence inside the section and make a toggled mask read as a
    // rewritten log — which is the opposite of what it is.
    let to = ["\n\nUnavailable this turn", "\n\nCall a tool"]
        .iter()
        .filter_map(|mark| rest.find(mark))
        .min()
        .unwrap_or(rest.len());
    (
        user[..from].to_string(),
        rest[..to].to_string(),
        rest[to..].to_string(),
    )
}

/// The line a workspace prompt puts above the observation section.
const FRAME: &str = "Observations so far (results of your tool calls):\n";

/// How the fold's own call is told apart from a step's.
const SUMMARISER: &str = "compacting an agent's own working notes";

/// What the section says when the run has observed nothing yet.
const EMPTY_LOG: &str = "(nothing yet — start by grepping or finding)";

/// The memory block, or the empty string when the turn carried none.
fn memory_block(section: &str) -> String {
    let Some(from) = section.find("\n[memory]") else {
        return String::new();
    };
    let rest = &section[from + 1..];
    let to = rest.find("\n\n[").unwrap_or(rest.len());
    rest[..to].to_string()
}

/// Every step's prompt is the step before it plus new bytes at the end.
///
/// The transcript is a pure function of the system string, the framing either side
/// of the observation section, the section itself and the turns so far — so a
/// system string that did not change, framing that did not change and a section
/// that only grew at the end is a vendor-side prefix that was not disturbed. Each
/// of the three is asserted rather than argued.
fn assert_append_only(requests: &[CompletionRequest], except: &[usize]) {
    for i in 1..requests.len() {
        if except.contains(&i) {
            continue;
        }
        let (before, after) = (&requests[i - 1], &requests[i]);
        assert_eq!(
            before.system, after.system,
            "step {i} changed the system prompt, which is the head of every prefix"
        );
        let (head_a, section_a, tail_a) = split(&before.user);
        let (head_b, section_b, tail_b) = split(&after.user);
        assert_eq!(head_a, head_b, "step {i} changed the framing above the log");
        assert_eq!(tail_a, tail_b, "step {i} changed the framing below the log");
        // The first step's section is the placeholder that stands in for an empty
        // log, and the second step replaces it rather than extending it. That is
        // one transition per run, at the point where there is no prefix to keep,
        // and it is exempted by matching the placeholder exactly rather than by
        // exempting the first step — an exemption that reads "index 1" would go on
        // covering step 2 the day something else moved into that position.
        if section_a == EMPTY_LOG {
            continue;
        }
        assert!(
            section_b.starts_with(&section_a),
            "step {i} rewrote what step {} was shown.\nbefore:\n{section_a}\nafter:\n{section_b}",
            i - 1
        );
    }
}

// ------------------------------------------------------- F1: a re-read appends

/// F1 — a stale read is refreshed by *appending* the current contents at the
/// tail, and the entry that went stale is left exactly as it was.
///
/// Through 0.84.0 the refresh was written in place with the assembling step's own
/// number baked into it (`re-read at step {step}`), so once a file had been read
/// and then written every later step rendered that observation differently. It
/// sat early in the transcript and it changed on every step, which made it the
/// worst of the six.
///
/// Replacing it with a *stub* was the first version of this release and is wrong
/// for the same reason: a stub is a rewrite of bytes the model has already been
/// shown, and it costs the cache from that byte on exactly as the old text did.
/// The stub is what the next fold does with the entry. Until then both copies
/// stand, and the appended one says which is current.
#[tokio::test]
async fn f1_a_stale_read_is_appended_at_the_tail_and_the_original_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "NEW-CONTENT\n").unwrap();
    let (workspace, policy) = ws(dir.path());
    let store = Store::memory().unwrap();

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        2,
        ObsKind::Read,
        Some("a.rs".into()),
        "\n[read a.rs]\nOLD-CONTENT\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        4,
        ObsKind::Write,
        Some("a.rs".into()),
        "\n[wrote a.rs] (12 chars)\n",
        Origin::File,
    ));

    let five = at_step(&mut ledger, &store, &workspace, &policy, 5).await;
    assert!(
        five.contains("re-read at step 5") && five.contains("NEW-CONTENT"),
        "the refreshed contents must arrive as a new entry naming the step that read them, \
         got:\n{five}"
    );
    assert!(
        five.contains("these are the current contents and that one is stale"),
        "the refresh must mark the copy above it stale, got:\n{five}"
    );

    // The stale entry's own line, which is what the next three steps must repeat
    // byte for byte — unchanged, not restated.
    let stale = "\n[read a.rs]\nOLD-CONTENT\n";
    assert!(
        five.contains(stale),
        "the entry the model has already been shown is left alone, got:\n{five}"
    );

    let mut previous = five;
    for step in 6..=8 {
        let text = at_step(&mut ledger, &store, &workspace, &policy, step).await;
        assert!(
            text.contains(stale),
            "step {step} rewrote the stale entry instead of leaving it:\n{text}"
        );
        assert!(
            text.contains("re-read at step 5")
                && !text.contains(&format!("re-read at step {step}")),
            "the refresh belongs to the step that made it, not to step {step}, got:\n{text}"
        );
        assert_eq!(
            text, previous,
            "nothing in the assembly may change between steps that observe nothing"
        );
        previous = text;
    }
}

// ------------------------------------------------- F2: the memory block is held

/// F2 — the notes render once and then stand still. A note the run writes about
/// its own work is an observation, not a rewrite of the block above everything.
///
/// The block is re-read from the store every turn by design, and it renders ahead
/// of the observations, so through 0.84.0 one `remember` call moved the earliest
/// user text in the prompt and cost the cache from the first byte on.
#[tokio::test]
async fn f2_a_note_written_mid_run_does_not_move_the_memory_block() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let key = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // One note from an earlier run, so there is a block to hold still.
    store
        .memory_put(&key, "earlier", "the parser rejects a trailing comma", 1, 1)
        .unwrap();

    // A different file each step: repeating one call is what the stall policy is
    // for, and a run that stalls never reaches the steps this case is about.
    for i in 0..8 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("file {i}\n")).unwrap();
    }
    let script = Script::new(
        (0..8)
            .map(|i| match i {
                2 => vec![call(
                    "remember",
                    json!({ "key": "midrun", "value": "this run learned something" }),
                )],
                _ => vec![call("read_file", json!({ "path": format!("f{i}.txt") }))],
            })
            .collect(),
    );
    run_with(
        &never_passes(dir.path(), 8),
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let requests = script.steps();
    assert!(requests.len() >= 8, "the fixture must reach step 8");
    let at_two = memory_block(&split(&requests[1].user).1);
    assert!(
        at_two.contains("earlier"),
        "the fixture needs a memory block to hold still, got:\n{at_two}"
    );
    for (i, request) in requests.iter().enumerate().skip(3).take(5) {
        assert_eq!(
            memory_block(&split(&request.user).1),
            at_two,
            "step {} rendered a different memory block from step 2",
            i + 1
        );
    }
    assert!(
        !split(&requests[7].user).1.contains("this run learned"),
        "a note written mid-run belongs to the run's observations, not to the block \
         above them"
    );
    assert_append_only(&requests, &[]);
}

// ------------------------------------------------ F3: the run says so out loud

/// F3 — a ten-step run with one fold emits no `PrefixBroke` on either side of it,
/// and every step's prompt extends the one before it within each range.
///
/// The property and the event are asserted together on purpose. The event alone
/// could be silenced by a check that never fires, and the byte comparison alone
/// would hold with nothing reporting a break that did happen — so the case asserts
/// the bytes directly and then asserts that the run agreed with them.
#[tokio::test]
async fn f3_a_ten_step_run_with_one_fold_breaks_its_prefix_nowhere_else() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(90)),
        )
        .unwrap();
    }
    let script = Script::new(
        (0..10)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i}.txt") }))])
            .collect(),
    );
    // Sized so the ledger crosses the fold threshold about two thirds of the way
    // in and lands back well under it: one fold, which is the case the contract
    // names. `keep_recent` of 4 lets the fold happen before the ceiling forces the
    // fit rule to elide, which is the regime the whole release is about.
    let contract = never_passes(dir.path(), 10)
        .with_context_budget(ContextBudget {
            max_tokens: 2_400,
            share: 0.5,
        })
        .with_compaction(io_harness::Compaction {
            at_share: 0.8,
            keep_recent: 4,
        });
    let store = Store::memory().unwrap();
    let watched = Watched::default();
    run_with_observed(
        &contract,
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
        &watched,
    )
    .await
    .unwrap();

    let requests = script.steps();
    assert_eq!(requests.len(), 10, "the fixture must reach step 10");
    let folded = watched.indices();
    assert_eq!(
        folded.len(),
        1,
        "one fold, or the case is not the one the contract names: {folded:?}"
    );
    assert_append_only(&requests, &folded);

    // And the run reported exactly what the bytes say: nothing.
    let announced = watched.breaks();
    assert!(
        announced.is_empty(),
        "the run announced a break its own prompts do not have: {announced:?}"
    );
}

/// F1 (the fold half) — once a fold spends the invalidation, the stub it leaves
/// behind is the same stub on every step after it.
///
/// The elision waits for a fold, and the stub it writes then is the one the model
/// reads for the rest of the run. A stub that named the *assembling* step would
/// be a rewrite of the head on every step, which is the defect this release
/// exists to remove — in the shape it took before 0.85.0, and in the shape it
/// would take again if the elision were written from the wrong step number.
///
/// Between folds there is no stub at all, so nothing here is covered by the case
/// above: that one asserts what an ordinary step does, and this one asserts what
/// survives the step that is allowed to change things.
#[tokio::test]
async fn f1_the_stub_a_fold_leaves_behind_does_not_move_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "NEW-CONTENT\n").unwrap();
    let (workspace, policy) = ws(dir.path());
    let store = Store::memory().unwrap();

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        2,
        ObsKind::Read,
        Some("a.rs".into()),
        "\n[read a.rs]\nOLD-CONTENT\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        4,
        ObsKind::Write,
        Some("a.rs".into()),
        "\n[wrote a.rs] (12 chars)\n",
        Origin::File,
    ));

    // Step 5 appends the refresh; nothing is elided yet.
    let five = at_since(&mut ledger, &store, &workspace, &policy, 5, 1).await;
    assert!(
        five.contains("OLD-CONTENT"),
        "between folds the stale entry keeps its bytes:\n{five}"
    );

    // Step 7 folds, which is where the invalidation is spent.
    let seven = at_since(&mut ledger, &store, &workspace, &policy, 7, 7).await;
    let stub = seven
        .lines()
        .find(|line| line.contains("[read a.rs] (elided:"))
        .unwrap_or_else(|| panic!("the fold must spend the invalidation:\n{seven}"))
        .to_string();
    assert!(
        stub.contains("invalidated by the write at step 4"),
        "the stub says why the entry went stale: {stub}"
    );

    // And every step after it repeats that stub byte for byte.
    for step in 8..=10 {
        let text = at_since(&mut ledger, &store, &workspace, &policy, step, 7).await;
        assert!(
            text.contains(&stub),
            "step {step} rewrote the stub the fold at step 7 wrote.\nwanted: {stub}\ngot:\n{text}"
        );
    }
}

// --------------------------------------------------- F10: masking stays in the tail

/// F10 — turning a tool mask on changes nothing above the newest message.
///
/// The system text and the tool catalogue are the head of the prefix on every chat
/// template, and on some the catalogue renders *before* the system text — so a
/// mask written into either would cost the whole prompt every time it was toggled.
/// It is a sentence in the user block, after the observations, and the catalogue
/// the mask applies to is sent unchanged: a withheld tool is refused when called,
/// not hidden.
#[tokio::test]
async fn f10_a_tool_mask_moves_nothing_above_the_newest_message() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "content\n").unwrap();

    let run = async |mask: io_harness::ToolMask| {
        let script = Script::new(vec![vec![call("read_file", json!({ "path": "a.txt" }))]]);
        let store = Store::memory().unwrap();
        run_with(
            &never_passes(dir.path(), 2).with_tool_mask(mask),
            &script,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        script.steps()
    };

    let plain = run(io_harness::ToolMask::none()).await;
    let masked = run(io_harness::ToolMask::withholding(["write_file"])).await;

    assert_eq!(
        plain[0].system, masked[0].system,
        "a mask must not touch the system prompt"
    );
    assert_eq!(
        plain[0].tools, masked[0].tools,
        "nor the catalogue: a withheld tool is refused when called, not hidden"
    );
    assert!(
        masked[0].user.contains("Unavailable this turn"),
        "and the turn is still told, in its newest message:\n{}",
        masked[0].user
    );
    let (_, section, _) = split(&masked[0].user);
    assert!(
        !section.contains("Unavailable this turn"),
        "the sentence sits after the observation section, past every breakpoint \
         this crate marks:\n{section}"
    );
}

// ------------------------------------------ F9: the fold reuses what it folds

/// F9 — the fold's own request is the step's request with one message added.
///
/// A fold is the moment a run can least afford a second full prefill of its own
/// conversation, and buying a summary with a request of its own is exactly that:
/// the same transcript, sent again, to a vendor that has already been paid to
/// hold it. Sending the step's own system, tools and messages with the
/// instruction appended means every byte before the instruction is served from
/// the cache entry the run is already keeping warm.
#[tokio::test]
async fn f9_the_folds_own_request_extends_the_step_before_it() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(90)),
        )
        .unwrap();
    }
    let script = Script::new(
        (0..10)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i}.txt") }))])
            .collect(),
    );
    let contract = never_passes(dir.path(), 10)
        .with_context_budget(ContextBudget {
            max_tokens: 2_400,
            share: 0.5,
        })
        .with_compaction(io_harness::Compaction {
            at_share: 0.8,
            keep_recent: 4,
        });
    let store = Store::memory().unwrap();
    run_with(&contract, &script, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let all = script.requests();
    let at = all
        .iter()
        .position(|r| r.user.contains(SUMMARISER))
        .expect("the fixture must fold, or there is no fold request to look at");
    let fold = &all[at];
    let before = &all[at - 1];

    assert_eq!(
        fold.system, before.system,
        "the fold sends the step's system prompt, which is the head of the prefix"
    );
    assert_eq!(
        fold.tools, before.tools,
        "and its tool list: on some chat templates the tools render before the \
         system text, so a fold with an empty catalogue shares no prefix at all"
    );
    assert_eq!(
        fold.messages.len(),
        before.messages.len() + 1,
        "one message added and none rewritten"
    );
    assert_eq!(
        fold.messages[..before.messages.len()],
        before.messages[..],
        "every message the step sent, byte for byte"
    );
    assert!(
        fold.user.starts_with(&before.user),
        "and the flat rendering extends the step's, which is the invariant the \
         transcript is built on"
    );
    assert_eq!(
        fold.session_key, before.session_key,
        "the fold asks the replica holding the prefix it is reusing"
    );
}

/// F3 (the positive half) — a run that cannot hold its ceiling any other way
/// elides anyway, and says so.
///
/// The case above asserts the event is *not* emitted, which a dead emitter would
/// satisfy — and a sabotage arm that disabled the whole check killed nothing,
/// which is how that was found. This is the arm that needs the machinery to work:
/// a ceiling too tight for `keep_recent` to fold into leaves the fit rule as the
/// only thing holding the prompt down, so entries the model has already been shown
/// are elided, the prefix really does move, and the run reports it by cause.
///
/// It is the floor under the property rather than a hole in it. What must not
/// happen is this going unreported, because an operator whose run is quietly
/// paying full price on every step has no other way to find out.
#[tokio::test]
async fn f3_a_ceiling_too_tight_to_fold_into_reports_every_break_it_causes() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..8 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(120)),
        )
        .unwrap();
    }
    let script = Script::new(
        (0..8)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i}.txt") }))])
            .collect(),
    );
    // `keep_recent` of 32 is more entries than this run will ever have, so
    // `compact_ledger` can never fold and the ceiling has nothing but the fit rule.
    let contract = never_passes(dir.path(), 8)
        .with_context_budget(ContextBudget {
            max_tokens: 1_400,
            share: 0.5,
        })
        .with_compaction(io_harness::Compaction {
            at_share: 0.8,
            keep_recent: 32,
        });
    let store = Store::memory().unwrap();
    let watched = Watched::default();
    run_with_observed(
        &contract,
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
        &watched,
    )
    .await
    .unwrap();

    assert!(
        watched.indices().is_empty(),
        "the fixture must not fold, or the ceiling is not what is eliding"
    );
    let announced = watched.breaks();
    assert!(
        !announced.is_empty(),
        "the run elided under its ceiling and reported nothing"
    );
    assert!(
        announced
            .iter()
            .all(|(_, reason)| matches!(reason, PrefixBreak::Stub | PrefixBreak::Other)),
        "a break the ceiling caused is attributed to the elision that caused it: \
         {announced:?}"
    );
}

// ------------------------------------------- F8: the budget is held between folds

/// F8 — a `max_tokens` run's assembly budget does not shrink under it.
///
/// `effective_tokens` is computed from what the run has left, so through 0.84.0
/// every step assembled against a smaller ceiling than the one before it and the
/// fit rule stubbed one more entry at a time — a rewrite of the head with no fold
/// anywhere near it.
#[tokio::test]
async fn f8_a_token_budgeted_run_does_not_stub_its_way_down_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..8 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(30)),
        )
        .unwrap();
    }
    let script = Script::new(
        (0..8)
            .map(|i| vec![call("read_file", json!({ "path": format!("f{i}.txt") }))])
            .collect(),
    );
    // Compaction is left at its default. `keep_recent` is 8, so eight observations
    // cannot fold and the run reaches step 8 without one — the property is being
    // asserted over ordinary steps rather than over the one step allowed to break
    // it. The token budget is what makes `effective_tokens` shrink under the
    // assembler, which is the whole point of the case.
    let contract = never_passes(dir.path(), 8)
        .with_token_budget(200_000)
        .with_context_budget(ContextBudget {
            max_tokens: 8_000,
            share: 0.5,
        });
    let store = Store::memory().unwrap();
    run_with(&contract, &script, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let requests = script.steps();
    assert!(requests.len() >= 8, "the fixture must reach step 8");
    assert!(
        !split(&requests[7].user)
            .1
            .contains("older than the current context window"),
        "eight reads fit this ceiling, so nothing may have been elided:\n{}",
        split(&requests[7].user).1
    );
    assert_append_only(&requests, &[]);
}

// ------------------------------------------------ F4: the rungs wait for a fold

/// F4 — with the ladder rungs on, an entry is untouched until the fold and
/// rewritten only there. The rungs keep their meaning; what changed is what they
/// judge an entry's age against.
///
/// A rung is a rewrite of the middle of the prompt, and a rewrite is free exactly
/// once: at the fold, where the prefix is being thrown away anyway. Gating the
/// rungs on the fold *step* is not enough and was tried first — a rung that fires
/// at the fold and lapses on the next step rewrites the prompt twice, once to
/// elide the entry and once to bring it back. Judged against the step the prefix
/// was built at, a rung's output is the same on every step between two folds.
#[tokio::test]
async fn f4_the_ladder_rungs_do_not_rewrite_an_entry_before_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..14 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(120)),
        )
        .unwrap();
    }
    // A lookup per step, which is the kind `snip` drops, beside a read that
    // carries enough bytes for the ledger to cross the fold threshold once
    // `keep_recent` stops holding it back.
    let script = Script::new(
        (0..14)
            .map(|i| {
                vec![
                    call("grep", json!({ "pattern": format!("fn{i}"), "path": "." })),
                    call("read_file", json!({ "path": format!("f{i}.txt") })),
                ]
            })
            .collect(),
    );
    // `keep_recent` is what decides whether the fold gets there first. At the
    // default of 8 this fixture crosses the assembly ceiling before it has enough
    // entries to fold, and a run that cannot fold has nothing but the fit rule to
    // hold its ceiling with — which is a different case, and the one
    // `tests/context.rs` already owns.
    let contract = never_passes(dir.path(), 14)
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        })
        // Six kept entries is three of this script's steps, so the fold leaves
        // something older than `snip`'s two-step grace for the rung to act on. Keep
        // fewer and the fold itself removes every site the rung would have had.
        .with_compaction(io_harness::Compaction {
            at_share: 0.8,
            keep_recent: 6,
        })
        .with_ladder(Ladder {
            reduce: true,
            snip: Some(io_harness::context::Snip {
                older_than_steps: 2,
            }),
            microcompact: true,
            ..Default::default()
        });
    let store = Store::memory().unwrap();
    let folds = Watched::default();
    run_with_observed(
        &contract,
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
        &folds,
    )
    .await
    .unwrap();

    let requests = script.steps();
    let sections: Vec<String> = requests.iter().map(|r| split(&r.user).1).collect();
    let folded_at = sections
        .iter()
        .position(|s| s.contains("[earlier work, summarised]"))
        .unwrap_or_else(|| {
            panic!(
                "the fixture must fold, or it is not testing when a rung runs — {} step(s), last \
                 section {} chars:\n{}",
                sections.len(),
                sections.last().map(|s| s.len()).unwrap_or(0),
                sections.last().cloned().unwrap_or_default()
            )
        });
    for (i, section) in sections.iter().enumerate().take(folded_at) {
        assert!(
            !section.contains("dropped as a lookup"),
            "step {} ran a rung before the fold at step {folded_at}:\n{section}",
            i + 1
        );
    }
    assert!(
        sections[folded_at].contains("dropped as a lookup"),
        "the rung must still run, and the fold is where it runs:\n{}",
        sections[folded_at]
    );
    for (i, section) in sections.iter().enumerate().skip(folded_at + 1) {
        assert!(
            section.contains("dropped as a lookup"),
            "step {} un-dropped what the fold at step {folded_at} dropped, which is a \
             rewrite in the other direction:\n{section}",
            i + 1
        );
    }
    // A fold is the one deliberate break, and this fixture is tight enough to make
    // several. They come from the run's own `Compacted` events rather than from
    // reading the prompts: a step excepted because its prompt changed would except
    // exactly the failure this is looking for.
    let folded = folds.indices();
    assert!(
        folded.contains(&folded_at),
        "the fold this read out of the prompts must be one the run announced: \
         {folded:?} against {folded_at}"
    );
    assert_append_only(&requests, &folded);
}

/// F4 — a run that never folds still runs its rungs.
///
/// The other half of the case above, and the one 0.85.0 nearly shipped broken.
/// Every rung judges an entry's age against the step the frozen prefix was built
/// at, and that anchor advances at a fold. A run with `Compaction { at_share: 1.0 }`
/// has no folds, so an anchor that only moved at one would sit at the run's first
/// step forever: nothing is ever older than it, `snip`, the skill bodies, the
/// microcompact and `reduce` never fire again, and 0.42.0's ladder — the only thing
/// holding such a run's prompt down — quietly does nothing for the whole run.
///
/// There is no prefix to protect here, which is why the anchor may move: a run that
/// cannot fold re-decides how it renders every step by construction, so this case
/// asserts the rung fires and deliberately does not assert append-only.
#[tokio::test]
async fn f4_a_run_that_never_folds_still_runs_its_rungs() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n{}", "filler line\n".repeat(60)),
        )
        .unwrap();
    }
    let script = Script::new(
        (0..10)
            .map(|i| {
                vec![
                    call("grep", json!({ "pattern": format!("fn{i}"), "path": "." })),
                    call("read_file", json!({ "path": format!("f{i}.txt") })),
                ]
            })
            .collect(),
    );
    let contract = never_passes(dir.path(), 10)
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        })
        // 0.42.0's behaviour: never fold, whatever the ledger costs.
        .with_compaction(io_harness::Compaction {
            at_share: 1.0,
            keep_recent: 6,
        })
        .with_ladder(Ladder {
            reduce: true,
            snip: Some(io_harness::context::Snip {
                older_than_steps: 2,
            }),
            microcompact: true,
            ..Default::default()
        });
    let store = Store::memory().unwrap();
    let folds = Watched::default();
    run_with_observed(
        &contract,
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
        &folds,
    )
    .await
    .unwrap();

    let sections: Vec<String> = script.steps().iter().map(|r| split(&r.user).1).collect();
    assert!(
        sections.len() >= 5,
        "the fixture must take enough steps for an entry to age past the rung's \
         grace, got {}",
        sections.len()
    );
    assert!(
        folds.indices().is_empty(),
        "this contract must never fold, or the case says nothing about a run \
         without folds: {:?}",
        folds.indices()
    );
    assert!(
        sections
            .iter()
            .any(|s| s.contains("dropped as a lookup") || s.contains("older than the current")),
        "no rung fired in {} steps of a run that never folds — the ladder is dead \
         for the whole of it. Last section:\n{}",
        sections.len(),
        sections.last().cloned().unwrap_or_default()
    );
}
