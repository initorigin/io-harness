//! Asking everything you need in one breath (0.72.0).
//!
//! `ask_question` takes one question and the run blocks inside `Responder::answer`
//! before the model can ask a second, so a model that needed five facts spent five
//! round trips and an interface downstream could not gather them into one surface — it
//! can only render what reaches it, and they reached it one at a time. `ask_questions`
//! is the plural tool; `Responder::answer_all` is the seam an interface overrides to
//! draw one overlay for five questions.
//!
//! These tests drive the real loop. The two that carry the release are:
//!
//! * `a_batch_reaches_an_overriding_responder_as_one_call` — the capability, and
//! * `the_default_loop_reaches_the_identical_observation` — the proof that the
//!   defaulted trait body is real and not a stub, asserted against a responder that
//!   records the order it was called in. Without the second, "no existing implementor
//!   changes" is a claim about code nobody ran.
//!
//! The cap, the per-index errors and the preview bounds are here rather than as unit
//! tests on the parser because what they must not do is end the run — a malformed ask
//! is something the model can send again, and a parser that returned `Err` up the stack
//! would kill a run over a typo.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    resume_with_answer, run_with, AnswerFuture, AnswersFuture, ApproveAll, Choice, EventKind,
    Policy, Provider, Question, Responder, ResponderNone, RunOutcome, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

/// Asks one `ask_questions` call on the first turn and nothing afterwards, so the run's
/// shape is decided by the batch rather than by the script.
struct AsksOnce {
    at: AtomicUsize,
    args: Value,
    seen: Arc<Mutex<Vec<String>>>,
}

impl AsksOnce {
    fn new(args: Value) -> Self {
        Self {
            at: AtomicUsize::new(0),
            args,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every user prompt the provider was handed, which is where an observation lands.
    fn prompts(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for AsksOnce {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req.user.clone());
        Ok(CompletionResponse {
            tool_calls: match i {
                0 => vec![ToolCall {
                    name: "ask_questions".into(),
                    arguments: self.args.clone(),
                }],
                _ => vec![],
            },
            ..Default::default()
        })
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// A contract that can never pass, so the run's length is decided by the thing under
/// test rather than by a verification that happens to succeed.
fn never_passes(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("find out what the operator meant", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

/// Three questions, two of them carrying described choices — the shape
/// `evidence_of_value` names.
fn three_questions() -> Value {
    json!({
        "questions": [
            {
                "question": "Which database?",
                "context": "Both are already vendored.",
                "choices": [
                    { "label": "sqlite", "description": "Bundled; no service to run." },
                    { "label": "postgres", "description": "Needs a server.", "preview": "url = \"postgres://localhost\"" }
                ]
            },
            {
                "question": "Which platforms?",
                "choices": ["linux", "windows", "macos"],
                "multiple": true
            },
            { "question": "What should the binary be called?" }
        ]
    })
}

/// Answers the whole batch in one pass, and records that it was called once with a
/// slice of three rather than three times with one.
#[derive(Debug, Default)]
struct AllAtOnce {
    batches: Mutex<Vec<usize>>,
}

impl Responder for AllAtOnce {
    fn answer<'a>(&'a self, _question: &'a Question) -> AnswerFuture<'a> {
        // Deliberately wrong, so a run that fell through to the singular path is
        // visible in the assertion rather than merely unproven.
        Box::pin(async { Some("SINGULAR PATH".to_string()) })
    }

    fn answer_all<'a>(&'a self, questions: &'a [Question]) -> AnswersFuture<'a> {
        self.batches.lock().unwrap().push(questions.len());
        Box::pin(async move {
            questions
                .iter()
                .map(|q| {
                    Some(match q.multiple {
                        // The crate's own spelling for a several-part answer, so this
                        // responder and any other produce the same text.
                        true => Question::answer_of(q.choices.iter().map(|c| c.label.as_str())),
                        false => q
                            .choices
                            .first()
                            .map(|c| c.label.clone())
                            .unwrap_or_else(|| format!("answer to: {}", q.question)),
                    })
                })
                .collect()
        })
    }
}

/// Implements `answer` ONLY, which is what all eight in-repo implementors do. Records
/// the order it was asked in, so "in question order" is asserted rather than assumed.
#[derive(Debug, Default)]
struct OneAtATime {
    asked: Mutex<Vec<String>>,
}

impl Responder for OneAtATime {
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
        self.asked.lock().unwrap().push(question.question.clone());
        Box::pin(async move {
            Some(match question.multiple {
                true => Question::answer_of(question.choices.iter().map(|c| c.label.as_str())),
                false => question
                    .choices
                    .first()
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| format!("answer to: {}", question.question)),
            })
        })
    }
}

// -------------------------------------------------- O1: the batch, answered at once

/// O1 — one `ask_questions` call with three questions reaches an overriding responder
/// as **one** call with a slice of three, and the run continues with all three answers
/// in one observation on the next step.
#[tokio::test]
async fn a_batch_reaches_an_overriding_responder_as_one_call() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let responder = Arc::new(AllAtOnce::default());
    let contract = never_passes(dir.path(), 3).with_responder(responder.clone());
    let provider = AsksOnce::new(three_questions());

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    // One call, carrying three. Not three calls carrying one.
    assert_eq!(
        *responder.batches.lock().unwrap(),
        vec![3],
        "the batch must reach `answer_all` once, as a slice of three"
    );

    // All three answers in ONE observation on the next step.
    let observation = provider
        .prompts()
        .into_iter()
        .find(|p| p.contains("[answers]"))
        .expect("the answers must arrive as an observation");
    for expected in ["sqlite", "linux, windows, macos", "answer to: What should"] {
        assert!(
            observation.contains(expected),
            "the observation must carry {expected:?}: {observation}"
        );
    }
    assert!(
        !observation.contains("SINGULAR PATH"),
        "the singular `answer` must not have been used: {observation}"
    );

    // One durable row for the whole ask, so the resume surface stays singular.
    let rows = store.questions(result.run_id).unwrap();
    assert_eq!(rows.len(), 1, "a batch is ONE parked question: {rows:#?}");
    assert!(rows[0].resolved);
    assert_eq!(rows[0].questions.len(), 3);
    assert_eq!(rows[0].answers.len(), 3);
    assert_eq!(rows[0].answered_by.as_deref(), Some("responder"));
}

// ------------------------------------- O2: the default loop reaches the same place

/// O2 — the same run against a responder implementing only `answer` produces the
/// identical observation, through the default loop, in question order.
#[tokio::test]
async fn the_default_loop_reaches_the_identical_observation() {
    /// Run the batch under `responder` and return the `[answers]` observation.
    async fn observation_under(responder: Arc<dyn Responder>) -> String {
        let dir = ws();
        let store = Store::memory().unwrap();
        let contract = never_passes(dir.path(), 3).with_responder(responder);
        let provider = AsksOnce::new(three_questions());
        run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();
        provider
            .prompts()
            .into_iter()
            .find(|p| p.contains("[answers]"))
            .expect("the answers must arrive as an observation")
    }

    let looping = Arc::new(OneAtATime::default());
    let overriding = observation_under(Arc::new(AllAtOnce::default())).await;
    let default = observation_under(looping.clone()).await;

    // The whole claim: the default body is real, and reaches the same place.
    assert_eq!(
        default, overriding,
        "the default loop must produce the identical observation"
    );

    // And it looped in question order, asserted against what the responder recorded
    // rather than against the answer text, which cannot distinguish order from luck.
    assert_eq!(
        *looping.asked.lock().unwrap(),
        vec![
            "Which database?".to_string(),
            "Which platforms?".to_string(),
            "What should the binary be called?".to_string(),
        ]
    );
}

/// N1 — `Responder` is still dyn-compatible. A batched responder stored behind the
/// trait object is what `TaskContract::with_responder` takes, and the test above
/// already passes one; this pins the property directly so a future edit that breaks
/// object safety fails with a clear name rather than inside a run.
#[test]
fn a_batching_responder_is_still_a_trait_object() {
    let responders: Vec<Arc<dyn Responder>> = vec![
        Arc::new(AllAtOnce::default()),
        Arc::new(OneAtATime::default()),
        Arc::new(ResponderNone),
    ];
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for responder in &responders {
        let answers = rt.block_on(responder.answer_all(&[Question::new("which?")]));
        assert_eq!(answers.len(), 1);
    }
}

// ------------------------------------------------- O5, O12: what the parser refuses

/// O5 and O12 — an empty array, a question missing its `question`, a malformed choice
/// object, more questions than the cap, and `multiple` with no choices each produce an
/// error naming the failing index or the cap, and **none of them ends the run**.
#[tokio::test]
async fn a_malformed_ask_is_an_observation_naming_the_index_rather_than_the_end_of_the_run() {
    let over_cap: Vec<Value> = (0..11).map(|i| json!({ "question": format!("q{i}?") })).collect();
    let cases: [(Value, &str); 6] = [
        (json!({ "questions": [] }), "no questions"),
        (
            json!({ "questions": [{ "question": "fine?" }, { "context": "no question here" }] }),
            "question 2",
        ),
        (
            json!({ "questions": [{ "question": "fine?", "choices": [{ "description": "no label" }] }] }),
            "choice 1",
        ),
        (json!({ "questions": over_cap }), "10"),
        (
            json!({ "questions": [{ "question": "fine?", "multiple": true }] }),
            "multiple",
        ),
        (json!({ "steps": [] }), "`questions` is required"),
    ];

    for (args, expected) in cases {
        let dir = ws();
        let store = Store::memory().unwrap();
        let contract = never_passes(dir.path(), 3).with_responder(Arc::new(AllAtOnce::default()));
        let provider = AsksOnce::new(args.clone());

        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        // The run reached its own end rather than being killed by the parse.
        assert!(
            !matches!(result.outcome, RunOutcome::AwaitingAnswer { .. }),
            "a malformed ask must not park the run: {args}"
        );
        let observation = provider
            .prompts()
            .into_iter()
            .find(|p| p.contains("[questions error]"))
            .unwrap_or_else(|| panic!("no error observation for {args}"));
        assert!(
            observation.contains(expected),
            "the error must name {expected:?}: {observation}"
        );
        // Nothing was parked, because nothing was validly asked.
        assert!(store.questions(result.run_id).unwrap().is_empty());
    }
}

// --------------------------------------------------- O6: parked, and resumed whole

/// O6 — a batch nobody answers parks the run as `AwaitingAnswer` with **one**
/// `question_id`, and resumes through the existing `resume_with_answer` with the answer
/// set. The resume function's signature is unchanged, which is what keeps the surface
/// singular.
#[tokio::test]
async fn an_unanswered_batch_parks_one_row_and_resumes_through_the_existing_function() {
    let dir = ws();
    let store = Store::memory().unwrap();
    // `ResponderNone` declines, which is the honest default for an unattended run.
    let contract = never_passes(dir.path(), 3).with_responder(Arc::new(ResponderNone));
    let provider = AsksOnce::new(three_questions());

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let RunOutcome::AwaitingAnswer { question_id, .. } = result.outcome else {
        panic!("an unanswered batch must park the run: {:?}", result.outcome);
    };

    let rows = store.questions(result.run_id).unwrap();
    assert_eq!(rows.len(), 1, "one row for the whole ask");
    assert_eq!(rows[0].id, question_id);
    assert!(!rows[0].resolved);
    assert_eq!(rows[0].questions.len(), 3);
    // A human reading the table sees the whole ask, not the first of it.
    for text in ["Which database?", "Which platforms?", "called?"] {
        assert!(rows[0].question.contains(text), "{}", rows[0].question);
    }

    // The answer set arrives as one text through the function that already existed,
    // with the signature it already had — which is what "no plural resume surface"
    // means in practice.
    let resumed = resume_with_answer(
        &contract,
        &AsksOnce::new(json!({})),
        &store,
        result.run_id,
        question_id,
        "sqlite; linux and windows; io",
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert!(!matches!(resumed.outcome, RunOutcome::AwaitingAnswer { .. }));

    let after = store.question(question_id).unwrap().unwrap();
    assert!(after.resolved);
    assert_eq!(after.answered_by.as_deref(), Some("human"));
    assert_eq!(after.answer.as_deref(), Some("sqlite; linux and windows; io"));
}

/// O7 — a batch is answered wholly or not at all, and a second attempt on a resolved
/// row changes nothing, exactly as for a single question.
#[test]
fn answering_a_batch_twice_changes_nothing() {
    let store = Store::memory().unwrap();
    let run = store.start_run("goal", "provider").unwrap();
    let id = store
        .put_questions(run, 1, &[Question::new("a?"), Question::new("b?")])
        .unwrap();

    assert!(store.answer_question(id, "first", "human").unwrap());
    assert!(!store.answer_question(id, "second", "attached").unwrap());
    assert_eq!(store.question(id).unwrap().unwrap().answer.as_deref(), Some("first"));
}

// ---------------------------------------------------------------- O8: the events

/// O8 — `QuestionsAsked` is emitted once for a batch and carries every question;
/// `QuestionAsked` is **not** emitted for a batch and still is for a singular ask;
/// `QuestionAnswered` is emitted once per answer.
#[tokio::test]
async fn a_batch_emits_one_questions_asked_and_one_answered_per_answer() {
    /// Collects the event stream, which is where `QuestionsAsked` is observable.
    #[derive(Debug, Default)]
    struct Recorder {
        events: Mutex<Vec<io_harness::RunEvent>>,
    }

    impl io_harness::Observer for Recorder {
        fn event(&self, event: &io_harness::RunEvent) -> io_harness::Flow {
            self.events.lock().unwrap().push(event.clone());
            io_harness::Flow::Continue
        }
    }

    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3).with_responder(Arc::new(AllAtOnce::default()));
    let provider = AsksOnce::new(three_questions());
    let watcher = Recorder::default();

    io_harness::run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &watcher,
    )
    .await
    .unwrap();

    let events = watcher.events.lock().unwrap().clone();
    let batches: Vec<&Vec<Question>> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::QuestionsAsked { questions } => Some(questions),
            _ => None,
        })
        .collect();
    assert_eq!(batches.len(), 1, "one `QuestionsAsked` for the batch");
    assert_eq!(batches[0].len(), 3, "carrying every question");
    assert_eq!(batches[0][1].question, "Which platforms?");
    assert!(batches[0][1].multiple, "and the offers each one carried");

    // The singular variant is NOT also emitted — otherwise an observer watching it
    // cannot tell three-in-a-batch from three-in-sequence, which is the whole point.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::QuestionAsked { .. })),
        "a batch must not also emit the singular variant"
    );

    // One per answer: an answer is an independent fact and a UI draws each one beside
    // its own question.
    let answered = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::QuestionAnswered { .. }))
        .count();
    assert_eq!(answered, 3, "one `QuestionAnswered` per answer");
}

// ------------------------------------------- O10, O11, O13, O14: the shapes it keeps

/// O10 — the name the reserved set has to carry. The set itself is `pub(crate)`, so
/// the assertion that a custom tool cannot claim it lives in `tests/custom_tools.rs`
/// beside every other reserved name rather than being re-machined here; this pins the
/// constant's value, which is the half that would silently drift.
#[test]
fn the_batch_tool_is_named_what_the_reserved_set_reserves() {
    assert_eq!(io_harness::ASK_QUESTIONS_TOOL, "ask_questions");
    assert_ne!(io_harness::ASK_QUESTIONS_TOOL, io_harness::ASK_QUESTION_TOOL);
}

/// O11 — `multiple` round-trips through the tool, through the store, and reaches a
/// `Responder` as `true`; a question omitting the key reaches it as `false`; and a row
/// written by 0.71.0 — whose column does not exist — reads back as `false`.
#[tokio::test]
async fn multiple_round_trips_and_defaults_to_false() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let responder = Arc::new(SeenFlags::default());
    let contract = never_passes(dir.path(), 3).with_responder(responder.clone());
    let provider = AsksOnce::new(three_questions());

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    // As the responder saw them: the second question said several may be taken, the
    // other two said nothing and mean the same thing they always did.
    assert_eq!(*responder.flags.lock().unwrap(), vec![false, true, false]);

    // And through the store.
    let row = &store.questions(result.run_id).unwrap()[0];
    assert_eq!(
        row.questions.iter().map(|q| q.multiple).collect::<Vec<_>>(),
        vec![false, true, false]
    );

    // A row whose JSON has no `multiple` key at all — which is every row any earlier
    // release wrote — reads back as `false` rather than failing to parse.
    let old: Question = serde_json::from_str(r#"{"question":"which?","context":null,"choices":["a"]}"#)
        .expect("a 0.71.0-shaped question must still deserialize");
    assert!(!old.multiple);
    assert_eq!(old.choices[0].label, "a");
}

/// Records the `multiple` flag of every question it is handed, in order.
#[derive(Debug, Default)]
struct SeenFlags {
    flags: Mutex<Vec<bool>>,
}

impl Responder for SeenFlags {
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
        self.flags.lock().unwrap().push(question.multiple);
        Box::pin(async { Some("noted".to_string()) })
    }
}

/// O13 — one documented spelling for a several-part answer, asserted against the helper
/// rather than against a literal, so the two cannot drift apart.
#[test]
fn a_several_part_answer_has_one_spelling() {
    let question = Question::new("Which platforms?")
        .with_choices(["Linux", "Windows", "macOS"])
        .multiple();

    let taken = [&question.choices[0], &question.choices[1]];
    let spelled = Question::answer_of(taken.iter().map(|c| c.label.as_str()));

    assert_eq!(spelled, "Linux, Windows");
    // The documented form, stated once. A second interface that joined with " and "
    // would produce text the first one's operator never sees.
    assert_eq!(spelled, Question::answer_of(["Linux", "Windows"]));
}

/// O14 — a choice carrying both optional fields, one carrying neither, and a bare
/// string all round-trip through the store. The third is what every 0.71.0 row holds,
/// which is why all three are one test.
#[test]
fn every_choice_shape_round_trips_through_the_store() {
    let store = Store::memory().unwrap();
    let run = store.start_run("goal", "provider").unwrap();
    let id = store
        .put_question(
            run,
            1,
            &Question::new("Which?").with_choices([
                Choice::new("both").describe("a sentence").preview("a block"),
                Choice::new("neither"),
                Choice::from("bare"),
            ]),
        )
        .unwrap();

    let read = store.question(id).unwrap().unwrap().choices;
    assert_eq!(read[0].description.as_deref(), Some("a sentence"));
    assert_eq!(read[0].preview.as_deref(), Some("a block"));
    assert!(read[1].description.is_none() && read[1].preview.is_none());
    assert_eq!(read[2].label, "bare");
    assert!(read[2].description.is_none() && read[2].preview.is_none());
}

// -------------------------------------------------------- O15: the preview is bounded

/// O15 — a preview over either bound is cut at a line boundary and the model is told
/// what was cut, with the rest of the question unaffected; and a preview carrying a
/// control character or an escape sequence is stripped, because this value is written
/// by a model and drawn into a terminal by every consumer.
#[tokio::test]
async fn an_over_long_preview_is_cut_at_a_line_boundary_and_the_model_is_told() {
    let many_lines = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let long_lines = (0..8).map(|i| format!("{i} {}", "x".repeat(200))).collect::<Vec<_>>().join("\n");

    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3).with_responder(Arc::new(AllAtOnce::default()));
    let provider = AsksOnce::new(json!({
        "questions": [{
            "question": "Which?",
            "choices": [
                { "label": "lines", "preview": many_lines },
                { "label": "bytes", "preview": long_lines },
                { "label": "escapes", "preview": "before\u{1b}[31m red \u{7} after" }
            ]
        }]
    }));

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let choices = &store.questions(result.run_id).unwrap()[0].questions[0].choices;

    let lines = choices[0].preview.as_deref().unwrap();
    assert!(lines.contains("[preview cut"), "the model must be told: {lines}");
    assert!(lines.starts_with("line 0"), "kept from the start: {lines}");
    // Cut at a line boundary: every kept line is a whole one.
    assert!(
        lines.lines().take_while(|l| !l.starts_with("[preview cut")).all(|l| l.starts_with("line ")),
        "no line was cut mid-way: {lines}"
    );

    let bytes = choices[1].preview.as_deref().unwrap();
    assert!(bytes.contains("[preview cut"), "the byte bound must also tell: {bytes}");
    assert!(bytes.len() < 1_000, "and must actually bound it: {}", bytes.len());

    // The escape and the bell are gone; the text around them is not.
    let escapes = choices[2].preview.as_deref().unwrap();
    assert!(!escapes.contains('\u{1b}') && !escapes.contains('\u{7}'), "{escapes:?}");
    assert!(escapes.contains("before") && escapes.contains("after"), "{escapes:?}");

    // The rest of the question is unaffected — a cut preview is not a rejected offer.
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[0].label, "lines");
}

/// The batch is never gated, for the same reason the singular is not: a permission rule
/// in front of the channel whose whole purpose is to ask would be a category error.
#[tokio::test]
async fn asking_is_not_gated_by_the_policy() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3).with_responder(Arc::new(AllAtOnce::default()));
    let provider = AsksOnce::new(three_questions());
    // A policy that permits nothing at all. Asking still works.
    let closed = Policy::default().layer("test");

    let result = run_with(&contract, &provider, &store, &closed, &ApproveAll)
        .await
        .unwrap();

    assert_eq!(store.questions(result.run_id).unwrap().len(), 1);
}
