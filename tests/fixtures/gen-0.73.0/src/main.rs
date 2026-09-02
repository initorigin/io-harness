//! Writes — and reads back — the fixture store that `tests/cross_version_0_73_0.rs`
//! uses to prove 0.74.0 and 0.73.0 read each other's stores, using a real io-harness
//! 0.73.0 from crates.io.
//!
//! The point of the crate is that the *other* binary is the previous release, not this
//! tree with a 0.73.0 label on it. 0.74.0 is a security release that changes no store
//! column and no serialized store value: the sessions module quotes the SQL identifiers
//! it composes, and the trace database is created with a tighter file mode. Neither is
//! supposed to reach a persisted byte, so both directions here are *expected* to pass.
//! That is the point. 0.72.0 also believed its own change was additive, its whole suite
//! agreed, and only a real previous-release binary showed otherwise; a belief that a
//! release touched no persisted surface is worth exactly as much as the binary that
//! checks it.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-73-0 write <output-dir>   # produce the committed fixture
//! gen-0-73-0 read  <database>     # print what 0.73.0 can see in a store, as JSON
//! ```
//!
//! `write` produces one database, `state.sqlite3`, holding a session and the trace of
//! the runs that served it — the two surfaces 0.74.0 moves — chosen so that every
//! statement whose identifiers this release rewrites has a row to run against:
//!
//! * **a session with two turns**, one finished and one still answering, so
//!   `session_turns`, `session_head`, `turn_for_run` and the parent link all have a
//!   value rather than a default to read back;
//! * **two runs**, one completed and one left `running`, because `session_size` and the
//!   sweep both walk a session's *tree* of runs and a single finished run exercises
//!   neither the walk nor the resumable-run refusal;
//! * **steps on both runs**, which is what puts rows in the trace tables that
//!   `archive_session` composes an `UPDATE` over and `delete_session` a `DELETE` over —
//!   the two statements 0.74.0 rewrites;
//! * **a prompt carrying a double quote, a single quote, a backslash and a bare SQL
//!   keyword**. Identifier quoting is where a value that looks like an identifier stops
//!   being harmless, and a fixture whose text is all lowercase words could not tell a
//!   correctly quoted statement from one that started interpolating what it should
//!   bind.
//!
//! The run serving the second turn is left **unfinished**, because a resumable run is
//! the interesting thing to hand to a newer binary twice over: a store whose rows read
//! back but whose run cannot be resumed is still broken, and the sweep's refusal only
//! has something to refuse while a session holds one.
//!
//! The sidecar is the specification. Every expectation in it is what 0.73.0's own API
//! returned from the finished store, so a 0.74.0 reader is compared against the
//! previous release's answers rather than against its own. `resumable` is recorded per
//! run rather than assumed, so the test asserts 0.73.0's verdict instead of guessing at
//! it.
//!
//! Fully offline and deterministic: no run loop, no provider, no network, no API key,
//! no wall-clock value in any expectation that is not read straight back out of the
//! committed file.

use io_harness::{StepRecord, Store};
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one useful
/// behaviour — print why and exit non-zero — so there is nothing for a typed error to
/// decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The contract root every run records, and the session's own root. Relative on
/// purpose: `runs.file` and `sessions.root` store it verbatim, so an absolute path
/// would bake this machine's home directory into a committed fixture.
const WORKSPACE: &str = "fixture-workspace";

/// The goal every run records. Fixed rather than absent: a `NULL` and a value that
/// round-tripped are different facts, and only the second proves the column survived.
const GOAL: &str = "port the parser";

/// The cutoff the sweep preview is taken at. Far enough forward that the fixture's own
/// `created_at` is strictly before it whenever the fixture is regenerated, so what the
/// preview reports is the *refusal*, not an empty selection that would report the same
/// thing for the wrong reason.
const SWEEP_CUTOFF: &str = "2999-01-01T00:00:00.000Z";

/// The first turn's prompt: a double quote, a single quote, a backslash and a bare SQL
/// keyword in one string. Identifier quoting is the change under test, and a value
/// shaped like an identifier is where a statement that interpolates what it should bind
/// stops being invisible.
const AWKWARD_PROMPT: &str =
    r#"Rename the "order" column — it's a keyword — and check C:\repo\io.toml"#;

/// Everything 0.73.0 can see of one session turn, as JSON.
///
/// Shared by `write` and `read` so the sidecar and the backwards half describe a turn
/// the same way, and a difference between them is a difference in the data.
fn turn(t: &io_harness::Turn) -> Value {
    json!({
        "id": t.id,
        "session_id": t.session_id,
        "parent_turn_id": t.parent_turn_id,
        "run_id": t.run_id,
        "prompt": t.prompt,
        "reply": t.reply,
        "outcome": t.outcome,
        "created_at": t.created_at,
    })
}

/// Everything 0.73.0 can see of one trace step, as JSON. Every column of the row, not a
/// summary of it: a step whose `tokens` survived and whose `tool_call` did not is a
/// store that lost data, and a comparison of the parts that are easy to compare would
/// pass.
fn step(s: &StepRecord) -> Value {
    json!({
        "step": s.step,
        "decision": s.decision,
        "result": s.result,
        "prompt": s.prompt,
        "tool_call": s.tool_call,
        "tokens": s.tokens,
    })
}

/// Everything 0.73.0 can see of one run and its trace, as JSON.
fn run(store: &Store, run_id: i64) -> Res<Value> {
    Ok(json!({
        "id": run_id,
        "status": store.status(run_id)?,
        "outcome": store.outcome(run_id)?,
        "last_step": store.last_step(run_id)?,
        "steps": store.steps(run_id)?.iter().map(step).collect::<Vec<_>>(),
        // The canonical trace is one string over every step and context event, and it
        // is the crate's own answer to "is this the same trace" — see
        // `tests/determinism.rs`. Recorded whole so a reader that got every column
        // right and the ordering wrong still fails.
        "canonical_trace": store.canonical_trace(run_id)?,
        // 0.73.0's verdict, not the fixture's guess. `check_resumable` compares the
        // file's `user_version` against `CHECKPOINT_FORMAT`, so it is the one call that
        // fails first if a release bumps the format — recording the answer here is what
        // lets the test assert the verdict rather than assume it.
        "resumable": store.check_resumable(run_id).is_ok(),
    }))
}

/// Write `state.sqlite3` and its sidecar into the current directory.
fn state() -> Res<()> {
    let store = Store::open("state.sqlite3")?;
    let session = store.create_session(WORKSPACE)?;

    // A turn that finished. Its run is completed, so it is the control for the parked
    // one below: the sweep's refusal must be about the *running* run and not about any
    // run at all.
    let first_run = store.start_run(GOAL, WORKSPACE)?;
    let first = store.record_turn(session, None, first_run, AWKWARD_PROMPT)?;
    store.record(
        first_run,
        &StepRecord::new(1, "read the schema", "seven tables name a run").with_trace(
            "what does the schema hold?",
            "",
            512,
        ),
    )?;
    store.record(
        first_run,
        &StepRecord::new(2, "wrote io.toml", "written").with_trace(
            "apply the rename",
            r#"{"name":"write_file","input":{"path":"io.toml"}}"#,
            768,
        ),
    )?;
    store.finish_run(first_run, "success")?;
    store.finish_turn(
        first,
        Some("Renamed it, and the keyword is quoted."),
        "success",
    )?;

    // A turn still answering when the process stopped: its run stays `running`, which
    // is what makes the session resumable and therefore what the sweep must refuse.
    // Parented on the first, so the conversation has a shape a flat list would lose.
    let second_run = store.start_run(GOAL, WORKSPACE)?;
    let second = store.record_turn(
        session,
        Some(first),
        second_run,
        "And the trace file's mode?",
    )?;
    store.record(
        second_run,
        &StepRecord::new(1, "read the trace", "the file is world-readable").with_trace(
            "check the mode",
            "",
            256,
        ),
    )?;
    store.set_session_head(session, Some(second))?;

    let preview = store.sweep_preview(SWEEP_CUTOFF)?;
    let size = store
        .session_size(session)?
        .ok_or("the session was just written and must have a size")?;

    sidecar(
        "state.json",
        &json!({
            "writer": "io-harness 0.73.0",
            "session_id": session,
            "session_root": store.session_root(session)?,
            "session_created_at": store.session_created_at(session)?,
            "session_head": store.session_head(session)?,
            "goal": GOAL,
            "workspace": WORKSPACE,
            "awkward_prompt": AWKWARD_PROMPT,
            "sweep_cutoff": SWEEP_CUTOFF,
            "turns": store.session_turns(session)?.iter().map(turn).collect::<Vec<_>>(),
            "runs": [run(&store, first_run)?, run(&store, second_run)?],
            // The parked run, named rather than derived: a test that looked for "the
            // run that is still running" would find whatever the store happened to
            // hold and prove nothing about what this generator meant to park.
            "parked_run": second_run,
            // What `session_size` measured, which is the read half of the identifier
            // composition 0.74.0 rewrites — `SELECT ... FROM {table} WHERE {key} IN
            // (...)`, once per table in the session's tree.
            "session_size": {
                "session_id": size.session_id,
                "turns": size.turns,
                "runs": size.runs,
                "rows": size.rows,
                "bytes": size.bytes,
            },
            // And what the sweep decided at a cutoff far past the session's stamp: it
            // takes nothing, because the session holds a resumable run.
            "sweep": {
                "sessions": preview.sessions,
                "refused": preview.refused,
            },
        }),
    )?;

    drop(store);
    Ok(())
}

// ---------- reading a store back, as 0.73.0 sees it ----------

/// Print everything 0.73.0 can see of `db`'s sessions and traces, as JSON on stdout.
///
/// The backwards half runs this against a database the *current* tree wrote. 0.74.0
/// claims to have changed nothing this binary reads — a quoted identifier composes the
/// same statement an unquoted one did, and a file's mode is not a column — so a clean
/// read here, with every turn, step and byte count intact, is the evidence for that
/// claim rather than the claim itself. Anything that opens and selects is fair game;
/// nothing here writes.
fn read(db: &str) -> Res<()> {
    let store = Store::open(db)?;
    let mut sessions = Vec::new();
    // Session ids are dense from 1 in a fixture database, and a session that is not
    // there is skipped rather than fatal — this mode must be able to read a store it
    // did not write.
    for session_id in 1..=64 {
        let turns = store.session_turns(session_id)?;
        if turns.is_empty() {
            continue;
        }
        let size = store.session_size(session_id)?;
        sessions.push(json!({
            "session_id": session_id,
            "root": store.session_root(session_id)?,
            "head": store.session_head(session_id)?,
            "turns": turns.iter().map(turn).collect::<Vec<_>>(),
            "runs": turns
                .iter()
                .map(|t| run(&store, t.run_id))
                .collect::<Res<Vec<_>>>()?,
            "size": size.map(|s| json!({
                "turns": s.turns,
                "runs": s.runs,
                "rows": s.rows,
                "bytes": s.bytes,
            })),
        }));
    }
    // The sweep's own decision over a store this binary did not write: a session
    // holding a run that is still `running` is refused, whatever wrote it.
    let refused = store.sweep_preview(SWEEP_CUTOFF)?.refused;
    let out = json!({
        "reader": "io-harness 0.73.0",
        "checkpoint_format": io_harness::CHECKPOINT_FORMAT,
        "sweep": { "refused": refused },
        "sessions": sessions,
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
    let usage = "usage: gen-0-73-0 write <output-dir> | gen-0-73-0 read <database>";
    let mode = args.next().ok_or(usage)?;
    let target = args.next().ok_or(usage)?;

    match mode.as_str() {
        "write" => {
            std::fs::create_dir_all(&target)?;
            // Everything after this names files relatively. `runs.file` stores the
            // contract root verbatim, so an absolute root would bake this machine's
            // home directory into a committed fixture.
            std::env::set_current_dir(&target)?;
            state()
        }
        "read" => read(&target),
        other => Err(format!("unknown mode `{other}`: expected `write` or `read`").into()),
    }
}
