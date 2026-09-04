# Observability and replay

Observability is the live and after-the-fact view of a run: an observer that is
called as things happen, a one-row summary of what a finished run cost, and a
recorded provider that lets the same case run again and get the same answers.

Three surfaces, in the order you reach for them:

| Surface | When | What it answers |
| --- | --- | --- |
| [`Observer`](#watching-a-run-while-it-happens) | during the run | What is it doing *right now* |
| [`RunSummary`](#what-a-finished-run-cost) | after the run | Did it work, how many steps, what did it spend, how long |
| [`Record`/`Replay`](#recording-and-replaying-a-run) | across runs | Run this exact case again and get the same answers |

## Watching a run while it happens

Everything a run does is durable in the trace and readable *afterwards*. That is
not enough for a run designed to last hours: an application that wanted to show
progress had to open the SQLite file with a second connection and poll it,
against a schema the crate never promised. Register an `Observer` instead and the
run calls it as things happen.

```rust
use io_harness::observe::{EventKind, Flow, Observer, RunEvent};

struct Printer;

impl Observer for Printer {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Step { decision, tokens, .. } = &event.kind {
            println!("step {} — {decision} ({tokens} tokens)", event.step);
        }
        Flow::Continue
    }
}
```

Every entry point has an **observed twin** that takes the observer last —
`run_observed`, `run_with_observed`, `resume_observed`, `resume_with_observed`,
`resume_from_stored_policy_observed`, `resume_with_decision_observed`,
`run_tree_observed`, `resume_tree_observed`,
`resume_tree_from_stored_policy_observed`, `resume_tree_with_decision_observed`.
They are separate functions rather than an extra parameter, so nothing a caller
already wrote had to change: the unobserved originals *are* the observed twins,
called with an observer that watches nothing.

```rust
use io_harness::{run_with_observed, ApproveAll, Policy, Store};

let result = run_with_observed(
    &contract, &provider, &store, &policy, &ApproveAll, &Printer,
).await?;
```

An observer does not change the run. A watched run and an unwatched one make the
same number of provider calls, commit the same steps with the same decisions and
token counts, and reach the same outcome — that is asserted in `tests/observe.rs`
by running the same case twice and comparing the two traces, not assumed.

### The event shape

```rust
pub struct RunEvent {
    pub run_id: i64,   // the *agent's* own run id; a child has its own
    pub step: u32,     // 0 for anything before the first step
    pub depth: u32,    // 0 for a single run or a tree's root
    pub kind: EventKind,
}
```

The common fields sit on `RunEvent` rather than on every variant, so a consumer
can route on `run_id`/`depth` without matching the payload first. In a
[composed tree](composition.md) that is what lets one observer over the whole
tree tell who is doing what: a child's events carry the child's own run id and a
non-zero depth.

### The event kinds

`EventKind` is one enum with one `Observer` method rather than a method per kind,
so adding a kind is a new variant a consumer ignores with a `_` arm instead of a
new trait method every implementer inherits.

| Variant | Emitted when |
| --- | --- |
| `Started { goal, provider }` | Once, before the first step |
| `Step { decision, tool_call, tokens, changed }` | A step completed and was committed |
| `ToolCall { name, target }` | A tool was invoked, before its result is known |
| `Refused { act, target, rule, layer }` | The policy refused an action — it did not happen |
| `ApprovalRequested { act, target }` | A sensitive action stopped to ask a human; the run is waiting |
| `ApprovalDecided { act, target, decision }` | A human answered: `approve`, `deny` or `defer` |
| `SpendDraw { tokens, remaining }` | A step's tokens were drawn against a tree's shared ceiling |
| `Retry { kind, attempt, delay_ms }` | A provider call failed retryably and will be retried |
| `FellBackTo { provider }` | A `Fallback` provider fell over, and this is who answered |
| `Replan { window }` | The agent changed nothing for a while and was told once to try something else |
| `Stalled` | It had already been told and is still going in circles. Terminal |
| `Spawned { child_run_id, goal }` | A sub-agent was started |
| `SpawnRefused { cap }` | A spawn was refused by containment — `agents`, `depth` or `budget`. Never concurrency: that queues |
| `Fleet { tier, working, queued, done }` | One tier of the tree changed shape: a child queued, was admitted, or finished |
| `MemoryWrote { key }` | The agent wrote to durable cross-run memory |
| `Sandbox { kind, backend }` | `create`, `exec`, `cap_hit`, `destroy` or `gate_phase_failed` |
| `Mcp { server, tool, ok, millis, tools }` | An MCP server was reached, or one of its tools called. `tools` is how many it offered, on the reaching event and `None` on the rest — `Some(0)` for a server that came up offering nothing |
| `Finished { outcome, steps, tokens }` | Once, last |

Every variant reports something the trace already records. The event stream added
no new facts about a run — it added a way to see them while the run is still
going.

`Sandbox { kind: "gate_phase_failed", .. }` is how a verification gate's failing
phase reaches a watcher live; see [Verification](verification.md#if-a-run-stopped-passing-at-081).

### Which surface is authoritative

**The trace is.** An event is a notification that something happened, not the
record of it: the durable row is what a resume, an audit and an evaluation all
read. If an event and the trace ever disagree, the trace is right and the event
is a bug.

One event does not mean one committed row. A step is committed inside a
transaction and its `Step` event is emitted after that transaction succeeds — but
a retry emits a `Retry` having written a row of its own under the *same* step
number, and a sub-agent step that pauses because one of its children deferred is
deliberately left uncommitted so a resume replays it. Count events if you want to
show activity; read the store if you need to know what is durable.

### Ordering, timing and failure

Events arrive in the order the run produces them, **synchronously, on the task
driving the run**. `Observer::event` is therefore on the run's critical path: a
slow observer slows the run down. Do the minimum — push to a queue, send on a
channel — and do the work elsewhere.

`Observer::event` returns no `Result` on purpose. An observer is a spectator, and
a run must not fail because something watching it did. If your observer can fail,
absorb the failure and report it out of band.

A *panic* is different. `event` is called on the run's own task, so a panicking
observer takes the run's future with it and leaves the run row `running`. Do not
panic in an observer.

The trait is `Send + Sync` with `&self` methods, held as `&dyn Observer` — shaped
after `Approver`, the crate's other inversion-of-control point. `&self` rather
than `&mut self` is not a style choice: a tree runs up to
`max_concurrent_agents` children per tier as concurrent futures on one task, and
a `&mut self` observer could not be shared between them. Keep whatever state you need behind a `Mutex`, an atomic,
or a channel.

### Forwarding events to another process

Every event serialises, because the process driving a run is often not the
process showing it to a person. A host can forward an event as JSON to a user
interface written in another language without hand-writing a mapping. The wire
shape is flat and tagged:

```json
{"run_id": 1, "step": 3, "depth": 0, "event": "step",
 "decision": "wrote src/a.rs", "tool_call": "write_file:{…}",
 "tokens": 412, "changed": true}
```

Flat, so a consumer reads one object rather than a nested one; tagged on `event`,
with a distinct tag per variant, so a consumer can dispatch without guessing.
Both properties are pinned by tests in `src/observe.rs` — including one that
fails to compile if a variant is added without being covered — because this is a
wire contract, not an internal detail.

### Stopping a run from outside it

`Observer::event` returns `Flow`. Returning `Flow::Cancel` is the only supported
way to stop a run from outside: before it existed, a caller's only option was to
drop the run's future, which abandoned it mid-step and left `runs.status` as
`running` forever, indistinguishable from a crashed process.

```rust
use io_harness::observe::{Flow, Observer, RunEvent};
use std::sync::atomic::{AtomicBool, Ordering};

struct StopWhenAsked(AtomicBool);

impl Observer for StopWhenAsked {
    fn event(&self, _event: &RunEvent) -> Flow {
        if self.0.load(Ordering::SeqCst) { Flow::Cancel } else { Flow::Continue }
    }
}
```

Cancellation is honoured **at the next step boundary**, not immediately: the
points in between are not safe to stop at — a tool call is mid-flight, a file may
be half-written, a child may be running. The run finishes the step it is on,
records `cancelled`, and returns `RunOutcome::Cancelled { steps }`. The step
after the cancellation is never started, so the provider is not called again.

A cancelled run is **finished, not abandoned**: `runs.status` is `completed`, the
ending is announced as a `Finished` event like any other, and the run stays
resumable. Resuming it reports `RunOutcome::Cancelled` again and drives nothing —
no provider call, and no events, because a resume that drives nothing has nothing
to report.

`Ignore` is the observer that watches nothing, and the default when a caller
registers none. It exists so the run has one code path rather than an
`Option<&dyn Observer>` threaded through every call site.

## Where a step spent its wall clock (0.75.0)

A summary says a run took four minutes. It does not say whether those minutes went
on the provider, in a tool, in the policy gate or in the store, and until 0.75.0
nothing in the trace did: the `steps` row carried no timing at all, and the only
fine-grained clock in the loop bracketed the provider call.

Each committed step now records its own span and how that span divides.
`Store::step_attributions` reads them back, with the step's time to first token
joined from `provider_calls` beside it, so one read answers "where did this step
go".

`EventKind::StepAttributed` carries the same numbers onto the event stream, beside
`EventKind::Step` rather than instead of it, so a slow run can be diagnosed while
it is still running rather than after it ends.

Four things about the numbers, each of which changes how they read:

- **A phase that is absent did not happen; it did not take zero.** A step that
  dispatched no tool has no tool phase. A step whose span was never closed — a
  tree paused for a child's approval — is not attributed at all, because a `0`
  there would be indistinguishable from a step that genuinely finished inside a
  millisecond.
- **The gate is part of the tool phase, not a sibling of it.** It is the policy
  resolution for the calls that step dispatched, and it includes waiting for a
  human when a call is one an approver must answer — which is why it can be most
  of a step and mean nothing is slow.
- **The store phase is the write that ended the *previous* step**, because that is
  the only step whose row can still be open to hold it. A run's first committed
  step therefore has none.
- **The parts need not sum to the span.** What is left over is real — prompt
  assembly, the fold, the loop's own bookkeeping — and it is reported by
  subtraction rather than folded into a neighbouring phase to make the arithmetic
  tidy.

The attribution is written inside the same transaction as the step it belongs to,
after the lease check, so a driver that lost its lease cannot write one. Nothing
here is a gate: no test asserts any of these durations, for the reason
`docs/MEASUREMENTS.md` gives at its top.

## What a finished run cost

`Store::run_summary` returns one row per finished run: did it work, how many
steps, what did it spend, how long did it take.

```rust
let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

if let Some(s) = result.summary(&store)? {
    println!(
        "{}: success={} steps={} tokens={} in {:?}ms",
        s.outcome, s.success, s.steps, s.tokens, s.duration_ms,
    );
}
```

`RunResult::summary` is a method that reads the store rather than a field filled
at each return site — so the caller and an auditor see the same row by
construction, and the endings that return `Err` and never build a `RunResult` at
all still get a summary.

| Field | What it is |
| --- | --- |
| `run_id` | The run this describes |
| `outcome` | The raw outcome string, as written to `runs.outcome` |
| `success` | True for exactly one outcome — `success` |
| `steps` | Steps completed, as `MAX(step)` |
| `tokens` | Tokens spent, summed from committed steps |
| `duration_ms` | Wall-clock milliseconds from start to end; `None` for a run started before 0.7.0 |
| `finished_at` | When the run ended, from the database clock |

Each field is there because assembling it by hand was either hard or impossible:

- **`success`** meant knowing which of eleven free-text outcome strings is the
  good one. Exactly one is. Every other ending — including the ones that are
  nobody's fault, like a rate-limited provider — is the task not being done.
  `outcome` is kept alongside `success` rather than replaced by it: the string
  says *which* ending, the flag says whether it was the good one, and collapsing
  them would throw away the difference between a step cap, a stall and a human's
  refusal.
- **`steps`** meant knowing that `MAX(step)` is the step count and `COUNT(*)` is
  not, because a retry writes a row under the same step number. For a
  [tree](composition.md) this is the *root* agent's step count; each agent has
  its own summary.
- **`tokens`** is tokens, not money. A provider reports usage and never a price,
  so the crate has nothing to convert with.
- **`duration_ms`** was simply unavailable: nothing recorded when a run ended,
  and `Store::elapsed_secs` measures against `now`, so it keeps growing after the
  run is over. It includes time the process was not running — a run that crashed
  at midnight and resumed at nine counts the nine hours, because that is how long
  the run took.

`RunSummary` serialises, so a scoring tool can store or ship it without restating
the shape.

**A missing summary is reported as absent, never as a row of zeroes**, which
would be indistinguishable from a run that did nothing. Three cases return
`None`: a run that has not finished; a run **paused awaiting a human**, which has
not ended and will be resumed; and a run finished by a pre-0.12.0 binary. When a
paused run resumes and really ends, it gets its summary then, describing the
whole run. `finish_run` is reachable more than once for one run, and the last
ending is the true one — the summary is replaced, not duplicated.

## Recording and replaying a run

`Record` wraps a real provider, forwards every call to it, and keeps the request
paired with the response it produced. `Replay` is a `Provider` with no provider
behind it: it loads a recording and answers from it.

```rust
use io_harness::provider::{Record, Replay};
use io_harness::{run, OpenRouter, Store};

// Record.
let provider = Record::new(OpenRouter::from_env()?);
let live = run(&contract, &provider, &Store::memory()?).await?;
provider.save("recording.json")?;

// Replay — no network, no key, no socket.
let replay = Replay::load("recording.json")?;
let again = run(&contract, &replay, &Store::memory()?).await?;
assert_eq!(again.outcome, live.outcome);
```

`Record::save` is callable mid-run and repeatedly: it snapshots rather than
drains, so a long run can checkpoint its recording and still record more. It
forwards `name`, `endpoint`, `endpoints`, `last_served` and `accepts_images` to
the provider it wraps — a recorder is not a provider anyone chose, and a wrapper
that reported fewer hosts than it can reach would be a way past an egress
[policy](mcp-and-network.md) that never saw them.

`Replay::endpoint` is `None`, so a run driven by a replay opens no socket and
needs no egress grant to do so.

### Why a file rather than the store

The step trace already records something per step, and it is not enough:

- the step row keeps `CompletionResponse::text` only when there were no tool
  calls, so the commentary a model emits alongside a call is dropped;
- it keeps `Usage::total_tokens` and discards the prompt/completion split;
- it flattens the calls into `"name:{json}"` joined with `" | "`, which any `|`
  inside an argument silently corrupts.

Nor could it be fixed by writing more columns. `Provider::complete` is RPITIT and
its future must be `Send`; `rusqlite::Connection` is `Send + !Sync`, so a
`&Store` captured across the inner provider's `.await` makes the future
non-`Send` and the trait bound fails. A recorder therefore cannot hold a store,
and the recording goes to a plain file — pretty-printed JSON, because a recording
is a fixture a human reads and diffs.

### How a request finds its answer

By its **content** — `system`, `user` and `tools`, which is the whole of a
`CompletionRequest` — and never by a call counter.

Every scripted mock keyed on an `AtomicUsize` is resume-unsafe for the same
reason: resume re-runs the step that was in flight when the process died, that
step calls `complete` again, and a counter-keyed script hands it the response
meant for the *next* step. The run then continues from a script one place ahead
of itself, silently. Content-keying makes the re-ask return what the first ask
returned, because it is the same question.

The lookup key is the JSON of the whole request rather than a hash, so a mismatch
can be read by a human debugging a divergence, and rather than a hand-rolled
concatenation, so no separator can be forged by a prompt containing it.

### What a replay guarantees

- **Same request, same answer.** The same request asked any number of times gets
  the same response, unless a *different* request is asked in between and the
  recording holds more than one answer for it. A re-run step is therefore
  reproducible.
- **A missing recording is loud.** A request the recording never saw is
  `Error::Provider` of kind `ProviderErrorKind::Request` — non-retryable — rather
  than a default response. Non-retryable on purpose: the same request will be
  missing next time too, so a retry burns the budget to reach the same place.
  `CompletionResponse::default()` reads exactly like "the model chose not to call
  a tool", so a silent default would make a diverged replay look like a
  successful one.
- **A recording is refused across a release series.** The recording carries the
  io-harness version that wrote it. A recording from another `major.minor` is a
  typed `Error::Config`, because a build whose request or response shape changed
  would replay something other than what was recorded. Patch releases are
  accepted.

### What a replay does not guarantee

- **Anything about a diverged prompt.** The key is the exact request text, so a
  replay must be driven against the same contract and the same workspace state as
  the recording. A goal reworded, a fixture file edited, or a step that reads a
  file the previous replay left different produces a request that was never
  recorded — reported as a missing recording, not silently absorbed.
- **Duplicate requests across a process restart.** A recording that answered one
  identical request differently twice is served in recorded order, tracked in
  memory. A resume in a *new* process starts that tracking over, so a re-run step
  whose request duplicates an earlier step's gets the earlier answer. Only
  identical requests are affected: a run's prompt carries its observations, so
  consecutive steps normally differ.
- **Ordering beyond what was recorded.** Once a key's recorded answers are
  exhausted, the last one is served again for every further ask. The stated
  guarantee is same-request-same-answer, and a request recorded once must still
  be answerable twice.
- **Failures.** `Error` is not serialisable, and a recorded failure would be a
  recorded *decision* about retry and fall-over rather than an answer. Only
  successful completions are recorded, so replaying a run whose provider failed
  reports a missing recording for that request rather than reproducing the
  failure. See [Resilience](resilience.md) for how failures are classified live.

## Two runs of one case, compared

`Store::canonical_trace` reduces a run's trace to the part that two identical runs
must match, as diffable text. It is the crate's definition of "the same run
twice", and it exists because equality could not be row identity: `steps` has no
`UNIQUE(run_id, step)` and a retry inserts its own row under the step number the
eventual commit will reuse, so comparing rows compares trace entries rather than
agent behaviour.

```rust
use io_harness::provider::Replay;

let mut traces = Vec::new();
for _ in 0..2 {
    let store = Store::memory()?;              // a FRESH store each time
    let provider = Replay::load("recording.json")?;
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    traces.push(store.canonical_trace(result.run_id)?);
}
assert_eq!(traces[0], traces[1]);
```

**What is compared:** every `steps` row — step number, decision, result, prompt,
tool call and tokens — and every `context_events` row's step, kind and detail.
Between them these are what the agent was shown, what it decided, what it did,
and what that cost.

**What is excluded, and why:** everything whose value is a fact about *this*
execution rather than about the run — wall-clock stamps (`runs.started_at`,
`memory.created_at`, `run_outcomes.finished_at` and `duration_ms`),
`mcp_events.millis`, `sandbox_events.detail` (it carries an argv containing an
ephemeral tempdir path), and run and child ids (`AUTOINCREMENT` values, meaningful
only within one store). Excluding a field is a decision that the crate cannot
promise it, not a convenience; a comparison that quietly excludes what it cannot
match is a comparison that asserts nothing.

**What it assumes:** that each run being compared has its **own fresh store**.
Run ids are excluded from the text, but a child agent's run id is embedded in the
parent's composed observation (`[child 5 "goal" -> …]`), which is real content the
model was shown. In a fresh store those ids start at 1 and are allocated in spawn
order, so they match; in a shared store the second run's ids are higher and the
traces differ for a reason that has nothing to do with the agent.

Deterministic replay also requires the provider to answer identically — that is
what `Replay` is for — and the same workspace state to start from.

## Exporting a run to OpenTelemetry (0.78.0)

Everything above is readable by this crate and by nothing else. Behind the `otel`
feature, a run can also be exported as OpenTelemetry spans, over OTLP/HTTP with a
JSON body, to any collector — which is how a run appears in the dashboard an
operator already runs, beside the services it called.

The exporter is an `Observer`. There is no new attachment mechanism and no change
to the loop:

```rust
use io_harness::{run_with_observed, ApproveAll, OtelConfig, OtelExporter};

let exporter = OtelExporter::open(
    OtelConfig::new("http://otel-collector.internal:4318").with_service_name("billing-agent"),
    "runs.db",
)?;

let result = run_with_observed(
    &contract, &provider, &store, &policy, &ApproveAll, &exporter,
).await?;
```

### The span tree

One `invoke_agent` span per run, opened at `Started` and closed at `Finished`;
one span per committed step beneath it; an `execute_tool <name>` span per tool
call; and a `chat <model>` span per provider call, carrying the model, the token
split and the latency that call actually cost. One trace id spans the run, and
every span but the root names its parent.

### Why it opens the store as well

The event channel is not enough to build that tree, and the reason is worth
knowing before writing an exporter of your own. There is no provider-call event
at all; `ToolCall` is emitted before its result is known and has no matching end;
and `RunEvent` carries no timestamp. The per-call model, token split, latency and
finish reason live in `provider_calls`, and a step's phase breakdown lives in the
five attribution columns on `steps`.

So the exporter opens **its own** `Store` against the same path and reads through
`Store::provider_calls` and `Store::step_attributions`. It never borrows the run's
store: an `Observer` is `Send + Sync`, the connection underneath a `Store` is
`Send` and not `Sync`, and an observer holding one by reference could not exist.
`Broadcast` writes to the store from inside an observer for the same reason; this
reads.

### What is never sent

The prompt, the model's replies, tool arguments and tool output. The GenAI
conventions mark those attributes opt-in and this crate does not implement them at
all — not defaulted off, absent — so there is no flag that could include them by
accident. What crosses the wire is structure and numbers: span names, span kinds,
ids, durations, model names, token counts and a tool's name.

### Limits, stated plainly

**A step's provider attempts are laid end to end, not independently timed.** Each
attempt's duration is exact — `provider_calls.latency_ms` — but no start instant
per attempt is recorded anywhere in the store. `provider_calls.at` is one-second
resolution and is stamped after the call rather than before it, so it can neither
place an attempt nor order two inside the same second. The exporter therefore
places attempts inside the step window it timed itself, in `attempt` order. The
durations are real; the gaps between them are not claimed.

**A tool span's duration is bounded by its step, not measured per call.** There is
no end event for a tool call, and a step that batched several reads ran them
concurrently. Where a step made more than one call, the per-call split is not
claimed.

**A run is a root trace.** There is no context propagation from an incoming
`traceparent` in this release, so a run started by an already-traced service
appears as its own trace rather than as a child of that request.

**Traces only.** No metrics signal and no logs signal. OTLP over gRPC is not
implemented; JSON over HTTP is what every collector accepts and what this crate
can already speak without a dependency.

**The conventions are at Development stability.** Attribute names have moved
before — `gen_ai.system` became `gen_ai.provider.name`. `GENAI_CONVENTIONS` names
the revision this crate follows, and a later one is adopted in a release with the
change in `CHANGELOG.md`, never silently.

**An export failure is not a run failure.** A collector that is down, slow or
returning 400 leaves the run's outcome, step count and token total exactly as they
would have been, and says so only through a `tracing::warn!`.

## See also

- [Durable runs](durable-runs.md) — the trace, checkpoints and resume the events report on
- [Agent composition](composition.md) — `depth`, child run ids, and the shared spend ledger
- [Resilience](resilience.md) — what `Retry`, `FellBackTo`, `Replan` and `Stalled` mean
- [Verification](verification.md) — the gate phases that arrive as `Sandbox` events
- [Permissions and approval](permissions.md) — what `Refused`, `ApprovalRequested` and `ApprovalDecided` report
- [The contract](../CONTRACT.md) — the crate's stability and API promises
- [README](../../README.md)
