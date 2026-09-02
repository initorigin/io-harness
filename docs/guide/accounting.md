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
- **`server_tool_requests` was zero on every row written before 0.22.0**, because
  nothing declared a tool for a provider to execute. Provider-executed web search
  is the first thing that moves it, and it is charged per request rather than per
  token — see [A search is charged per request](#a-search-is-charged-per-request).
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
| `server_tool_requests` | Provider-executed tool requests — a search or fetch the vendor ran on its own side — billed per request rather than per token. Zero on every row written before 0.22.0, because nothing declared such a tool; a run that declares web access is the first that reports one. The counter shipped in 0.18.0 so that using it later was not a second break of `Usage`. |
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

## Reading the table back (0.71.0)

A table you built is also one you can ask about — which models it covers, and
whether it covers anything at all.

```rust
use io_harness::pricing::{Price, PriceTable, PriceTier};

let prices = PriceTable::new("2026-07-29")
    .with("some-vendor/zeta", Price { input: 1_000_000, ..Price::ZERO })
    .with("some-vendor/alpha", Price { input: 2_000_000, ..Price::ZERO })
    .with_tiers(
        "some-vendor/tiers-only",
        vec![PriceTier { min_prompt_tokens: 200_000, price: Price::ZERO }],
    );

assert_eq!(prices.models(), vec!["some-vendor/alpha", "some-vendor/zeta"]);
assert_eq!(prices.len(), 2);
assert!(!prices.is_empty());
```

`models()` comes back sorted, because the table is a `BTreeMap` — the same order
on every call and on every machine, so a rendered list does not reshuffle between
two reads of the same table. `is_empty()` is the check for the table nobody has
entered a price into yet, which is the state `PriceTable::new` starts in and the
state in which every group comes back a floor of zero with all of its calls
counted as unpriced.

**It lists what the table can *price*, which is not every model it mentions.**
Base prices and prompt-length tiers are independently keyed maps: `with_tiers`
can name a model `with` never priced, and such a model is **unpriced** —
`cost_micros` returns `None` for it, exactly as it does for a model the table
never heard of, because the tier is a rate that replaces a base price rather than
one that stands in for a missing one. So `models()` and `len()` are the base
keys alone and a tier-only model is absent from both. Rendering "the models we
can price" from the union of the two instead would name a model the table then
declines to price, and the run would report an unpriced call for a model the
same UI had just claimed to cover.

## A search is charged per request

`Price::per_server_tool_request` is the one line in the table that is **not** per
million: it is micro-units for **one** provider-executed request, because that is
how a vendor bills a search it ran on its own side. It has existed since 0.18.0
and had nothing to charge until 0.22.0, when a run that declares web access
started reporting `Usage::server_tool_requests` above zero.

```rust
use io_harness::pricing::{Price, PriceTable};

let prices = PriceTable::new("2026-07-30")
    .with("some-vendor/some-model", Price {
        input: 3_000_000,
        output: 15_000_000,
        // $0.01 per search — per request, not per million requests.
        per_server_tool_request: 10_000,
        ..Price::ZERO
    });
```

The charge follows the counter, not the citation and not the
`server_tool_calls` row: a run whose responses reported two requests draws the
per-request price twice on top of whatever its tokens cost, and a run that
reported none is charged for its tokens alone even if it declared web access and
came back with sources. A search that failed still counted as a request if the
vendor said so — a vendor that bills for a broken search is billing for it in the
counter, and the trace records what the vendor reported rather than what it ought
to have charged.

## Grouped rows, and where the crate stops

Four groupings, each returning the raw sums with the derived cost beside them:

- `Store::spend_by_model` — keyed by the model that served each call. A call
  whose provider named no model groups under `UNKNOWN_MODEL` and counts as
  unpriced, rather than being merged into a neighbour.
- `Store::spend_by_day` — keyed `YYYY-MM-DD`, from the database clock, the same
  clock `runs.started_at` uses.
- `Store::spend_by_run` — keyed by run id.
- `Store::spend_by_session` (0.75.0) — keyed by session id. Every turn of a
  session is its own run and the session tables carry no token columns, so this
  is the join rather than a fold the caller writes.

A run that belongs to no session is **absent** from the session grouping rather
than bucketed under a sentinel. That is the opposite of `spend_by_model`'s answer
for an unknown model, and deliberately: an unpriced call is still a call somebody
paid for and has to stay visible, where a one-shot run is not an unattributed
conversation — it is not a conversation. Inventing a group for it would make the
sessions disagree with the runs for a reason no reader could see.

Rows come back ordered by key, which is the only ordering promised. Streaks,
leaderboards, heat maps, per-day charts and every other rendering are the
consuming application's decision, not the harness's.

## The cache hit rate, and the wire that cannot report half of it

The cache counters have been recorded since 0.18.0 and, until 0.75.0, nothing
divided one by the other. Two breakpoints place the cacheable prefix — the end of
the system block (0.38.0) and inside the transcript (0.44.0) — and their effect
was unobservable, which is a poor position for a design constraint to be in.

`Usage::cache_hit_rate` and `Spend::cache_hit_rate` answer the share of the prompt
that was served from cache. A call with no prompt has no rate, rather than a rate
of zero: nothing to have hit and having missed are different facts.

**`Usage::cache_write_tokens` is an `Option`, and the `None` is load-bearing.**
The OpenAI wire — which fronts OpenAI, OpenRouter and every `Compatible` endpoint
— carries no cache-write counter at all. Before 0.75.0 that absence was recorded
as `0`, which reads exactly like a call that wrote nothing. It is not: it is a
call whose write cost the vendor never told us. `Usage::cache_writes_reported`
says which you are looking at.

So a hit rate taken from an OpenRouter run is a **read rate over an unknown write
cost**, and this crate's own live cache evidence is taken on that wire. Read
`Spend::unreported_cache_writes` beside the rate: it counts the calls in the group
whose writes were never reported, in the same "this group is a floor, not a total"
role `unpriced_calls` has. Nothing in the crate infers a write from the prompt
length — an invented number is worse than an absent one.

Pricing is unaffected. An unreported write is billed as fresh input, exactly as it
was before the counter became an option.

## What the runs did, not just what they cost

Money is one question about a trace and it is not the first one an operator asks
after a week of runs. Since 0.30.0 the same store answers five more, over the rows
it was already writing:

```rust
use io_harness::Store;

# fn demo(store: &Store) -> io_harness::Result<()> {
for row in store.runs_by_outcome()? {
    println!("{}: {}", row.key, row.count);      // success: 41, stalled: 6, …
}

let first = store.first_try()?;
println!("{} runs, {} succeeded, {} clean first time", first.runs, first.succeeded, first.first_try);

let recovery = store.recovery()?;
println!("{} fallbacks, {} replans, {} resumes", recovery.fallbacks, recovery.replans, recovery.resumes);
# Ok(()) }
```

`runs_by_outcome`, `runs_by_day` and `gate_failures_by_phase` all return
`Vec<Tally>` — one `{ key, count }` shape for every grouped count this crate
returns, so a caller reads the same row whether it asked about outcomes, days or
phases. `runs_by_day` keys `YYYY-MM-DD` from the database clock, the same clock
`spend_by_day` groups on, so a cost row and an outcome row for one day describe
the same day rather than two that can disagree.

**`first_try` returns three counts and no rate.** *first_try / succeeded* is "when
we got there, how often first time"; *first_try / runs* is "how often does this
work at all on the first attempt". Both are legitimate, the difference between
them is large, and returning one number would be picking for the caller and hiding
which was picked. "First try" means finished successfully with no
`gate_phase_failed` event recorded against the run.

**Gate failures group by the phase the gate records** — `compile`,
`criterion-compile`, `test-run` — not by criterion text, because the phase is what
the column holds and reporting it as a criterion would be dressing a column up as
something it is not. Counted per failure, so a run that failed the same phase
three times is three; "how many *runs* failed this phase" is a different question
and is left unanswered rather than answered ambiguously.

**`recovery` counts three mechanisms and deliberately not a fourth.** There is no
escalation count: nothing records an escalation as an event, and an escalation is
in any case the opposite of a rescue — it is the run handing the problem back.
An aggregate that cannot be computed honestly is worse than a missing one, and
`unpriced_calls` above is the precedent.

Same line 0.18.0 drew: **rows out, and the crate renders nothing.** A rate, a
trend line, a colour for a bad week — all of it is the consuming application's.

An empty store answers zero everywhere and no row anywhere, not an error and not a
plausible-looking number: `runs_by_outcome` and its two siblings return an empty
`Vec`, `first_try` and `recovery` return their zeroed structs.

Each is one indexed query rather than a scan over the trace. Measured on a debug
build against an in-memory store at 20,000 finished runs: `runs_by_outcome` 1.8ms,
`gate_failures_by_phase` 2.5ms, `first_try` 7.6ms, `recovery` 1.2ms. None of them
is constant-time — the cost is linear in rows, roughly 90–380ns per run — so a
store an order of magnitude larger costs an order of magnitude more, and a caller
polling one of these on every frame should cache it.

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

- [Web search and fetch](web.md) — what a provider-executed request is, and where
  the per-request charge above comes from
- [Observability and replay](observability.md) — watching a run while it happens
- [Durable runs](durable-runs.md) — what survives a crash, and how a resume reads it
- [Resilience](resilience.md) — retry and fallback, which are what make a step
  several calls in the first place
