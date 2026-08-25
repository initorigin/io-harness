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
in total size, and every write, eviction, withdrawal and recall is in the trace.
`Store` also exposes `memory_get` for a single key, and `memory_similar` for the
question "does this scope already hold this note under another name?".

### What goes when the store is full

Until 0.56.0 a cap dropped the oldest entry, which is curation by clock: the
build command learned on the first run and drawn on by every run since was the
first thing to go, and this morning's triviality survived it. Since 0.56.0 the
candidate order is evidence first — the number of **distinct runs** that carried
the entry, then how recently one did, and only then the write clock.

Two things about that number are worth knowing before you rely on it. It counts
*runs*, not recall rows: a row is written once per carried key per step, so rows
would measure how long the entry has existed and one long run would outvote fifty
short ones. And a recall means the entry was **carried into a prompt**, which is
the strongest signal available — nothing observes whether the model read the
line. An entry with no recalls yet is ordered exactly as it was before 0.56.0,
so a note written this morning is a candidate, and the entry just written is
never one, because evicting it would make a write a silent no-op. A pinned entry
is never a candidate at all.

### Which notes a turn is handed

Eviction decides what the store keeps. Selection decides what one turn gets, and
the two are separate questions: a store inside its share hands over everything it
holds and never selects at all. Past that share something has to go, and until
0.57.0 what went was whatever was written least recently — the same curation by
clock that eviction stopped doing in 0.56.0, applied one layer later.

Since 0.57.0 the entries that survive the fit are ranked by three terms, in order:

1. **How much of the entry is about this turn** — the count of normalised words
   the entry's key and value share with the turn's signals, which are the words of
   the run's goal plus every path or subject a tool has already named in this run
   (the `target` of each observation in the ledger).
2. **How many separate runs have carried it** — distinct runs in `memory_recalls`,
   not rows, for exactly the reason 0.56.0's eviction counts runs: rows are written
   once per carried key per step, so counting them would measure how long the entry
   has existed.
3. **The order the store returned**, which is `(created_at, key)`.

Normalisation is lowercase, split on anything that is not alphanumeric, drop
anything shorter than three characters — which is why `src/state.rs` in a note
matches the same path in a run's ledger: both reduce to `src` and `state`. Nothing
is scored by a model and nothing leaves the process, so a replayed run selects
what the run it replays selected. The third term is what makes the change safe to
adopt: an entry with no signal and no evidence keeps exactly the position it had
before, so a turn about nothing the store knows behaves as 0.56.0 did.

**The printed order does not change.** However the ranking comes out, the block is
rendered in the store's own `(created_at, key)` order, and that is a guarantee
rather than an implementation detail. The memory block is a byte-prefix of the
user turn, and 0.44.0's second cache breakpoint is withheld unless that prefix
repeats byte-identically; reordering the print would have turned cache reads into
cache writes on every marked wire, on every step whose signals moved. So a store
that fits its share assembles a byte-identical prompt to the one 0.56.0 produced,
and only the over-cap regime — the one raising a cap creates — sees any change.

What is left out is visible to the model rather than silent, on a line that no
longer claims the omission was about age:

```text
- (3 note(s) elided to fit — Store::memory_list has all of them)
```

### The caps, and what raising one costs

`MEMORY_MAX_ENTRIES` (64), `MEMORY_MAX_CHARS` (16,000 across the workspace) and
`MEMORY_MAX_ENTRY_CHARS` (an eighth of that for any one value, truncated with a
visible marker rather than refused) are the defaults. Since 0.56.0 they are an
operator's numbers — `[memory]` in `io.toml`, or
`TaskContract::with_memory_limits`:

```rust
let contract = contract.with_memory_limits(MemoryLimits {
    max_entries: 256,
    ..MemoryLimits::default()
});
```

Those three numbers were not arbitrary. The memory block gets a quarter of a
turn's effective tokens, and the defaults were chosen so the *whole* store fits
inside that share — which is why raising them is not the free win it looks like.
Past that point recall can no longer carry everything and selection starts
deciding what the model sees. What that selection is has moved twice: 0.56.0 put
evidence under it, and 0.57.0 put the turn's own subject on top of that, so the
notes that survive the fit are the ones the goal and the paths already touched
point at. From a *selection* standpoint a bigger store is now safe to have.

What is left is the bill, and it is a per-turn one. Ranking a scope normalises
every entry it holds into a token set on every turn, because the ranking is
computed from the store and the turn rather than stored: about 1.106 ms at the
default 64 entries, 11.088 ms at 512 and 119.171 ms at 4,096. A `remember`
including its duplicate check is a second pass over the same entries — 1.946,
21.172 and 201.369 ms at those sizes, paid by writes only, and already inclusive
of the eviction ranking a capped write does (0.56.0's own measurement of that
alone is about 73 ms at 4,096, so the duplicate check is roughly the balance).
Both costs are linear in the number of entries and flat in the size
of the recall table. About a millisecond a turn against a provider call measured in
seconds is nothing; 120 ms a turn, every turn, is the honest reason not to raise
`max_entries` past what a workspace actually needs. See
[docs/MEASUREMENTS.md](../MEASUREMENTS.md) for the machine and the method.

### Unlearning, and a scope above the workspace

`remember` writes by key and rewriting a key replaces it, so an agent that
learned the same wrong thing under two names had two disagreeing notes and no way
to withdraw either. `forget` removes one entry. A note an operator pinned is
refused, exactly as a write to it is, and the removal is undone by `rewind_run`
like every other write a run makes.

Both tools take a `scope`. `"workspace"` is the default and is what every version
before 0.56.0 did. `"global"` writes into `GLOBAL_MEMORY_WORKSPACE`, the scope
every run over every workspace recalls — for a fact true wherever you run, such
as the package manager an operator uses. The scope is narrower than it sounds:
**a workspace's own note of the same key wins**, and the global one is not
rendered beside it, because the specific place always knows better than the
general one. That is also what makes a wrong global note locally correctable. The
block renders the two under separate headings, so a note kept for every workspace
is never presented as something learned about this one.

Each scope holds its own caps, its own pins and its own eviction, so a run
recalling both may carry up to twice one scope's characters — inside a block
ceiling that has not grown, and the workspace's own notes take that space first.

`remember` and `forget` are deliberately narrow: they write one keyed note into
the harness's own store, not into the workspace, so neither is a path act.

### A note that restates one already held (0.57.0)

`forget` closes the contradiction the agent noticed. The one it does not notice is
the one that costs: the same fact learned twice under two names leaves two entries
that disagree, both carried into the prompt, and the model acting on whichever it
read last. Since 0.57.0 that is caught where it is written. On a write whose text
closely overlaps an entry already stored **in the same scope under a different
key**, the tool result names that key and quotes what it holds, bounded by the
same `…[truncated]` marker every other bounded result uses.

**The write still lands.** This is a report and never a refusal, and both halves
of that are deliberate: refusing because two strings overlapped would be guessing
at intent, and merging them would write a fact nobody stated. The model has the
key and the text in the same turn, so it resolves the contradiction with a
`remember` or a `forget` while it still knows which one it meant.

Two writes are not reported, because neither is a contradiction. Rewriting the
**same key** is the replacement writing by key has meant since 0.10.0. And a
workspace note restating a **global** one is the override the second scope exists
for — the way a wrong global note is corrected locally — so the check is only ever
within the scope being written.

The comparison is a normalised token overlap, shared words over union words,
computed in this process on the write path already running: no embedding, no model
call, nothing over a network. `Store::memory_similar(workspace, key, value)` is
the same answer for a caller who wants to ask it directly.

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
not what was available to it. Since 0.57.0 that fit is decided by what the turn is
about, so on an over-cap store these rows are also the record of which notes the
goal and the paths already touched pulled in, turn by turn.
`Assembled::recalled_keys` is the same list for the turn in hand, beside the
`recalled` count that was already there: the count says how much a turn leaned on
memory, the keys say what it leaned on. Both remain a record of **carriage** — the
note was in the prompt — and nothing here observes whether the model read it.

One row per key per recall, never a replacement. A run that recalls the same entry
on three turns is three rows, and a caller that wants the set deduplicates it —
that is a decision about what is being counted, and the crate does not make it for
you. A recall is a fact about a run rather than a flag on an entry, so two runs
over one workspace each record their own and neither disturbs the other.

## When the history is folded instead of truncated (0.43.0)

Elision has one failure it cannot avoid: past a point, the oldest observations
become one-line stubs. A stub says a read happened and how big it was. It does not
say what the run *learned* from it, which file it decided to change, or what it has
not done yet — so on step sixty the agent is working from its last few
observations and a list of sizes.

Compaction replaces that truncation with a paragraph:

```rust
use io_harness::{Compaction, TaskContract};

let contract = TaskContract::workspace("port the parser", "/repo")
    // Fold sooner and keep less whole: a small window, or large observations.
    .with_compaction(Compaction { at_share: 0.6, keep_recent: 4 });
```

When the ledger crosses `at_share` of the turn's own budget, everything but the
newest `keep_recent` observations becomes one model-written paragraph covering
four named things — what was attempted, which files were touched, what was
decided, and what is still open — and the run continues from that.

**It is on by default**, at `at_share: 0.8` and `keep_recent: 8`. The failure it
replaces is invisible from outside: nothing reports that a run's oldest work became
a list of byte counts. Turning it off is a setting rather than an absence:

```rust
use io_harness::{Compaction, TaskContract};

let contract = TaskContract::workspace("port the parser", "/repo")
    .with_compaction(Compaction { at_share: 1.0, ..Compaction::default() });
```

**What it costs, and where you see it.** One ordinary completion, written by the
run's own provider and model — there is no second provider to configure. It lands
one `provider_calls` row for the step it happened in, is inside `spent_tokens` and
inside the token budget, and emits `EventKind::Compacted` with the tokens before
and after. A fold is spend you can see where you already look.

**What it never loses.** Every folded observation stays in the store. The
paragraph is a `summaries` row, and a resumed, branched or replayed run replays its
folds rather than paying for them again — so a fold is bought once per run, not
once per process. `Session::transcript` is how a person reads back what a fold took
out of the model's context.

## Folding because the operator said so (0.68.0)

The threshold decides the folds nobody asked for. `fold_now` is how somebody asks:

```rust
use io_harness::TaskContract;

// The operator typed `/compact`. Fold the thread, then answer.
let contract = TaskContract::workspace("summarise where we got to", "/repo")
    .with_fold_now(true);
```

The turn's first step folds before it assembles its first request — so the summary
is in the request the operator is waiting on, not in some later one. Everything
else is the same machinery: the same summariser, the same `summaries` row, the same
`EventKind::Compacted`, and nothing about automatic compaction changes. A caller
who never sets it sees exactly the behaviour they had.

Before 0.68.0 the only way to ask was to lower `at_share` for a turn and hope the
ledger crossed it, which mutates your own setting to fake a request and can promise
neither when it lands nor whether it does.

Three things it does not do, each on purpose:

- **It is not mid-turn.** The request is read once, at the turn's first step. It is
  a property of the turn, so a contract you build once and reuse folds every turn.
- **It does not override an off setting.** `Compaction { at_share: 1.0, .. }` never
  folds, and that includes this. Off is a setting rather than an absence.
- **It does not reach a spawned child.** A contract reaches the whole tree, but a
  child's ledger is its own work with no conversation in it. Only the root turn
  honours the request.

## Folding a turn that is already running (0.69.0)

`fold_now` is fixed when the turn starts, which is no use to an operator watching a
turn that has been going for twenty steps. `Steer::fold` is that third trigger, and
it sits beside `say` and `interrupt` on the channel a session turn already accepts:

```rust,no_run
use io_harness::{ApproveAll, Ignore, OpenRouter, Policy, Session, Steer, Store};

# async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
let (steer, inbox) = Steer::channel();
let mut session = Session::open(store, "/repo")?;

let handle = steer.clone();
tokio::spawn(async move {
    // The operator hit `/compact` while the turn was still working.
    let _ = handle.fold();
});

let result = session
    .turn_steered("port the parser", &OpenRouter::from_env()?, store, policy,
                  &ApproveAll, &Ignore, &inbox)
    .await?;
# Ok(()) }
```

The step that drains the inbox folds before it assembles its own request, so the
summary is in the next thing the model is sent rather than the one after that, and
the request itself is a `ContextEvent::steered` line in the trace at the step that
read it — what the operator asked for is recorded beside what the fold then did.

The three boundaries above hold here too, with two more. It is not immediate: it
lands at the next step boundary, because a tool call in flight is not a safe place
to change the conversation out from under. It does not override an off setting. It
does not reach a spawned child. It loses to an interrupt sent before the same
boundary — the turn is cancelled and no summariser call is spent on it. And it does
nothing when there is nothing to fold: a conversation shorter than `keep_recent` has
no prefix to stand in for, so the request is spent and the turn goes on. Read the
`Compacted` event for whether a fold happened; the request alone does not say.

Two asks that reach the same boundary are one fold; two asks separated by a boundary
are two.

What a turn read at a boundary is reported as a `Steering`, whose `messages`,
`interrupted` and `fold` are what `SteerInbox::pending` returns.

## A fold outlives the turn that made it (0.69.0)

Compaction is bought per run, and in a session every turn is its own run. So a fold
used to last exactly as long as the turn that paid for it: the next turn's seed
rebuilt the conversation from the turn rows, and the paragraph the operator had just
asked for was replaced by the prompts and replies it stood in for. Over a long
conversation that meant folding again on every turn, and paying again on every turn.

The seed now looks for the newest turn on the path whose run folded, and seeds that
turn's newest summary paragraph in place of the conversation entries the fold
consumed. So a `/compact` at turn nine is still in force at turn ten, and at turn
twenty, until something folds again.

It does not flatten the whole conversation. A fold keeps the newest `keep_recent`
entries whole, so the seed keeps that tail whole too, and every turn after the
folding one is seeded as it always was — what an operator sees over a long session
is an older span that has become one paragraph and a recent span that is still
verbatim.

The paragraph reads the same as an in-turn fold's, `[earlier work, summarised]` and
all, and reaches the model as narration rather than as something either party said.
`Session::transcript` is unchanged and still renders every prompt and every reply,
and the folded observations are still in the store: what got shorter is the seed,
not the record. Nothing new is stored for it — it is a join between the turn rows
and the `summaries` rows, both of which were already there.

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
