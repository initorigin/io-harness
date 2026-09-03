//! 0.76.0 — withholding a tool from a turn without moving the catalogue.
//!
//! Every assertion reads what a fixture provider actually received, because the
//! whole claim is about the bytes that go out. A helper's return value would
//! prove nothing about a request.
//!
//! Nothing here measures a duration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, Session, Store, TaskContract, ToolMask, ToolSpec,
    Verification,
};
use serde_json::json;

// ----------------------------------------------------------------- scaffolding

/// Records every request and plays a fixed script.
struct Rec {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Rec {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Rec {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "rec"
    }
}

/// A tool that writes its own name down the moment it is entered.
///
/// **"Did not run" has to be an observation rather than an absence.** A masked
/// `read_file` leaves no trace whether it was refused or simply read nothing, so
/// an assertion over it would pass for the wrong reason — the shape 0.41.0 paid
/// for with five `read_file` calls that could not express the defect. This one
/// records on entry, so the absence of its name means the body was never reached.
struct Marker {
    name: String,
    ran: Arc<Mutex<Vec<String>>>,
    effect: ToolEffect,
}

impl Marker {
    fn new(name: &str, ran: &Arc<Mutex<Vec<String>>>, effect: ToolEffect) -> Self {
        Self {
            name: name.into(),
            ran: Arc::clone(ran),
            effect,
        }
    }
}

impl Tool for Marker {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Reports that it ran.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.ran.lock().unwrap().push(self.name.clone());
            Ok(format!("{} ran", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("do the thing", root.to_string_lossy().as_ref())
        .with_max_steps(3)
        .with_verification(Verification::None)
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}

/// Drive a run and hand back the requests it made and the tools that ran.
async fn drive(
    contract: &TaskContract,
    steps: Vec<Vec<ToolCall>>,
    ran: &Arc<Mutex<Vec<String>>>,
) -> Vec<CompletionRequest> {
    let provider = Rec::new(steps);
    let store = Store::memory().unwrap();
    let _ = run_with(
        contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;
    let _ = ran;
    provider.requests()
}

// ------------------------------------------------------------------------- F1

/// F1 — a mask changes what may be called and not one byte of what is offered.
///
/// The two runs differ in exactly one thing, and the catalogue and the system
/// block must both compare equal. This is the assertion the whole design exists
/// to satisfy: Anthropic orders a request's cacheable prefix tools-then-system,
/// so a tool array that moved between an unmasked run and a masked one would
/// invalidate 0.38.0's breakpoint and everything after it.
#[tokio::test]
async fn a_mask_moves_no_byte_of_the_catalogue_or_the_system_block() {
    let dir = workspace();
    let ran = Arc::new(Mutex::new(Vec::new()));

    let plain = drive(&contract(dir.path()), vec![vec![]], &ran).await;
    let masked = drive(
        &contract(dir.path()).with_tool_mask(ToolMask::withholding([
            "write_file",
            "edit_file",
            "exec",
        ])),
        vec![vec![]],
        &ran,
    )
    .await;

    assert!(
        !plain.is_empty() && !masked.is_empty(),
        "both runs must have reached the provider, or this compares nothing"
    );
    assert!(
        plain[0].tools.len() > 3,
        "the catalogue must be non-trivial for its stability to mean anything, got {}",
        plain[0].tools.len()
    );
    assert_eq!(
        plain[0].tools, masked[0].tools,
        "a masked run must offer the identical catalogue: the tool array sits ahead of the \
         system breakpoint, so moving it costs a cache write on every later turn"
    );
    assert_eq!(
        plain[0].system, masked[0].system,
        "the mask must not reach the system block, which is what the first breakpoint covers"
    );
}

// ------------------------------------------------------------------------- F3

/// F3 — the mask is told to the model somewhere that costs no cache.
///
/// Asserted by position rather than by membership: the sentence must come *after*
/// the observation section, because that is what puts it past both breakpoints —
/// 0.38.0's at the end of `system`, and 0.44.0's inside the observations, at the
/// end of a fold's summary. A `contains` assertion would pass with the sentence
/// at the top of the prompt, which is the one place it must never be.
#[tokio::test]
async fn the_withheld_sentence_lands_after_the_observations_and_never_in_the_system_block() {
    let dir = workspace();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let seen = drive(
        &contract(dir.path()).with_tool_mask(ToolMask::withholding(["exec", "shell"])),
        vec![
            vec![ToolCall {
                name: "read_file".into(),
                arguments: json!({ "path": "a.txt" }),
            }],
            vec![],
        ],
        &ran,
    )
    .await;

    assert!(
        seen.len() >= 2,
        "the second step is the one with observations in it, got {} requests",
        seen.len()
    );
    let user = &seen[1].user;
    let obs = user
        .find("Observations so far")
        .expect("the prompt must carry an observation section for the ordering to mean anything");
    let sentence = user
        .find("Unavailable this turn")
        .expect("a masked turn must tell the model which tools are withheld");
    assert!(
        sentence > obs,
        "the withheld sentence must follow the observations, or it sits inside the prefix the \
         second breakpoint marks; obs at {obs}, sentence at {sentence}"
    );
    assert!(
        user.contains("hello"),
        "the observation section must actually hold this run's read, or the ordering above is \
         over a section that is empty"
    );
    let read_at = user.find("hello").unwrap();
    assert!(
        sentence > read_at,
        "the sentence must follow the last observation, not merely the section heading"
    );
    assert!(
        !seen[1].system.contains("Unavailable this turn"),
        "the mask must never reach the system block"
    );
    for name in ["exec", "shell"] {
        assert!(
            user.contains(name),
            "the withheld names must be named, or the model cannot tell which tools they are"
        );
    }
}

/// F3, the half that says changing the mask costs nothing.
///
/// A mask is a property of a turn, not of a step (`US-IO-HARNESS-0.76.0-I02`), so
/// the unit it varies between is a session turn. Three turns, three different
/// masks including an empty one, and the composed system block must not move —
/// which is the cached prefix the first breakpoint covers.
#[tokio::test]
async fn a_session_whose_turns_carry_different_masks_composes_one_system_block() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let masks = [
        ToolMask::none(),
        ToolMask::withholding(["exec"]),
        ToolMask::withholding(["shell", "write_file", "edit_file"]),
    ];

    let mut systems = Vec::new();
    for mask in masks {
        let provider = Rec::new(vec![vec![]]);
        let turn = contract(dir.path()).with_tool_mask(mask);
        let _ = session
            .turn_bounded(
                &turn,
                &provider,
                &store,
                &Policy::permissive(),
                &ApproveAll,
            )
            .await;
        let seen = provider.requests();
        assert!(
            !seen.is_empty(),
            "every turn must reach the provider, or the comparison below is over nothing"
        );
        systems.push((seen[0].system.clone(), seen[0].tools.clone()));
    }

    assert_eq!(systems.len(), 3, "three turns, three captures");
    for (i, (system, tools)) in systems.iter().enumerate().skip(1) {
        assert_eq!(
            system, &systems[0].0,
            "turn {i} composed a different system block than turn 0, so every turn after a mask \
             change is billed as a cache write"
        );
        assert_eq!(
            tools, &systems[0].1,
            "turn {i} offered a different catalogue than turn 0"
        );
    }
}

// ------------------------------------------------------------------------- F2

/// F2 — a masked call is refused at the head of `dispatch`, and starts nothing.
///
/// The tool records its own name on entry, so the assertion is over what did not
/// happen rather than over an absence nothing could have filled.
#[tokio::test]
async fn a_masked_call_is_refused_before_the_tool_is_entered() {
    let dir = workspace();
    let ran: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tools = Toolbox::new().with(Marker::new("marker_write", &ran, ToolEffect::Mutating));

    let contract = contract(dir.path())
        .with_tools(tools)
        .with_tool_mask(ToolMask::withholding(["marker_write"]));

    let provider = Rec::new(vec![vec![call("marker_write")], vec![]]);
    let store = Store::memory().unwrap();
    let _ = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;

    assert!(
        ran.lock().unwrap().is_empty(),
        "the masked tool ran: {:?}",
        ran.lock().unwrap()
    );
    let seen = provider.requests();
    assert!(
        seen.len() >= 2,
        "the run must have taken a second step for its observation to be readable"
    );
    assert!(
        seen[1].user.contains("marker_write refused"),
        "the model must be told the call was refused rather than left to infer it from silence"
    );
}

/// F2, the other entry point. A batch of read-only calls does not route through
/// `dispatch` at all — `read_batch` is its own loop — so a mask enforced in one
/// place would let a batched call through.
///
/// Two read-only calls in one completion is what puts the run on the batch path;
/// one of them is masked and the other is not, so the run is the control for
/// itself.
#[tokio::test]
async fn a_masked_call_inside_a_read_batch_is_refused_and_its_sibling_still_runs() {
    let dir = workspace();
    let ran: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tools = Toolbox::new()
        .with(Marker::new("marker_a", &ran, ToolEffect::ReadOnly))
        .with(Marker::new("marker_b", &ran, ToolEffect::ReadOnly));

    let contract = contract(dir.path())
        .with_tools(tools)
        .with_tool_mask(ToolMask::withholding(["marker_a"]));

    let provider = Rec::new(vec![vec![call("marker_a"), call("marker_b")], vec![]]);
    let store = Store::memory().unwrap();
    let _ = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;

    let ran = ran.lock().unwrap().clone();
    assert!(
        ran.contains(&"marker_b".to_string()),
        "the unmasked sibling must still run, or this test proves the batch broke rather than \
         that the mask bit; ran {ran:?}"
    );
    assert!(
        !ran.contains(&"marker_a".to_string()),
        "the masked call ran inside the batch: `read_batch` does not route through `dispatch`, \
         so a mask applied only there is not applied at all; ran {ran:?}"
    );
}

// ------------------------------------------------------------------------- F4

/// F4 — a contract that names no mask sends what it sent before.
///
/// The sentence is the only thing masking adds to a request, so its absence from
/// an unmasked run is the whole claim. Paired with a positive control, because a
/// "does not contain" assertion over an empty string passes for the wrong reason.
#[tokio::test]
async fn an_unmasked_turn_carries_no_trace_of_the_feature() {
    let dir = workspace();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let seen = drive(&contract(dir.path()), vec![vec![]], &ran).await;

    assert!(!seen.is_empty(), "the run must have reached the provider");
    assert!(
        seen[0].user.contains("Call a tool to make progress"),
        "the ordinary prompt must be present, or the absence below is an absence of everything"
    );
    assert!(
        !seen[0].user.contains("Unavailable"),
        "an unmasked turn must say nothing about availability"
    );
    assert!(!seen[0].system.contains("Unavailable"));
}
