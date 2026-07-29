# Accounting: what a run cost, and which model spent it

Since 0.18.0 the trace answers the first question anyone asks about an agent run.
Every call to a provider is its own row, carrying the model that served it, the
tokens it reported, how long it took, and why the model stopped. Every file
change records the lines it added and removed. Money is derived from a price
table you own, at read time, and is never stored.

Before this release none of it existed. The model identifier was recorded
nowhere; `runs.provider` names a *vendor* and stops being true the moment a
`Fallback` swaps mid-run. Cache tokens were parsed by neither provider client
even though a cached read is billed at a fraction of a fresh one. No provider
call was timed. `steps.tokens` held one integer per step, so a step that retried
twice and then fell over to a second vendor collapsed into a single number
attributed to nothing at all.

## A step is not a call

This is the distinction the whole design turns on, and it is easy to lose.

```rust
use io_harness::Store;

# fn demo(store: &Store, run_id: i64) -> io_harness::Result<()> {
for call in store.provider_calls(run_id)? {
    println!(
        "step {} attempt {} — {} — {:?} — {}ms",
        call.step,
        call.attempt,
        call.provider,
        call.model,
        call.latency_ms,
    );
}
# Ok(()) }
```

A step that failed twice and then answered is three rows. The two that failed are
kept, and deliberately: a model that produced tokens before the connection broke
was still billed for them, so a trace that kept only the winning attempt would
understate the bill. `ProviderCall::failure` is `None` on the call that answered
and holds the failure's short name — the same string the retry row in `steps`
records — on the ones that did not.

## The limits, stated plainly

Every figure here has a provenance, and the provenance matters more than the
figure. None of this is fixed as of 0.18.0; all of it is deliberate.

- **A token count is the provider's report**, not a measurement this crate made.
  If the vendor is wrong, the trace is wrong in the same direction.
- **A cost is only as right as your price table.** The crate ships no prices and
  cannot: it releases on its own schedule and vendors change prices on theirs.
- **An unpriced call is counted, not costed at zero.** A group with
  `unpriced_calls` above zero is reporting a floor.
- **`server_tool_requests` is zero everywhere** until 0.22.0 declares
  provider-executed tools. The meter exists before the spending, so that adding
  them is not a second break of `Usage`.
- **A run older than 0.18.0 has no rows**, and the queries say nothing rather
  than zero. Nothing is backfilled because nothing was recorded.
- **Line counts are a size, not a patch** — see [Edits](#edits).
- **A latency includes this crate's own work**, not just the provider's.

## What each number is, and is not

| Field | What it actually is |
| --- | --- |
| `usage` | **The provider's report**, not the crate's measurement. `None` means the provider said nothing, which is not the same fact as zero. |
| `total_tokens` | Taken as reported, never re-derived from the parts. A vendor whose total disagrees with its own breakdown is billing on the total. |
| `cache_read_tokens`, `cache_write_tokens` | A *breakdown* of `prompt_tokens`, not an addition to it. Anthropic reports these beside a prompt count that excludes them; the crate reconciles that at the wire boundary so a reader does not have to know which vendor wrote the row. |
| `reasoning_tokens` | A breakdown of `completion_tokens`, where the provider reports one. Anthropic bills extended thinking inside its output count and reports no separate figure, so it stays zero there rather than being guessed at. |
| `server_tool_requests` | Provider-executed tool requests, billed per request rather than per token. Zero everywhere until the crate declares such tools (0.22.0); the counter exists first so adding them is not a second break of `Usage`. |
| `latency_ms` | **The harness's own wall clock**, bracketing `Provider::complete`. It includes this crate's request building and stream consumption, not only the provider's part. |
| `ttft_ms` | Milliseconds to the first content-bearing chunk, measured from before the socket opens. `None` — never zero — where nothing measured it: an unmeasured wait and an instant one are different facts. |
| `finish_reason` | The vendor's own word, verbatim and un-normalised, because the vendor's documentation is what explains it. |

## Cost is derived, never stored

There is no cost column. A price written into the database is wrong the moment a
vendor changes it or the moment it was entered wrong, and repairing it would mean
rewriting history. A price kept outside the database is corrected once and every
past run reads true.

```rust
use io_harness::pricing::{Price, PriceTable};
use io_harness::Store;

// Micro-units per MILLION tokens: $3.00 is 3_000_000. Integers, because a
// fraction of a cent rounding on every one of a million rows accumulates into a
// figure that matches no invoice.
let prices = PriceTable::new("2026-07-29")
    .with("some-vendor/some-model", Price {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: 300_000,
        ..Price::ZERO
    });

# fn demo(store: &Store, prices: &PriceTable) -> io_harness::Result<()> {
for row in store.spend_by_model(prices)? {
    println!(
        "{}: {} calls, {} tokens, {} micro-units{}",
        row.key,
        row.calls,
        row.usage.total_tokens,
        row.cost_micros,
        if row.unpriced_calls > 0 {
            format!(" (+{} calls nobody priced)", row.unpriced_calls)
        } else {
            String::new()
        },
    );
}
# Ok(()) }
# demo(&Store::memory().unwrap(), &prices).unwrap();
```

**The crate ships no prices.** `PriceTable::new` takes the date its prices were
accurate as of and starts empty. There is no built-in vendor price list because
this crate cannot keep one accurate: it publishes on its own schedule and vendors
change prices on theirs, so a shipped number would be a confident wrong answer
for anyone who upgraded late. That is worse than an explicit unknown — and
`Spend::unpriced_calls` is that explicit unknown. A group with calls in it is
reporting a **floor**, not a total, and a renderer that hides the number is lying
by omission.

The as-of date is required at construction for the same reason: a price list with
no date is a claim with no expiry, and yours *will* go stale.

## Grouped rows, and where the crate stops

Three groupings, each returning the raw sums with the derived cost beside them:

- `Store::spend_by_model` — keyed by the model that served each call. A call
  whose provider named no model groups under `UNKNOWN_MODEL` and counts as
  unpriced, rather than being merged into a neighbour.
- `Store::spend_by_day` — keyed `YYYY-MM-DD`, from the database clock, the same
  clock `runs.started_at` uses.
- `Store::spend_by_run` — keyed by run id.

Rows come back ordered by key, which is the only ordering promised. Streaks,
leaderboards, heat maps, per-day charts and every other rendering are the
consuming application's decision, not the harness's.

## Edits

Every file change a run makes records the lines it added and removed, for
`write_file` and `edit_file` alike.

```rust
use io_harness::Store;

# fn demo(store: &Store, run_id: i64) -> io_harness::Result<()> {
let (added, removed): (u64, u64) = store
    .edits(run_id)?
    .iter()
    .fold((0, 0), |(a, r), e| (a + e.lines_added, r + e.lines_removed));
println!("this run changed +{added} -{removed} lines");
# Ok(()) }
```

The counts come from comparing the file's lines before and after and trimming the
common head and tail — not from a minimal diff. A one-line replacement is one
added and one removed; rewriting the middle of a file is the size of that middle;
writing a file with what it already held is neither. It answers "how much did this
run change" without the crate carrying a diff algorithm it has no other use for.

## A run older than this release

A run recorded before 0.18.0 has no rows in either table, and the queries return
nothing rather than zeros. Nothing is backfilled, because there is nothing to
backfill from: the facts were never recorded. An unrecorded run and a free one
are different facts, and the empty result is the honest one.

Both tables are additive `CREATE TABLE IF NOT EXISTS`, no existing table changed,
and `CHECKPOINT_FORMAT` stays at 7 — so a 0.17.0 database opens and resumes
against 0.18.0, and a 0.17.0 binary still opens a database this release wrote.

## See also

- [Observability and replay](observability.md) — watching a run while it happens
- [Durable runs](durable-runs.md) — what survives a crash, and how a resume reads it
- [Resilience](resilience.md) — retry and fallback, which are what make a step
  several calls in the first place
