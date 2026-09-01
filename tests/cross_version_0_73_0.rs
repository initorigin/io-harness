//! 0.74.0's cross-version fixture: a store written by a real io-harness 0.73.0 from
//! crates.io is read by this tree, and a store this tree writes is read by that same
//! 0.73.0 binary.
//!
//! **This test is expected to pass, and it is expected to pass for a reason worth
//! stating plainly rather than dressing up.** 0.74.0 is a security release that changes
//! no store column, no serialized store value and not `CHECKPOINT_FORMAT`: the sessions
//! module quotes the SQL identifiers it composes, and the trace database is created
//! with a tighter file mode. A quoted identifier names the same table an unquoted one
//! did, and a file's mode is not a column — so neither is supposed to reach a persisted
//! byte. This file is not hunting a break; it is the evidence for the claim that there
//! is none.
//!
//! That is the rule 0.72.0 wrote for itself, and it wrote it by being wrong. 0.72.0
//! changed `Question::choices` from `Vec<String>` to `Vec<Choice>` believing it
//! additive; its entire suite was green, because every test in it read rows the same
//! tree had just written; and only a real `io-harness =0.71.0` reading a store the
//! working tree wrote could see the defect. A release's own belief that it touched
//! nothing persisted is worth exactly as much as the previous binary that checks it.
//!
//! What that rule buys *this* release is specific, because 0.74.0 is the first since
//! 0.72.0 to touch the state layer at all, and it touches it twice:
//!
//! * **The sessions module composes SQL identifiers.** `session_size`,
//!   `archive_session` and `delete_session` build their statements by interpolating
//!   table and column names read out of the schema. Quoting them is the safe form of
//!   what was already there, and the failure mode of getting it wrong is not a
//!   compile error: it is a statement that names a table nobody has, or one that starts
//!   interpolating a *value* where it used to bind one. Both are silent against rows
//!   this tree just wrote and loud against a store it did not.
//! * **The trace database is created with a tighter file mode.** A store that already
//!   exists was created under the old mode, and a change that refused to open it would
//!   lock every operator out of their own history. So the forwards half deliberately
//!   loosens the working copy to `0o644` before opening it — see `working_copy`.
//!
//! Two halves, as in `cross_version_0_72_0.rs`:
//!
//! * **Forwards** — the fixture under `tests/fixtures/store-0.73.0/`, written by a real
//!   0.73.0 (the generator is `tests/fixtures/gen-0.73.0/`), reads back here row for
//!   row: the session keeps its root, its head, its parentage and its turns, both runs
//!   keep every trace column and their canonical traces, the sweep still refuses the
//!   session that holds a resumable run, and the archive and the delete still compose
//!   statements that run. Runs everywhere, on every OS.
//! * **Backwards** — a store *this* tree wrote is read by that 0.73.0 binary, which
//!   must lose nothing at all, since this release added nothing for it to be ignorant
//!   of. That one needs the generator built, so it is `#[ignore]` by default and CI's
//!   `cross-version-0.73.0` job runs it with `-- --ignored`.
//!
//! Nothing here writes to the fixture: each test copies the database into a temp dir
//! first. A fixture a test mutates passes exactly once, and two of the three forwards
//! tests mutate on purpose.
//!
//! Expectations come from the JSON sidecar — every value in it is what 0.73.0's own API
//! returned from the finished store. Nothing is re-derived from the database under
//! test, which would be a test that cannot fail.

use std::path::{Path, PathBuf};

use io_harness::{StepRecord, Store, CHECKPOINT_FORMAT};
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store-0.73.0")
}

fn sidecar(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.json"));
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")),
    )
    .unwrap()
}

/// A working copy of the fixture database, in the mode a 0.73.0 store was written in.
///
/// The mode is set rather than inherited. `std::fs::copy` carries the source file's
/// permissions, but git records only the execute bit, so a fresh checkout's mode is
/// whatever umask the machine running the test happens to have — which would make the
/// half of this file that is about a *file mode* change assert something different on
/// every host. `0o644` is what a 0.73.0 `Store::open` left behind on a default umask:
/// world-readable, which is the thing 0.74.0 stops doing to new stores and must keep
/// tolerating in old ones. An operator's existing history is not theirs to lock.
fn working_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{name}.sqlite3"));
    let from = fixtures().join(format!("{name}.sqlite3"));
    // The fixture is written by the pinned generator, never by this tree, so a missing
    // one is a step that was skipped rather than a file that was lost. Say which step.
    std::fs::copy(&from, &db).unwrap_or_else(|e| {
        panic!(
            "{from:?}: {e}\nregenerate it with:\n  cargo build --manifest-path \
             tests/fixtures/gen-0.73.0/Cargo.toml\n  \
             tests/fixtures/gen-0.73.0/target/debug/gen-0-73-0 write tests/fixtures/store-0.73.0"
        )
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    (dir, db)
}

/// The ids of a JSON array of integers.
fn ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

// ---------- forwards: 0.73.0 wrote it, this release reads it ----------

/// Every session row and every trace row a real 0.73.0 recorded reads back here
/// identically — the conversation's shape, both runs' columns, their canonical traces,
/// the size the sessions module measures across the tree, and the sweep's refusal — and
/// the run 0.73.0 left parked is still resumable.
#[test]
fn a_0_73_0_store_reads_back_unchanged() {
    let (_dir, db) = working_copy("state");
    let expected = sidecar("state");

    // The store is world-readable, because 0.73.0 wrote it that way. Opening it is the
    // whole assertion for the file-mode half: a release that tightens the mode of the
    // stores it *creates* must still open the ones it did not. What mode this release
    // leaves the file in afterwards is deliberately not asserted here — that is the
    // new behaviour's own gate, and pinning it in the cross-version file would make
    // this test fail for a reason that has nothing to do with 0.73.0.
    let store = Store::open(&db).unwrap();

    // Opening migrated nothing that matters: the format is still 7, so a 0.73.0 binary
    // is not locked out by anything this release did. Asserted against the literal, so
    // a silent bump cannot pass as a successful upgrade.
    assert_eq!(CHECKPOINT_FORMAT, 7);

    // ---- the conversation ----

    let session = expected["session_id"].as_i64().unwrap();
    assert_eq!(
        store.session_root(session).unwrap().as_deref(),
        expected["session_root"].as_str(),
        "the session lost the workspace it was opened over"
    );
    assert_eq!(
        store.session_created_at(session).unwrap().as_deref(),
        expected["session_created_at"].as_str(),
        "the stamp a sweep is compared against is not the stamp 0.73.0 wrote"
    );
    assert_eq!(
        store.session_head(session).unwrap(),
        expected["session_head"].as_i64(),
        "the session is answering from a different turn than 0.73.0 left it on"
    );

    let turns = store.session_turns(session).unwrap();
    let want = expected["turns"].as_array().unwrap();
    assert_eq!(
        turns.len(),
        want.len(),
        "0.73.0 wrote {} turns and this release reads {}",
        want.len(),
        turns.len()
    );
    for (turn, want) in turns.iter().zip(want) {
        assert_eq!(turn.id, want["id"].as_i64().unwrap());
        assert_eq!(turn.session_id, want["session_id"].as_i64().unwrap());
        // `None` for the root of the conversation and the first turn's id for the
        // second. A flat list would read both back as `None` and lose the shape.
        assert_eq!(turn.parent_turn_id, want["parent_turn_id"].as_i64());
        assert_eq!(turn.run_id, want["run_id"].as_i64().unwrap());
        assert_eq!(turn.prompt, want["prompt"].as_str().unwrap());
        assert_eq!(turn.reply.as_deref(), want["reply"].as_str());
        assert_eq!(turn.outcome.as_deref(), want["outcome"].as_str());
        assert_eq!(turn.created_at, want["created_at"].as_str().unwrap());
        // The seam the run loop and the session meet at: a turn whose run cannot be
        // found from its run id is a turn a resume cannot pick up.
        assert_eq!(store.turn_for_run(turn.run_id).unwrap(), Some(turn.id));
    }

    // The prompt carrying a double quote, a single quote, a backslash and a bare SQL
    // keyword, asserted by itself. It is in the loop above too, but only as one of
    // several strings; named here because it is the row that fails first if a statement
    // that used to bind a value started interpolating one.
    assert_eq!(
        turns[0].prompt,
        expected["awkward_prompt"].as_str().unwrap(),
        "a value shaped like an identifier did not survive"
    );

    // ---- the trace ----

    for want in expected["runs"].as_array().unwrap() {
        let run_id = want["id"].as_i64().unwrap();
        assert_eq!(
            store.status(run_id).unwrap().as_deref(),
            want["status"].as_str()
        );
        assert_eq!(
            store.outcome(run_id).unwrap().as_deref(),
            want["outcome"].as_str()
        );
        assert_eq!(
            store.last_step(run_id).unwrap(),
            want["last_step"].as_u64().unwrap() as u32
        );

        let steps = store.steps(run_id).unwrap();
        let want_steps = want["steps"].as_array().unwrap();
        assert_eq!(
            steps.len(),
            want_steps.len(),
            "run {run_id}: 0.73.0 wrote {} steps and this release reads {}",
            want_steps.len(),
            steps.len()
        );
        for (step, want) in steps.iter().zip(want_steps) {
            // Every column, not a summary of them. A step whose `tokens` survived and
            // whose `tool_call` did not is a store that lost data, and a comparison of
            // the parts that are easy to compare would pass.
            assert_eq!(step.step, want["step"].as_u64().unwrap() as u32);
            assert_eq!(step.decision, want["decision"].as_str().unwrap());
            assert_eq!(step.result, want["result"].as_str().unwrap());
            assert_eq!(step.prompt, want["prompt"].as_str().unwrap());
            assert_eq!(step.tool_call, want["tool_call"].as_str().unwrap());
            assert_eq!(step.tokens, want["tokens"].as_u64().unwrap());
        }

        // The crate's own answer to "is this the same trace", over the same run.
        // A reader that got every column right and the ordering wrong still fails here.
        assert_eq!(
            store.canonical_trace(run_id).unwrap(),
            want["canonical_trace"].as_str().unwrap(),
            "run {run_id} does not render the trace 0.73.0 rendered"
        );

        // 0.73.0's own verdict on resumability, not this test's guess at it.
        // `check_resumable` is the call that compares the file's `user_version` against
        // `CHECKPOINT_FORMAT`, so it is where a silent format bump would surface as a
        // refusal rather than as a wrong number.
        assert_eq!(
            store.check_resumable(run_id).is_ok(),
            want["resumable"].as_bool().unwrap(),
            "run {run_id} disagrees with 0.73.0 about whether it can be resumed"
        );
    }

    // A store whose rows read back but whose parked run cannot be picked up is still
    // broken, so the parked run is named and checked rather than left to the loop.
    store
        .check_resumable(expected["parked_run"].as_i64().unwrap())
        .unwrap();

    // ---- what the sessions module composes ----

    // The read half of the identifier composition this release rewrites: one
    // `SELECT ... FROM {table} WHERE {key} IN (...)` per table in the session's tree.
    // Compared exactly, because 0.74.0 adds no column and no row — a figure that moved
    // is a finding, not a tolerance to widen.
    let size = store
        .session_size(session)
        .unwrap()
        .expect("the session is there");
    let want = &expected["session_size"];
    assert_eq!(size.session_id, want["session_id"].as_i64().unwrap());
    assert_eq!(size.turns, want["turns"].as_u64().unwrap());
    assert_eq!(size.runs, want["runs"].as_u64().unwrap());
    assert_eq!(size.rows, want["rows"].as_u64().unwrap());
    assert_eq!(size.bytes, want["bytes"].as_u64().unwrap());

    // And the decision half. The session holds a run 0.73.0 left `running`, so a cutoff
    // far past its stamp refuses it rather than taking it — a sweep that started
    // *removing* these sessions would be the worst possible way for this release to
    // change behaviour, and it would be invisible to a preview nobody compared.
    let preview = store
        .sweep_preview(expected["sweep_cutoff"].as_str().unwrap())
        .unwrap();
    let refused = ids(&expected["sweep"]["refused"]);
    assert!(
        !refused.is_empty(),
        "the fixture must hold a resumable run for the refusal to mean anything"
    );
    assert_eq!(
        preview.sessions,
        expected["sweep"]["sessions"].as_u64().unwrap()
    );
    assert_eq!(preview.refused, refused);
}

/// The `UPDATE` half of the identifier composition, run against a store this tree did
/// not write. `archive_session` clears the words and keeps the facts by interpolating
/// column names it read out of the schema — the statement most exposed to a quoting
/// change — and it must still empty exactly the columns 0.73.0 would have emptied.
#[test]
fn a_0_73_0_session_still_archives_its_words_and_keeps_its_facts() {
    let (_dir, db) = working_copy("state");
    let expected = sidecar("state");
    let store = Store::open(&db).unwrap();
    let session = expected["session_id"].as_i64().unwrap();

    let before = store.session_turns(session).unwrap();
    assert!(
        before.iter().any(|t| !t.prompt.is_empty()),
        "there must be words to clear, or the archive below asserts nothing"
    );

    let archived = store.archive_session(session).unwrap();
    assert_eq!(
        archived.turns,
        expected["session_size"]["turns"].as_u64().unwrap(),
        "an archive keeps the conversation's shape"
    );
    assert!(
        archived.rows > 0 && archived.bytes > 0,
        "the archive cleared nothing at all: {archived:?}"
    );

    // The words are gone and the facts are not. Both halves matter: an archive that
    // cleared everything would pass a test that only checked the prompt.
    let after = store.session_turns(session).unwrap();
    assert_eq!(after.len(), before.len(), "an archive removes no turn");
    for (now, was) in after.iter().zip(&before) {
        assert_eq!(now.id, was.id);
        assert_eq!(now.run_id, was.run_id);
        assert_eq!(now.parent_turn_id, was.parent_turn_id);
        assert_eq!(now.created_at, was.created_at, "a stamp is a fact");
        assert_eq!(now.outcome, was.outcome, "an outcome is a fact");
        assert!(now.prompt.is_empty(), "the prompt is still there");
        // A cleared column is `''` and an unfinished turn's was `NULL`; neither holds
        // words, and the assertion is about the words rather than about which of the
        // two empty spellings the row landed on.
        assert!(now.reply.as_deref().unwrap_or_default().is_empty());
    }
    assert_eq!(
        store.session_root(session).unwrap().as_deref(),
        expected["session_root"].as_str(),
        "the root is a fact and an archive keeps it"
    );

    // Idempotent, and visibly so. This is where a mis-composed `UPDATE` shows even if
    // the first pass looked right: a statement that matched no row reports the same
    // bytes twice, and one that matched the wrong rows keeps finding more.
    let second = store.archive_session(session).unwrap();
    assert_eq!(second.rows, 0);
    assert_eq!(second.bytes, 0);
    assert_eq!(second.turns, archived.turns);
}

/// The `DELETE` half, over the same store. `delete_session` removes the session, its
/// turns and every row keyed to the runs in its tree by interpolating a table name per
/// statement — so a quoting change that named a table wrong would leave rows behind
/// rather than fail, which is the shape of an orphan nothing ever mentions again.
#[test]
fn a_0_73_0_session_still_deletes_with_its_whole_tree() {
    let (_dir, db) = working_copy("state");
    let expected = sidecar("state");
    let store = Store::open(&db).unwrap();
    let session = expected["session_id"].as_i64().unwrap();
    let runs: Vec<i64> = expected["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();

    let pruned = store.delete_session(session).unwrap();
    assert_eq!(pruned.sessions, 1);
    assert_eq!(
        pruned.turns,
        expected["session_size"]["turns"].as_u64().unwrap()
    );
    assert_eq!(
        pruned.runs,
        expected["session_size"]["runs"].as_u64().unwrap(),
        "the delete walks the session's tree, not just the runs its turns drove"
    );
    assert!(pruned.rows > 0 && pruned.bytes > 0, "{pruned:?}");
    // Naming a session is a decision somebody made, so the resumable-run refusal that
    // guards the *sweep* does not apply here — the same run the sweep refused above.
    assert!(
        pruned.refused.is_empty(),
        "naming a session overrides the refusal a date-driven sweep respects"
    );

    assert_eq!(store.session_root(session).unwrap(), None);
    assert!(store.session_turns(session).unwrap().is_empty());
    for run in runs {
        // The cross-table claim. The turns going is the easy half; the trace rows keyed
        // to the runs those turns drove are what a statement naming the wrong table
        // would silently strand.
        assert!(
            store.steps(run).unwrap().is_empty(),
            "run {run} kept its trace rows after its session was deleted"
        );
    }
}

// ---------- backwards: this release wrote it, 0.73.0 reads it ----------

/// A store this tree wrote is opened and read by a real 0.73.0 binary, which must lose
/// **nothing** — because this release added no column, no spelling and no value for an
/// older reader to be ignorant of. That is the claim; this is where it stops being a
/// claim.
///
/// `#[ignore]` because it needs `tests/fixtures/gen-0.73.0` built, which resolves
/// `io-harness =0.73.0` from crates.io. CI's `cross-version-0.73.0` job builds it and
/// runs this with `-- --ignored`; running it by hand is `cargo build` in that directory
/// first, then `cargo test --test cross_version_0_73_0 -- --ignored`.
#[test]
#[ignore = "needs tests/fixtures/gen-0.73.0 built; CI's cross-version-0.73.0 job runs it"]
fn a_current_store_is_read_by_a_0_73_0_binary() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.73.0/target/debug/gen-0-73-0");
    assert!(
        generator.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.73.0/Cargo.toml ({generator:?})"
    );

    let expected = sidecar("state");
    let awkward = expected["awkward_prompt"].as_str().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("written-by-0.74.0.sqlite3");
    let session;
    {
        let store = Store::open(&db).unwrap();
        session = store.create_session("fixture-workspace").unwrap();

        // A finished turn, whose prompt is the value shaped like an identifier. If this
        // release's quoting change started interpolating what it used to bind, this is
        // the string that arrives at the older reader mangled or not at all.
        let first_run = store
            .start_run("port the parser", "fixture-workspace")
            .unwrap();
        let first = store
            .record_turn(session, None, first_run, awkward)
            .unwrap();
        store
            .record(
                first_run,
                &StepRecord::new(1, "read the schema", "seven tables name a run").with_trace(
                    "what does the schema hold?",
                    "",
                    512,
                ),
            )
            .unwrap();
        store.finish_run(first_run, "success").unwrap();
        store
            .finish_turn(first, Some("Renamed it."), "success")
            .unwrap();

        // A turn still answering, parented on the first: the run stays `running`, which
        // is what the older binary's sweep must still refuse.
        let second_run = store
            .start_run("port the parser", "fixture-workspace")
            .unwrap();
        let second = store
            .record_turn(
                session,
                Some(first),
                second_run,
                "And the trace file's mode?",
            )
            .unwrap();
        store
            .record(
                second_run,
                &StepRecord::new(1, "read the trace", "the file is world-readable").with_trace(
                    "check the mode",
                    "",
                    256,
                ),
            )
            .unwrap();
        store.set_session_head(session, Some(second)).unwrap();
    }

    let out = std::process::Command::new(&generator)
        .arg("read")
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "0.73.0 could not read a 0.74.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["reader"], "io-harness 0.73.0");
    // The previous release agrees about the format number, which is the cheapest
    // possible statement of "nothing locked it out".
    assert_eq!(seen["checkpoint_format"], CHECKPOINT_FORMAT);

    let seen_session = &seen["sessions"][0];
    assert_eq!(seen_session["session_id"], session);
    assert_eq!(seen_session["root"], "fixture-workspace");

    let turns = seen_session["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "0.73.0 must see both turns: {turns:#?}");
    assert_eq!(
        turns[0]["prompt"], awkward,
        "a value shaped like an identifier did not reach the older reader intact"
    );
    assert_eq!(turns[0]["reply"], "Renamed it.");
    assert_eq!(turns[0]["outcome"], "success");
    assert_eq!(turns[0]["parent_turn_id"], Value::Null);
    // The conversation's shape, which is the one thing a reader that saw every row and
    // no parentage would still have lost.
    assert_eq!(turns[1]["parent_turn_id"], turns[0]["id"]);
    assert_eq!(seen_session["head"], turns[1]["id"]);

    // The trace under the finished turn, through the older binary's own reader.
    let steps = seen_session["runs"][0]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["decision"], "read the schema");
    assert_eq!(steps[0]["tool_call"], "");
    assert_eq!(steps[0]["tokens"], 512);
    assert_eq!(seen_session["runs"][0]["outcome"], "success");
    assert_eq!(seen_session["runs"][1]["status"], "running");
    assert_eq!(seen_session["runs"][1]["resumable"], true);

    // What 0.73.0's own `session_size` measures over a store this tree wrote — the
    // statements whose identifiers this release quotes, composed by the binary that
    // does not quote them, over rows the quoting release produced.
    assert_eq!(seen_session["size"]["turns"], 2);
    assert_eq!(seen_session["size"]["runs"], 2);
    assert!(seen_session["size"]["bytes"].as_u64().unwrap() > 0);

    // And its sweep still refuses the session, because it still holds a running run.
    assert_eq!(
        seen["sweep"]["refused"],
        serde_json::json!([session]),
        "0.73.0 must still refuse to sweep a session holding a resumable run"
    );
}
