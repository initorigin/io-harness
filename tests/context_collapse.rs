//! 0.76.0 — Context Collapse: the rung beneath a fold.
//!
//! The claim is not "the section got smaller" — the existing budget already does
//! that by stubbing. The claim is that an entry which would have become a
//! one-line stub is carried *shortened* instead, at no provider call, with the
//! ledger untouched, and that both of those are things a fold cannot say.
//!
//! Every test here pairs its claim with the fold as a control, because a
//! property this release has and the previous one lacks is only demonstrated by
//! showing the previous one lacking it.
//!
//! Nothing here measures a duration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::context::{
    assemble, Assembly, Collapse, Compaction, ContextBudget, Ledger, ObsKind, Observation, Origin,
};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::Workspace;
use io_harness::{run_with, ApproveAll, Policy, Provider, Store, TaskContract, Verification};
use serde_json::json;

/// The text of the first observation the folding control makes, so the assertion
/// names a specific thing rather than a length.
const FIRST_OBSERVATION: &str = "ZZ-FIRST-OBSERVATION-ZZ";

/// Reads a file twice, then answers, so the ledger has something to fold.
struct Folding {
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Folding {
    fn new() -> Self {
        Self {
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Folding {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        // The fold's own completion is the one made with no tools at all — that
        // is what buying a summary costs, and it is the cost the collapse avoids.
        // Answering it with text is what makes the fold actually happen: an empty
        // summary is refused rather than allowed to replace the entries.
        if req.tools.is_empty() {
            self.seen.lock().unwrap().push(req);
            return Ok(CompletionResponse {
                text: Some("A paragraph standing in for the earlier observations.".into()),
                ..Default::default()
            });
        }
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // Enough separate reads to give the fold something to replace: it keeps
        // the newest `keep_recent` whole and only fires once the ledger is longer
        // than that.
        let calls = match i {
            0..=9 => vec![ToolCall {
                name: "read_file".into(),
                arguments: json!({ "path": format!("f{i}.txt") }),
            }],
            _ => Vec::new(),
        };
        Ok(CompletionResponse {
            tool_calls: calls,
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "folding"
    }
}

/// A ledger of `n` collapsible entries, each `chars` long, over distinct targets.
///
/// **Greps, and every target distinct, both deliberately.** A read is never
/// collapsed — see `a_read_is_never_collapsed_because_a_collapsed_read_would_be_a_tail`
/// — so a fixture that interleaved reads would have the fit walk stop at the first
/// one and measure that instead of the rung. Distinct targets keep supersession
/// out of it for the same reason: two entries of one kind and target are one
/// answer, and an elision that happens for *that* reason is 0.10.0's, not this
/// release's.
fn ledger(n: u32, chars: usize) -> Ledger {
    let mut l = Ledger::new();
    for i in 0..n {
        l.push(Observation::new(
            i + 1,
            ObsKind::Grep,
            Some(format!("f{i}.txt")),
            format!("\n[entry {i}]\n{}\n", "y".repeat(chars)),
            Origin::File,
        ));
    }
    l
}

struct Fixture {
    _dir: tempfile::TempDir,
    ws: Workspace,
    policy: Policy,
    store: Store,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::permissive();
    let ws = Workspace::with_policy(dir.path(), policy.clone());
    Fixture {
        _dir: dir,
        ws,
        policy,
        store: Store::memory().unwrap(),
    }
}

async fn assembled(
    f: &Fixture,
    l: &Ledger,
    budget: u64,
    collapse: Collapse,
) -> io_harness::context::Assembled {
    assemble(
        l,
        budget,
        &[],
        &[],
        Assembly {
            collapse,
            ws: Some(&f.ws),
            policy: &f.policy,
            store: &f.store,
            run_id: 1,
            step: 9,
        },
    )
    .await
    .unwrap()
}

// ------------------------------------------------------------------------- F5

/// F5 — the projection shrinks what is sent, and the ledger does not move.
///
/// The anti-vacuity guard matters here: if the ledger fitted the budget whole
/// there would be nothing to collapse and every assertion below would hold for
/// the wrong reason, so the fixture is deliberately far larger than the budget
/// and the test says so.
#[tokio::test]
async fn a_collapse_carries_what_would_have_been_stubbed_and_leaves_the_ledger_alone() {
    let f = fixture();
    let l = ledger(40, 2_000);
    let before = l.entries().len();

    let plain = assembled(&f, &l, 4_000, Collapse::default()).await;
    let collapsed = assembled(&f, &l, 4_000, Collapse { keep_chars: 600 }).await;

    assert!(
        plain.stubbed > 0,
        "the fixture must overflow the budget or there is nothing to collapse; stubbed {}",
        plain.stubbed
    );
    assert_eq!(
        plain.shortened, 0,
        "an unconfigured collapse must shorten nothing"
    );
    assert!(
        collapsed.shortened > 0,
        "the collapse carried nothing shortened, so this proves nothing about it"
    );
    // 0.76.0 — added because a sabotage survived without it. Turning the fit
    // walk's `continue` into a `break` collapses exactly one entry and stubs
    // every older one, and every assertion here passed over that: "more than
    // zero" cannot tell one from many. Carrying on past a collapsed entry is the
    // whole point — stopping there throws away the room the collapse just made.
    assert!(
        collapsed.shortened > 1,
        "only {} entry was shortened: the walk stopped at the first collapse instead of \
         continuing, so the budget the collapse freed went unused",
        collapsed.shortened
    );
    assert!(
        collapsed.carried > plain.carried,
        "a collapse must carry more than the same turn stubbing everything: {} against {}",
        collapsed.carried,
        plain.carried
    );
    assert_eq!(
        l.entries().len(),
        before,
        "a read-time projection must not shorten the ledger: the watermark index that decides \
         what has already been persisted counts entries, and moving it corrupts a resume"
    );
    assert!(
        collapsed.est_tokens <= 4_000,
        "the collapse must respect the budget it is projecting into, got {}",
        collapsed.est_tokens
    );
}

/// F5, the half about cost. A read-time projection buys nothing from a provider,
/// and the way to say that structurally is that it runs inside the assembler —
/// which has no provider to call.
///
/// Asserted against the store rather than against a counter: a fold writes a
/// `summaries` row, and a collapse must not.
#[tokio::test]
async fn a_collapse_buys_no_summary() {
    let f = fixture();
    let l = ledger(40, 2_000);
    let out = assembled(&f, &l, 4_000, Collapse { keep_chars: 600 }).await;

    assert!(out.shortened > 0, "the collapse must have done something");
    assert!(
        f.store.summary_for(1, 0).unwrap().is_none(),
        "a collapse must not write a summary row: that is what the rung above it does, and it \
         is what costs a model call"
    );
}

// ------------------------------------------------------------------------- F6

/// F6 — the collapse is reversible, and that is the property a fold does not have.
///
/// Same ledger, same store. Assembled once with the rung on and once with it off,
/// and the second must carry the entries the first shortened, whole.
#[tokio::test]
async fn turning_the_collapse_off_assembles_the_shortened_entries_whole_again() {
    let f = fixture();
    let l = ledger(20, 2_000);

    let collapsed = assembled(&f, &l, 4_000, Collapse { keep_chars: 500 }).await;
    assert!(
        collapsed.shortened > 0,
        "nothing was shortened, so there is nothing for the reversal to restore"
    );

    let back = assembled(&f, &l, 4_000, Collapse::default()).await;
    assert_eq!(
        back.shortened, 0,
        "with the rung off nothing may be shortened"
    );

    // The whole text of the newest entry is present again. The newest is the one
    // guaranteed to be carried under either setting, so a difference here is
    // about shortening rather than about what fitted.
    let newest = &l.entries()[l.entries().len() - 1].text;
    assert!(
        back.text.contains(newest.trim()),
        "an entry the collapse had shortened must come back whole once it is off"
    );
}

/// F6's control — the same reversal through a fold, which cannot do it.
///
/// `Ledger::fold_first` is crate-internal, so this drives the real thing: a run
/// that folds, and then a later turn of the same run with compaction turned off.
/// The entries do not come back, because the fold already replaced them and
/// bought a paragraph to stand in for them. That is the whole asymmetry the
/// ladder's order rests on.
///
/// Note what is *not* being claimed. A fold is durably non-destructive too — the
/// `ledger_observations` rows are still there, which this asserts. What it cannot
/// undo is the working view any later turn is assembled from, and no setting
/// reverses that within the run.
#[tokio::test]
async fn a_fold_is_not_reversible_which_is_why_the_ladder_takes_the_collapse_first() {
    let dir = tempfile::tempdir().unwrap();
    // The first file carries the marker, and every file is under the per-entry
    // cap so the reads build a ledger rather than a wall of refusals — a read
    // over the cap comes back as a one-line error, which would grow nothing.
    std::fs::write(
        dir.path().join("f0.txt"),
        format!("{FIRST_OBSERVATION}\n{}\n", "y".repeat(1_800)),
    )
    .unwrap();
    for i in 1..10 {
        std::fs::write(
            dir.path().join(format!("f{i}.txt")),
            format!("[file {i}]\n{}\n", "y".repeat(1_800)),
        )
        .unwrap();
    }
    let store = Store::memory().unwrap();

    // Ten reads, then a fold requested for the turn. `fold_now` is the public
    // trigger; the summary is bought from the provider, which is the cost this
    // release's rung exists to avoid. `keep_recent: 2` so the fold has most of
    // the ledger to replace rather than only its tail.
    let provider = Folding::new();
    let contract = TaskContract::workspace("do the thing", dir.path().to_string_lossy().as_ref())
        .with_max_steps(12)
        .with_verification(Verification::None)
        // The automatic trigger rather than `fold_now`: `fold_now` is consumed on
        // the first step, when the ledger is still empty and there is nothing to
        // fold — it is consumed either way, which its own documentation says.
        .with_compaction(Compaction {
            at_share: 0.8,
            keep_recent: 2,
        })
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        });
    let _ = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;

    let seen = provider.requests();
    assert!(
        seen.len() >= 3,
        "the run must have taken enough steps for a fold to be visible, got {}",
        seen.len()
    );
    // The fold's own completion is the one with no tools. Its presence is the
    // anti-vacuity guard: without it nothing folded and every assertion below
    // would hold because nothing happened.
    assert!(
        seen.iter().any(|r| r.tools.is_empty()),
        "no summary was bought, so no fold happened and this test demonstrates nothing"
    );
    // The turn after the fold — the last request that is a real step rather than
    // the summary purchase — no longer carries what the fold replaced.
    let after = seen
        .iter()
        .rfind(|r| !r.tools.is_empty())
        .expect("there must be a step after the fold");
    assert!(
        !after.user.contains(FIRST_OBSERVATION),
        "the fold must have replaced the first observation, or there is no irreversibility to \
         demonstrate"
    );

    // And it does not come back. Assembling the same run's ledger again — with
    // every compaction setting off — still cannot produce what the fold replaced,
    // because the working ledger no longer holds it.
    let restored = store.observations(1).unwrap();
    assert!(
        restored.iter().any(|o| o.text.contains(FIRST_OBSERVATION)),
        "the durable rows must survive a fold: what a fold destroys is the working view, and \
         saying otherwise would overstate the difference"
    );
}

// ------------------------------------------------------------------------- F7

/// F7 — a shortened entry keeps its kind and its target, so the stale-read rules
/// still see it.
///
/// This is the property that makes the collapse compose with `rewind`. Assembly
/// invalidates a read when a later write names the same path; behind a fold those
/// entries have become one prose message targeted `summary`, and the machinery is
/// structurally blind to them. Behind a collapse they are still reads of a path.
#[tokio::test]
async fn a_shortened_entry_is_still_a_read_of_a_path() {
    let f = fixture();
    let mut l = Ledger::new();
    // A large read of a path, then a later write of the same path. The read is
    // the entry the collapse will shorten.
    l.push(Observation::new(
        1,
        ObsKind::Read,
        Some("f.txt".into()),
        format!("\n[the file]\n{}\n", "y".repeat(6_000)),
        Origin::File,
    ));
    l.push(Observation::new(
        2,
        ObsKind::Write,
        Some("f.txt".into()),
        "\n[wrote f.txt]\n".to_string(),
        Origin::File,
    ));

    // A grep of the same path, large enough to be the collapse candidate. The
    // read above is deliberately NOT the candidate: a read is never collapsed.
    l.push(Observation::new(
        3,
        ObsKind::Grep,
        Some("f.txt".into()),
        format!("\n[grep f.txt]\n{}\n", "z".repeat(20_000)),
        Origin::File,
    ));

    let out = assembled(&f, &l, 2_000, Collapse { keep_chars: 400 }).await;

    // The anti-vacuity guard this test was missing, and its absence is why it
    // passed while proving nothing. Every other test in this file carries it.
    assert!(
        out.shortened > 0,
        "nothing was shortened, so nothing here is about the collapse at all"
    );

    // The claim, asserted on the piece that was actually shortened rather than on
    // any non-prose piece: a shortened entry still declares its kind and its
    // target, which is what invalidation and re-read are keyed on. The previous
    // form of this assertion was satisfied by the tiny write observation and by
    // any non-prose piece whatever its text, so it could not fail.
    let shortened_piece = out
        .emitted
        .iter()
        .find(|e| e.text.contains("grep f.txt") && e.text.contains("elided"))
        .expect("the shortened grep must still name itself a grep of f.txt");
    assert!(
        shortened_piece.text.len() < 6_000,
        "the piece found is the whole entry, not a shortened one"
    );
}

/// F7's other half, and the one the adversarial review found missing: a read is
/// never collapsed, because a collapsed read would be a tail.
///
/// `bound` keeps a read's *tail* — the end of a file is what a writer needs when
/// one oversized read is capped. Inside an assembled projection that shape puts
/// the end of a file into the prompt under a header saying the file was read,
/// with no filename and no `offset`/`limit` advice, and the model cannot tell it
/// from a whole read. 0.55.0 removed exactly that and its own test still asserts
/// it; this is the same assertion with the rung turned on, which is the variant
/// that was missing.
#[tokio::test]
async fn a_read_is_never_collapsed_because_a_collapsed_read_would_be_a_tail() {
    let f = fixture();
    let mut l = Ledger::new();
    l.push(Observation::new(
        1,
        ObsKind::Read,
        Some("src/lib.rs".into()),
        format!(
            "\n[read src/lib.rs]\nHEAD-SENTINEL\n{}\nTAIL-SENTINEL\n",
            "y".repeat(6_000)
        ),
        Origin::File,
    ));
    // Something newer and small, so the read is the entry that does not fit.
    l.push(Observation::new(
        2,
        ObsKind::Grep,
        Some("other".into()),
        "\n[grep other]\nnothing\n".to_string(),
        Origin::File,
    ));

    let out = assembled(&f, &l, 500, Collapse { keep_chars: 200 }).await;

    assert!(
        !out.text.contains("TAIL-SENTINEL"),
        "a collapsed read served its tail: that is the shape 0.55.0 removed, and the model \
         cannot tell it from a whole read. Projection was:\n{}",
        out.text
    );
    assert_eq!(
        out.shortened, 0,
        "the read was collapsed; a read is whole or a stub, never a partial"
    );
    assert!(
        out.text.contains("src/lib.rs"),
        "the stub must still name the file, which is the thing a tail cannot do: {}",
        out.text
    );
    assert!(
        out.text.contains("offset"),
        "the stub must still say how to get the part that matters back: {}",
        out.text
    );
}

// ------------------------------------------------------------------------- F9

/// F9 — the emitted pieces still reconstruct the assembled text byte for byte
/// with the rung on.
///
/// Two subsystems locate themselves by searching this string — `frozen_prefix`
/// finds the fold's summary inside the prompt, and `transcript` splits the
/// framing off with `split_once`. Both are exact only because assembly emits a
/// carried entry's text verbatim. A projection that broke the identity would
/// yield "no boundary" and "everything is prose", silently, with nothing failing.
#[tokio::test]
async fn the_emitted_pieces_still_reconstruct_the_text_when_entries_are_shortened() {
    let f = fixture();
    let l = ledger(30, 2_000);
    let out = assembled(&f, &l, 4_000, Collapse { keep_chars: 500 }).await;

    assert!(
        out.shortened > 0,
        "nothing was shortened, so this is the 0.75.0 assertion wearing a new name"
    );
    let rebuilt: String = out.emitted.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(
        rebuilt, out.text,
        "the pieces must concatenate to the assembled text byte for byte, or the boundary search \
         and the transcript split both fail without an error"
    );
}

// ------------------------------------------------------------------------- F8

/// F8 — off is the default, and off assembles what 0.75.0 assembled.
///
/// The strongest available form: byte identity of the whole section between a
/// default `Collapse` and the value the type is constructed with when a caller
/// says nothing at all.
#[tokio::test]
async fn an_unconfigured_collapse_assembles_the_same_bytes() {
    let f = fixture();
    let l = ledger(30, 2_000);

    let a = assembled(&f, &l, 4_000, Collapse::default()).await;
    let b = assembled(&f, &l, 4_000, Collapse { keep_chars: 0 }).await;

    assert!(
        !a.text.is_empty(),
        "an empty section compares equal to itself"
    );
    assert_eq!(a.text, b.text, "zero is off and default is zero");
    assert_eq!(a.shortened, 0);
    assert_eq!(a.stubbed, b.stubbed);
    assert_eq!(a.carried, b.carried);
}

// ------------------------------------------------------------------- N5 / N6

/// N5/N6 — what the rung saves, and what it costs to compute.
///
/// Printed, never asserted: a number asserted on a CI runner is a flake, and this
/// release is partly about a suite that asserted on clocks. The method and the
/// machine go in `docs/MEASUREMENTS.md`.
///
/// `cargo test --release --test context_collapse n5_ -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measurement, not a gate — see docs/MEASUREMENTS.md"]
async fn n5_what_a_collapse_carries_that_a_stub_does_not() {
    let f = fixture();
    let l = ledger(40, 2_000);
    println!("entries=40 chars_each=2000 budget=4000");
    for keep in [0usize, 300, 600, 1_200] {
        let started = std::time::Instant::now();
        let out = assembled(&f, &l, 4_000, Collapse { keep_chars: keep }).await;
        let took = started.elapsed();
        println!(
            "keep_chars={keep:<5} carried={:<3} shortened={:<3} stubbed={:<3} est_tokens={:<5} assemble={:?}",
            out.carried, out.shortened, out.stubbed, out.est_tokens, took
        );
    }
}
