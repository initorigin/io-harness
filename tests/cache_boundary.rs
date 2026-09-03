//! The second cache breakpoint (0.44.0): where the frozen prefix ends, and the rule
//! that decides whether it is offered at all.
//!
//! 0.38.0 marked the end of the `system` block and deliberately left the transcript
//! unmarked, because `context::assemble` supersedes, invalidates, re-reads and re-fits
//! earlier observations on every turn — so it was not a byte-stable prefix, and a
//! marker there would have been billed as a cache *write* on nearly every turn.
//! 0.43.0's compaction changed the premise for the part of the prompt ahead of the
//! folded summary.
//!
//! It did **not** change it for the whole prefix, and that is what this file is really
//! about. The memory block renders ahead of the summary (`context::assemble`) and is
//! re-read from the store on every turn by design, so a note the run writes about its
//! own work moves the prefix out from under the summary. The crate therefore holds the
//! previous step's candidate prefix and marks only when this step's is byte-identical
//! to it. The four-step sequence — unmarked, marked, withdrawn, re-marked with
//! different bytes — is the release, and an implementation that marks whenever a fold
//! exists passes the first assertion and fails the rest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, Message, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, Compaction, ContextBudget, EventKind, Flow, Observer, Policy, Provider,
    RunEvent, Store, TaskContract, Verification,
};
use serde_json::json;

/// Every `CacheMarked` the run emitted, as `(through_step, prefix_bytes)`.
///
/// Read off the event stream rather than out of the store, so the count and the
/// requests the provider recorded are two independent halves of the claim.
#[derive(Default)]
struct Marks(Arc<Mutex<Vec<(u32, u64)>>>);

impl Observer for Marks {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::CacheMarked {
            through_step,
            prefix_bytes,
        } = &event.kind
        {
            self.0.lock().unwrap().push((*through_step, *prefix_bytes));
        }
        Flow::Continue
    }
}

/// The one sentence the summarising model writes.
const SUMMARY_SENTENCE: &str = "ZZ-SUMMARY-ZZ read alpha.txt and kept the token enum.";

/// How a summarising request is told apart from a working one without the test
/// re-implementing the prompt.
const SUMMARISER: &str = "compacting an agent's own working notes";

/// Records the whole request rather than `(system, user)`: the boundary is only
/// observable on the `CompletionRequest`, and it is the field under test.
struct Recorder {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Recorder {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The working requests, in order, with the summarising calls filtered out.
    fn working(&self) -> Vec<CompletionRequest> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.system.contains(SUMMARISER))
            .cloned()
            .collect()
    }
}

impl Provider for Recorder {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let summarising = req.system.contains(SUMMARISER);
        self.seen.lock().unwrap().push(req);
        let usage = Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            ..Default::default()
        });
        if summarising {
            return Ok(CompletionResponse {
                text: Some(SUMMARY_SENTENCE.into()),
                usage,
                ..Default::default()
            });
        }
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            usage,
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "recorder"
    }
}

fn read(path: &str) -> ToolCall {
    ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path }),
    }
}

fn remember(key: &str, value: &str) -> ToolCall {
    ToolCall {
        name: "remember".into(),
        arguments: json!({ "key": key, "value": value }),
    }
}

const NAMES: [&str; 10] = [
    "alpha.txt",
    "beta.txt",
    "gamma.txt",
    "delta.txt",
    "epsilon.txt",
    "zeta.txt",
    "eta.txt",
    "theta.txt",
    "iota.txt",
    "kappa.txt",
];

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in NAMES {
        std::fs::write(
            dir.path().join(name),
            format!("{name} padding\n{}", "ordinary padding line\n".repeat(90)),
        )
        .unwrap();
    }
    dir
}

/// Never satisfied, so the loop runs its whole step budget, and small enough that a
/// few reads cross the fold threshold.
fn contract(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("read the files and report", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        })
        .with_compaction(Compaction {
            at_share: 0.8,
            keep_recent: 2,
        })
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
}

/// The prefix a request was told to mark, as bytes of its own `user`.
fn marked(req: &CompletionRequest) -> Option<&str> {
    req.cache_boundary.map(|at| &req.user[..at])
}

// ------------------------------------------------------------------------ F3

/// F3 — the boundary is offered only after a fold, and only once the prefix has
/// repeated.
///
/// Three assertions, and the second is the discriminating one. The step on which the
/// fold happens carries no boundary, because that prefix has never been sent and
/// marking it would be a cache write the crate cannot know will pay for itself. The
/// step after it carries one. And the marked span ends with the summary's own
/// sentence, which is what says the boundary is *the compaction boundary* rather than
/// an arbitrary offset that happens to be stable.
#[tokio::test]
async fn the_boundary_appears_only_after_a_fold_and_only_once_the_prefix_repeats() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), NAMES.len() as u32),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let working = provider.working();
    let first_with_summary = working
        .iter()
        .position(|r| r.user.contains(SUMMARY_SENTENCE))
        .expect("the run never folded; nothing below is meaningful");

    // Nothing before the fold is marked: there is no frozen prefix to mark.
    for (i, req) in working.iter().enumerate().take(first_with_summary) {
        assert_eq!(
            req.cache_boundary, None,
            "request {i} was marked before any fold had happened"
        );
    }

    // The fold's own step is not marked either — this prefix has been sent zero
    // times, and the guard marks only what has already gone out once.
    assert_eq!(
        working[first_with_summary].cache_boundary, None,
        "the step the fold happened on must not be marked"
    );

    // The step after it is, and the marked span ends at the summary.
    let after = &working[first_with_summary + 1];
    let prefix = marked(after).expect("the step after the fold must carry a boundary");
    assert!(
        prefix.trim_end().ends_with(SUMMARY_SENTENCE),
        "the marked prefix must end at the folded summary, got: …{}",
        &prefix[prefix.len().saturating_sub(120)..]
    );

    // And it is a genuine prefix: everything the model still has to re-read fresh
    // sits after it.
    assert!(
        after.user.starts_with(prefix),
        "the marked span must be a prefix of the request"
    );
    assert!(
        !prefix.is_empty() && prefix.len() < after.user.len(),
        "a boundary at either end is not a boundary"
    );
}

// ------------------------------------------------------------------------ F4

/// F4 — a note written mid-run moves the prefix, and the marker is withdrawn for
/// exactly one step.
///
/// This is the criterion that exists because the roadmap's "immutable by construction"
/// is not true of the whole prefix. `remember` writes to the store, the memory block is
/// re-read every turn and renders *ahead* of the summary, so the frozen prefix moves
/// underneath the summary without the summary changing at all. An implementation that
/// compares the summary rather than the whole candidate prefix passes F3 and fails
/// here, having asked the vendor to cache bytes it never sent.
#[tokio::test]
async fn a_note_written_mid_run_withdraws_the_marker_for_one_step() {
    let dir = workspace();
    // Read enough to fold, then keep reading, then write a note, then read again.
    let mut script: Vec<Vec<ToolCall>> = NAMES.iter().map(|n| vec![read(n)]).collect();
    script.push(vec![remember("layout", "the parser lives in src/parse.rs")]);
    script.push(vec![read("alpha.txt")]);
    script.push(vec![read("beta.txt")]);
    let steps = script.len() as u32;

    let provider = Recorder::new(script);
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), steps),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let working = provider.working();
    let boundaries: Vec<Option<usize>> = working.iter().map(|r| r.cache_boundary).collect();

    // The run must actually have marked something, or this asserts nothing.
    let first_marked = boundaries
        .iter()
        .position(Option::is_some)
        .expect("the run never marked a prefix");

    // Somewhere after the first mark the note lands and the marker is withdrawn.
    let withdrawn = boundaries
        .iter()
        .enumerate()
        .skip(first_marked)
        .find(|(_, b)| b.is_none())
        .map(|(i, _)| i)
        .expect("the note never withdrew the marker; the prefix is not being compared whole");

    // The step after the withdrawal is marked again — one step, not permanently.
    let back = boundaries
        .get(withdrawn + 1)
        .copied()
        .flatten()
        .expect("the marker never came back after the note");

    // And the prefix that came back is a different one: the memory block grew.
    let before = marked(&working[first_marked]).expect("the earlier marked prefix");
    let after = &working[withdrawn + 1].user[..back];
    assert_ne!(
        before, after,
        "the prefix after the note must differ, or nothing moved and the withdrawal \
         was spurious"
    );
    assert!(
        after.contains("the parser lives in src/parse.rs"),
        "the note is what moved the prefix, so it must be inside the new one"
    );
    assert!(
        working[withdrawn + 1].user.starts_with(after),
        "the marked span must still be a prefix of the request"
    );
}

// ------------------------------------------------------------------------ N6

/// N6 — the boundary is one helper both loops call, not a rule written twice.
///
/// A grep alone proves nothing (0.33.0's fact 70), so this asserts the shape a copy
/// would break: exactly one definition, and at least two call sites — the flat
/// workspace loop and the tree loop. The same assertion `tests/session_fanout.rs` makes
/// for the five session rules, and it is here for the reason it is there: a rule added
/// to one of two near-parallel loops is a rule that lapses in the other, and the
/// symptom would be a contained run and a flat run caching differently while every test
/// still passed.
#[test]
fn the_boundary_is_one_helper_that_both_loops_call() {
    let src = run_subsystem_source();

    for helper in ["cache_boundary_for", "frozen_prefix"] {
        let defs = src.matches(&format!("fn {helper}")).count();
        assert_eq!(defs, 1, "{helper} is defined exactly once");
    }
    let calls = src.matches("cache_boundary_for(").count() - 1;
    assert!(
        calls >= 2,
        "cache_boundary_for is called by both loops, found {calls} call sites"
    );

    // The guard's own state is per loop and per agent, and each loop declares it
    // once. Two declarations, no more: a third would be a third loop nobody told
    // this test about, and zero in one of them is the drift above.
    assert_eq!(
        src.matches("let mut marked_prefix = PrefixGuard::default();")
            .count(),
        2,
        "one run-scoped guard per loop"
    );
}

/// 0.77.0 — provenance framing is the same shape of rule, held to the same shape of
/// assertion.
///
/// It belongs in this file rather than beside its own tests because what it must not
/// do is disturb the boundary above, and the way it would come to disturb one loop
/// and not the other is by being written twice or called once. A tree run whose
/// external content is unframed while a flat run's is framed is the 0.44.0 drift
/// again, with a security marker in place of a cache marker.
///
/// The position is asserted too, and it is the load-bearing half: framing after
/// `user` is derived leaves the flat string and the transcript as two different
/// accounts of one turn, which is the thing three subsystems assume cannot happen.
#[test]
fn framing_is_one_helper_both_loops_call_before_they_derive_the_user_block() {
    let src = run_subsystem_source();

    assert_eq!(
        src.matches("fn frame_external").count(),
        1,
        "frame_external is defined exactly once"
    );
    let calls = src.matches("frame_external(&mut assembled);").count();
    assert_eq!(
        calls, 2,
        "frame_external is called by both loops and by nothing else, found {calls} call sites"
    );

    // Each loop assembles, frames, then derives `user` — in that order. Asserted by
    // position rather than by presence, because all three lines can be there in the
    // wrong order and every other test in this file still passes.
    for (assembled_at, _) in src.match_indices("let mut assembled = assemble(") {
        let rest = &src[assembled_at..];
        let framed_at = rest
            .find("frame_external(&mut assembled);")
            .expect("an assembly this loop never frames");
        let user_at = rest
            .find("let user = match &conversational {")
            .expect("an assembly this loop never turns into a user block");
        assert!(
            framed_at < user_at,
            "a loop derives `user` before framing, so its flat string and its transcript \
             are no longer the same bytes"
        );
    }
}

// ------------------------------------------------------------------------ O3

/// O3 — no shipped sentence still says the transcript carries no breakpoint.
///
/// Two places said it before this release and one of them was a doc comment on a
/// function this release edits, which is the easiest kind to leave behind: the code
/// changes, the paragraph above it does not, and the crate ships documentation that
/// contradicts itself. `validate` cannot see this and neither can a compiler; only a
/// grep can, so here is the grep.
///
/// `CHANGELOG.md` is deliberately **not** searched. Its 0.38.0 entry says the
/// transcript is unmarked and that is a true statement about what 0.38.0 shipped — a
/// changelog is immutable history, not a claim about today.
#[test]
fn nothing_shipped_still_denies_the_second_breakpoint() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let stale = [
        "transcript deliberately carries no breakpoint",
        "Only the instructions are marked, never the transcript",
        "The transcript is not cached",
        "Nothing is marked in the transcript",
    ];

    for rel in [
        "README.md",
        "docs/CONTRACT.md",
        "docs/guide/providers.md",
        "src/provider/openai_wire.rs",
        "src/provider/anthropic.rs",
    ] {
        let text = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("{rel}: {e}"))
            .replace("\r\n", "\n");
        for phrase in stale {
            assert!(
                !text.contains(phrase),
                "{rel} still says {phrase:?}, which 0.44.0 made false"
            );
        }
    }
}

// ------------------------------------------------------------------------ F8

/// F8 — `CacheMarked` fires when the marked prefix changes, and not once per step.
///
/// This is 0.34.0's `Routed` defect reproduced deliberately: a rule applied to each
/// freshly built request reports a transition every step and stops meaning anything.
/// The count is what discriminates — a run marking one prefix for many steps emits
/// once, and the withdrawal-and-return of F4 emits a second time and no more.
#[tokio::test]
async fn cache_marked_fires_on_change_and_not_once_per_step() {
    let dir = workspace();
    let mut script: Vec<Vec<ToolCall>> = NAMES.iter().map(|n| vec![read(n)]).collect();
    script.push(vec![remember("layout", "the parser lives in src/parse.rs")]);
    script.push(vec![read("alpha.txt")]);
    script.push(vec![read("beta.txt")]);
    let steps = script.len() as u32;

    let provider = Recorder::new(script);
    let store = Store::memory().unwrap();
    let marks = Marks::default();
    let seen = Arc::clone(&marks.0);

    io_harness::run_with_observed(
        &contract(dir.path(), steps),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &marks,
    )
    .await
    .unwrap();

    let events = seen.lock().unwrap().clone();
    let marked_steps: Vec<usize> = provider
        .working()
        .iter()
        .enumerate()
        .filter(|(_, r)| r.cache_boundary.is_some())
        .map(|(i, _)| i)
        .collect();

    assert!(
        marked_steps.len() > events.len(),
        "the event must be rarer than the marking: {} marked steps, {} events",
        marked_steps.len(),
        events.len()
    );
    assert_eq!(
        events.len(),
        2,
        "one for the first prefix and one for the prefix the note moved it to, got {events:?}"
    );

    // Each event's `prefix_bytes` is the offset that was actually sent on that step,
    // and its `through_step` is the envelope's step.
    let working = provider.working();
    for (through_step, prefix_bytes) in &events {
        let sent = working
            .iter()
            .find(|r| r.cache_boundary == Some(*prefix_bytes as usize))
            .unwrap_or_else(|| panic!("no request carried {prefix_bytes} bytes"));
        assert_eq!(sent.cache_boundary, Some(*prefix_bytes as usize));
        assert!(*through_step > 0, "a marker is never sent before step 1");
    }

    // The two prefixes really are different, which is what "on change" means.
    assert_ne!(events[0].1, events[1].1, "{events:?}");
}

/// The negative control: a run that cannot fold emits none.
#[tokio::test]
async fn a_run_that_never_folds_emits_no_cache_marked() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();
    let marks = Marks::default();
    let seen = Arc::clone(&marks.0);

    let never = contract(dir.path(), NAMES.len() as u32).with_compaction(Compaction {
        at_share: 1.0,
        ..Compaction::default()
    });

    io_harness::run_with_observed(
        &never,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        &marks,
    )
    .await
    .unwrap();

    assert!(
        seen.lock().unwrap().is_empty(),
        "the absence of the event is the signal that nothing was marked"
    );
}

// ------------------------------------------------------------------------ N5

/// N5 — a run that never folds is 0.43.0 exactly.
///
/// With compaction off, no request carries a boundary at all. The cost of this release
/// to a run too short to fold is zero, and that is asserted rather than reasoned.
#[tokio::test]
async fn a_run_that_never_folds_marks_nothing() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();

    let never = contract(dir.path(), NAMES.len() as u32).with_compaction(Compaction {
        at_share: 1.0,
        ..Compaction::default()
    });

    run_with(&never, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let working = provider.working();
    assert!(!working.is_empty(), "the run made no requests");
    for (i, req) in working.iter().enumerate() {
        assert_eq!(
            req.cache_boundary, None,
            "request {i} was marked in a run that cannot fold"
        );
        assert!(
            !req.user.contains(SUMMARY_SENTENCE),
            "request {i} carries a summary in a run that cannot fold"
        );
    }
}

// ------------------------------------------- 0.49.0: the same boundary, in messages

/// The transcript's own text through `cache_through`, or `None` when unmarked.
fn marked_messages(req: &CompletionRequest) -> Option<String> {
    let through = req.cache_through?;
    Some(
        req.messages[..through]
            .iter()
            .map(|m| match m {
                Message::User(text) => text.clone(),
                Message::Assistant { .. } => String::new(),
                Message::Results(results) => results.iter().map(|r| r.content.as_str()).collect(),
            })
            .collect(),
    )
}

/// **F7** — a request carrying a transcript marks the same content the byte offset
/// marks, expressed as a count of messages.
///
/// The two markers are asserted against each other rather than each against a
/// literal: `cache_through`'s messages must concatenate to exactly the span
/// `cache_boundary` names. That is what fails an implementation that marks a
/// plausible-looking message boundary somewhere else — which would cost a cache
/// write on every step while every other test in this file still passed.
///
/// And the guard's rule is asserted to survive the translation: the fold's own step
/// carries neither marker, because that prefix has been sent zero times.
#[tokio::test]
async fn the_transcript_marker_covers_the_span_the_byte_offset_names() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), NAMES.len() as u32),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let working = provider.working();
    let folded = working
        .iter()
        .position(|r| r.user.contains(SUMMARY_SENTENCE))
        .expect("the run never folded; nothing below is meaningful");

    assert_eq!(
        working[folded].cache_through, None,
        "the step the fold happened on must not be marked, in either expression"
    );

    let after = &working[folded + 1];
    assert!(
        !after.messages.is_empty(),
        "the step after the fold sends a transcript, or this asserts nothing"
    );
    let by_offset = marked(after).expect("the byte offset is still computed");
    let by_messages = marked_messages(after).expect("and the message count with it");
    assert_eq!(
        by_messages, by_offset,
        "the marked messages must cover exactly the span the offset names"
    );
    assert!(
        by_messages.trim_end().ends_with(SUMMARY_SENTENCE),
        "and that span still ends at the folded summary, got: …{}",
        &by_messages[by_messages.len().saturating_sub(120)..]
    );
    // Never the whole conversation: the last message is the turn being asked
    // about, and marking it would write a prefix that moves on every step.
    assert!(
        after.cache_through.expect("marked") < after.messages.len(),
        "the turn being asked about must stay outside the marked prefix"
    );
}

/// A run that never folds marks nothing, in the message expression as in the byte
/// one — 0.44.0's `a_run_that_never_folds_marks_nothing`, asserted again for the
/// field this release added.
#[tokio::test]
async fn a_run_that_never_folds_marks_no_messages() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().take(2).map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();
    let never = contract(dir.path(), 2).with_compaction(Compaction {
        at_share: 1.0,
        ..Compaction::default()
    });

    run_with(&never, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    for (i, req) in provider.working().iter().enumerate() {
        assert_eq!(req.cache_through, None, "request {i} was marked");
    }
}

// ---------------------------------------------- 0.77.0: the framing pays no cache

/// **F20** — 0.77.0's provenance framing is confined to transcript content, so the
/// system block is byte-identical across turns whose tool output changed.
///
/// The first breakpoint is the end of the `system` block, and it is a byte-prefix
/// condition like the second one: a single byte moved there is a cache *write* on
/// every step of every run, not a smaller read. Provenance framing wraps what a tool
/// returned — which sits after both markers — and this is the arm that says so
/// mechanically rather than by reading the diff and believing it.
///
/// Asserted on a run that reads a **different file every step**, so the observation
/// section, the transcript and every framed span move underneath a block that must
/// not. A framing applied one layer too high — to the whole prompt, to the
/// instructions, or to anything `compose` emits — fails here while F3, F4 and F7
/// still pass, because none of those looks at `system` at all.
///
/// The inequality on `user` is the anti-vacuity guard. Without it a run whose steps
/// happened to send the same prompt twice would pass this by asserting that nothing
/// changed anywhere, which is not the claim.
#[tokio::test]
async fn the_system_block_is_byte_identical_across_turns_whose_tool_output_changed() {
    let dir = workspace();
    let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
    let store = Store::memory().unwrap();

    run_with(
        &contract(dir.path(), NAMES.len() as u32),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let working = provider.working();
    assert!(
        working.len() > 1,
        "one working request proves nothing about what moves between turns"
    );
    assert!(
        working
            .iter()
            .any(|r| r.user != working[0].user && !r.messages.is_empty()),
        "the fixture must actually change its tool output and carry a transcript, \
         or this passes vacuously"
    );

    let first = &working[0].system;
    for (i, req) in working.iter().enumerate() {
        assert_eq!(
            &req.system, first,
            "the system block moved on working request {i}: the first cache breakpoint \
             is a byte prefix, so anything emitted into `system` per turn is a cache \
             write on every step of every run"
        );
    }
}

// ------------------------------------------------------------------------ O8

/// 0.57.0 O8 — selection does not cost the marker on a store that fits.
///
/// 0.57.0 chooses which notes survive the memory block's share by what the turn
/// is about, and the turn's signals grow as the run reads. The block is a
/// byte-prefix of the user turn, so a block whose CONTENT moved between steps
/// would withhold this marker on every step it moved — which is why the release
/// selects rather than reorders, and why what is printed is always the store's
/// own order.
///
/// Both regimes are asserted, because only stating the safe one would be
/// choosing the flattering half:
///
/// - A store inside its share is carried whole, so nothing selects, the block is
///   byte-identical every step, and the marker is offered exactly as it is with
///   no notes at all.
/// - A store past its share is the regime this release created. There the chosen
///   set can move when the run reads something new, and a moved prefix withholds
///   the marker — it can never add one. The inequality is asserted rather than a
///   count, because how many steps move is the ceiling's business.
#[tokio::test]
async fn a_store_that_fits_its_share_keeps_the_marker_selection_only_costs_it_past_the_cap() {
    async fn marks(notes: &[(&str, String)]) -> (usize, usize) {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let key = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (i, (k, v)) in notes.iter().enumerate() {
            store.memory_put(&key, k, v, 1, 1 + i as u32).unwrap();
        }
        let provider = Recorder::new(NAMES.iter().map(|n| vec![read(n)]).collect());
        run_with(
            &contract(dir.path(), NAMES.len() as u32),
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
        let working = provider.working();
        let marked_steps = working
            .iter()
            .filter(|r| r.cache_boundary.is_some())
            .count();
        (working.len(), marked_steps)
    }

    // No notes at all: the baseline this crate had before durable memory existed.
    let (_, bare) = marks(&[]).await;
    assert!(
        bare > 0,
        "the fixture must reach a fold and offer a boundary, or nothing below is a comparison"
    );

    // Two short notes, well inside the block's quarter-share: carried whole,
    // every step, so the prefix is as repeatable as it is with no notes.
    let small: Vec<(&str, String)> = vec![
        ("layout", "the parser lives in src/syn".to_string()),
        ("tests", "the fixtures are regenerated by hand".to_string()),
    ];
    let (_, fitting) = marks(&small).await;
    assert_eq!(
        fitting, bare,
        "a store that fits its share must not cost a single marked step"
    );

    // Twenty long notes on the subjects the run reads: past the share, so the
    // block selects, and the selection moves as the run reads new paths.
    let big: Vec<(&str, String)> = NAMES
        .iter()
        .enumerate()
        .flat_map(|(i, name)| {
            let stem = name.trim_end_matches(".txt");
            [
                (
                    i * 2,
                    format!("reading {stem} matters {}", "detail ".repeat(40)),
                ),
                (
                    i * 2 + 1,
                    format!("{stem} padding is ordinary {}", "detail ".repeat(40)),
                ),
            ]
        })
        .map(|(i, v)| (NOTE_KEYS[i], v))
        .collect();
    let (_, past) = marks(&big).await;
    assert!(
        past <= fitting,
        "a moving prefix can only withhold the marker, never add one: {past} > {fitting}"
    );
    println!("O8 marked steps — no notes {bare}, fitting {fitting}, past the cap {past}");
}

/// Keys for O8's over-cap fixture. Written out rather than formatted, so the
/// store's `(created_at, key)` order is legible in a failure.
const NOTE_KEYS: [&str; 20] = [
    "k00", "k01", "k02", "k03", "k04", "k05", "k06", "k07", "k08", "k09", "k10", "k11", "k12",
    "k13", "k14", "k15", "k16", "k17", "k18", "k19",
];

/// `src/run.rs` and every `src/run/<subject>.rs`, concatenated.
///
/// 0.63.0 moved the run subsystem's private machinery into submodules, so a
/// source-reading checker pointed at the parent alone now sees a fraction of it —
/// and a count that comes back zero reads exactly like a rule that was deleted.
/// The floor below is what turns "the walk went blind" into a failure instead of
/// a silent pass.
fn run_subsystem_source() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = std::fs::read_to_string(root.join("src/run.rs"))
        .expect("src/run.rs")
        .replace("\r\n", "\n");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("src/run"))
        .expect("src/run/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 5,
        "src/run/ holds only {} modules — the split has been undone or this walk is blind, \
         and either way every count taken from it is meaningless",
        paths.len()
    );
    for path in paths {
        all.push('\n');
        all.push_str(
            &std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n"),
        );
    }
    all
}
