//! Live session: a conversation, streamed, steered, branched, and reopened (0.20.0).
//!
//! The suite proves the mechanism against a scripted provider. Only a live
//! session proves an operator can hold a conversation with this crate — that the
//! second turn knows what the first one established, that text arrives while the
//! model is still producing it, that a mid-turn message changes what the agent
//! does next, that a branch forgets the turns after it without deleting them, and
//! that a session picked up from its id alone still remembers.
//!
//! Every claim below is asserted by this file, so a regression fails the example
//! rather than printing output nobody reads.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example session_live
//! ```

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use io_harness::{
    ApproveAll, EventKind, Flow, Observer, OpenRouter, Policy, RunEvent, Session, Steer, Store,
};

/// Prints the answer as it is typed, and records what it saw so the example can
/// assert that it saw it before the turn was over.
#[derive(Default)]
struct Live {
    text: Mutex<String>,
    deltas: AtomicUsize,
    first_at: Mutex<Option<Instant>>,
    /// Deltas seen since the last step committed, and how many steps had their
    /// text streamed before they committed.
    ///
    /// Per step, not per turn: a turn's early steps are tool calls that produce no
    /// text at all, so "a delta arrived before ANY step committed" is the wrong
    /// question — the claim is that the text of a step reaches the operator before
    /// that step is over.
    since_step: AtomicUsize,
    steps_streamed_ahead: AtomicUsize,
}

impl Observer for Live {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::Token { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.text.lock().unwrap().push_str(text);
                self.deltas.fetch_add(1, Ordering::SeqCst);
                let mut first = self.first_at.lock().unwrap();
                if first.is_none() {
                    *first = Some(Instant::now());
                }
                self.since_step.fetch_add(1, Ordering::SeqCst);
            }
            EventKind::Step { .. } => {
                if self.since_step.swap(0, Ordering::SeqCst) > 0 {
                    self.steps_streamed_ahead.fetch_add(1, Ordering::SeqCst);
                }
                println!();
            }
            _ => {}
        }
        Flow::Continue
    }
}

/// Steers the turn the first time the agent reads a file: an operator watching the
/// stream and typing a correction, without a terminal to type it into.
struct SteerOnFirstToolCall {
    steer: Steer,
    said: AtomicBool,
}

impl Observer for SteerOnFirstToolCall {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::ToolCall { .. } = &event.kind {
            if !self.said.swap(true, Ordering::SeqCst) {
                println!("\n[operator types] stop; only write to SECOND.md");
                let _ = self.steer.say(
                    "change of plan: do not write NOTES.md at all. Write SECOND.md instead, \
                     with the same content.",
                );
            }
        }
        Flow::Continue
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-session-example");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("src/alpha.rs"),
        "pub fn alpha() -> u32 { 41 }\npub fn helper() -> u32 { 1 }\n",
    )?;
    std::fs::write(root.join("src/beta.rs"), "pub fn beta() -> u32 { 0 }\n")?;

    // `git*` is allowed for a reason worth knowing: a *denied* git built-in
    // propagates `Error::Refused` out of the run rather than becoming an
    // observation the model can adapt to (see docs/CONTRACT.md). This example is
    // about sessions, so it does not want a stray `git status` ending a turn.
    let policy = Policy::default()
        .layer("session-example")
        .allow_read("*")
        .allow_write("*.md")
        .allow_write("src/*")
        .allow_exec("git*");
    let provider = OpenRouter::from_env()?;
    let db = root.join("runs.db");
    let store = Store::open(&db)?;

    // ---- turn one, streamed ------------------------------------------------
    let mut session = Session::open(&store, &root)?;
    println!("session {} over {}\n", session.id(), root.display());
    println!("--- turn 1 -------------------------------------------------------");
    let live = Live::default();
    let started = Instant::now();
    let first = session
        .turn_observed(
            "Read every file under src/ and tell me, in one sentence, which function \
             returns the largest number. Do not write anything.",
            &provider,
            &store,
            &policy,
            &ApproveAll,
            &live,
        )
        .await?;
    println!("\noutcome: {:?}", first.outcome);

    // Streaming, asserted: deltas arrived, they arrived before the step that
    // carried them committed, and they reconstruct what the turn reported.
    let streamed = live.text.lock().unwrap().clone();
    assert!(
        live.deltas.load(Ordering::SeqCst) > 1,
        "the provider sent one delta or none; nothing was streamed"
    );
    assert!(
        live.steps_streamed_ahead.load(Ordering::SeqCst) > 0,
        "no step's text reached the observer before that step committed"
    );
    assert!(
        first
            .reply
            .as_deref()
            .is_some_and(|reply| streamed.contains(reply.trim())),
        "the streamed text and the reported reply disagree"
    );
    println!(
        "streamed {} deltas across {} step(s) whose text arrived before the step committed \
         (first delta at +{:?} after the turn began)",
        live.deltas.load(Ordering::SeqCst),
        live.steps_streamed_ahead.load(Ordering::SeqCst),
        live.first_at
            .lock()
            .unwrap()
            .map(|at| at.duration_since(started)),
    );

    // ---- turn two: continuity, and a steer mid-turn ------------------------
    println!("\n--- turn 2 (steered mid-turn) ------------------------------------");
    let (steer, inbox) = Steer::channel();
    let watcher = SteerOnFirstToolCall {
        steer,
        said: AtomicBool::new(false),
    };
    let second = session
        .turn_steered(
            // No file is named for the *answer*: the model has to use what turn one
            // established, which is what makes this a conversation rather than two
            // tasks. The re-read at the start is what gives the operator a step to
            // steer at — a steer lands at the NEXT step boundary, so a one-step task
            // cannot be redirected, and saying otherwise would be a demo that lied.
            "Read each file under src/ one more time, then write that function's name \
             and its value to NOTES.md, and stop.",
            &provider,
            &store,
            &policy,
            &ApproveAll,
            &watcher,
            &inbox,
        )
        .await?;
    println!("outcome: {:?}", second.outcome);

    // What the crate guarantees is that the operator's message reached the model's
    // next request and is in the trace — not that the model then obeyed it. So that
    // is what this asserts: the delivery, from the stored prompt of the step after
    // the steer, and the `steered` row beside it. What the model did with it is
    // printed and not asserted, because asserting it would be asserting the model.
    let delivered = store
        .steps(second.run_id)?
        .into_iter()
        .find(|step| step.prompt.contains("[operator, mid-turn]"));
    let delivered = delivered.expect("the operator's message reached a request");
    assert!(
        delivered.prompt.contains("Write SECOND.md instead"),
        "the request carried the marker but not the message"
    );
    assert!(
        store
            .context_events(second.run_id)?
            .iter()
            .any(|e| e.kind == "steered"),
        "a steered turn left no `steered` row in the trace"
    );
    println!(
        "the steer reached the model at step {} of {} — and the trace says so",
        delivered.step,
        store.last_step(second.run_id)?,
    );
    println!(
        "what the model did with it: SECOND.md exists: {}, NOTES.md exists: {}",
        root.join("SECOND.md").exists(),
        root.join("NOTES.md").exists(),
    );

    // ---- a branch: back to turn one, a different second turn ---------------
    println!("\n--- branch from turn 1 -------------------------------------------");
    session.branch_from(&store, first.turn_id)?;
    let branched = session
        .turn(
            "Instead, tell me which function returns the SMALLEST number. One sentence.",
            &provider,
            &store,
            &policy,
            &ApproveAll,
        )
        .await?;
    println!("{}", branched.reply.clone().unwrap_or_default());

    // The abandoned turn is untouched, and the branch's own path does not contain it.
    let path: Vec<i64> = session.history(&store)?.iter().map(|t| t.id).collect();
    assert_eq!(
        path,
        vec![first.turn_id, branched.turn_id],
        "the branch's path is not root -> branch"
    );
    assert!(
        store
            .session_turn(second.turn_id)?
            .is_some_and(|t| t.outcome.is_some()),
        "the abandoned turn lost its record"
    );
    println!(
        "tree: {} turns, path after the branch: {:?}",
        store.session_turns(session.id())?.len(),
        path
    );

    // ---- reopened from the id alone, in a second connection ---------------
    println!("\n--- reopened -----------------------------------------------------");
    let id = session.id();
    drop(session);
    drop(store);

    let store = Store::open(&db)?;
    let mut later = Session::reopen(&store, id)?;
    assert_eq!(later.root(), root.as_path());
    let reopened = later
        .turn(
            "What was the first thing I asked you in this conversation?",
            &provider,
            &store,
            &policy,
            &ApproveAll,
        )
        .await?;
    let recalled = reopened.reply.clone().unwrap_or_default();
    println!("{recalled}");
    assert!(
        recalled.to_lowercase().contains("largest")
            || recalled.to_lowercase().contains("src")
            || recalled.to_lowercase().contains("function"),
        "a reopened session did not recall its own first turn: {recalled}"
    );

    // ---- what the whole conversation cost, read from the runs -------------
    println!("\n--- the conversation, from the store ----------------------------");
    let mut total = 0u64;
    for turn in store.session_turns(later.id())? {
        let summary = store.run_summary(turn.run_id)?;
        let tokens = summary.as_ref().map(|s| s.tokens).unwrap_or(0);
        total += tokens;
        println!(
            "  turn {} (parent {:?}, run {}): {:?}, {tokens} tokens — {}",
            turn.id,
            turn.parent_turn_id,
            turn.run_id,
            turn.outcome.as_deref().unwrap_or("running"),
            turn.prompt.lines().next().unwrap_or_default(),
        );
    }
    println!("\n{total} tokens across the conversation");
    println!("trace: {}", db.display());
    Ok(())
}
