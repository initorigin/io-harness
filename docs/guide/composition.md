# Agent composition and containment

A parent agent can decompose its work and spawn sub-agents, and containment is
the boundary that caps what the whole resulting tree may do and spend.

A single loop does work one step wide. For large or parallelisable tasks, a
**parent agent decomposes the work and spawns sub-agents** — a tree of a hundred
or more — each running the same observe/reason/act/verify/stop loop over the
**same workspace and the same trace**. A child's result composes back so the
parent continues from what it produced, and children may nest.

Sub-agents are **opt-in**: `run_tree`, and — since 0.39.0 —
`Session::turn_contained`, are the entry points that offer the `spawn_agent` tool.
Since 0.66.0 a contained turn can also carry a contract, through
`Session::turn_contained_bounded`. Pass a `Containment` and the tree runs under it.

```rust
use io_harness::{run_tree, ApproveAll, Containment, Policy, Store, TaskContract, Verification};

let provider = io_harness::OpenRouter::from_env()?;
let store = Store::memory()?;

let contract = TaskContract::workspace(
    "Coordinate: delegate each file to a sub-agent, then combine.",
    "path/to/workspace",
)
   .with_verification(Verification::WorkspaceFileContains {
       file: "summary.txt".into(),
       needle: "DONE".into(),
   });

// Caps for the WHOLE tree — no spawned task can raise them.
let containment = Containment::new(
    /* max_total_agents      */ 100,
    /* max_concurrent_agents */ 16,
    /* max_depth             */ 3,
    /* max_total_tokens      */ 500_000,
);

let result = run_tree(&contract, &provider, &store, &Policy::permissive(), &ApproveAll, &containment).await?;
```

## The two agent caps are different in kind

`max_total_agents` bounds how many agents may ever *exist* in the tree, the root
included, and it **refuses**: the spawn that would cross it comes back to the
parent as `SpawnRefusal::AgentCap`, the same way the spend ceiling comes back as
`BudgetExhausted`. It is a limit meant to stop a run.

`max_concurrent_agents` bounds how many may be *working* at once, and it
**throttles**: a spawn past it is not refused, the child takes a place in a FIFO
queue and starts when a slot frees. It is a limit meant to shape a run. So a
hundred-agent task under a cap of sixteen runs a hundred agents sixteen at a time
until it is done, rather than failing at its seventeenth child.

Results are still collected in the order the model asked for them rather than the
order they happen to finish, because a tree whose composed observations arrive in
completion order is not reproducible and deterministic replay cannot be built on
it. `max_depth` counts from the root, which is depth 0.

**The concurrency cap is per tier.** Each nesting level has its own set of slots,
and that is the deadlock argument rather than a convenience: a parent holds a slot
at its own tier while it waits for children at the tier below, so the wait graph
runs strictly downward and cannot contain a cycle. One tree-global pool would hang
the first time the agent holding the last slot spawned a child, because only that
child could free it. The consequence to hold in mind: a tree of depth *d* can have
up to `max_concurrent_agents * d` agents working at once. Bound the whole thing
with `max_total_agents` and `max_total_tokens`, which is what they are for.

## Watching a fleet drain

`EventKind::Fleet { tier, working, queued, done }` reaches an `Observer` every
time a child queues, is admitted, or finishes. Per tier, because one tree-wide
number cannot tell an operator whether the fan-out at depth two is stuck behind
the one at depth one. An application holding the `Ledger` can also read the same
figures synchronously with `Ledger::tally(tier)`, which returns a `FleetTally`.

The queue is durable. Waiting is a row in the store, written when a child starts
waiting and deleted when it is admitted, so a tree that drains leaves none and a
process that is killed mid-fleet leaves exactly the backlog it had. The next
process reports that backlog — at the tier it had — before it calls a provider,
and `Store::queued_agents` answers "what was still waiting when this died" long
after the process is gone.

A child that only ever waited is not charged. It has no run row, no steps and no
tokens against the tree's ceiling, because nothing about it was started. The other
side of that bargain: it is invisible to `Store::children` and to
`Store::agent_events`, and `Store::queued_agents` is the only place it appears.

## Containment is inherit-and-narrow

The permission policy becomes the boundary for spawned agents. Where
`Policy::merge` lets an overlay *widen* a base (allows union), `Policy::contain`
derives a **child** policy that can only *narrow*:

- **denies union** downward — a child adds restrictions;
- **allows intersect** downward — a child can never read, write, or execute
  anything its parent could not;
- the rule holds at **any depth** — no descendant can hold an effective allow the
  root did not grant.

```rust
let child_effective = parent_policy.contain(&child_overlay); // child cannot widen
```

Mechanically, `contain` clones the parent's layers, appends only the child's
non-allow rules — the child's allows grant nothing and are dropped — and
tightens each default to the stricter of the two. Because it only ever appends
denies and tightens defaults, applying it again for a grandchild preserves the
invariant.

A parent can tighten a specific child at the call site: the `spawn_agent` tool
accepts optional `deny_write` and `deny_net` glob arrays, which become the
child's overlay. It can also pass `max_steps`. It cannot pass anything that
widens.

## One spend ceiling above the task contract

The whole tree draws its token spend from **one shared ledger**. A spawned
`TaskContract` can set a *tighter* budget but never a looser one than the tree has
left; when the aggregate `max_total_tokens` is reached the tree halts as a whole.
A spawn that would breach a cap meant to *stop* the run — agents, depth, or budget
— is **refused** as a tool result the parent can adapt to, and every spawn,
refusal, and budget draw is in the rusqlite trace as one reconstructable graph.
The concurrency cap is not among them: it queues.

The ledger is one lock around the check-and-add, so a hundred concurrent agents
cannot race spend past the ceiling: a draw that would cross it is rejected
outright rather than recorded, and returns `Draw::Halted`. The provider was
still paid for the halting step's tokens; the ledger declines to count them and
stops the tree rather than letting the recorded total drift over the ceiling.

A refusal is typed — `SpawnRefusal::AgentCap`, `DepthCap`, or `BudgetExhausted`
— never a panic, and it is recorded against the *parent's* run and depth,
because no child exists to attribute it to. That is the point of a refusal.

`Containment` also carries two optional aggregate ceilings:

- `max_total_duration` — a wall-clock ceiling for the whole tree, measured from
  when the **root** run started, so it counts a 24-hour tree's whole life
  including time the process was down, not the age of whichever agent notices.
  Crossing it halts the tree with `RunOutcome::BudgetCeilingReached`.
- `max_total_cost` — **reserved, and not enforced. Setting it has no effect.**
  Enforcing a cost ceiling needs a price per token, and the crate has no price
  telemetry: a provider reports tokens, never money, so any figure the harness
  compared against would be one it invented. The field is kept rather than
  removed because it serialises in callers' stored configuration and deleting it
  would break their deserialisation for no gain. To bound money, convert your
  budget to tokens at your provider's rate and set `max_total_tokens`.

Run it live: `cargo run --example subagents`.

## A child can have its own checkout (0.36.0)

Everything above is about how many agents run at once. This is about what they
run *on*, and until 0.36.0 the answer was: one working directory, shared by the
whole tree. Two children editing the same file are one overwriting the other, so
the concurrency the caps allow was usable only for work that does not overlap —
a real bound on a feature whose whole point is overlap.

`AgentDef::with_worktree()` gives a child its own git worktree and its own
branch, created before its first step:

```rust,no_run
use io_harness::AgentDef;

let shared = AgentDef::new("searcher");
assert!(!shared.worktree, "one checkout, as every release before 0.36.0");

let own = AgentDef::new("reviewer").with_worktree();
assert!(own.worktree);
```

The worktree is placed at `<root>/.worktrees/<agent>-<parent run>-<step>-<goal
digest>`. Every component of that path is load-bearing, and the last one is the
least obvious: two children of the *same* definition spawned in the *same* step
— the ordinary shape of a fan-out — differ in nothing but their goal, so without
the digest they would be handed one worktree between them, which is the
collision the field exists to remove reappearing one level down.

It is derived from the key a spawn is *adopted* by, which is what makes it
survive a crash: a resumed tree finds the worktree it already made and continues
in it, rather than re-creating it and discarding what the child had written.

Four bounds, stated rather than implied:

- **A failure to create one fails the spawn.** No `git`, not a repository, or a
  policy that refuses the path — the child does not start, and the reason is the
  spawn's. It does **not** fall back to the parent's tree, because that fallback
  is precisely the collision the field removes.
- **The path is checked against the parent's policy before `git` is asked.** The
  crate is writing somewhere the model did not name, and an unchecked write is a
  claim it does not get to make. A policy denying `.worktrees/**` turns the
  feature off loudly rather than quietly.
- **Nothing removes a worktree or deletes a branch, ever.** Removing one deletes
  the work a child was spawned to produce, so it stays the operator's call, and
  worktrees a run created accumulate until someone removes them.
- **A worktree is visible to the parent's own `git_status`**, as one untracked
  `?? .worktrees/` entry, because git summarises an untracked directory rather
  than descending into it — so a `git_add` naming `.` in the parent stages the
  children's trees. This crate writes to no repository metadata on your behalf;
  add `/.worktrees/` to `.git/info/exclude` if you want it hidden.

## A conversation can fan out too (0.39.0)

Everything on this page is reachable from a session turn:
`Session::turn_contained` takes the same `Containment` and drives the same loop,
so an operator who asks for something wide *inside a conversation* gets a tree
under the session's own policy, one shared ledger, and the same events — without
leaving the transcript for a one-shot run.

Two differences worth knowing, both about the turn rather than the tree: the
ledger is built per turn, so it is not a ceiling across the conversation; and a
child is given its goal rather than the conversation, so what the operator said
three turns ago reaches the root and stops there. [Sessions](sessions.md) has the
rest.

**And since 0.66.0 that turn takes your contract.**
`Session::turn_contained_bounded` takes a `TaskContract` beside the `Containment`,
so a fan-out inside a conversation can carry a plan gate, registered tools, a
budget or a verification gate — the things a `turn_bounded` has always accepted and
a contained turn could not be given. The contract bounds the agent answering the
turn; the containment still bounds the tree, including its one shared spend
ceiling.

## See also

- [Sessions](sessions.md) — the same tree, driven from a conversation
- [Permissions and approval](permissions.md) — the policy `contain` narrows
- [Durable runs](durable-runs.md) — how a crashed tree resumes agent by agent
- [Execution sandbox](sandbox.md) — what confines many concurrent agents' code
- [Observability and replay](observability.md) — reading the tree back as a graph
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
