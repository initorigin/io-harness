//! 0.73.0's cross-version fixture: a store written by a real io-harness 0.72.0 from
//! crates.io is read by this tree, and a store this tree writes is read by that same
//! 0.72.0 binary.
//!
//! **This test is expected to pass, and it is expected to pass for a reason worth
//! stating plainly rather than dressing up.** 0.73.0 changes no store column, no
//! serialized store value and not `CHECKPOINT_FORMAT`: `Skill` gains a field and
//! becomes `#[non_exhaustive]`, `plugin.toml` gains `[[bin]]`, and a shell redirect
//! check moves from spawn time to parse time. None of those reaches SQLite. So this
//! file is not hunting a break — it is the evidence for the claim that there is none.
//!
//! That is the whole rule 0.72.0 wrote for itself, and it wrote it by being wrong.
//! 0.72.0 changed `Question::choices` from `Vec<String>` to `Vec<Choice>` believing it
//! additive; its entire suite was green, because every test in it read rows the same
//! tree had just written; and only a real `io-harness =0.71.0` reading a store the
//! working tree wrote could see the defect. A release's own belief that it touched
//! nothing persisted is worth exactly as much as the previous binary that checks it,
//! which is why every release after 0.72.0 carries this fixture against its own
//! predecessor whether or not it thinks it needs one. A green run here is the claim
//! discharged, not the test wasted.
//!
//! Two halves, as in `cross_version_0_71_0.rs`:
//!
//! * **Forwards** — the fixture under `tests/fixtures/store-0.72.0/`, written by a real
//!   0.72.0 (the generator is `tests/fixtures/gen-0.72.0/`), reads back here row for
//!   row: the object spelling of `choices` keeps its descriptions and previews, the
//!   string spelling keeps its labels, the batch keeps its parts, and the run 0.72.0
//!   left parked is still resumable and still answerable. Runs everywhere, on every OS.
//! * **Backwards** — a store *this* tree wrote is read by that 0.72.0 binary, which
//!   must lose nothing at all, since this release added nothing for it to be ignorant
//!   of. That one needs the generator built, so it is `#[ignore]` by default.
//!
//! Nothing here writes to the fixture: each test copies the database into a temp dir
//! first. A fixture a test mutates passes exactly once.
//!
//! Expectations come from the JSON sidecar — `read_back` is what 0.72.0's own API
//! returned from the finished store. Nothing is re-derived from the database under
//! test, which would be a test that cannot fail.

use std::path::{Path, PathBuf};

use io_harness::{Choice, Question, Store, CHECKPOINT_FORMAT};
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.72.0")
}

fn sidecar(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.json"));
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")),
    )
    .unwrap()
}

/// A working copy of the fixture database.
fn working_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{name}.sqlite3"));
    std::fs::copy(fixtures().join(format!("{name}.sqlite3")), &db).unwrap();
    (dir, db)
}

// ---------- forwards: 0.72.0 wrote it, this release reads it ----------

/// Every `pending_questions` row a real 0.72.0 recorded reads back here identically —
/// both spellings of `choices`, the batch column, the answers and the resolution — and
/// the run it left parked is still resumable and still answerable.
#[test]
fn a_0_72_0_store_reads_back_unchanged() {
    let (_dir, db) = working_copy("questions");
    let expected = sidecar("questions");
    let store = Store::open(&db).unwrap();

    // Opening migrated nothing that matters: the format is still 7, so a 0.72.0 binary
    // is not locked out by anything this release did. Asserted against the literal, so
    // a silent bump cannot pass as a successful upgrade.
    assert_eq!(CHECKPOINT_FORMAT, 7);

    // The run itself, before its rows. A store whose questions read back but whose run
    // cannot be resumed is still a broken store.
    let run_id = expected["run_id"].as_i64().unwrap();
    store.check_resumable(run_id).unwrap();
    assert_eq!(
        store.status(run_id).unwrap().as_deref(),
        expected["status"].as_str()
    );
    assert_eq!(
        store.outcome(run_id).unwrap().as_deref(),
        expected["outcome"].as_str()
    );
    assert_eq!(
        store.last_step(run_id).unwrap(),
        expected["last_step"].as_u64().unwrap() as u32
    );

    let rows = store.questions(run_id).unwrap();
    let want = expected["read_back"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        want.len(),
        "0.72.0 wrote {} questions and this release reads {}",
        want.len(),
        rows.len()
    );

    for (row, want) in rows.iter().zip(want) {
        assert_eq!(row.id, want["id"].as_i64().unwrap());
        assert_eq!(row.run_id, want["run_id"].as_i64().unwrap());
        assert_eq!(row.step, want["step"].as_u64().unwrap() as u32);
        assert_eq!(row.question, want["question"].as_str().unwrap());
        assert_eq!(row.context.as_deref(), want["context"].as_str());
        assert_eq!(row.answer.as_deref(), want["answer"].as_str());
        assert_eq!(row.answered_by.as_deref(), want["answered_by"].as_str());
        assert_eq!(row.resolved, want["resolved"].as_bool().unwrap());

        // The offers, compared through `Choice`'s own `Serialize` — which is exactly
        // what the `choices` column holds. Field-by-field assertions would let a
        // changed wire form pass while the struct still looked right, and the wire form
        // is the thing an older binary reads.
        assert_eq!(
            serde_json::to_value(&row.choices).unwrap(),
            want["choices"],
            "row {} did not read its offers back as 0.72.0 wrote them",
            row.id
        );

        // A batch's parts, by text. `questions` is 0.72.0's own column and the rendered
        // `question` text surviving is not the same claim as the parts surviving.
        let parts: Vec<&str> = row.questions.iter().map(|q| q.question.as_str()).collect();
        let want_parts: Vec<&str> = want["batch_questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        assert_eq!(
            parts, want_parts,
            "row {} lost or reordered its batch",
            row.id
        );

        // Empty for every row, and asserted rather than skipped: 0.72.0's
        // `answer_questions` is `pub(crate)`, so the generator cannot fill this column
        // and this fixture makes no claim about it. Saying so here is the honest form
        // of that gap — the alternative is a test that quietly covers three columns
        // while its name suggests four.
        assert!(
            row.answers.is_empty(),
            "the 0.72.0 generator cannot write `answers`: {:?}",
            row.answers
        );
    }

    // A row 0.72.0 parked is still a row this release can answer. Loading it is not the
    // same claim as being able to act on it, and a store whose parked questions are
    // readable but unanswerable is still broken.
    let parked = expected["unanswered_ids"].as_array().unwrap();
    assert!(!parked.is_empty(), "the fixture must park at least one row");
    for id in parked {
        let id = id.as_i64().unwrap();
        assert!(store
            .answer_question(id, "answered under 0.73.0", "human")
            .unwrap());
        let after = store.question(id).unwrap().unwrap();
        assert!(after.resolved);
        assert_eq!(after.answer.as_deref(), Some("answered under 0.73.0"));
    }

    // The described row specifically: its descriptions and previews are the part of the
    // column no release before 0.72.0 could write, so they are the part most worth
    // pinning, and answering it above must not have disturbed them.
    let described = store
        .question(expected["described_id"].as_i64().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        described.choices.iter().any(|c| c.description.is_some())
            && described.choices.iter().any(|c| c.preview.is_some()),
        "0.72.0's object spelling lost its descriptions or previews: {:?}",
        described.choices
    );
}

// ---------- backwards: this release wrote it, 0.72.0 reads it ----------

/// A store this tree wrote is opened and read by a real 0.72.0 binary, which must lose
/// **nothing** — because this release added no column, no spelling and no value for an
/// older reader to be ignorant of. That is the claim; this is where it stops being a
/// claim.
///
/// `#[ignore]` because it needs `tests/fixtures/gen-0.72.0` built, which resolves
/// `io-harness =0.72.0` from crates.io. Run it by hand with `cargo build` in that
/// directory first, then `cargo test --test cross_version_0_72_0 -- --ignored`.
#[test]
#[ignore = "needs tests/fixtures/gen-0.72.0 built; see the module docs"]
fn a_current_store_is_read_by_a_0_72_0_binary() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.72.0/target/debug/gen-0-72-0");
    assert!(
        generator.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.72.0/Cargo.toml ({generator:?})"
    );

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.73.0.sqlite3");
    {
        let store = Store::open(&db).unwrap();
        let run = store.start_run("goal", "/repo").unwrap();
        let described = store
            .put_question(
                run,
                2,
                &Question::new("Which config should I edit?").with_choices([
                    Choice::new("io.toml").describe("committed"),
                    Choice::new("io.local.toml").preview("gitignored"),
                ]),
            )
            .unwrap();
        store
            .answer_question(described, "io.toml", "human")
            .unwrap();
        store
            .put_question(
                run,
                3,
                &Question::new("Keep it?").with_choices(["yes", "no"]),
            )
            .unwrap();
        store
            .put_questions(
                run,
                4,
                &[Question::new("which port?"), Question::new("which host?")],
            )
            .unwrap();
        store.finish_run(run, "success").unwrap();
    }

    let out = std::process::Command::new(&generator)
        .arg("read")
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "0.72.0 could not read a 0.73.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["reader"], "io-harness 0.72.0");
    // The previous release agrees about the format number, which is the cheapest
    // possible statement of "nothing locked it out".
    assert_eq!(seen["checkpoint_format"], CHECKPOINT_FORMAT);

    let run = &seen["runs"][0];
    assert_eq!(run["outcome"], "success");
    let rows = run["questions"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "0.72.0 must see all three rows: {rows:#?}");

    // The described ask, whole. Under 0.72.0's own predecessor this row read back as an
    // empty offer list, because a described choice had nowhere to live in a
    // `Vec<String>`; here there is no such loss to accept, and asserting the full
    // object spelling is what makes that a finding rather than an assumption.
    assert_eq!(rows[0]["question"], "Which config should I edit?");
    assert_eq!(rows[0]["answer"], "io.toml");
    assert_eq!(rows[0]["answered_by"], "human");
    assert_eq!(rows[0]["resolved"], true);
    assert_eq!(
        rows[0]["choices"],
        serde_json::json!([
            {"label": "io.toml", "description": "committed"},
            {"label": "io.local.toml", "preview": "gitignored"},
        ]),
        "0.72.0 must see this release's described offers exactly as it wrote its own"
    );

    // The bare-label ask, still a bare string in the column. `Choice` serializes a
    // label with nothing said about it as a plain string precisely so this row stays
    // legible to older readers; a derived `Serialize` would write `{"label": "yes"}`
    // and every offer in every question would be lost to a reader expecting strings.
    assert_eq!(rows[1]["question"], "Keep it?");
    assert_eq!(rows[1]["choices"], serde_json::json!(["yes", "no"]));

    // The batch, parts and all. 0.72.0 introduced this column, so unlike its own
    // predecessor it sees the parts rather than only the rendered text.
    assert_eq!(rows[2]["resolved"], false);
    assert_eq!(
        rows[2]["batch_questions"],
        serde_json::json!(["which port?", "which host?"])
    );
    let text = rows[2]["question"].as_str().unwrap();
    assert!(
        text.contains("which port?") && text.contains("which host?"),
        "a 0.72.0 reader must still see the whole ask: {text}"
    );
}
