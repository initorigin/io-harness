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

## The eleven entry points

| Method | Bound | Observer | Streams | Steerable | May spawn |
| --- | --- | --- | --- | --- | --- |
| `turn` | no criterion | — | no | no | no |
| `turn_observed` | no criterion | yes | yes | no | no |
| `turn_steered` | no criterion | yes | yes | yes | no |
| `turn_bounded` | your `TaskContract` | — | no | no | no |
| `turn_bounded_observed` | your `TaskContract` | yes | yes | no | no |
| `turn_bounded_steered` | your `TaskContract` | yes | yes | **yes** | no |
| `turn_contained` | no criterion | — | no | no | **yes** |
| `turn_contained_observed` | no criterion | yes | yes | no | **yes** |
| `turn_contained_bounded` | your `TaskContract` | — | no | no | **yes** |
| `turn_contained_bounded_observed` | your `TaskContract` | yes | yes | no | **yes** |
| `turn_contained_bounded_steered` | your `TaskContract` | yes | yes | **yes** | **yes** |

The two steered rows that take a contract are 0.67.0. Before them, an operator who
wanted to correct an agent mid-run and an operator who wanted that run to carry
skills, a step budget or a verification gate were told to pick one. On the
contained row the inbox reaches the root agent only — a spawned child is never
steerable by an operator it has not spoken to.

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

## Binding the host once (0.63.0)

Every signature above takes the provider, the store, the policy and the approver,
and most programs pass the same four every time. `Harness` binds them, along with
the settings on `TaskContract` that describe the *host* rather than a task — the
toolbox, MCP and LSP servers, the browser, the skills directory, the plugin
bundles, the agent roster, the responder and web access:

```rust,no_run
use io_harness::{ApproveAll, Harness, OpenRouter, Policy, Store, TaskContract};

# async fn demo(policy: Policy) -> io_harness::Result<()> {
let store = Store::open("runs.db")?;
let provider = OpenRouter::from_env()?;

let harness = Harness::new(&provider, &store)
    .with_policy(policy)
    .with_approver(&ApproveAll)
    .with_defaults(TaskContract::workspace("", "/repo").with_skills("/repo/.io/skills"));

let mut session = harness.session("/repo")?;
harness.turn(&mut session, "what does the retry policy actually retry?").await?;
harness.turn(&mut session, "now make it retry a 503 as well").await?;
# Ok(()) }
```

Three things worth knowing about it:

- **It borrows.** `rusqlite::Connection` is `Send` and not `Sync`, so a `Harness`
  that owned the store would push that constraint onto anything holding one. Every
  entry point above already takes these by reference, so borrowing composes with
  what you already have.
- **The template is not merged.** `harness.workspace(...)` and `harness.task(...)`
  start from what `with_defaults` bound; a contract *you* built and handed to
  `harness.run(...)` is used exactly as you built it. Nothing is filled in behind
  your back, because a rule you cannot evaluate at the call site is worse than
  typing a setting twice.
- **It adds no path.** Every method calls the same free function or `Session` method
  you would have called yourself, which is asserted as trace equality rather than
  left as an intention. The functions above keep working exactly as they are; the
  `Harness` is a convenience over them and never a replacement.

## Not every turn is work (0.37.0)

Someone types `hi`. Through 0.36.1 that opened a run: a `runs` row, a plan gate
they might have had to answer, a checkpoint on disk, and a ledger entry saying
work happened. Nothing did.

Since 0.37.0 the turn's **own first completion** decides. It is made exactly as it
has always been made — the workspace tools offered, the conversation seeded, the
operator's text as the goal — and what comes back is read rather than assumed:

- **Stopped on text, no tool call** → the turn is an answer. `TurnResult::kind` is
  `TurnKind::Reply`, and the turn wrote no step, no gate attempt, no checkpoint, no
  snapshot, no plan gate and no call to your `Approver`.
- **Carrying a tool call** → the turn is work. `kind` is `TurnKind::Run` and the
  loop continues **from that same completion**, so the run's first step is the call
  that was already paid for.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store, TurnKind};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let provider = OpenRouter::from_env()?;
let mut session = Session::open(store, "/path/to/repo")?;
let turn = session.turn("hi", &provider, store, policy, &ApproveAll).await?;

match turn.kind {
    // Print it and wait for the next thing they say. Nothing was staged.
    TurnKind::Reply => println!("{}", turn.reply.unwrap_or_default()),
    // A run happened; everything the crate offers about a run applies.
    _ => println!("{:?}", turn.outcome),
}
# Ok(()) }
```

An `EventKind::Answered { turn_id }` reaches every `Observer` — and therefore
every attached process — when a turn closes as a reply, so a transcript can tell an
answer from a run without opening the store.

**It costs nothing extra.** A turn that answers makes one provider call. A turn
that promotes and takes two steps makes two, not three. The classification is not
a separate cheaper model, a pre-pass, or a second request: it is the reading of a
completion that had to happen anyway.

**There is no list of greetings**, here or in your program. That is the point. A
list is a list in one language, matches `hi` and not `namaste`, and answers
`hi, the login page is broken` correctly only by accident. A model reading the
sentence has strictly more to go on.

### The limits of it

- **Only the first completion of a turn can be a reply.** A run whose fifth step
  stops on text is a run that finished, as it always was.
- **Prose *and* a tool call is work.** The call decides.
- **A contract carrying a `Verification` is never a reply — unless you say
  otherwise (0.63.0).** By default, you said how the turn is judged, so you said
  it is work; a bounded contract with no verification classifies like an unbounded
  turn. That inference is right for most callers and wrong for one real shape: a
  chat surface that attaches a criterion to *every* turn loses greeting handling
  entirely, and before 0.63.0 there was no way to ask for it back.

  `TaskContract::with_conversational_turns` is that way back, and its opposite:

  ```rust,no_run
  use io_harness::{TaskContract, Verification};

  # fn demo(repo: &str) {
  // Judged, and still allowed to answer "hello".
  let chat = TaskContract::workspace("hello", repo)
      .with_verification(Verification::Command {
          argv: vec!["cargo".into(), "test".into()],
          expect_exit: 0,
      })
      .with_conversational_turns(true);

  // Unjudged, and required to do the work anyway.
  let strict = TaskContract::workspace("summarise the README", repo)
      .with_conversational_turns(false);
  # let _ = (chat, strict); }
  ```

  Leaving it unset is a third state and not a spelling of `false`: it means
  "infer", which is what every contract written before 0.63.0 does and what any
  contract that never calls the builder keeps doing. It governs session turns
  only — a one-shot `run_*` never classifies at all.
- **`run_with` never classifies.** A one-shot contract is work by declaration.
- **A reply is billed.** `Store::run_summary` reports its tokens and the per-call
  accounting row carries its model and latency. A turn that cannot afford its own
  reply under its token budget is refused rather than served free.
- **A reply is not resumable**, and a turn killed while it was still deciding is
  refused by `Store::check_resumable` rather than offered as work to continue.
  There is nothing to continue: one completion, which asking again replaces at the
  same price.
- **The prompt changed for every session turn**, not only for greetings. The first
  completion is told the message may not be work at all — and told to act where
  both readings are possible. A model that answers in prose when it should have
  acted costs you one retype; that asymmetry is chosen, and the alternative is
  worse.

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

## A turn can fan out (0.39.0)

An operator asks for something wide inside the conversation — *migrate these forty
handlers*, *review these twelve files*. Through 0.38.0 that was one agent working
through forty items in one context window, because the sub-agent tool was
registered only inside the tree loop and no session turn reached it: a run was a
tree or a turn and not both.

`turn_contained` takes a `Containment` and drives the turn through that loop, so
the agent answering it may decompose the work:

```no_run
use io_harness::{ApproveAll, Containment, OpenRouter, Policy, Session, Store};

# async fn demo(store: &Store) -> io_harness::Result<()> {
let mut session = Session::open(store, "/repo")?;

// The boundary for the whole fan-out. A child inherits it through
// `Policy::contain` and may only narrow it, at any depth.
let policy = Policy::default().layer("app").allow_read("*").allow_write("docs/*");

let turn = session
    .turn_contained(
        "document every public module under docs/, one file per module",
        &OpenRouter::from_env()?, store, &policy, &ApproveAll,
        // Twelve agents in all, four at once per tier, two deep, one token
        // ceiling for this turn. A spawn past the concurrency cap queues; one
        // past the total cap is refused and the parent adapts.
        &Containment::new(12, 4, 2, 500_000),
    )
    .await?;

// One turn, whatever it spawned. The children are runs under this turn's run.
println!("{} children", store.children(turn.run_id)?.len());
# Ok(()) }
```

What the fan-out inherits is what [composition.md](composition.md) already
describes — inherit-and-narrow policy, one shared ledger, per-tier concurrency
slots with a durable queue, the whole tree reconstructable from
`Store::agent_events`. What is new is only the caller.

**A contained turn can carry a contract too (0.66.0).** `turn_contained` builds
the session's default contract from your text, which is the right shape for
*decompose this* and the wrong one the moment the turn needs a plan gate, a
preset, registered tools, a budget or a verification gate. `turn_contained_bounded`
and `turn_contained_bounded_observed` take a `TaskContract` beside the
`Containment` — the contract bounds the agent answering the turn, the containment
bounds the tree it may grow:

```no_run
use io_harness::{ApproveAll, Containment, OpenRouter, Policy, Session, Store,
                 TaskContract, Verification};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let mut session = Session::open(store, "/repo")?;
let contract = TaskContract::workspace("document every public module", "/repo")
    .with_verification(Verification::Command {
        argv: vec!["cargo".into(), "doc".into()],
        expect_exit: 0,
    })
    .with_max_steps(30);

let turn = session
    .turn_contained_bounded(
        &contract, &OpenRouter::from_env()?, store, policy, &ApproveAll,
        &Containment::new(12, 4, 2, 500_000),
    )
    .await?;
println!("{:?}", turn.outcome);
# Ok(()) }
```

Four things the tree loop reads differently from the flat one, none of them new in
0.66.0 — all of them are equally true of `run_tree`, which has taken a contract
since 0.39.0. The tree's spend ceiling comes from the `Containment` and not from
the contract's `max_tokens`, which bounds one agent. `Routing`'s `escalate_after`
and `downshift_under` do not move the model per step. The preflight checks a flat
run makes before its first request — a `Verification::Review` with no reviewer, a
reviewer that is the model under review, `Routing::require_primary` — are not made,
though a model approving its own call is still refused at the root. And
`max_parallel_reads` bounds a batch only the flat loop builds.

Four things are worth knowing before you reach for it:

* **The ledger is per turn, not per session.** Each contained turn builds a fresh
  ledger from the `Containment` you pass, so turn five gets the ceiling turn one
  got. A conversation's total spend is the sum of its turns'; there is no single
  ceiling across them. This is the same rule the guide's "a turn is a run,
  including the cost" states below, applied to the tree.
* **A child is given its goal, not the conversation.** Forty children each
  carrying the transcript is the multiplied version of the cost `ContextBudget`
  exists to bound — and a child that has read the conversation is one that can act
  on an instruction three turns old that the operator has since withdrawn. The
  parent composes each child's result back into its own next step.
* **A child is a run, never a second turn.** `Session::history` renders one entry
  for a turn that spawned forty agents. Their traces are under the turn's run id.
* **A paused contained turn takes the tree resumes.** If the turn stops on an
  approval, a question or a plan, continue it with `resume_tree_with_decision`,
  `resume_tree_with_answer` or `resume_tree_with_plan_decision` on
  `TurnResult::run_id` — not the flat `resume_with_*` family. The turn row was
  closed when the turn returned, so it reports the state at the pause and is not
  rewritten by a resume the session did not drive.

The five entry points above are untouched by this and still never offer the spawn
tool: a session that does not ask for containment behaves exactly as it did.

## Reading a conversation back (0.43.0)

```rust,no_run
use io_harness::{Session, Store};

# fn demo(store: &Store, session: &Session) -> io_harness::Result<()> {
std::fs::write("session.md", session.transcript(store)?.to_markdown())?;
# Ok(()) }
```

`Session::transcript` is a read: no provider is called and no row is written. It
renders the **whole tree**, not the path — a `branch_from` leaves earlier turns off
what the model sees, and those are precisely the turns nothing else will show you.
Each one is marked `on_path` so a reader can tell which the model can still see.

It is also where a compacted run's folded history comes back out: each turn carries
the summaries its run wrote, rendered as a line saying what each paragraph stands
in for.

## A turn can carry an image (0.43.0)

```rust,no_run
use io_harness::provider::Media;
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let mut session = Session::open(store, "/repo")?;
let shot = Media::image("image/png", &std::fs::read("screenshot.png")?)?;

session.attach([shot]);
session.turn("why is this button misaligned?", &OpenRouter::from_env()?,
             store, policy, &ApproveAll).await?;
# Ok(()) }
```

The staging rides **the next turn only** and is cleared once that turn has been
driven, whatever its outcome — a screenshot is about the thing being said now.
One method covers all seven entry points, because staging is orthogonal to how a
turn is driven. `TaskContract::with_images` is the other half and still means what
it always did, for the whole run, so a `turn_bounded` carrying both sends both.

`media` feature only. A provider whose `accepts_images` is false refuses the turn
before anything is sent.

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
* **One session, one driver, and the loser is told (0.62.0).** Two processes
  taking turns on the same session id at the same time still do not both land on
  the head path. What no longer happens is one of them vanishing: the head moves
  by compare-and-swap, so the write that loses gets `Error::Conflict` and its turn
  row stays in `session_turns` exactly as it was — answered, billed, and readable,
  ready to be rebased onto the head that won. The run behind each turn is leased
  too, so a second process driving one run is refused before it takes a step.
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
