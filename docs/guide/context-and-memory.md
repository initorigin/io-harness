# Context and memory

The harness assembles each request's prompt under a budget rather than
accumulating one string forever, and the agent can record what it learned against
the workspace so a later run over the same workspace gets it back.

The alternative — keeping one string, appending every tool result to it, and
re-sending the whole thing verbatim every turn — is bounded by nothing, drops no
read the agent has already replaced, and notices nothing when a write makes an
earlier read wrong. A long run spends most of every request re-sending stale
text: the token budget is still enforced, it is just being spent on repetition.

```rust
use io_harness::{context::ContextBudget, TaskContract};

let contract = TaskContract::workspace("Refactor the parser.", &root)
    .with_token_budget(400_000)
    // Absolute ceiling per request, and the share of the *unspent* token
    // budget a request may carry of what the run has already observed.
    .with_context_budget(ContextBudget { max_tokens: 24_000, share: 0.5 });
```

Those are the values `ContextBudget::default()` already holds; the call above
states them rather than changes them.

Each turn, under that budget:

- **Superseded observations compact.** Two reads of one file, or two greps of
  one pattern, are one answer. The later one is carried whole; the earlier
  becomes a one-line stub naming the step that replaced it.
- **A stale read is re-read, not trusted.** If the agent wrote to a file it read
  earlier, the earlier read is refreshed at assembly time — through the same
  policy and the same workspace containment as any other read. If the policy
  refuses, or the path is gone, the entry becomes a stub naming the write that
  invalidated it and why the refresh failed.
- **Every observation is bounded where it enters the context**, with the elision
  visible to the model so it can ask for the rest. One budget derives both the
  request ceiling and the per-observation cap: `entry_cap_chars` gives one
  observation an eighth of the effective budget, so a single result cannot crowd
  out the seven before it.
- **The trace keeps everything.** `steps.result` records the full, unelided log.
  Bounding what the model sees must never bound what an operator can audit.

Run it live: `cargo run --example context_growth` and
`cargo run --example context_growth_bounded`.

## Durable memory

The agent can also record what it learned, keyed to the workspace rather than to
the run, and get it back on a later run over the same workspace:

```rust
// The agent calls the built-in `remember` tool during a run; the operator reads
// and clears what it wrote.
for entry in store.memory_list(&workspace)? {
    println!("{}: {}  (run {}, step {})", entry.key, entry.value, entry.run_id, entry.step);
}
store.memory_delete(&workspace, "build-command")?;
store.memory_clear(&workspace)?;
```

Entries are attributed to the run and step that wrote them, capped in count and
in total size with oldest-first eviction, and every write, eviction and recall is
in the trace. The caps are public: `MEMORY_MAX_ENTRIES` (64),
`MEMORY_MAX_CHARS` (16,000 across the workspace) and `MEMORY_MAX_ENTRY_CHARS`
(an eighth of that for any one value, truncated with a visible marker rather than
refused). Eviction never removes the entry just written, because that would make
a write a silent no-op. `Store` also exposes `memory_get` for a single key.

`remember` is deliberately narrow: it writes one keyed note into the harness's
own store, not into the workspace, so it is not a path act.

## A correction that sticks

A flat list of notes has one failure mode that matters: a person corrects
something, the agent re-learns the wrong version on the next run, and the
correction is gone. Since 0.30.0 an entry carries what kind of thing it is and
whether a run may overwrite it.

```rust
use io_harness::MemoryKind;

// A decision somebody took, pinned so a run cannot quietly reverse it.
store.memory_write(&workspace, "retries", "three", run_id, step, MemoryKind::Decision)?;
store.memory_pin(&workspace, "retries", true)?;
```

`MemoryKind` is `Fact` or `Decision` and defaults to `Fact`. Nothing in the run
loop treats the two differently — the crate stores what it was told and reports
it — because a decision is something a *person* took, and a harness inferring one
from a tool call would be guessing at intent. What the agent writes through
`remember` is therefore always a `Fact`, whatever the entry was before. Every
entry written before 0.30.0 reads back as a `Fact`, unpinned, which is what it
was. The enum is `#[non_exhaustive]`: match it with a `_ =>` arm.

Pinning is the half with teeth, and it is **a caller's act, never a run's**. A
pinned entry is not overwritten by a run and is exempt from cap eviction, so a
correction does not disappear because the agent wrote twenty notes afterwards. It
still counts toward the caps — pinning everything makes writes fail loudly rather
than silently raising the ceiling.

A refused write is not a silence. It is a `context_events` row with kind
`memory_refused`, *and* it goes back to the model as an observation saying the key
is pinned and the existing note stands. An agent that believes it corrected
something and did not will act on the correction it thinks it made, which is the
exact failure the flag exists to prevent — reporting the refusal to the trace and
not to the model would fix it for the operator and leave it broken for the run.

`Store::memory_write` is the full form and returns `MemoryWrite { refused,
evicted }`. `Store::memory_put` is unchanged and is that call with `kind` fixed to
`Fact` and the refusal dropped on the floor; prefer `memory_write` anywhere the
answer matters.

## Which notes a run actually used

`memory_list` says what the agent knows about a workspace. That is not the same
question as which of it was load-bearing on a given run, and only the second one
tells you whether an entry is earning its place in every prompt.

```rust
for recall in store.memory_recalls(run_id)? {
    println!("step {} recalled {}", recall.step, recall.key);
}
```

The context assembler writes these at recall time, so they record what actually
reached the model after the memory block was fitted to its share of the budget —
not what was available to it. `Assembled::recalled_keys` is the same list for the
turn in hand, beside the `recalled` count that was already there: the count says
how much a turn leaned on memory, the keys say what it leaned on.

One row per key per recall, never a replacement. A run that recalls the same entry
on three turns is three rows, and a caller that wants the set deduplicates it —
that is a decision about what is being counted, and the crate does not make it for
you. A recall is a fact about a run rather than a flag on an entry, so two runs
over one workspace each record their own and neither disturbs the other.

## The limits, stated plainly

Assembly **bounds** what a request carries and applies exactly the two staleness
rules above. It does **not** promise the model sees everything relevant to the
task: an observation older than the current window is a stub, and a stub the
model does not act on is information it does not have. If your application needs
a particular observation present, put it in the task contract, where it is not
subject to elision.

The token figure the assembler enforces against is an **estimate** — four chars
per token, computed in-crate, no tokenizer dependency. The trace records that
estimate beside the provider's own reported usage for the same request, so the
drift is a number you can read rather than a claim. The default share leaves
margin for it; a provider that rejects a request for context length is reported
as such rather than retried identically.

Memory entries are **agent-authored notes, not instructions**. A fact one run
recorded is read by later runs over that workspace, so a wrong or planted note
persists until someone removes it — which is why entries carry their origin, are
rendered to the model as its own notes rather than as directives, and are
listable and deletable through `Store`. An operator can always see and clear what
the agent believes.

**A pin stops a write, not a delete, and `MemoryKind` stops nothing at all.** The
kind is a label the crate stores and reports; the pin is enforced, and only
against the run — `memory_delete` and `memory_clear` are the caller's and remove a
pinned entry like any other, because an operator who cannot clear their own store
has been locked out by their own correction. Neither is a boundary against
anything but the agent's own `remember`.

**A rewind ignores the pin, and that is deliberate (0.36.0).** `rewind_run` puts
every entry a run wrote back to the value that was there before its **first**
write to that key — the same restore-point rule files have had since 0.28.0 — and
removes an entry the run created. It bypasses `memory_write`, so an entry pinned
*after* the run wrote it is still put back; the alternative is telling a caller a
rewind happened when it had not. The cost is one restore-point row the first time
a run writes a given key, bounded by keys touched per run rather than by writes
made. See [Durable runs](durable-runs.md#putting-a-whole-run-back-0360).

## See also

- [Tools and skills](tools-and-skills.md) — the per-result cap this budget derives
- [Permissions and approval](permissions.md) — the policy a stale-read refresh
  passes through, exactly like any other read
- [Durable runs](durable-runs.md) — the store memory shares, and what survives a
  restart
- [Resilience](resilience.md) — what happens when a request is rejected or a run
  stops making progress
- [Observability and replay](observability.md) — the unelided trace
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
