# Measurements — IO Harness

Numbers this repository has actually measured, with the machine named and the
method stated. **Nothing here is a gate.** No test asserts any of it: a duration
asserted on a CI runner is a flake waiting to be written, and this project has
paid for that lesson more times than any other. Acceptance criteria assert
structure; this file records timing.

Each entry says what was measured, with what, and on what. A number without a
machine is a number nobody can reproduce or refute.

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
