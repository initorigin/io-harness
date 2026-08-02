//! A deterministic, offline run that parks *live* on a gate nothing in its own
//! process can answer, used by `tests/attach.rs` to prove that a second process
//! can answer it — and that killing either process does what 0.33.0 claims.
//!
//! This is the difference between this fixture and `crash_fixture`,
//! `plan_gate_fixture` and `fleet_fixture`, which all park after the run has
//! *stopped*. Here the gate itself never returns, so the run is still going, still
//! holding its question, and unreachable by any `resume_*` call — which is the
//! whole situation 0.33.0 exists to fix. The parent answers it through
//! [`Attach`](io_harness::Attach) and this process finishes on its own, never
//! killed and never resumed.
//!
//! Usage:
//!
//! ```text
//! attach_fixture approve  <db> <workspace>   # parks in the Approver
//! attach_fixture question <db> <workspace>   # parks in the Responder
//! attach_fixture plan     <db> <workspace>   # parks in the PlanGate
//! attach_fixture watch    <db> <run_id>      # attaches and polls until killed
//! ```
//!
//! Every mode broadcasts, so the durable stream exists for the parent to read.
//! Nothing here touches the network or needs an API key.

use std::sync::Arc;
use std::time::Duration;

use io_harness::approve::{AnswerFuture, DecisionFuture, PlanReview};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with_observed, Approver, Attach, Broadcast, Ignore, Plan, Policy, Provider, Question,
    Request, Responder, Store, TaskContract, Verification, ASK_QUESTION_TOOL, PROPOSE_PLAN_TOOL,
};
use serde_json::json;

/// An approver that never answers — the unattended run holding an approval.
///
/// Not [`DenyAll`](io_harness::DenyAll) and not a `Defer`: both of those *answer*,
/// and a run that has been answered is not holding anything. This is the terminal
/// nobody is sitting in front of.
#[derive(Debug)]
struct NeverDecides;

impl Approver for NeverDecides {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(park())
    }
}

/// The same, for a question.
#[derive(Debug)]
struct NeverAnswers;

impl Responder for NeverAnswers {
    fn answer<'a>(&'a self, _question: &'a Question) -> AnswerFuture<'a> {
        Box::pin(park())
    }
}

/// The same, for a plan.
#[derive(Debug)]
struct NeverReviews;

impl io_harness::PlanGate for NeverReviews {
    fn review<'a>(&'a self, _plan: &'a Plan) -> PlanReview<'a> {
        Box::pin(park())
    }
}

/// Wait forever without spinning.
async fn park<T>() -> T {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Asks once if it is offered the tool, proposes once if it is offered that, and
/// otherwise writes the file the contract is about.
///
/// Driven off the tools it was offered rather than a step counter, so the fixture
/// keeps working if the loop's step numbering ever changes.
struct Scripted {
    /// Only the `question` mode asks. The tool is offered on every run, so
    /// without this the `approve` mode would park on a question the parent is not
    /// waiting for instead of on the approval it is.
    ask: bool,
}

impl Provider for Scripted {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let offered = |name: &str| req.tools.iter().any(|t| t.name == name);
        // Already asked or already planned? The transcript says so, and asking
        // twice would park on a second question the parent is not waiting for.
        let asked = req.user.contains("[answer]");
        let call = if offered(PROPOSE_PLAN_TOOL) {
            ToolCall {
                name: PROPOSE_PLAN_TOOL.into(),
                arguments: json!({ "steps": [{ "intent": "write SOLUTION-DONE into out.txt" }] }),
            }
        } else if self.ask && offered(ASK_QUESTION_TOOL) && !asked {
            ToolCall {
                name: ASK_QUESTION_TOOL.into(),
                arguments: json!({ "question": "which file?", "choices": ["out.txt", "other.txt"] }),
            }
        } else {
            ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "out.txt", "content": "SOLUTION-DONE\n" }),
            }
        };
        Ok(CompletionResponse {
            tool_calls: vec![call],
            usage: Some(Usage {
                total_tokens: 10,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io_harness::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    let db = args.next().expect("db path");
    let third = args.next().expect("workspace dir or run id");

    let store = Store::open(&db)?;

    // The observer half of the release. A second process reads what this writes.
    let watching = Broadcast::new(Store::open(&db)?, &Ignore);

    if mode == "watch" {
        // The attaching process, for the "the observer dying changes nothing"
        // test. It only ever reads — there is no method here that could do more.
        let mut view = Attach::to(&store, third.parse().expect("run id"));
        loop {
            for event in view.poll()? {
                println!("saw {} step {}", event.run_id, event.step);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let root = third;
    // `Policy::default()`'s write tier is `Ask`, so the write below reaches the
    // approver rather than going straight through.
    let policy = Policy::default()
        .layer("fixture")
        .allow_read("*")
        .allow_exec("*");

    // Verified rather than step-capped, so the run *stops* once the answer has
    // had its effect. Without a verification the loop would come straight back to
    // the approver that never answers, and "the fixture exited" would be a claim
    // about the step cap rather than about the answer reaching it.
    let mut contract = TaskContract::workspace("write SOLUTION-DONE into out.txt", &root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "SOLUTION-DONE".into(),
        })
        .with_max_steps(6);
    if mode == "plan" {
        contract = contract.with_plan_gate(Arc::new(NeverReviews));
    }
    if mode == "question" {
        contract = contract.with_responder(Arc::new(NeverAnswers));
    }

    let provider = Scripted {
        ask: mode == "question",
    };
    let outcome = run_with_observed(
        &contract,
        &provider,
        &store,
        &policy,
        &NeverDecides,
        &watching,
    )
    .await;
    let result = match &outcome {
        Ok(r) => r,
        Err(e) => {
            println!("error={e}");
            return Ok(());
        }
    };

    // What it acted on, read back from the durable trace rather than from
    // anything this process decided — the same discipline the loop itself uses.
    // A fixture that printed its own intent would report `approve` whether or not
    // the answer had ever reached it.
    let decisions: Vec<String> = store
        .events(result.run_id)?
        .into_iter()
        .filter_map(|e| e.decision)
        .collect();
    println!("run_id={}", result.run_id);
    println!("decisions={}", decisions.join(","));
    println!("outcome={:?}", result.outcome);
    println!(
        "wrote={}",
        std::path::Path::new(&root).join("out.txt").exists()
    );
    Ok(())
}
