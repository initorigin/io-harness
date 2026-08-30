//! Writes — and reads back — the fixture store that `tests/cross_version_0_71_0.rs`
//! uses to prove 0.72.0's additive change to `pending_questions` works in both
//! directions, using a real io-harness 0.71.0 from crates.io.
//!
//! The point of the crate is that the *other* binary is the previous release, not
//! this tree with a 0.71.0 label on it. 0.72.0 changes `Question::choices` from
//! `Vec<String>` to `Vec<Choice>` and adds two nullable JSON columns, and the two
//! claims that need a 0.71.0 binary to be evidence rather than assertion are:
//!
//! * **forwards** — every `pending_questions` row this binary wrote, whose `choices`
//!   column is a JSON array of plain strings because that is the only spelling 0.71.0
//!   could write, reads back under 0.72.0 as `Choice`s with no description and with
//!   nothing migrated, and a question parked here is still answerable there.
//! * **backwards** — a database 0.72.0 wrote is still read and resumed by this
//!   binary, which never selects the `questions` or `answers` columns.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-71-0 write <output-dir>   # produce the committed fixture
//! gen-0-71-0 read  <database>     # print what 0.71.0 can see in a store, as JSON
//! ```
//!
//! `write` produces one database, `questions.sqlite3`, holding four
//! `pending_questions` rows chosen so that every shape a 0.72.0 reader has to cope
//! with is present:
//!
//! * one with two choices, unanswered — the row the both-spellings deserializer
//!   exists for, and the one a 0.72.0 binary must be able to answer;
//! * one with two choices, answered — so `answer`, `answered_by` and `resolved`
//!   are proven to survive beside the changed column rather than only the column;
//! * one with no choices at all, so `NULL` is covered as well as `'[...]'`;
//! * one whose single choice contains a comma and a quote, because the 0.72.0
//!   several-part answer spelling joins labels with `", "` and a label that already
//!   contains one is exactly where a reader that split rather than parsed would pass
//!   its own tests and lose data on a real row.
//!
//! The sidecar is the specification. `composition` holds what this generator
//! *intended*; `read_back` holds what 0.71.0's own API returned from the finished
//! store, so a 0.72.0 reader is compared against the previous release's answers
//! rather than against its own.
//!
//! Fully offline and deterministic: no run loop, no provider, no network, no API key,
//! no wall-clock value in any expectation.

use io_harness::{Question, Store};
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one useful
/// behaviour — print why and exit non-zero — so there is nothing for a typed error to
/// decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The provider every run records. Fixed rather than absent: a `NULL` and a value that
/// round-tripped are different facts, and only the second proves the column survived.
const PROVIDER: &str = "fixture-provider";

/// One planned row: the question, its context, its offers, and its answer if it has
/// one. Written through the `Store` API directly rather than through the run loop,
/// because the composition is the specification here and a scripted agent run would
/// produce a composition nobody chose.
struct Planned {
    question: &'static str,
    context: Option<&'static str>,
    choices: &'static [&'static str],
    step: u32,
    /// `Some((answer, by))` for a resolved row; `None` leaves it parked, which is the
    /// state a 0.72.0 binary has to be able to answer.
    answered: Option<(&'static str, &'static str)>,
}

const PLANNED: &[Planned] = &[
    Planned {
        question: "Which config should I edit?",
        context: Some("There is a committed io.toml and a gitignored io.local.toml."),
        choices: &["io.toml", "io.local.toml"],
        step: 2,
        answered: None,
    },
    Planned {
        question: "Should the old column be dropped or kept?",
        context: None,
        choices: &["drop it", "keep it"],
        step: 5,
        answered: Some(("keep it", "human")),
    },
    Planned {
        question: "Why did the parser change?",
        context: Some("Nothing in the diff explains it."),
        choices: &[],
        step: 7,
        answered: Some(("a lexer bug", "responder")),
    },
    Planned {
        // A label carrying the very separator 0.72.0 joins several-part answers with,
        // and a quote for the JSON encoding to get wrong.
        question: "Which platforms?",
        context: None,
        choices: &["Linux, and the BSDs", "Windows (\"NT\")"],
        step: 9,
        answered: None,
    },
];

/// Write `questions.sqlite3` and its sidecar into the current directory.
fn questions() -> Res<()> {
    let store = Store::open("questions.sqlite3")?;
    let run_id = store.start_run("port the parser", PROVIDER)?;

    let mut composition = Vec::new();
    let mut ids = Vec::new();
    for planned in PLANNED {
        let mut question = Question::new(planned.question);
        if let Some(context) = planned.context {
            question = question.with_context(context);
        }
        if !planned.choices.is_empty() {
            question = question.with_choices(planned.choices.to_vec());
        }
        let id = store.put_question(run_id, planned.step, &question)?;
        if let Some((answer, by)) = planned.answered {
            // 0.33.0's compare-and-swap. `true` here is part of the fixture's own
            // self-check: a generator that silently failed to answer a row would
            // produce a fixture whose sidecar disagrees with its database.
            let won = store.answer_question(id, answer, by)?;
            if !won {
                return Err(format!("row {id} was already answered while writing the fixture").into());
            }
        }
        ids.push(id);
        composition.push(json!({
            "id": id,
            "step": planned.step,
            "question": planned.question,
            "context": planned.context,
            "choices": planned.choices,
            "answer": planned.answered.map(|(a, _)| a),
            "answered_by": planned.answered.map(|(_, by)| by),
            "resolved": planned.answered.is_some(),
        }));
    }

    // What 0.71.0's own API says about the store it just wrote. This is the
    // expectation side of every forwards assertion: nothing is re-derived from the
    // database under test, which would be a test that cannot fail.
    let read_back: Vec<Value> = store
        .questions(run_id)?
        .iter()
        .map(|q| {
            json!({
                "id": q.id,
                "run_id": q.run_id,
                "step": q.step,
                "question": q.question,
                "context": q.context,
                "choices": q.choices,
                "answer": q.answer,
                "answered_by": q.answered_by,
                "resolved": q.resolved,
            })
        })
        .collect();

    sidecar(
        "questions.json",
        &json!({
            "writer": "io-harness 0.71.0",
            "run_id": run_id,
            "provider": PROVIDER,
            "unanswered_ids": PLANNED.iter().zip(&ids)
                .filter(|(p, _)| p.answered.is_none())
                .map(|(_, id)| *id)
                .collect::<Vec<_>>(),
            "composition": composition,
            "read_back": read_back,
        }),
    )?;

    drop(store);
    Ok(())
}

// ---------- reading a store back, as 0.71.0 sees it ----------

/// Print everything 0.71.0 can see of `db`'s questions, as JSON on stdout.
///
/// The backwards half runs this against a database the *current* tree wrote. 0.71.0
/// has no idea the `questions` or `answers` columns exist and reads `choices` as
/// `Vec<String>`, so a clean read here is the evidence that the 0.72.0 migration is
/// additive in fact and not only in intention. Anything that opens, selects and
/// answers is fair game; nothing here writes.
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
            "questions": questions.iter().map(|q| json!({
                "id": q.id,
                "step": q.step,
                "question": q.question,
                "context": q.context,
                "choices": q.choices,
                "answer": q.answer,
                "answered_by": q.answered_by,
                "resolved": q.resolved,
            })).collect::<Vec<_>>(),
        }));
    }
    let out = json!({
        "reader": "io-harness 0.71.0",
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
    let usage = "usage: gen-0-71-0 write <output-dir> | gen-0-71-0 read <database>";
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
