# Measurements — IO Harness

Numbers this repository has actually measured, with the machine named and the
method stated. **Nothing here is a gate.** No test asserts any of it: a duration
asserted on a CI runner is a flake waiting to be written, and this project has
paid for that lesson more times than any other. Acceptance criteria assert
structure; this file records timing.

Each entry says what was measured, with what, and on what. A number without a
machine is a number nobody can reproduce or refute.

## What removing history costs (0.58.0)

**What is being measured.** 0.58.0 gives an operator four instruments — a size,
a deletion, a sweep and an archive — plus a compaction. Three questions matter
before anyone runs them on a store that has been accumulating for a year: what a
removal costs, whether a sweep is really one pass over the schema rather than a
loop wearing a different name, and what a `VACUUM` costs and needs while it runs.

**The shape to expect, stated before it was measured.** A removal is linear in
the rows removed **and** linear in the number of tables, with a full scan of
every table that carries no index on `run_id` — which is most of them. Both
terms are visible below: at ten steps the per-table fixed cost dominates and the
figure barely moves, and by a thousand steps the row term is what is being paid.

**Method.** `retention_cost` in `src/state.rs`, `#[ignore]`d because it prints
rather than asserts. Stores are built **on disk**, not in memory, because a
compaction is about a file. Each store holds ten sessions of one turn and one run
each, with the given number of steps and one ledger observation per step; one
session is removed and timed.

```text
cargo test --release --lib retention_cost -- --ignored --nocapture
```

**Machine.** Apple M1, macOS 25.5.0, release profile, `rusqlite` 0.40.1 bundled.

### Removing one session from a store of ten

| Steps in the session | Rows removed | Bytes removed | Time |
| --- | --- | --- | --- |
| 10 | 23 | 1,473 | 1.140 ms |
| 100 | 203 | 13,353 | 1.564 ms |
| 1,000 | 2,003 | 132,153 | 5.832 ms |

A hundredfold more rows costs about five times as much, which is the two terms
adding up rather than a sublinear removal: thirty-four statements are issued
whatever the size, and at ten steps they are nearly all of the cost.

### A sweep against the loop it is not

| Steps per session | `sweep_sessions`, 10 sessions | Ten `delete_session` calls | Ratio |
| --- | --- | --- | --- |
| 10 | 2.286 ms | 11.234 ms | 4.9× |
| 100 | 3.681 ms | 14.690 ms | 4.0× |

This is the timing beside the structural assertion, not instead of it: the
statement count is what the suite asserts
(`a_sweep_of_many_sessions_issues_the_same_statements_as_a_sweep_of_one`), and
this is what that buys on one machine.

### Compaction

A store of twenty sessions of four hundred steps, half of them removed:

| | |
| --- | --- |
| File before | 1,560 KiB |
| Free inside it after the removals | 644 KiB |
| File after `compact` | 908 KiB |
| Returned to the filesystem | 652 KiB |
| Time | 6.292 ms |

**The peak extra disk a compaction needs is a second copy of the file** — about
1,560 KiB here, and on a store of a gigabyte, a gigabyte. That requirement, not
the duration, is why compaction is a call an operator makes knowingly rather than
something a deletion does on their behalf.

## What ranking a turn's recall costs (0.57.0)

**What is being measured.** 0.57.0 chooses which notes survive the memory block's
share by what the turn is about rather than by the write clock, and reports a note
that restates one already held at the moment it is written. The first runs **once
per scope per turn**, which is the hottest path anything in this feature touches;
the second runs once per `remember`. The question an operator raising
`max_entries` has is what each of them starts to cost.

**Method.** `memory_recall_cost` in `src/run.rs`, `#[ignore]`d because it prints
rather than asserts. A store is filled to the given size, every entry is given a
recall row from each of twenty separate runs, and a turn is constructed with a
real goal and a **200-observation ledger** — a long run's worth of read targets,
which is 208 signal tokens. Twenty rankings and twenty `remember` calls (each
including its `memory_similar` check) are timed at each size.

```text
cargo test --release --lib memory_recall_cost -- --ignored --nocapture
```

**Measured on an Apple M1, macOS 26.5.2, release profile, 2026-08-15:**

| Entries | Recall rows | ms per ranking (median of 20) | ms per `remember` (median of 20) |
| --- | --- | --- | --- |
| 64 (the default) | 1,280 | 1.106 | 1.946 |
| 512 | 10,240 | 11.088 | 21.172 |
| 4,096 | 81,920 | 119.171 | 201.369 |

**What the shape says.** Both costs are linear in the number of **entries** and
flat in the size of the recall table — eight times the entries costs roughly ten
times as long while the recall table grows by the same factor and contributes
nothing visible. That is 0.56.0's `memory_recalls_entry` still paying for itself,
and it is the claim the suite asserts rather than times:
`ranking_recall_draws_seeks_the_recalls_rather_than_scanning_them` checks the
query plan of the statement the crate runs, over ten thousand rows.

What the linearity costs is the entry-by-entry work, and it is honest to name it:
every entry's key and value is normalised into a token set on **every turn**,
because the ranking is computed from the store and the turn rather than stored.
At the default 64 entries that is about a millisecond per turn against a provider
call measured in seconds. At 4,096 entries it is about 120 ms per turn, every
turn, which is a real reason not to raise `max_entries` past what a workspace
needs. The upgrade path, if a store that large ever becomes ordinary, is to cache
each entry's token set against its `created_at` — not to make the ranking
approximate.

The `remember` column is the same work plus the duplicate check, which reads the
same entries again: at the default it is about two milliseconds per write, and it
is paid by writes only.

## What a capped memory write costs (0.56.0)

**What is being measured.** 0.56.0 made eviction rank candidates by the evidence
in `memory_recalls` instead of by the write clock, and made the three caps an
operator's numbers. Those two changes meet on the write path, so the question an
operator raising `max_entries` actually has is what the write starts to cost.

**Method.** `memory_eviction_cost` in `src/state.rs`, `#[ignore]`d because it
prints rather than asserts — a duration asserted anywhere in this suite is a
flake waiting to be written. A store is filled to the cap, every entry is given a
recall row from each of twenty separate runs (the shape a busy workspace reaches,
where a note written early has been carried by every run since), and then twenty
further writes are timed, each of which evicts.

```text
cargo test --release --lib memory_eviction_cost -- --ignored --nocapture
```

**Measured on an Apple M1, macOS 26.5.2, release profile, 2026-08-14:**

| Entries | Recall rows | ms per capped write (median of 20) |
| --- | --- | --- |
| 64 (the default) | 1,280 | 0.965 |
| 512 | 10,240 | 9.042 |
| 4,096 | 81,920 | 73.175 |

**What the shape says.** The cost is linear in the number of **entries**, not in
the size of the recall table: eight times the entries costs roughly eight to nine
times as long, while the recall table grows by the same factor and contributes
nothing visible. That is what `memory_recalls_entry` buys, and it is the claim
the suite actually asserts — `ranking_eviction_candidates_seeks_the_recalls_rather_than_scanning_them`
checks the query plan of the statement the crate runs. Without the index the cost
would be linear in entries times rows, and the bottom row would be seconds.

The operator-facing number is the last one: a store of 4,096 notes makes each
`remember` that evicts cost about 73 ms. That is a tool call, not a turn, and it
is paid only by writes at the cap — but it is a real reason not to raise
`max_entries` past what a workspace needs.

**One defect this measurement found.** The first run of it reported 0.069, 0.143
and 0.068 ms — flat, and too fast. The cap comparison was
`chars <= limits.max_chars as i64`, and the measurement set `max_chars` out of
the way with `usize::MAX`, which wraps to `-1` in that cast: the break could never
fire, so every write evicted the entire workspace and no store ever held more
than two entries. Exact for the crate's own 16,000 and catastrophic for a large
number an operator may now write. The comparison is `u128` since, and
`a_character_cap_too_large_for_an_i64_is_a_ceiling_and_not_a_purge` is the guard.

## What the image door costs (0.55.0)

**What is being measured.** 0.55.0 widened what `Media` and `view_image` accept.
The four types every provider documents pass through byte-identically; BMP, TIFF,
ICO, TGA and PNM are decoded and re-encoded to PNG. The question an operator has
is whether that conversion is worth doing in the run or before it.

**Method.** `examples/transcode_cost.rs`. A 512×512 gradient — flat colour would
measure the encoder's best case rather than an ordinary one — encoded into each
source format, then handed to `Media::attach` twenty times after one untimed
round. No provider is called: this is the door, not the wire.

```text
cargo run --release --features media --example transcode_cost
```

**Measured on an Apple M1, macOS 26.5.2, release profile, 2026-08-14:**

| Source | In (bytes) | Out (bytes) | Path | ms |
| --- | --- | --- | --- | --- |
| `image/png` | 295,476 | 295,476 | pass-through | 0.14 |
| `image/jpeg` | 24,252 | 24,252 | pass-through | 0.01 |
| `image/bmp` | 1,048,698 | 295,476 | decode → PNG | 2.55 |
| `image/tiff` | 1,048,806 | 295,476 | decode → PNG | 1.75 |
| `image/x-tga` | 1,052,660 | 295,476 | decode → PNG | 2.18 |
| `image/x-portable-anymap` | 786,495 | 229,940 | decode → PNG | 2.06 |

**What it says.** A conversion costs single-digit milliseconds on an image of the
size a model actually looks at — against a request that takes seconds, it is
free, and the operator should stop converting these by hand. The pass-through
rows are the floor: they are a base64 encode and nothing else, and the JPEG's
0.01 ms against the PNG's 0.14 is the input size rather than the path.

The other number in the table is the one worth reading twice: an uncompressed
BMP is 1 MB where its PNG is 295 KB, so the conversion also moves the image
comfortably under `MAX_IMAGE_BYTES` — a scan that would have been refused for
size arrives.

**Not measured, deliberately:** what a decode costs at the pixel bound. Anything
approaching `MAX_IMAGE_PIXELS` is refused from its header before it is decoded,
so the number would describe a path no run takes.

## Starting a read before the completion ends (0.54.0)

**What decides whether this helps: the window.** A completion arrives over time,
and a tool call inside it is complete long before the message is. The window is
how long the provider keeps streaming *after* a call's arguments are finished —
everything the model says afterwards is time the harness used to spend idle. A
model that emits a bare tool call and stops has no window and gains nothing; a
model that narrates its plan around the call has a large one.

What is saved is bounded above by `min(window, read)`. That is the whole model,
and it is worth more than any single number.

**Method.** `examples/speculation_window.rs`, against a scripted provider rather
than a live one — a real model's window is a property of that model and that day,
not of this crate. The provider reports one finished tool call, then keeps
streaming deltas for a fixed tail; the tool takes a fixed time. The same turn is
run twice, once with `max_parallel_reads` at its default and once at `1`, which is
what turns starting early off.

```text
cargo run --release --example speculation_window
```

**Measured on an Apple M1, macOS 26.5.2, release profile, 2026-08-14:**

| | |
| --- | --- |
| Tail after the tool call (the window, configured) | 400 ms |
| The read itself (configured) | 300 ms |
| Window actually measured | 415.3 ms |
| Turn, starting early | **416.8 ms** — `Speculated { started: 1, used: 1, discarded: 0 }` |
| Turn, `with_max_parallel_reads(1)` | **720.6 ms** — no `Speculated` event |
| Saved | **303.8 ms** |

The read disappeared into the window almost exactly: 720.6 − 416.8 ≈ the 300 ms
the read takes. That is the best case for a single call, and it is the case the
release is designed for — the read is shorter than the window, so all of it is
absorbed.

**Two things this number does not say.**

- **A read longer than the window is only partly absorbed.** With the numbers
  reversed — a 300 ms window and a 400 ms read — the saving would be the window,
  not the read.
- **A discarded speculation costs its whole read and saves nothing.** The example
  reports `discarded: 0`; a run against a provider that streams its calls late, a
  model that revises its arguments, or a step whose completion had to be retried
  will not. `EventKind::Speculated` is what makes that visible on a real run, and
  it is the number to watch before concluding the feature is helping.

**One thing worth knowing before measuring this yourself.** Speculation follows
streaming, and streaming follows the turn entry point: only the `_observed` and
`_steered` session turns stream. A measurement taken through `Session::turn_bounded`
or `Session::turn` shows no saving at all, for a reason that has nothing to do
with this feature — that was the first result this example produced, and it was
wrong.
