//! F1 and F3 of 0.78.0, on the arms of the exporter that are reachable from
//! outside the crate.
//!
//! The span tree itself is asserted in `src/otel.rs`'s own `mod span_tests`: a
//! finished span leaves the exporter through a crate-private seam, and making
//! that seam public in order to test it would put a transport detail in
//! `docs/public-api.txt`. What is out here is everything a *consumer* can see —
//! that the exporter is an `Observer` and attaches through a door that already
//! exists, that it is `Send + Sync`, that it contains no SQL, and that a run
//! watched by one is the same run.
//!
//! Each rule is checked by a function over its input rather than by an assertion
//! written inline, so a `control_` test can feed that function a deliberately
//! wrong input and prove the check says no. A checker nobody has watched fail is
//! a checker nobody has shown to work.

// The whole exporter is behind the feature, so a build that did not ask for an
// outbound network capability does not compile these either.
#![cfg(feature = "otel")]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, ApproveAll, Ignore, Observer, OtelConfig, OtelExporter, Policy, Provider,
    RetryPolicy, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ------------------------------------------------------------------------ F1

/// F1, at compile time. [`Observer`](io_harness::Observer) requires `Send +
/// Sync`, and the exporter holds a `Store`, whose connection is `Send` and not
/// `Sync` — the `Mutex` around it is what makes this line compile, and this line
/// is what says so out loud rather than leaving it to the day someone removes
/// the mutex and reads the resulting error as being about something else.
const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<OtelExporter>();

/// F1. The exporter attaches through a door that already exists, and adds none.
///
/// The assertion is that this compiles: `run_with_observed` takes `&dyn
/// Observer` and has since 0.12.0, so an exporter that needed a new attachment
/// mechanism could not be passed here at all.
#[tokio::test]
async fn f1_the_exporter_attaches_through_an_entry_point_that_already_existed() {
    let bed = Bed::new();
    let exporter = OtelExporter::open(OtelConfig::default(), &bed.db).unwrap();

    let (outcome, _) = drive(&bed, &exporter).await;
    assert_eq!(outcome, RunOutcome::Success { steps: 2 });
}

/// F1. `Observer` gains no method.
///
/// The exporter's whole claim is that the run loop is unchanged by its presence,
/// and the cheapest way for that claim to become false is a second trait method
/// added for the exporter's benefit — which every existing implementer would then
/// have to write. Read off the declaration rather than off a diff, so it stays
/// true release after release.
#[test]
fn f1_the_observer_trait_still_declares_exactly_one_method() {
    let source = read_crate_file("src/observe.rs");
    let methods = trait_methods(&source, "Observer");

    assert_eq!(
        methods,
        vec!["event".to_string()],
        "`Observer` must declare exactly one method; a second one is a change \
         every implementer inherits"
    );
}

/// The names of the methods declared directly in `pub trait <name>`'s body.
///
/// The trait ends at the first `}` in the first column, which is what stops this
/// from running on into the next item and counting its functions.
fn trait_methods(source: &str, name: &str) -> Vec<String> {
    let Some((_, after)) = source.split_once(&format!("pub trait {name}")) else {
        return Vec::new();
    };
    let Some((_, body)) = after.split_once('{') else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with('}') {
            break;
        }
        if let Some(rest) = line.trim_start().strip_prefix("fn ") {
            out.push(rest.split('(').next().unwrap_or_default().to_string());
        }
    }
    out
}

#[test]
fn control_the_trait_reader_counts_every_method_and_stops_at_the_trait() {
    let source = "\
pub trait Observer: Send + Sync {
    /// One.
    fn event(&self, event: &RunEvent) -> Flow;
    fn flush(&self);
}

pub fn not_a_method() {}

pub trait Other {
    fn elsewhere(&self);
}
";
    assert_eq!(
        trait_methods(source, "Observer"),
        vec!["event".to_string(), "flush".to_string()],
        "both methods of the trait, and nothing after its closing brace"
    );
    // A trait that is not there is not a trait with no methods, but the F1 test
    // above compares against a one-element list, so an empty answer fails it.
    assert!(trait_methods(source, "Missing").is_empty());
}

// ------------------------------------------------------------------------ F3

/// F3. The exporter writes no SQL.
///
/// It reads through `Store::provider_calls` and `Store::step_attributions`,
/// which are public and already carry every column it needs — so there is one
/// query per fact in this crate rather than two that can drift apart, and
/// `tests/state_error.rs`'s rule that no public surface names `rusqlite` holds by
/// construction rather than by remembering. A grep is the direct check: a second
/// statement cannot appear without appearing here.
#[test]
fn f3_the_exporter_contains_no_sql_of_its_own() {
    let source = read_crate_file("src/otel.rs");

    let offending = sql_statements_in(&source);
    assert!(
        offending.is_empty(),
        "src/otel.rs must contain no SQL — it reads the store through \
         `Store::provider_calls` and `Store::step_attributions`. Found: {offending:?}"
    );
}

/// The keywords a hand-written query cannot be written without.
const SQL_KEYWORDS: &[&str] = &[
    "SELECT ",
    "INSERT INTO",
    "UPDATE ",
    "DELETE FROM",
    "CREATE TABLE",
];

/// Every line of `source` that names one of [`SQL_KEYWORDS`].
///
/// Uppercase, and matched with the trailing space that a statement has and an
/// English sentence about selection does not, so a comment reading "the exporter
/// selects nothing" is not a finding while `SELECT step` is.
fn sql_statements_in(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter(|line| SQL_KEYWORDS.iter().any(|keyword| line.contains(keyword)))
        .collect()
}

#[test]
fn control_the_sql_checker_finds_a_query_and_ignores_prose() {
    let query = "\
        let mut stmt = self.conn.prepare(\n\
        \"SELECT step, attempt, model FROM provider_calls WHERE run_id = ?1\",\n\
        )?;\n";
    assert_eq!(
        sql_statements_in(query).len(),
        1,
        "a hand-written read is a finding"
    );
    assert_eq!(sql_statements_in("INSERT INTO spans VALUES (1)").len(), 1);
    assert_eq!(sql_statements_in("DELETE FROM runs WHERE id = 1").len(), 1);

    // The other half: the checker must not report the file's prose. The trailing
    // space is what separates a statement from a word — an English sentence
    // about selection, and a capitalised word that is not a keyword, both pass.
    assert!(sql_statements_in("// the exporter selects nothing").is_empty());
    assert!(sql_statements_in("// no row is SELECTED, nothing is written").is_empty());
    assert!(sql_statements_in("let update = 1;").is_empty());
}

// ------------------------------------------------------------- the no-op arm

/// A working exporter changes nothing about the run it watches.
///
/// F7 owns the failure arms — a collector that is down, slow or refusing — and
/// the task after this one owns them. What belongs here is the case that is easy
/// to assume: that an exporter doing its whole job, opening a second connection
/// to the run's own database and reading it while the run finishes, still leaves
/// the outcome, the step count and the token total exactly where they were.
#[tokio::test]
async fn f7_a_run_watched_by_an_exporter_is_the_same_run() {
    let watched_bed = Bed::new();
    let exporter = OtelExporter::open(OtelConfig::default(), &watched_bed.db).unwrap();
    let (watched, watched_run) = drive(&watched_bed, &exporter).await;

    let plain_bed = Bed::new();
    let (plain, plain_run) = drive(&plain_bed, &Ignore).await;

    assert_eq!(
        watched, plain,
        "the outcome and the step count are the same"
    );
    assert_eq!(
        watched_bed.store.spent_tokens(watched_run).unwrap(),
        plain_bed.store.spent_tokens(plain_run).unwrap(),
        "the token total is the same"
    );
    assert_eq!(
        watched_bed.store.provider_calls(watched_run).unwrap().len(),
        plain_bed.store.provider_calls(plain_run).unwrap().len(),
        "the same provider calls were made"
    );
}

// ---------------------------------------------------------------- scaffolding

fn read_crate_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// A workspace, and a database beside it rather than inside it, so the agent
/// cannot see the file it is being recorded in.
struct Bed {
    workspace: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    store: Store,
    db: std::path::PathBuf,
}

impl Bed {
    fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("runs.db");
        let store = Store::open(&db).unwrap();
        Self {
            workspace,
            _db_dir: db_dir,
            store,
            db,
        }
    }
}

/// Plays two turns: one step that edits, one that satisfies the gate.
struct Mock {
    at: AtomicUsize,
}

impl Provider for Mock {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: vec![if i == 0 {
                write("src.txt", "one\n")
            } else {
                write("NOTES.md", "done")
            }],
            text: Some("working".into()),
            usage: Some(Usage {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                total_tokens: 1_400,
                ..Default::default()
            }),
            model: Some("model-a".into()),
            finish_reason: Some("stop".into()),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

async fn drive(bed: &Bed, observer: &dyn Observer) -> (RunOutcome, i64) {
    let contract = TaskContract::workspace("write the notes", bed.workspace.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        })
        .with_max_steps(4)
        .with_retry_policy(RetryPolicy {
            base: Duration::ZERO,
            max: Duration::ZERO,
        });

    let result = run_with_observed(
        &contract,
        &Mock {
            at: AtomicUsize::new(0),
        },
        &bed.store,
        &Policy::permissive(),
        &ApproveAll,
        observer,
    )
    .await
    .unwrap();
    (result.outcome, result.run_id)
}
