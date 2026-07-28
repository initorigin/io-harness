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
use io_harness::{context::ContextBudget, TaskContract, Verification};

let contract = TaskContract::workspace("Refactor the parser.", &root, verify)
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
