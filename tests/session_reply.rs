//! A turn that answers without opening a run (0.37.0).
//!
//! The claim this file has to make true is not "the reply is right" — an
//! implementation that classified perfectly and still drove the loop would pass
//! that and change nothing. It is that a turn which needed no tool leaves *no work
//! behind*: no step row, no gate attempt, no checkpoint, no snapshot, no plan gate
//! and no call to the approver, while the same entry point given the same words
//! followed by work runs exactly as it did in 0.36.1.
//!
//! Every assertion about an absence is paired with the same assertion in the
//! positive on the same fixture, because a count of zero is worth nothing until
//! something has made it non-zero.
//!
//! The other half is honesty. A reply is a provider call that cost money, so it is
//! billed, drawn and readable in the trace. A "reply" that recorded nothing would
//! satisfy every assertion about what is absent, and is the failure most of this
//! file exists to catch.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::approve::DecisionFuture;
use io_harness::provider::{CompletionRequest, CompletionResponse, Message, ToolCall, Usage};
use io_harness::tools::Workspace;
use io_harness::{
    rewind_run, ApproveAll, Approver, EventKind, Flow, Observer, Policy, Provider, Request,
    RunEvent, RunOutcome, Session, Store, TaskContract, TurnKind, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one call.
#[derive(Clone)]
enum Say {
    /// Answer with text and no tool call.
    Text(&'static str),
    /// Make one tool call, so the turn is work.
    Call(ToolCall),
}

/// Plays a script and counts the completions it served.
///
/// The count is the discriminating assertion of this file: the implementation
/// this release most has to forbid — classify with a cheap call, throw it away,
/// ask again — produces perfectly correct `TurnKind` values throughout and costs
/// two completions where one was needed.
struct Mock {
    script: Vec<Say>,
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl Mock {
    fn new(script: Vec<Say>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// How many completions this provider was asked for.
    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }

    /// The system prompt of the nth completion it served.
    fn system(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].system.clone()
    }

    /// The user prompt of the nth completion it served.
    fn user(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].user.clone()
    }

    /// The transcript of the `n`th request (0.49.0).
    fn messages(&self, n: usize) -> Vec<Message> {
        self.seen.lock().unwrap()[n].messages.clone()
    }
}

impl Provider for Mock {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // Past the end of the script the model stops talking, which ends a run
        // rather than hanging the test on the step cap.
        let say = self.script.get(i).cloned().unwrap_or(Say::Text("done"));
        Ok(CompletionResponse {
            text: match &say {
                Say::Text(t) => Some((*t).to_string()),
                Say::Call(_) => None,
            },
            tool_calls: match &say {
                Say::Call(c) => vec![c.clone()],
                Say::Text(_) => Vec::new(),
            },
            usage: Some(Usage {
                total_tokens: 7,
                ..Default::default()
            }),
            model: Some("mock-1".into()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// A provider whose completion never returns.
///
/// The turn is driven under a timeout and its future is then dropped, which
/// leaves the run row `running` with no step ever committed — a process killed
/// mid-answer, without spawning one.
///
/// A provider that returns `Err` is deliberately *not* the fixture here: an error
/// is an escalation, and the loop records a step saying so. A run that got to
/// explain itself is not the shape this criterion is about.
struct Hangs;

impl Provider for Hangs {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        std::future::pending().await
    }

    fn name(&self) -> &str {
        "hangs"
    }
}

/// An approver that panics if it is reached at all.
///
/// Panicking rather than recording: a soft assertion on a call that should never
/// happen is a test that passes when the call is made once and swallowed.
struct NeverAsk;

impl Approver for NeverAsk {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        panic!(
            "a reply reached the approver, so the boundary machinery was staged for a turn \
             that did no work"
        );
    }
}

/// Collects the events a turn emitted, so `Answered` can be asserted on the wire
/// an attached process actually reads.
#[derive(Default)]
struct Seen(Mutex<Vec<EventKind>>);

impl Observer for Seen {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.kind.clone());
        Flow::Continue
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "# notes\n").unwrap();
    dir
}

/// A boundary wide enough that nothing in this file is refused for the wrong
/// reason.
fn policy() -> Policy {
    Policy::default()
        .layer("reply-test")
        .allow_read("*")
        .allow_write("*")
}

/// A boundary that asks about every act it can be asked about, so a turn that
/// staged any work at all would reach the approver.
fn asks_about_everything() -> Policy {
    Policy::default()
        .layer("ask-about-everything")
        .allow_read("*")
        .ask_write("*")
}

/// Everything a reply must not have written, counted from the store.
struct Staged {
    steps: usize,
    gate_attempts: usize,
    spawns: usize,
    pending_approvals: usize,
}

fn staged(store: &Store, run_id: i64) -> Staged {
    Staged {
        steps: store.steps(run_id).unwrap().len(),
        gate_attempts: store.gate_attempts(run_id).unwrap().len(),
        spawns: store.agent_events(run_id).unwrap().len(),
        pending_approvals: store.unresolved_approvals(run_id).unwrap().len(),
    }
}

/// How many restore points a run left behind.
///
/// Read through [`rewind_run`], which is the public surface over the `snapshots`
/// table — the store exposes no listing of its own, and a rewind of a run that
/// took no snapshot puts nothing back, which is the same fact stated as a
/// behaviour rather than as a row count.
fn snapshots(root: &std::path::Path, store: &Store, run_id: i64) -> usize {
    rewind_run(&Workspace::new(root), store, run_id)
        .unwrap()
        .files
        .len()
}

// ------------------------------------------------------------------------ F1

/// F1 — a turn that needs no tool leaves no work behind, and the control does.
///
/// One session, one input, two scripted providers. The assertions about absence
/// are worth nothing on their own, so arm B is the control: the same fixture, the
/// same store, the same assertions, and every count that must be zero for an
/// answer must be non-zero for work.
#[tokio::test]
async fn a_turn_that_needs_no_tool_leaves_no_work_behind() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    // Arm A: the completion stops on text.
    let answers = Mock::new(vec![Say::Text("I read repositories and change them.")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let reply = session
        .turn("what can you do?", &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(reply.kind, TurnKind::Reply);
    assert_eq!(
        reply.reply.as_deref(),
        Some("I read repositories and change them."),
        "the turn did not report what the model actually said"
    );
    let a = staged(&store, reply.run_id);
    assert_eq!(a.steps, 0, "a reply committed a step");
    assert_eq!(a.gate_attempts, 0, "a reply ran a gate");
    assert_eq!(a.spawns, 0, "a reply recorded a spawn");
    assert_eq!(a.pending_approvals, 0, "a reply deferred an approval");
    assert_eq!(
        store.last_step(reply.run_id).unwrap(),
        0,
        "a reply left a checkpoint to resume from"
    );

    // Arm B, the control: the same words, a completion that reaches for a tool —
    // and one that writes, so the restore point below has something to count. Every
    // number that must be zero above has to be non-zero here, or the zeroes prove
    // nothing.
    let works = Mock::new(vec![Say::Call(ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": "notes.md", "content": "# notes\nchanged\n" }),
    })]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let run = session
        .turn("what can you do?", &works, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(run.kind, TurnKind::Run);
    let b = staged(&store, run.run_id);
    assert!(
        b.steps > 0,
        "the control staged no work either, so the assertions about absence are measuring nothing"
    );
    assert!(store.last_step(run.run_id).unwrap() > 0);
    assert!(matches!(run.outcome, RunOutcome::Finished { .. }));

    // The restore points, last: reading them puts the workspace back, so nothing
    // may run after it. The reply left none and the control left one, which is what
    // makes the zero mean something.
    assert_eq!(
        snapshots(ws.path(), &store, reply.run_id),
        0,
        "a reply took a file snapshot"
    );
    assert_eq!(
        snapshots(ws.path(), &store, run.run_id),
        1,
        "the control took no snapshot either, so the reply's zero is measuring nothing"
    );
}

// ------------------------------------------------------------------------ F2

/// F2 — exactly one completion, either way.
///
/// The named sabotage this criterion exists to forbid is classify-then-re-ask: it
/// produces identical `TurnKind` values everywhere in this file and fails here, on
/// the count alone.
#[tokio::test]
async fn a_turn_pays_for_exactly_the_completions_it_needed() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let answers = Mock::new(vec![Say::Text("nothing to do")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let reply = session
        .turn("hi", &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert_eq!(reply.kind, TurnKind::Reply);
    assert_eq!(
        answers.calls(),
        1,
        "a turn that answered asked the provider more than once"
    );

    // Two steps of work: the classifying completion IS step one, so a turn that
    // reads a file and then speaks costs two completions and not three.
    let works = Mock::new(vec![
        Say::Call(ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": "notes.md" }),
        }),
        Say::Text("the notes are empty"),
    ]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let run = session
        .turn(
            "what is in notes.md?",
            &works,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    assert_eq!(run.kind, TurnKind::Run);
    assert_eq!(
        works.calls(),
        2,
        "the classifying completion was thrown away and the model asked again"
    );
    assert_eq!(
        store.steps(run.run_id).unwrap().len(),
        2,
        "the run's first step is not the completion that classified it"
    );

    // And the prompt change is this turn's opening only. The first completion is
    // told it may answer; every later step of a promoted turn is asked the way
    // 0.36.1 asked it, so a real task cannot stop at a plan in prose on step nine.
    assert!(
        works.system(0).contains("may not be work at all"),
        "the first completion of a turn was not permitted to answer:\n{}",
        works.system(0)
    );
    assert!(
        works.system(1).contains("Do not explain; call tools."),
        "a promoted turn kept the conversational prompt past its first completion:\n{}",
        works.system(1)
    );
}

// ------------------------------------------------------------------------ F3

/// F3 — a reply is billed, and says so.
///
/// The discriminating assertion is the accounting: an implementation that
/// short-circuited before the ledger passes every assertion in F1 and makes the
/// crate's own cost reconstruction wrong.
#[tokio::test]
async fn a_reply_is_billed_and_the_trace_says_what_it_cost() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let answers = Mock::new(vec![Say::Text("nothing to do")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let reply = session
        .turn("hi", &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert_eq!(reply.kind, TurnKind::Reply);

    let summary = store
        .run_summary(reply.run_id)
        .unwrap()
        .expect("a turn that was answered has finished, so it has a summary");
    assert_eq!(
        summary.tokens, 7,
        "the reply's tokens are missing from the run summary, so a session's cost \
         cannot be reconstructed from the store"
    );
    assert_eq!(summary.steps, 0, "a reply reported a step in its summary");

    // The per-call accounting row, with what served it and how long it took.
    let calls = store.provider_calls(reply.run_id).unwrap();
    assert_eq!(calls.len(), 1, "a reply wrote no per-call accounting row");
    assert_eq!(calls[0].provider, "mock");
    assert_eq!(calls[0].model.as_deref(), Some("mock-1"));
    assert_eq!(calls[0].usage.map(|u| u.total_tokens), Some(7));

    // The control: a ceiling below what the reply costs. The answer is refused
    // rather than served free — a reply is bounded by the same budget as work.
    let answers = Mock::new(vec![Say::Text("nothing to do")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let contract = TaskContract::workspace("hi", ws.path()).with_token_budget(1);
    let refused = session
        .turn_bounded(&contract, &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(refused.outcome, RunOutcome::CostBudgetExceeded { .. }),
        "a reply was served under a ceiling it could not afford: {:?}",
        refused.outcome
    );
    assert_eq!(
        store.spent_tokens(refused.run_id).unwrap(),
        7,
        "the refused reply's completion was not billed, so the ceiling was enforced \
         against a number the store does not have"
    );
}

// ------------------------------------------------------------------------ F4

/// F4 — a reply is part of the conversation.
///
/// The sabotage this catches omits the reply from `finish_turn`: it passes F1 and
/// F3 and leaves the next turn reading a conversation with a hole in it.
#[tokio::test]
async fn a_reply_is_read_by_the_turn_that_follows_it() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let provider = Mock::new(vec![
        Say::Text("I read repositories and change them."),
        Say::Text("still here"),
    ]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let first = session
        .turn(
            "what can you do?",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    assert_eq!(first.kind, TurnKind::Reply);

    session
        .turn("and now?", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(session.history(&store).unwrap().len(), 2);
    let second = provider.user(1);
    assert!(
        second.contains("what can you do?"),
        "the second turn did not read the first turn's prompt:\n{second}"
    );
    // 0.49.0 — the wording changed because the release changed it. The seed used to
    // narrate the agent's own past turn in the third person ("you answered: …")
    // inside the one user message a request could carry; the attribution now lives
    // in the message's role, and the entry is `[agent]`. What this test is about —
    // that a reply is part of the conversation the next turn reads — is unchanged,
    // and F6 asserts the role-tagged half.
    assert!(
        second.contains("[agent] I read repositories and change them."),
        "the second turn did not read the first turn's answer, so a reply is not \
         part of the conversation:\n{second}"
    );

    // A reply is a turn like any other: it can be branched from.
    session.branch_from(&store, first.turn_id).unwrap();
    let branched = session
        .turn("try again", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    let recorded = store.session_turn(branched.turn_id).unwrap().unwrap();
    assert_eq!(recorded.parent_turn_id, Some(first.turn_id));
}

// ------------------------------------------------------------------------ F5

/// F5 — a declared criterion is never answered instead of run.
///
/// A caller who said how the turn is judged has said it is work. The control is
/// the identical contract with no verification, which classifies as a reply.
#[tokio::test]
async fn a_contract_carrying_a_criterion_is_never_answered() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let answers = Mock::new(vec![Say::Text("looks fine to me")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let gated = TaskContract::workspace("check it", ws.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "notes.md".into(),
            needle: "# notes".into(),
        })
        .with_max_steps(1);
    let turn = session
        .turn_bounded(&gated, &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        turn.kind,
        TurnKind::Run,
        "a turn whose caller declared a criterion was answered instead of run"
    );
    let attempts = store.gate_attempts(turn.run_id).unwrap();
    assert!(
        !attempts.is_empty(),
        "the criterion was declared and never evaluated"
    );
    // And it was never even asked as conversation. The classification is refused at
    // two independent points — the session decides not to ask for it, and the loop
    // would refuse it anyway because `finished` carries the same condition — so
    // this asserts the outer one, which is the half that also decides the prompt.
    assert!(
        answers.system(0).contains("Do not explain; call tools."),
        "a turn whose caller declared a criterion was offered the chance to answer:\n{}",
        answers.system(0)
    );

    // The control: the same contract with no criterion classifies.
    let answers = Mock::new(vec![Say::Text("looks fine to me")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let ungated = TaskContract::workspace("check it", ws.path()).with_max_steps(1);
    let turn = session
        .turn_bounded(&ungated, &answers, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert_eq!(
        turn.kind,
        TurnKind::Reply,
        "a bounded contract with no criterion did not classify, so F5 is asserting \
         that bounded turns never classify rather than that criteria are honoured"
    );
    assert!(store.gate_attempts(turn.run_id).unwrap().is_empty());
}

// ------------------------------------------------------------------------ F6

/// F6 — a reply that died is not offered as resumable work.
///
/// A turn killed while it was still deciding what it was leaves a run row typed as
/// a reply and left `running`. There is nothing to continue: no step was
/// committed, and what it was doing was one completion, which asking again
/// replaces at the same price.
#[tokio::test]
async fn a_turn_that_died_while_answering_is_not_resumable_work() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let mut session = Session::open(&store, ws.path()).unwrap();
    let died = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        session.turn("hi", &Hangs, &store, &policy(), &ApproveAll),
    )
    .await;
    assert!(
        died.is_err(),
        "the turn returned, so nothing was killed mid-completion"
    );

    // The run the dead turn left behind.
    let run_id = store
        .last_run()
        .unwrap()
        .expect("the turn opened a run row");
    assert_eq!(
        store.steps(run_id).unwrap().len(),
        0,
        "the dead turn left a half-written step for a resume to adopt"
    );
    let refused = store.check_resumable(run_id);
    assert!(
        matches!(refused, Err(io_harness::Error::Resume { .. })),
        "a turn that died while answering was offered as work to continue: {refused:?}"
    );

    // The control: a run that is not a conversational turn, left in exactly the
    // same state, is still resumable — the refusal is about what the run was, not
    // about it having no steps.
    let plain = store.start_run("migrate the handlers", "/repo").unwrap();
    assert!(
        store.check_resumable(plain).is_ok(),
        "the refusal fires on any unfinished run, so it is not about replies at all"
    );

    // And taking the turn again is a new turn, not a resume of the dead one.
    let provider = Mock::new(vec![Say::Text("hello")]);
    let again = session
        .turn("hi", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert_ne!(again.run_id, run_id);
    assert_eq!(again.kind, TurnKind::Reply);
}

// ------------------------------------------------------------------------ F7

/// F7 — the boundary machinery is never reached by a reply.
///
/// The approver panics if it is called at all, and the policy's defaults would
/// make it ask about everything.
#[tokio::test]
async fn a_reply_never_reaches_the_approver_or_the_plan_gate() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();

    let answers = Mock::new(vec![Say::Text("I read repositories and change them.")]);
    let seen = Seen::default();
    let mut session = Session::open(&store, ws.path()).unwrap();
    let reply = session
        .turn_observed(
            "what can you do?",
            &answers,
            &store,
            &asks_about_everything(),
            &NeverAsk,
            &seen,
        )
        .await
        .unwrap();

    assert_eq!(reply.kind, TurnKind::Reply);
    let a = staged(&store, reply.run_id);
    assert_eq!(
        a.pending_approvals, 0,
        "a reply wrote a deferred approval, so something was staged for a human"
    );
    assert!(
        store
            .sandbox_events(reply.run_id)
            .unwrap()
            .iter()
            .all(|e| e.kind != "selected"),
        "a sandbox was selected for a turn that did no work"
    );

    // The event an attached process reads, and the one it must not: `Answered`
    // reaches the observer, and no `Step` does.
    let kinds = seen.0.lock().unwrap();
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EventKind::Answered { turn_id } if *turn_id == reply.turn_id)),
        "a turn that was answered emitted no Answered event: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(k, EventKind::Step { .. })),
        "a reply announced a step: {kinds:?}"
    );
}

// ------------------------------------------------------------------------ N3

/// N3 — the one-shot path pays nothing.
///
/// `run_with` and `run_with_observed` are work by declaration. The claim is
/// structural rather than behavioural: neither reaches the classification at all,
/// because the only thing that turns it on is a field the session layer sets and
/// they drive the loop with the default.
///
/// A source-reading test in the shape 0.33.0, 0.35.0 and 0.36.0 used, with a
/// splice-in control: the checker must report a violation when one is put in front
/// of it, or it is a green light wired to nothing.
#[test]
fn the_one_shot_entry_points_reach_no_classification() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/run.rs"),
    )
    .expect("the crate can read its own source")
    // A Windows checkout holds this file with CRLF, and a parse looking for "\n}"
    // would find nothing there and fail on one platform only.
    .replace("\r\n", "\n");

    for entry in ["run_with", "run_with_observed"] {
        let body = body_of(&src, entry);
        assert!(
            !mentions_classification(&body),
            "{entry} reaches the classification, so a one-shot contract can be answered \
             instead of run:\n{body}"
        );
    }

    // The control. The same checker, fed a body that does set it, must say so —
    // otherwise the loop above would pass against any source at all.
    assert!(
        mentions_classification(
            "    let extras = TurnExtras { classify: true, ..Default::default() };"
        ),
        "the checker cannot see a classification it is looking straight at"
    );
    // And it must not fire on a body that merely mentions the word in prose.
    assert!(
        !mentions_classification("    // classification is decided by the session layer"),
        "the checker fires on a comment, so it reports where the word appears rather \
         than where the field is set"
    );
}

/// The body of `fn <name>` in `src`, from its opening brace to the first line that
/// closes at column zero.
fn body_of(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("pub async fn {name}<P: Provider>("))
        .unwrap_or_else(|| panic!("{name} is declared in src/run.rs"));
    let rest = &src[at..];
    let open = rest
        .find(") -> Result<RunResult> {")
        .expect("the body opens");
    let body = &rest[open..];
    let close = body.find("\n}\n").expect("the body closes at column zero");
    body[..close].to_string()
}

/// Whether a body turns the classification on. The field is the only switch there
/// is, so naming it is the whole test.
fn mentions_classification(body: &str) -> bool {
    body.contains("classify:") || body.contains("classify =")
}

// ------------------------------------------------- 0.49.0: a prior turn is a turn

/// **F6** — a session's earlier turns reach the model as real user and assistant
/// messages, and the third-person narration is gone.
///
/// Both halves are asserted together on purpose. The presence assertion alone
/// passes for a build that sends the turns as messages *and* keeps narrating them;
/// the absence assertion alone passes for a build that simply dropped them.
#[tokio::test]
async fn an_earlier_turn_arrives_as_that_speakers_own_message() {
    let ws = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Mock::new(vec![
        Say::Text("I read repositories and change them."),
        Say::Text("still here"),
    ]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    session
        .turn(
            "what can you do?",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    session
        .turn("and now?", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    let messages = provider.messages(1);
    assert!(
        !messages.is_empty(),
        "the second turn must carry a conversation, or this asserts nothing"
    );

    let asked = messages
        .iter()
        .any(|m| matches!(m, Message::User(text) if text.contains("what can you do?")));
    let answered = messages.iter().any(|m| {
        matches!(
            m,
            Message::Assistant { text: Some(text), calls }
                if text.contains("I read repositories and change them.") && calls.is_empty()
        )
    });
    assert!(
        asked,
        "the operator's earlier turn is a user message: {messages:#?}"
    );
    assert!(
        answered,
        "and the agent's own answer is an assistant message, not something it is \
         told about: {messages:#?}"
    );

    // The narration is gone from the whole request, `user` included — the
    // attribution lives in the role now.
    let user = provider.user(1);
    assert!(
        !user.contains("the operator asked:") && !user.contains("you answered:"),
        "the third-person narration must be gone:\n{user}"
    );
}
