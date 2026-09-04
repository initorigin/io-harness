//! The live evidence for 0.79.0 (O3) and the measurement behind it (O6): one
//! real task run twice against a real provider, once as a program and once as a
//! chain of tool calls.
//!
//! ```text
//! set -a; . ./.env; set +a
//! cargo run --features codeact --example codeact_live
//! ```
//!
//! # What only a live run can show
//!
//! The suite drives programs a test wrote. That proves the machinery and proves
//! the boundary, and it says nothing about the thing this capability actually
//! rests on: whether a model, handed the generated surface and told what it may
//! call, writes a program that uses it. A fixture program cannot be evidence of
//! that, because a fixture is written by somebody who already knows the answer.
//!
//! # What is measured, and what is not
//!
//! Token totals come from this crate's own `provider_calls` rows — the same
//! accounting every run already keeps — so the number is what the provider
//! actually billed rather than an estimate of it.
//!
//! **Nothing here is a gate.** The example prints and asserts nothing about the
//! numbers. A run where the program costs *more* is a publishable result and the
//! record says what was measured, not what was hoped for: a chain of round trips
//! resends the whole transcript each time, and a program does not, but whether
//! that dominates depends on the task, the model and the context settings — which
//! is precisely why the vendor's figure, measured on a different harness, is not
//! repeated as this crate's.
//!
//! The two runs use the same workspace contents, the same model and the same
//! contract. They are one sample each. `docs/MEASUREMENTS.md` records the machine
//! and the method; a reader who wants a distribution runs it more than once.
//!
//! **The provider is OpenRouter, and the record says so.** `OPENROUTER_API_KEY`
//! is the key this checkout carries; the Anthropic and OpenAI entries are empty.

use std::sync::Mutex;

use io_harness::{
    run_with_observed, ApproveAll, CodeActConfig, EventKind, Flow, Observer, OpenRouter, Policy,
    RunEvent, Store, TaskContract,
};

/// Counts the programs that ran and the callbacks they made.
///
/// The arm is not evidence unless a program was actually written *and used*. The
/// first live run of this release finished with the right answer having never
/// used the capability — the model wrote a program that reached for the workspace
/// with Python's own file reads, got nothing, and did the task as ordinary tool
/// calls in the steps that followed — and the second was refused outright because
/// this example's own policy denied exec, which starting an interpreter now is.
/// Both looked like successes from the outside. So the run reports what happened
/// rather than leaving it to be inferred from the total.
#[derive(Default)]
struct Programs(Mutex<Vec<(String, u32)>>);

impl Observer for Programs {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Program { calls, outcome, .. } = &event.kind {
            self.0.lock().unwrap().push((outcome.clone(), *calls));
        }
        Flow::Continue
    }
}

/// The two variables this example cannot run without. The operator sources the
/// repository's `.env`; nothing here reads that file.
const REQUIRED: [&str; 2] = ["OPENROUTER_API_KEY", "OPENROUTER_MODEL"];

/// A task a single command cannot do, which is what makes it a test of this
/// capability rather than of the toolchain.
///
/// An earlier version of this example asked for a line count. Given `exec`, the
/// model did the whole job with one command and never wrote a program — a true
/// and useful answer about when a program is worth reaching for, and useless as
/// evidence that programs work. This task needs a **loop with a branch and a
/// write per iteration**: `exec` takes a fixed argv and never a shell string, and
/// the `shell` tool's grammar refuses control flow by name, so neither can
/// express it. A program can.
const GOAL: &str = "Every .txt file in the workspace root that has fewer than three lines must \
                    have the single line SHORT appended to it. Files with three or more lines are \
                    left exactly as they are. Then write the number of files you changed, alone, \
                    into COUNT.txt.";

/// Six files, four of them under the threshold, so the branch is exercised in
/// both directions and the answer is not the file count.
const FILES: [(&str, &str); 6] = [
    ("alpha.txt", "one\ntwo\nthree\n"),
    ("beta.txt", "one\n"),
    ("gamma.txt", "one\ntwo\n"),
    ("delta.txt", "one\ntwo\nthree\nfour\n"),
    ("epsilon.txt", "one\ntwo\n"),
    ("zeta.txt", "one\n"),
];

/// What a correct run writes into `COUNT.txt`: `beta`, `gamma`, `epsilon` and
/// `zeta` are the four under three lines.
const EXPECTED: &str = "4";

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    for name in REQUIRED {
        if std::env::var(name).unwrap_or_default().is_empty() {
            eprintln!(
                "{name} is not set, and this example makes real provider calls.\n\
                 Set both variables and run it again:\n\n  \
                 set -a; . ./.env; set +a\n  \
                 cargo run --features codeact --example codeact_live\n"
            );
            std::process::exit(2);
        }
    }
    let model = std::env::var("OPENROUTER_MODEL").unwrap_or_default();
    println!("model: {model}");

    // Three arms, because two were not enough to tell two different things
    // apart: whether a model *chooses* a program, and whether what it writes
    // when it does actually works. The offered arm answers the first and has
    // said no three times running; the directed one answers the second.
    let offered = measure(Arm0::Offered).await?;
    let directed = measure(Arm0::Directed).await?;
    let chain = measure(Arm0::Chain).await?;

    println!("\n  arm            steps  prompt  completion   total");
    println!("  ------------------------------------------------");
    report("offered", &offered);
    report("directed", &directed);
    report("tool calls", &chain);

    let with_program = &directed;
    let with_calls = &chain;

    // Printed as a ratio because that is the shape of the claim, and printed
    // without a verdict because the example asserts nothing.
    if with_calls.total > 0 {
        let ratio = with_program.total as f64 / with_calls.total as f64;
        println!("\n  program / chain: {ratio:.2}x total tokens");
    }
    println!(
        "\nOne sample each, on this machine, with this model. Nothing above is asserted \
         anywhere;\nthe method and the machine belong in docs/MEASUREMENTS.md beside the numbers."
    );
    Ok(())
}

/// What one arm cost.
struct Arm {
    steps: usize,
    prompt: u64,
    completion: u64,
    total: u64,
    answer: Option<String>,
}

fn report(label: &str, arm: &Arm) {
    println!(
        "  {label:<13}  {:>4}  {:>6}  {:>10}  {:>6}   -> COUNT.txt {} {}",
        arm.steps,
        arm.prompt,
        arm.completion,
        arm.total,
        match &arm.answer {
            Some(text) => format!("{:?}", text.trim()),
            None => "(not written)".to_string(),
        },
        // Whether the arm got the task right at all. A cheap wrong answer is not
        // a cheaper way of doing the work, and a ratio beside one would be
        // comparing a shortcut to a solution.
        match arm.answer.as_deref().map(str::trim) {
            Some(EXPECTED) => "correct",
            _ => "WRONG",
        }
    );
}

/// Which of the three arms this is.
///
/// `Offered` is the honest question — the tool is there, nothing points at it,
/// and what the model does is the answer. `Directed` asks the model to use it, so
/// that the surface itself is exercised live even when the model would not have
/// chosen it; without that arm a release could ship a generated module no model
/// had ever actually called. `Chain` is the baseline.
#[derive(Clone, Copy, PartialEq)]
enum Arm0 {
    Offered,
    Directed,
    Chain,
}

/// Run the task once and read the cost off the run's own accounting rows.
async fn measure(arm: Arm0) -> io_harness::Result<Arm> {
    let program = arm != Arm0::Chain;
    let label = match arm {
        Arm0::Offered => "offered",
        Arm0::Directed => "directed",
        Arm0::Chain => "calls",
    };
    let root = std::env::temp_dir().join(format!("io-harness-codeact-live-{label}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root)?;
    for (name, body) in FILES {
        std::fs::write(root.join(name), body)?;
    }

    let goal = match arm {
        // The nudge is one sentence and it is deliberately explicit. An arm that
        // hinted would measure how well the hint was worded.
        Arm0::Directed => format!(
            "{GOAL} Do all of it inside a single run_program call rather than as separate tool \
             calls."
        ),
        _ => GOAL.to_string(),
    };
    let mut contract = TaskContract::workspace(&goal, &root).with_max_steps(14);
    if program {
        contract = contract.with_codeact(CodeActConfig::default());
    }

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;
    // Both arms get the same permissions, including exec — which starting a
    // program now needs, since the interpreter is checked like any other binary.
    // Denying exec here refused the program outright and made the comparison
    // meaningless while still printing a number, which is the trap this line
    // exists to have already fallen into once.
    //
    // It does mean the chain arm may shell out to `wc` and win on this task
    // without a program. That is a real answer rather than a flaw in the test:
    // where one command does the whole job, a program is not what to reach for,
    // and the run below says which arm did what.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*");

    println!("\n[{label}] running…");
    let seen = Programs::default();
    let result =
        run_with_observed(&contract, &provider, &store, &policy, &ApproveAll, &seen).await?;
    println!("[{label}] outcome: {:?}", result.outcome);
    let programs = seen.0.lock().unwrap().clone();
    let ran: Vec<&(String, u32)> = programs
        .iter()
        .filter(|(outcome, _)| outcome != "available" && outcome != "withheld")
        .collect();
    if program {
        match ran.as_slice() {
            [] => println!("[{label}] NO PROGRAM RAN — this arm is not evidence"),
            got => {
                let calls: u32 = got.iter().map(|(_, c)| c).sum();
                println!(
                    "[{label}] {} program(s), {calls} callback(s): {}",
                    got.len(),
                    got.iter()
                        .map(|(o, c)| format!("{o}/{c}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if calls == 0 {
                    println!(
                        "[{label}] a program ran and called nothing — this arm is not evidence"
                    );
                }
            }
        }
    }

    let calls = store.provider_calls(result.run_id)?;
    let mut arm = Arm {
        steps: calls.len(),
        prompt: 0,
        completion: 0,
        total: 0,
        answer: std::fs::read_to_string(root.join("COUNT.txt")).ok(),
    };
    for call in &calls {
        // A provider that reported nothing leaves `usage` as `None`, and an
        // unreported call is not a free one — it is skipped rather than counted
        // as zero, and the step count above still includes it.
        if let Some(usage) = &call.usage {
            arm.prompt += usage.prompt_tokens;
            arm.completion += usage.completion_tokens;
            arm.total += usage.total_tokens;
        }
    }
    Ok(arm)
}
