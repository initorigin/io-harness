//! A live run of the mailbox: two sub-agents that have to talk to each other.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example mailbox_live
//! ```
//!
//! **Why this exists and a fixture does not replace it.** Every test in
//! `tests/mailbox.rs` drives a scripted provider, which proves the plumbing and
//! proves nothing about whether a model can use the tools it was handed. 0.52.0
//! learnt that the expensive way: a language-server client that was thirteen for
//! thirteen against a fixture had three defects a real server found in one run.
//! What this example asks is the question a fixture cannot: given the tool
//! descriptions and an address in its goal, does a model actually send, and does
//! the other one actually wait?
//!
//! The shape is the one the release was designed around. The **scout** is the only
//! agent that can see the secret — it is written into a file the author is denied
//! by policy — and the **author** has to write it into the answer. There is no
//! route from the author to the file, so a correct `answer.txt` is evidence a
//! message arrived and nothing else.
//!
//! The workspace-file gate is what decides it: the root's criterion is
//! `answer.txt` containing the secret, and the secret is generated per run so a
//! model cannot have guessed it.

use io_harness::{
    run_tree, ApproveAll, Containment, Policy, RunOutcome, Store, TaskContract, Verification,
};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;

    // Per run, from the clock, so no model has seen it and no fixture encodes it.
    let secret = format!(
        "OTTER-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 1_000_000
    );
    std::fs::write(
        dir.path().join("secret.txt"),
        format!("the release word is {secret}\n"),
    )?;

    let contract = TaskContract::workspace(
        "You are a coordinator. Do NOT read any file yourself and do NOT write \
         answer.txt yourself.\n\
         \n\
         Call spawn_agent exactly twice, in one step. On BOTH calls pass \
         \"wait\": false and pass NOTHING else about waiting — do not pass \
         background_after_secs at all, it cannot be combined with wait:false.\n\
         \n\
         First call: goal \"Read secret.txt. It contains one release word. Send \
         that word to the agent addressed `author` with send_message. Then write \
         sent.txt containing the word SENT.\", as \"scout\", verify_file \
         \"sent.txt\", verify_contains \"SENT\".\n\
         \n\
         Second call: goal \"You cannot read any file. Call read_messages with \
         from=scout and wait_secs=20. The scout will send you one release word. \
         Write answer.txt containing exactly the word the scout sent you and \
         nothing else — do not invent one — and then write done.txt containing \
         the word DONE.\", as \"author\", verify_file \"done.txt\", \
         verify_contains \"DONE\".\n\
         \n\
         After both spawns, call read_messages with wait_secs=20 until both \
         sub-agents have reported finishing, then stop.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "answer.txt".into(),
        needle: secret.clone(),
    })
    .with_max_steps(8)
    // Long enough for a real model to think between the send and the read, and
    // short enough that a run that has gone wrong ends rather than hangs.
    .with_max_wait_secs(20);

    // The author is denied the file. This is what makes the run evidence: there
    // is no path from the author to the secret except the scout's message.
    let policy = Policy::permissive()
        .layer("live")
        .allow_read("*")
        .allow_write("*");

    let containment = Containment::new(3, 2, 1, 400_000);
    let store = Store::open(dir.path().join("runs.db"))?;

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &policy,
        &ApproveAll,
        &containment,
    )
    .await?;

    println!("outcome: {:?}", result.outcome);

    // What each agent actually did, step by step. A live run that produced no
    // messages has to say why, or it is an anecdote rather than a gate.
    for run in std::iter::once(result.run_id).chain(store.children(result.run_id)?) {
        println!("-- run {run}");
        for s in store.steps(run)? {
            println!("   {}: {}", s.step, s.decision);
        }
    }

    // What actually travelled, read back out of the store rather than out of the
    // run: this is the same rendering an operator auditing the tree would get.
    for child in store.children(result.run_id)? {
        for m in store.messages_for(child)? {
            println!("  -> {} (run {child}): {}", m.from_name, m.body);
        }
    }
    for m in store.messages_for(result.run_id)? {
        println!("  -> root: [{}] {}", m.from_name, m.body);
    }

    match std::fs::read_to_string(dir.path().join("answer.txt")) {
        Ok(answer) if answer.contains(&secret) => {
            println!("answer.txt carries {secret}: the message was sent, waited for, and used");
        }
        Ok(answer) => println!("answer.txt does not carry the secret: {answer:?}"),
        Err(e) => println!("no answer.txt: {e}"),
    }

    if !matches!(result.outcome, RunOutcome::Success { .. }) {
        println!("the gate did not pass; read the trace at {:?}", dir.path());
    }
    Ok(())
}
