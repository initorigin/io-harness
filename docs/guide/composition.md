# Agent composition and containment

A parent agent can decompose its work and spawn sub-agents, and containment is
the boundary that caps what the whole resulting tree may do and spend.

A single loop does work one step wide. For large or parallelisable tasks, a
**parent agent decomposes the work and spawns sub-agents** — a tree of a hundred
or more — each running the same observe/reason/act/verify/stop loop over the
**same workspace and the same trace**. A child's result composes back so the
parent continues from what it produced, and children may nest.

Sub-agents are **opt-in**: only `run_tree` offers the `spawn_agent` tool. Pass a
`Containment` and the tree runs under it.

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

## See also

- [Permissions and approval](permissions.md) — the policy `contain` narrows
- [Durable runs](durable-runs.md) — how a crashed tree resumes agent by agent
- [Execution sandbox](sandbox.md) — what confines many concurrent agents' code
- [Observability and replay](observability.md) — reading the tree back as a graph
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
