//! Live OpenRouter run against a **non-Rust** project — OP5, the criterion the
//! test suite cannot satisfy.
//!
//! The suite proves the mechanism; only a live run proves the claim. A model
//! that is not told what to do, working in a JavaScript project it has to
//! discover, has to: read the detected toolchain out of its own context, install
//! the project's dependency through `exec`, run the suite, read a real test
//! runner's real failure, find the bug, fix it with `edit_file` rather than by
//! rewriting the file, and stop when the project's own `npm test` exits zero.
//!
//! No Rust is on that path anywhere.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example exec_live
//! ```
//!
//! Needs `node` and `npm` on PATH and network access to the npm registry — the
//! install runs through `exec`, which is deliberately **not** sandboxed, because
//! the sandbox denies egress and `npm install` would be impossible inside it.

use std::time::Duration;

use io_harness::{
    run_with, ApproveAll, OpenRouter, Policy, RunOutcome, Store, TaskContract, Verification,
};

/// The bug the agent has to find: `sum` starts its accumulator at 1.
const BUGGY: &str = r#"const isOdd = require('is-odd');

function sum(numbers) {
  let total = 1;
  for (const n of numbers) {
    total += n;
  }
  return total;
}

function oddsOnly(numbers) {
  return numbers.filter((n) => isOdd(n));
}

module.exports = { sum, oddsOnly };
"#;

const TESTS: &str = r#"const assert = require('node:assert');
const { sum, oddsOnly } = require('./index.js');

assert.strictEqual(sum([]), 0, 'the sum of nothing is zero');
assert.strictEqual(sum([1, 2, 3]), 6);
assert.deepStrictEqual(oddsOnly([1, 2, 3, 4]), [1, 3]);
console.log('all assertions passed');
"#;

const PACKAGE_JSON: &str = r#"{
  "name": "exec-live-fixture",
  "version": "1.0.0",
  "private": true,
  "scripts": { "test": "node test.js" },
  "dependencies": { "is-odd": "3.0.1" }
}
"#;

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-exec-live");
    // A clean slate every time: a leftover node_modules would mean the run never
    // exercised the install, and a live run that quietly skipped half of what it
    // claims to prove is worse than no live run.
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("package.json"), PACKAGE_JSON)?;
    std::fs::write(root.join("index.js"), BUGGY)?;
    std::fs::write(root.join("test.js"), TESTS)?;

    let contract = TaskContract::workspace(
        "This JavaScript project's test suite does not pass. Install its dependencies, run \
         the suite, work out what is wrong from the failure, and fix it. Change as little as \
         possible: use edit_file rather than rewriting a whole file, and do not edit test.js.",
        &root,
    )
    // The project's own command is the definition of done. Nothing about this
    // criterion knows what language the project is written in.
    .with_verification(Verification::Command {
        argv: vec!["npm".into(), "test".into()],
        expect_exit: 0,
    })
    .with_max_steps(20)
    .with_token_budget(400_000)
    // A cold `npm install` on a slow link is the case the default is sized for;
    // this is well inside it and is set explicitly so the run states its own bound.
    .with_exec_timeout(Duration::from_secs(300));

    // The boundary the run works under, written the way an operator would write
    // it: the project's own toolchain is allowed, and the two things that would
    // reach outside this task are not.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("npm*")
        .allow_exec("node*")
        .deny_exec("npm publish*")
        .deny_exec("rm*")
        .deny_write("test.js");

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

    println!("outcome:  {:?}", result.outcome);
    println!("provider: {:?}", store.provider(result.run_id)?);
    if let Some(s) = result.summary(&store)? {
        println!("spend:    {} tokens", s.tokens);
    }

    // What the agent asked to do, step by step. `tool_call` is where the argvs
    // are: an allowed action writes no `policy_events` row — for `exec` no more
    // and no less than for a read or a write — so the step trace is what answers
    // "what actually ran", and `policy_events` answers "what was refused".
    println!("\n--- steps ---");
    for step in store.steps(result.run_id)? {
        println!("{:>3}  {}", step.step, step.decision);
        if !step.tool_call.is_empty() {
            println!("     {}", step.tool_call);
        }
    }

    println!("\n--- every exec row in policy_events ---");
    for e in store.events(result.run_id)? {
        if e.act == "exec" {
            println!(
                "step {:>3}  {}  {}  (rule {:?}, layer {:?})",
                e.step, e.kind, e.target, e.rule, e.layer
            );
        }
    }

    println!("\n--- index.js as the agent left it ---");
    println!("{}", std::fs::read_to_string(root.join("index.js"))?);

    // test.js is denied by the policy, so a run that "passed" by rewriting the
    // test would have been refused. Asserted rather than assumed.
    assert_eq!(
        std::fs::read_to_string(root.join("test.js"))?,
        TESTS,
        "the suite the run was judged against is the one it started with"
    );

    match result.outcome {
        RunOutcome::Success { steps } => {
            println!("\nverified by the project's own `npm test`, in {steps} steps")
        }
        other => println!("\ndid not reach a passing suite: {other:?}"),
    }
    Ok(())
}
