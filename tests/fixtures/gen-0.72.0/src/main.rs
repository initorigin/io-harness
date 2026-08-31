//! Writes — and reads back — the fixture store that `tests/cross_version_0_72_0.rs`
//! uses to prove 0.73.0 and 0.72.0 read each other's stores, using a real io-harness
//! 0.72.0 from crates.io.
//!
//! The point of the crate is that the *other* binary is the previous release, not this
//! tree with a 0.72.0 label on it. 0.73.0 changes no store column and no serialized
//! store value — `Skill` gains a field and becomes `#[non_exhaustive]`, `plugin.toml`
//! gains `[[bin]]`, and a shell redirect check moves from spawn time to parse time,
//! none of which reaches SQLite or `CHECKPOINT_FORMAT` — so both directions here are
//! *expected* to pass. That is the point. 0.72.0 also believed its own change was
//! additive, its whole suite agreed, and only a real previous-release binary showed
//! otherwise; a belief that a release touched no persisted surface is worth exactly as
//! much as the binary that checks it.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-72-0 write <output-dir>   # produce the committed fixture
//! gen-0-72-0 read  <database>     # print what 0.72.0 can see in a store, as JSON
//! ```
//!
//! `write` produces one database, `questions.sqlite3`, holding four
//! `pending_questions` rows chosen so that every shape 0.72.0 could put in that table —
//! and only 0.72.0 could, since the object spelling and the batch column are its own
//! additions — is present for a 0.73.0 reader:
//!
//! * one with described and previewed offers, unanswered — the **object** spelling of
//!   `choices`, which no release before 0.72.0 could write, and the row a 0.73.0 binary
//!   must still be able to answer;
//! * one with bare labels, answered — the **string** spelling, still what `Choice`
//!   writes for a label with nothing said about it, proving `answer`, `answered_by` and
//!   `resolved` survive beside the column rather than only the column;
//! * one with no choices at all, so `NULL` is covered as well as `'[...]'`;
//! * one written by `put_questions`, which fills the `questions` column 0.72.0 added —
//!   the batch's parts have to arrive intact under 0.73.0, not merely the rendered text.
//!
//! One thing is deliberately *not* covered: the `answers` column. `Store::answer_questions`
//! is `pub(crate)` in 0.72.0, so a generator built against the published crate cannot
//! fill it, and a fixture that faked it with raw SQL would be this tree writing a row
//! and calling it 0.72.0's. Every row here therefore has an empty `answers`, and the
//! test asserts that rather than pretending otherwise.
//!
//! The run is left **unfinished**, parked on two unanswered questions, because a
//! resumable run is the interesting thing to hand to a newer binary: a store whose rows
//! read back but whose run cannot be resumed is still broken.
//!
//! The sidecar is the specification. `read_back` holds what 0.72.0's own API returned
//! from the finished store, so a 0.73.0 reader is compared against the previous
//! release's answers rather than against its own.
//!
//! Fully offline and deterministic: no run loop, no provider, no network, no API key,
//! no wall-clock value in any expectation.

use io_harness::{Choice, Question, Store};
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one useful
/// behaviour — print why and exit non-zero — so there is nothing for a typed error to
/// decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The contract root every run records. Relative on purpose: `runs.file` stores it
/// verbatim, so an absolute path would bake this machine's home directory into a
/// committed fixture.
const WORKSPACE: &str = "fixture-workspace";

/// The goal every run records. Fixed rather than absent: a `NULL` and a value that
/// round-tripped are different facts, and only the second proves the column survived.
const GOAL: &str = "port the parser";

/// Answer a row and refuse to continue if the compare-and-swap did not win. A generator
/// that silently failed to answer a row would produce a fixture whose sidecar disagrees
/// with its database, and the test reading it would blame the reader.
fn answer(store: &Store, id: i64, text: &str, by: &str) -> Res<()> {
    if !store.answer_question(id, text, by)? {
        return Err(format!("row {id} was already answered while writing the fixture").into());
    }
    Ok(())
}

/// Everything 0.72.0 can see of one `pending_questions` row, as JSON.
///
/// Shared by `write` and `read` so the sidecar and the backwards half describe a row
/// the same way, and a difference between them is a difference in the data.
fn row(q: &io_harness::PendingQuestion) -> Value {
    json!({
        "id": q.id,
        "run_id": q.run_id,
        "step": q.step,
        "question": q.question,
        "context": q.context,
        // `Choice`'s own `Serialize`, which is what the column holds: a bare label is
        // written as a plain string, a described one as an object. Recorded through it
        // rather than field by field so the expectation is the wire form.
        "choices": q.choices,
        // Only the texts of a batch's parts. The whole `Question` would drag every
        // unrelated field of a struct this release may extend into an expectation about
        // a column, and the claim is that the parts survive.
        "batch_questions": q.questions.iter().map(|b| b.question.clone()).collect::<Vec<_>>(),
        "answers": q.answers,
        "answer": q.answer,
        "answered_by": q.answered_by,
        "resolved": q.resolved,
    })
}

/// Write `questions.sqlite3` and its sidecar into the current directory.
fn questions() -> Res<()> {
    let store = Store::open("questions.sqlite3")?;
    let run_id = store.start_run(GOAL, WORKSPACE)?;

    // The object spelling, which is 0.72.0's own addition and the only thing in this
    // table a 0.71.0 binary could not have produced. Left parked: a 0.73.0 binary has
    // to be able to answer it, not merely read it.
    //
    // The third label carries a comma and a quote, because the several-part answer
    // spelling joins labels with `", "` and a label that already contains one is exactly
    // where a reader that split rather than parsed would pass its own tests and lose
    // data on a real row.
    let described = store.put_question(
        run_id,
        2,
        &Question::new("Which config should I edit?")
            .with_context("There is a committed io.toml and a gitignored io.local.toml.")
            .with_choices([
                Choice::new("io.toml").describe("committed, and inherited by the team"),
                Choice::new("io.local.toml").preview("io.local.toml is gitignored"),
                Choice::new("neither, and \"stop\", please"),
            ]),
    )?;

    // Bare labels, answered. Byte-identical in the column to what every release before
    // 0.72.0 wrote, so this row is the control for the one above.
    let bare = store.put_question(
        run_id,
        5,
        &Question::new("Should the old column be dropped or kept?")
            .with_choices(["drop it", "keep it"]),
    )?;
    answer(&store, bare, "keep it", "human")?;

    // No offers at all: `NULL` in the column, not `'[]'`.
    let free = store.put_question(
        run_id,
        7,
        &Question::new("Why did the parser change?")
            .with_context("Nothing in the diff explains it."),
    )?;
    answer(&store, free, "a lexer bug", "responder")?;

    // A batch: one row, the `questions` column populated with both parts. Left parked,
    // like the first, so the newer binary has to answer a batch and not only read one.
    let batch = store.put_questions(
        run_id,
        9,
        &[
            Question::new("which port?"),
            Question::new("which host?").with_context("staging or production"),
        ],
    )?;

    // What 0.72.0's own API says about the store it just wrote. This is the expectation
    // side of every forwards assertion: nothing is re-derived from the database under
    // test, which would be a test that cannot fail.
    let read_back: Vec<Value> = store.questions(run_id)?.iter().map(row).collect();

    sidecar(
        "questions.json",
        &json!({
            "writer": "io-harness 0.72.0",
            "run_id": run_id,
            "goal": GOAL,
            "workspace": WORKSPACE,
            // The run is deliberately unfinished — see the module docs.
            "status": store.status(run_id)?,
            "last_step": store.last_step(run_id)?,
            "outcome": store.outcome(run_id)?,
            "described_id": described,
            "batch_id": batch,
            "unanswered_ids": [described, batch],
            "read_back": read_back,
        }),
    )?;

    drop(store);
    Ok(())
}

// ---------- reading a store back, as 0.72.0 sees it ----------

/// Print everything 0.72.0 can see of `db`'s questions, as JSON on stdout.
///
/// The backwards half runs this against a database the *current* tree wrote. 0.73.0
/// claims to have changed nothing this binary reads; a clean read here, with every
/// description, preview and batch part intact, is the evidence for that claim rather
/// than the claim itself. Anything that opens and selects is fair game; nothing here
/// writes.
fn read(db: &str) -> Res<()> {
    let store = Store::open(db)?;
    let mut runs = Vec::new();
    // Run ids are dense from 1 in a fixture database, and a run that is not there is
    // skipped rather than fatal — this mode must be able to read a store it did not
    // write.
    for run_id in 1..=64 {
        let questions = store.questions(run_id)?;
        if questions.is_empty() {
            continue;
        }
        runs.push(json!({
            "run_id": run_id,
            "status": store.status(run_id)?,
            "outcome": store.outcome(run_id)?,
            "last_step": store.last_step(run_id)?,
            "questions": questions.iter().map(row).collect::<Vec<_>>(),
        }));
    }
    let out = json!({
        "reader": "io-harness 0.72.0",
        "checkpoint_format": io_harness::CHECKPOINT_FORMAT,
        "runs": runs,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Write a sidecar next to its database, pretty-printed and newline-terminated so a
/// diff of a regenerated fixture is readable line by line.
fn sidecar(path: &str, value: &Value) -> Res<()> {
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

fn main() -> Res<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: gen-0-72-0 write <output-dir> | gen-0-72-0 read <database>";
    let mode = args.next().ok_or(usage)?;
    let target = args.next().ok_or(usage)?;

    match mode.as_str() {
        "write" => {
            std::fs::create_dir_all(&target)?;
            // Everything after this names files relatively. `runs.file` stores the
            // contract root verbatim, so an absolute root would bake this machine's
            // home directory into a committed fixture.
            std::env::set_current_dir(&target)?;
            questions()
        }
        "read" => read(&target),
        other => Err(format!("unknown mode `{other}`: expected `write` or `read`").into()),
    }
}
