//! 0.72.0 N3 and N4: the `pending_questions` change is additive and the
//! both-spellings deserializer is real, proven in both directions against a real
//! io-harness 0.71.0.
//!
//! This release changes `Question::choices` from `Vec<String>` to `Vec<Choice>` and
//! adds two nullable JSON columns to `pending_questions`. The first of those is a
//! *source* break with a **data** consequence that is easy to be wrong about and
//! silent when you are: every `pending_questions` row in the field holds `choices` as
//! a JSON array of plain strings, because that is the only spelling any release before
//! this one could write. A deserializer that understood only the object form would
//! fail to load every parked question in every existing store — a data-loss-shaped
//! defect in a release that changed no data — and it would pass a suite that only ever
//! reads rows this tree wrote.
//!
//! So the evidence has to come from the other binary:
//!
//! * **Forwards** — the fixture under `tests/fixtures/store-0.71.0/`, written by a
//!   real io-harness 0.71.0 from crates.io (the generator is
//!   `tests/fixtures/gen-0.71.0/`), reads back here with every offer intact as a
//!   `Choice` with no description, and a question 0.71.0 parked is still answerable.
//! * **Backwards** — a store *this* tree wrote, including a batched ask that fills
//!   both new columns, is read by that same 0.71.0 binary, which knows nothing about
//!   either of them. That one needs the generator built, so it is `#[ignore]` by
//!   default and CI's `cross-version-0.71.0` job runs it with `-- --ignored`.
//!
//! Nothing here writes to the fixture: each test copies the database into a temp dir
//! first. A fixture a test mutates passes exactly once.
//!
//! Expectations come from the JSON sidecar — `read_back` is what 0.71.0's own API
//! returned from the finished store, `composition` is what the generator chose.
//! Nothing is re-derived from the database under test, which would be a test that
//! cannot fail.

use std::path::{Path, PathBuf};

use io_harness::{Question, Store, CHECKPOINT_FORMAT};
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.71.0")
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

// ---------- forwards: 0.71.0 wrote it, this release reads it ----------

/// N3, forwards. Every row 0.71.0 recorded reads back identically, and its
/// string-spelled `choices` arrive as `Choice`s with no description — which is the
/// whole of the both-spellings claim, made against a store this release did not write.
#[test]
fn a_0_71_0_store_reads_its_string_choices_back_as_described_choices() {
    let (_dir, db) = working_copy("questions");
    let expected = sidecar("questions");
    let store = Store::open(&db).unwrap();

    // Opening migrated nothing that matters: the format is still 7, so a 0.71.0
    // binary is not locked out by the two added columns. Asserted directly, so a
    // silent format bump cannot pass as a successful upgrade.
    assert_eq!(CHECKPOINT_FORMAT, 7);
    let run_id = expected["run_id"].as_i64().unwrap();
    store.check_resumable(run_id).unwrap();

    let rows = store.questions(run_id).unwrap();
    let want = expected["read_back"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        want.len(),
        "0.71.0 wrote {} questions and this release reads {}",
        want.len(),
        rows.len()
    );

    for (row, want) in rows.iter().zip(want) {
        assert_eq!(row.id, want["id"].as_i64().unwrap());
        assert_eq!(row.step, want["step"].as_u64().unwrap() as u32);
        assert_eq!(row.question, want["question"].as_str().unwrap());
        assert_eq!(row.context.as_deref(), want["context"].as_str());
        assert_eq!(row.answer.as_deref(), want["answer"].as_str());
        assert_eq!(row.answered_by.as_deref(), want["answered_by"].as_str());
        assert_eq!(row.resolved, want["resolved"].as_bool().unwrap());

        // The claim. 0.71.0 wrote an array of strings; this release reads the same
        // labels in the same order, each as a `Choice` carrying neither optional
        // field — not an error, not an empty list, and not a silent drop.
        let wrote: Vec<&str> = want["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().expect("0.71.0 could only write strings"))
            .collect();
        let read: Vec<&str> = row.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(read, wrote, "row {} lost or reordered its offers", row.id);
        assert!(
            row.choices
                .iter()
                .all(|c| c.description.is_none() && c.preview.is_none()),
            "a 0.71.0 row cannot carry a description or a preview: {:?}",
            row.choices
        );

        // Both columns this release added are empty for a row written before they
        // existed, which is what "NULL for a singular ask" has to mean in practice.
        assert!(row.questions.is_empty() && row.answers.is_empty());
    }

    // A row 0.71.0 parked is still a row this release can answer. Loading it is not
    // the same claim as being able to act on it, and a store whose parked questions
    // are readable but unanswerable is still broken.
    let parked = expected["unanswered_ids"].as_array().unwrap();
    assert!(!parked.is_empty(), "the fixture must park at least one row");
    for id in parked {
        let id = id.as_i64().unwrap();
        assert!(store.answer_question(id, "answered under 0.72.0", "human").unwrap());
        let after = store.question(id).unwrap().unwrap();
        assert!(after.resolved);
        assert_eq!(after.answer.as_deref(), Some("answered under 0.72.0"));
        // Answering did not disturb the offers it was answering against.
        assert!(!after.choices.is_empty());
    }
}

/// N3, the negative control. A row whose `choices` column is the object spelling this
/// release writes reads its descriptions back — so the assertion above is passing
/// because 0.71.0's rows really are the string spelling, not because the reader throws
/// every description away.
#[test]
fn the_object_spelling_is_not_being_silently_flattened() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("new.sqlite3")).unwrap();
    let run = store.start_run("goal", "provider").unwrap();
    let id = store
        .put_question(
            run,
            1,
            &Question::new("Which?").with_choices([
                io_harness::Choice::new("a").describe("the cheap one"),
                io_harness::Choice::new("b").preview("cost = 2"),
            ]),
        )
        .unwrap();

    let row = store.question(id).unwrap().unwrap();
    assert_eq!(row.choices[0].description.as_deref(), Some("the cheap one"));
    assert_eq!(row.choices[1].preview.as_deref(), Some("cost = 2"));
}

// ---------- backwards: this release wrote it, 0.71.0 reads it ----------

/// N4. A store this tree wrote — including a batched ask, which fills both of the
/// columns 0.71.0 has never heard of — is opened and read by a real 0.71.0 binary,
/// whose `choices` reader is `Vec<String>` and whose queries never name the new
/// columns.
///
/// `#[ignore]` because it needs `tests/fixtures/gen-0.71.0` built, which resolves
/// `io-harness =0.71.0` from crates.io. CI's `cross-version-0.71.0` job builds it and
/// runs this with `-- --ignored`; running it by hand is `cargo build` in that
/// directory first.
#[test]
#[ignore = "needs tests/fixtures/gen-0.71.0 built; CI's cross-version-0.71.0 job runs it"]
fn a_current_store_is_read_by_a_0_71_0_binary() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.71.0/target/debug/gen-0-71-0");
    assert!(
        generator.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.71.0/Cargo.toml ({generator:?})"
    );

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.72.0.sqlite3");
    {
        let store = Store::open(&db).unwrap();
        let run = store.start_run("goal", "/repo").unwrap();
        // A singular ask with described choices: the `choices` column now holds the
        // object spelling, which 0.71.0 reads with `serde_json::from_str::<Vec<String>>`
        // and therefore cannot parse. Its reader falls back to the empty list rather
        // than failing the row, and that degradation — the offers invisible, the
        // question and its answer intact — is precisely what this test pins down.
        let described = store
            .put_question(
                run,
                2,
                &Question::new("Which config should I edit?")
                    .with_choices([io_harness::Choice::new("io.toml").describe("committed")]),
            )
            .unwrap();
        store.answer_question(described, "io.toml", "human").unwrap();
        // A plain ask, whose column is byte-identical to what 0.71.0 would write.
        store
            .put_question(run, 3, &Question::new("Keep it?").with_choices(["yes", "no"]))
            .unwrap();
        // A batch: one row, both new columns populated. The backwards claim for this
        // release is that these cost a 0.71.0 reader nothing, and it is executed here
        // rather than argued.
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
        "0.71.0 could not read a 0.72.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["reader"], "io-harness 0.71.0");

    let rows = seen["runs"][0]["questions"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "0.71.0 must see all three rows: {rows:#?}");

    // The described ask, and the ONE thing a 0.71.0 reader loses. Its text, its answer
    // and its attribution survive; the offers are opaque, because a described choice
    // has nowhere to live in a `Vec<String>` and this release will not write a lie into
    // the column to pretend otherwise.
    assert_eq!(rows[0]["question"], "Which config should I edit?");
    assert_eq!(rows[0]["answer"], "io.toml");
    assert_eq!(rows[0]["answered_by"], "human");
    assert_eq!(rows[0]["resolved"], true);
    assert_eq!(
        rows[0]["choices"],
        serde_json::json!([]),
        "a described offer is the one thing an older reader cannot see"
    );

    // The plain ask is FULLY legible, and this is the assertion that earns its keep.
    // A derived `Serialize` on `Choice` writes `{{\"label\": \"yes\"}}` for a bare label
    // too, and this row then read back as `[]` — every offer in every question lost to
    // a 0.71.0 binary, not merely the described ones. `Choice` serializes a bare label
    // as a bare string precisely so this row survives.
    assert_eq!(rows[1]["question"], "Keep it?");
    assert_eq!(rows[1]["choices"], serde_json::json!(["yes", "no"]));

    // The batch. 0.71.0 never names `questions` or `answers`, so it sees one ordinary
    // unresolved question whose text is the whole ask — readable, and answerable by an
    // older binary rather than a row it must skip.
    assert_eq!(rows[2]["resolved"], false);
    let text = rows[2]["question"].as_str().unwrap();
    assert!(
        text.contains("which port?") && text.contains("which host?"),
        "a 0.71.0 reader must still see the whole ask: {text}"
    );
}
