<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.44.0

**Execution state:** DEVELOPING
**Release fit:** high
**Target mode:** published
**Outcome:** Run a long job and stop paying full price for the part of the request that never changes.
**Today exactly one thing in a request is marked cacheable, and it is the smallest half.** 0.38.0 put one breakpoint at the end of the `system` block — `src/provider/anthropic.rs:161` for Anthropic, `cached_system` (`src/provider/openai_wire.rs:84`) for OpenRouter — and covered the tool schemas and the instructions with it, because those vendors order a request's cacheable prefix tools-then-system. That block is a few thousand tokens and it is fixed for the life of a run. The transcript is the part that grows: on step sixty the observation section is most of what is sent and all of it is billed fresh, every step, forever.
**0.38.0 left the transcript unmarked on purpose, and the reason has expired.** `context::assemble` (`src/context.rs:558`) supersedes, invalidates, re-reads and re-fits earlier observations on every turn, so the observation section was not a byte-stable prefix and marking it would have been billed as a cache *write* on nearly every turn — costing money instead of saving it. That reasoning is recorded in the function's own doc comment (`src/provider/openai_wire.rs:80`) and in 0.38.0's `known_limitations`. 0.43.0 changed the premise: when the ledger crosses `Compaction::at_share` the oldest observations become one model-written paragraph stored in `summaries` and replayed on resume, and everything from the top of the prompt through the end of that paragraph stops moving.
**So a second breakpoint goes at the end of that frozen prefix.** The run loop computes where the prefix ends, hands the offset to the provider on the request, and the two vendors that take a request-side marker split the user turn into two content blocks with the marker on the first. A run that folds once and then works for forty more steps re-sends that prefix forty times and is charged the cache-read rate for it — a tenth of the input rate on the Anthropic table this crate already ships (`Price::cache_read`, `src/pricing.rs:95`).
**And the marker is withheld unless the prefix has actually repeated.** "Immutable by construction" is not true of the whole prefix as the prompt is assembled today, and this release says so rather than assuming it: the memory block renders *ahead* of the summary (`src/context.rs:581`) and is re-read from the store on every turn by deliberate design (`src/run.rs:4295`), so a note the run writes about its own work moves the prefix underneath the summary. The loop therefore holds the previous step's candidate prefix and marks only when this step's is byte-identical to it. A note written mid-run drops the marker for one turn and it returns on the next. The crate never asks a vendor to cache a prefix it has not already seen twice, which makes "this release cannot cost money" a property of the mechanism rather than a hope about assembly.
**What the reads cost is already where an operator looks, and this release proves it rather than building it.** `Usage::cache_read_tokens` has existed since 0.18.0 (`src/provider/mod.rs:618`), both wires parse it from the vendor's own counters (`src/provider/anthropic.rs:628`, `src/provider/openai_wire.rs:340`), every completion lands a `provider_calls` row carrying it (`src/state.rs:2991`), and `Store::spend_by_run` / `spend_by_model` / `spend_by_day` (`src/state.rs:3921`–`:3934`) price it through `Price::cache_read`. Nothing new is needed for the money to show up. What is new is the *cause*: an operator whose run gets no transcript cache hits currently has no way to tell whether the marker was never offered, was withheld by the guard, or was offered and ignored. One additive event says which.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
