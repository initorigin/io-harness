# Sessions: a durable conversation

A one-shot run answers one question and returns. A **session** holds a
conversation: an operator sends a turn, watches the answer arrive token by token,
says something else mid-turn or interrupts, comes back tomorrow, and branches from
any earlier turn instead of starting over.

The whole of it rests on one sentence: **a turn is a run.** Each turn gets its own
`runs` row, its own steps, its own budgets, its own policy boundary, its own
sandbox and its own checkpoint. Nothing about durability, auditing, resuming or
refusing is rebuilt for conversations — a session adds a tree over the runs, and
that is all it adds.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

# async fn demo(policy: &Policy) -> io_harness::Result<()> {
let store = Store::open("runs.db")?;
let provider = OpenRouter::from_env()?;
let mut session = Session::open(&store, "/path/to/repo")?;

let first = session
    .turn("what does the retry policy actually retry?", &provider, &store, policy, &ApproveAll)
    .await?;
println!("{}", first.reply.unwrap_or_default());

// The next turn reads the previous ones: the conversation *is* the context.
session
    .turn("now make it retry a 503 as well", &provider, &store, policy, &ApproveAll)
    .await?;

// Keep the id. It is all a later process needs.
let id = session.id();
# Ok(()) }
```

## The five entry points

| Method | Bound | Observer | Streams | Steerable |
| --- | --- | --- | --- | --- |
| `turn` | no criterion | — | no | no |
| `turn_observed` | no criterion | yes | yes | no |
| `turn_steered` | no criterion | yes | yes | yes |
| `turn_bounded` | your `TaskContract` | — | no | no |
| `turn_bounded_observed` | your `TaskContract` | yes | yes | no |

An unbounded turn runs with `Verification::None`: it ends when the agent stops
calling tools, reported as `RunOutcome::Finished`. That is the conversational
shape — there is no criterion to pass, so the model saying its last word *is* the
ending.

A bounded turn takes a `TaskContract`, which is where a verification gate, a step
or token budget, a `Toolbox`, MCP servers or a skills directory go. The bound
applies to **that turn only**; the next turn is unbounded again unless it carries
its own contract. The contract's `root` is replaced by the session's — a turn is
about the conversation's workspace, and a contract naming a different directory
would be answering about a different project.

`TaskContract` is therefore what the roadmap always said it was: an optional bound
on a turn, or a headless one-shot for unattended work. It stopped being the only
way in.

## Durability, and what "a later process" means

`Session::open` returns a session whose `id()` is durable. `Session::reopen` picks
it up from any process against the same database, and the workspace root comes
from the store rather than from the caller — a session whose root argument changed
between processes would otherwise carry a conversation about one directory into
another.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

# async fn demo(store: &Store, policy: &Policy, id: i64) -> io_harness::Result<()> {
// Tomorrow, in a different process, with nothing but the id.
let mut session = Session::reopen(store, id)?;
for turn in session.history(store)? {
    println!("> {}\n{}", turn.prompt, turn.reply.unwrap_or_default());
}
session.turn("where were we?", &OpenRouter::from_env()?, store, policy, &ApproveAll).await?;
# Ok(()) }
```

A turn whose process died mid-answer is a run that died mid-step, and every
`resume*` entry point already knows what to do with one: the turn's row carries
its `run_id`, and `resume_with` continues it from its last committed step.

## The tree

`branch_from` makes any earlier turn the parent of the next one. It is one write:
nothing is edited, nothing is deleted, and the branch you left is still readable
afterwards.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let provider = OpenRouter::from_env()?;
let mut session = Session::open(store, "/repo")?;

let plan = session.turn("draft a migration plan", &provider, store, policy, &ApproveAll).await?;
session.turn("do it with a blue-green cutover", &provider, store, policy, &ApproveAll).await?;

// Wrong road. Go back to the plan and take the other one — the blue-green turn
// stays in the tree, readable, with its own run and its own trace.
session.branch_from(store, plan.turn_id)?;
session.turn("do it with a read-only window instead", &provider, store, policy, &ApproveAll).await?;
# Ok(()) }
```

`history` is the path from the root to the current head — what the model sees.
`Store::session_turns` is the *whole* tree, every branch, which is what a UI
drawing the conversation wants.

## How the conversation reaches the model

The turns on the path are handed to the next turn as observations, and the
existing context assembler decides what fits under the contract's
`ContextBudget`. There is no second compaction rule for conversations: a long
conversation is elided by the machinery that already elides a long run, and a
`CompletionRequest` still carries one `system` and one `user` string.

That is a deliberate exclusion. A role-tagged message array would be a second
context path over the same job, and everything the assembler does — bounding,
compacting, invalidating an observation a later write superseded — would have to
exist twice.

## Streaming

An observed turn asks the provider for deltas and emits them as
`EventKind::Token { text }` while the request is still open.

```rust,no_run
use io_harness::{ApproveAll, EventKind, Flow, Observer, OpenRouter, Policy, RunEvent, Session,
                 Store};
use std::io::Write;

struct Live;

impl Observer for Live {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Token { text } = &event.kind {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        Flow::Continue
    }
}

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let mut session = Session::open(store, "/repo")?;
session.turn_observed("summarise this crate", &OpenRouter::from_env()?, store, policy,
                      &ApproveAll, &Live).await?;
# Ok(()) }
```

The deltas of one step concatenate to exactly that step's final assistant text, in
order. That is the property to rely on and the property the suite asserts — a
stream that drops or reorders a chunk reads like a complete answer and is not.

Streaming is **opt-in**: `run_with_observed` and every other 0.19.0 entry point
still calls `Provider::complete` and emits no `Token` events at all. A turn with
no observer does not enter the streaming path either.

For an out-of-tree `Provider`, `complete_streaming` has a default that delegates
to `complete` and emits the finished text as a single delta — so a UI written
against the event renders something rather than nothing, while being honest that
nothing was incremental. The three built-in providers override it and emit each
delta as its SSE event arrives.

## Steering and interruption

```rust,no_run
use io_harness::{ApproveAll, Ignore, OpenRouter, Policy, Session, Steer, Store};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let (steer, inbox) = Steer::channel();
let mut session = Session::open(store, "/repo")?;

// `Steer` is `Send + Sync` and cheap to clone: a UI thread, a key handler or
// another task can hold one while the turn runs.
let handle = steer.clone();
tokio::spawn(async move {
    let _ = handle.say("actually, only touch the docs");
});

let result = session
    .turn_steered("bring the docs up to date", &OpenRouter::from_env()?, store, policy,
                  &ApproveAll, &Ignore, &inbox)
    .await?;
# Ok(()) }
```

`say` queues a message that reaches the model at the next step boundary as an
observation the next request carries. `interrupt` stops the turn at the next step
boundary through the same path `Flow::Cancel` uses: the step it is on commits
whole, the run records `cancelled`, the outcome is `RunOutcome::Cancelled`, and
the turn stays resumable. The session goes on — the interrupted turn is in the
tree with its outcome, and the next turn reads it like any other.

Both land at a step boundary and nowhere else, for the reason cancellation always
has: in between, a tool call is in flight and a file may be half-written.

A message sent after its turn has ended is an error rather than a shrug. An
operator whose correction went nowhere needs to know it went nowhere.

## The limits, stated plainly

* **Steering is text, not authorization.** An operator's mid-turn message reaches
  the model exactly as a `TaskContract` constraint does, and every tool call it
  leads to is checked against the same `Policy` by the same code. "Just do it" in
  a steer does not widen a boundary — the suite steers a turn to perform a denied
  write and asserts the refusal, with the deny lifted as the control.
* **A streamed delta is provisional.** It is what the model has said so far, not a
  decision it has made: the turn may still fall over to another provider, be
  retried, or be interrupted, and text already emitted is not withdrawn. Render
  it; do not act on it. The committed step is what is settled.
* **One session, one driver.** Two processes taking turns on the same session id
  at the same time is not supported and not defended against beyond SQLite's own
  busy timeout. The turns would interleave into one tree in an order nobody chose.
* **The tree is append-only.** There is no edit, no delete, and no "compact this
  conversation". A branch abandons turns; it does not remove them. What bounds a
  long conversation is the `ContextBudget`, which elides what the model sees and
  never what the store holds.
* **A turn is a run, including the cost.** Every turn is a fresh run with its own
  budgets: `max_steps` on one turn does not bound the next, and a session has no
  aggregate ceiling of its own. If a conversation needs one, read
  `Store::run_summary` per turn, or bound each turn with a contract.
* **The reply is the agent's last message, or nothing.** A turn that stopped on a
  ceiling, a refusal or an interrupt has no closing message, and `TurnResult::reply`
  is `None` rather than a guess assembled from the trace.
