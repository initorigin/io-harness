# Measurements — IO Harness

Numbers this repository has actually measured, with the machine named and the
method stated. **Nothing here is a gate.** No test asserts any of it: a duration
asserted on a CI runner is a flake waiting to be written, and this project has
paid for that lesson more times than any other. Acceptance criteria assert
structure; this file records timing.

Each entry says what was measured, with what, and on what. A number without a
machine is a number nobody can reproduce or refute.

## What an append-only prefix is served from cache (0.85.0)

**What is being measured.** The cached share of every step's prompt on a live
session against Fireworks, once the assembly is append-only between folds and
every request of the session carries the same routing key. Reported by
`EventKind::StepUsage::cached_fraction`, which is `cache_read_tokens` over
`prompt_tokens` in permille.

**The shape to expect, stated before it was measured.** A vendor can serve
everything it has already seen and nothing it has not, so the ceiling on any one
request is `(prompt − newest message) / prompt`. The share therefore has no fixed
value: it rises as a session's history grows past the size of one step's
observation, and it dips whenever a step observes something large. What is worth
measuring is not the percentage but the *gap* — how far below that ceiling the
crate actually lands.

**The machine.** macOS 15 on Apple silicon, against Fireworks serverless,
2026-09-10, on the seven models the account could reach that day. A ten-step
session against `accounts/fireworks/models/glm-5p3`, reading one ~440-token file
per step:

| Step | cached / prompt | share |
| --- | --- | --- |
| 1 | 0 / 5,032 | 0.0% |
| 2 | 5,008 / 5,257 | 95.2% |
| 3 | 5,242 / 5,677 | 92.3% |
| 5 | 6,102 / 6,557 | 93.0% |
| 6 | 6,542 / 6,997 | 93.4% |
| 8 | 7,495 / 7,960 | 94.1% |

**The gap, which is the actual result.** On every step after the first,
`cache_read_tokens` was the *previous step's whole prompt* minus about 20 tokens.
The crate is at the ceiling: what is not served is the newest message, which no
cache can serve, plus roughly 20 tokens where the prompt's closing instruction
moves down as observations are appended below it. Raising the share above 94% is
a matter of a session's history being long relative to one step's observation,
not of anything left on the table here.

**A cached share is a fact about a deployment.** The same bytes and the same key,
sent five times in a row to each model — the first warms, the next four are
reported:

| Model | rounds 2–5 | rounds that hit |
| --- | --- | --- |
| `glm-5p3` | 99, 99, 99, 99 | 4 / 4 |
| `minimax-m3` | 99, 99, 99, 99 | 4 / 4 |
| `glm-5p3-flash` | 97, 97, 97, 97 | 4 / 4 |
| `deepseek-v4-flash-0731` | 12, 12, 12, 12 | 0 / 4 |
| `deepseek-v4-pro-0813` | 0, 0, 0, 0 | 0 / 4 |
| `kimi-k3` | 0, 0, 0, 0 | 0 / 4 |
| `qwen3p8-max` | 0, 0, 0, 0 | 0 / 4 |

Four of the seven served little or nothing of a **byte-identical prompt repeated
immediately**. `deepseek-v4-flash-0731` served 804 of 6,313 tokens — one block —
four times within a token. The crate sent the same request to all seven, and the
event stream reports what each one said. **A run that reports a low share is not
necessarily a run doing anything wrong**, and a first reading can mislead in the
other direction too: a single warm-then-measure pass put `glm-5p3` at 0% and then
at 99% twenty minutes later, which is one draw from a fleet rather than a
property of the model.

**Consistency across a session is its own question.** `minimax-m3` matches
`glm-5p3` on the repeated-prefix probe and does not hold it over a live session:
the same ten-step run read 99.9%, 97.8%, 92.1%, then **84.1%**, and a second
attempt dropped to **79.1%** at step 10 and reported one step at 0%. Nothing in
the crate changed between them. Where a session must hold a floor, the floor is a
property to measure per deployment rather than to assume from a probe.

**Which surface reported it.** All three the contract names, on this vendor:
`usage.prompt_tokens_details.cached_tokens` in the body of a *streaming*
response, the final chunk's `perf_metrics` when `perf_metrics_in_response` was
sent, and `fireworks-cached-prompt-tokens` on a **non-streaming** response only —
a streaming response carries `fireworks-prompt-tokens` but no cached header,
which is what `perf_metrics_in_response` exists to answer.

## What a run's cache hit rate actually is (0.83.0)

**What is being measured.** `Spend::cache_hit_rate` over a whole live run —
cache-read tokens as a fraction of prompt tokens, across every call the run made.
The accounting layer has recorded the number since 0.75.0 and no page had ever
printed one, because the only way to get it is a multi-turn run against a vendor
that caches, which is a cost this file's own standing rule says to pay rather
than to guess around.

**Why it is measured here at all.** The entry two sections below records that a
tool mask's benefit "is a cache hit rate on a live run over several turns, and
this release did not take it". This is that number. It does not settle the mask
question on its own — that needs the same run with the mask off — but it settles
the denominator, which was the part nobody knew.

**Method.** `cargo run --example live_accounting` with `OPENROUTER_API_KEY` set
and `OPENROUTER_MODEL=anthropic/claude-sonnet-4`. An Anthropic slug on purpose:
an OpenAI-family model caches a repeated prefix by itself, which would report
cached tokens that owe nothing to this crate's marker. A six-step workspace run
over three small source files, ending in one edit. Measured 2026-09-07 on an
Apple silicon laptop against OpenRouter; the vendor's cache clock is theirs, not
ours, so a re-run minutes later is the same measurement and a re-run hours later
may not be.

**The numbers.**

| | |
| --- | --- |
| Provider calls | 6 |
| Prompt tokens across the run | 40,076 |
| Cache-read tokens across the run | 27,150 |
| `Spend::cache_hit_rate` | **0.677** |
| Cache-write tokens reported | none — OpenRouter reports no write counter for this route |

**The shape is more interesting than the ratio, and it was not what was
expected.** Cache-read tokens were **5,430 on every step after the first, exactly
and unchangingly**, while the prompt grew from 6,035 to 8,168 tokens as the run
accumulated observations. The cached prefix is the system block 0.38.0 marks and
nothing else: everything a step adds is new tokens at full price. So the hit rate
a longer run reports *falls*, not because caching degrades but because the
denominator grows against a fixed numerator — 0.677 is a six-step number and a
twenty-step run of the same shape would report less.

**What that means for the mask question.** It bounds the prize. A tool mask
protects the cacheable prefix, and the prefix here is worth 5,430 tokens a step;
whatever a mask saves, it cannot save more than the prefix is worth, and it costs
nothing to keep. It also says where the real spend is — the growing history, which
is what the compaction ladder addresses and what `[run.context] max_tokens`
bounds.

**What it does not measure.** One vendor, one route, one model, one moment, and
one prompt shape. It says nothing about a provider that reports a write counter,
about a longer horizon, or about what the same run costs with the mask off.

## What each compaction rung costs and keeps (0.81.0)

**What is being measured.** The first thing this crate's own evaluation suite was
built to answer: does a graduated projection beat a fold on the same run. Prompt
tokens over a whole run, and retention — whether the facts the case declared it
needs were still in the last request — for each rung of the ladder against the
same script.

**Why it is measured here at all.** Context Collapse shipped in 0.76.0 and the
three rungs beside it ship in this release, and none of them had a number
attached. The crate has decided twice that a default is not changed ahead of the
measurement that decides it; this is that measurement, and the answer is that it
does not decide much yet.

**The shape expected before it was measured.** Reduction lossless and cheap, snip
cheap and slightly lossy, microcompact the largest saving and the largest loss,
retention falling as the rungs get more aggressive.

**Method.** `n5_what_each_compaction_rung_costs_and_keeps` in `tests/eval.rs`,
`#[ignore]`d because it prints rather than asserts. Each arm records its own
cassette and replays it: the replay key is the request's own bytes, so an arm that
compacts differently cannot answer from another arm's recording. The arms are
comparable because the **script is identical**, which fixes the model's answers by
construction. Eleven scripted turns, two tool calls a turn, a 1,500-token context
ceiling. Run it with:

```text
cargo test --test eval -- --ignored --nocapture
```

**Machine.** Apple M1, macOS 26.5.2, debug profile, 2026-09-06.

**Numbers.**

| Arm | Prompt tokens | Retention |
| --- | --- | --- |
| none (0.80.0) | 55,160 | 1.00 |
| collapse only | 55,160 | 1.00 |
| reduce | 55,160 | 1.00 |
| snip | 54,365 | 1.00 |
| microcompact | 55,160 | 1.00 |

**Only snip moves the number, and it moves it by 1.4%.** Every rung keeps
everything the case declared it needed, so nothing here argues for changing any
default — and no default changed.

**What it does not measure, and this is the finding rather than a caveat.** Three
of the four arms report the baseline because the case never reaches them, and each
for its own reason, all of them correct behaviour: **reduce** trims the memory
block's share and this case has no memory notes, so there is nothing to reduce;
**microcompact** needs three contiguous results from one step and the script makes
two calls a turn; **collapse** shortens an entry that would otherwise be stubbed,
and the fit loop stubs these before it gets there. A rung that does not fire
reports the same number as one that fires and buys nothing, and only the arm's own
reason tells them apart.

So the case set is the limit, exactly as this release's contract said it would be:
*"four cases that all read files will argue for dropping the document tools"*. The
suite is what makes the next case set cheap to run; it is not itself evidence that
a rung is worthless.

## What the tool mask costs (0.81.0)

**What is being measured.** 0.76.0 decided that a masked run offers exactly the
catalogue an unmasked one offers — the same `ToolSpec` values, in the same order,
serialised to the same bytes — so that the vendor's cacheable prefix does not
move. The decision was argued from first principles and never measured.

**Method.** `n5_what_the_tool_mask_costs` in `tests/eval.rs`, `#[ignore]`d. Two
arms over one script, masking a tool the case never calls. Masking a tool the case
*does* call measures something else — the run loses a step to a refusal and the
request count roughly doubles, which is a real number about masking a needed tool
and not this question. Same command as above.

**Machine.** Apple M1, macOS 26.5.2, debug profile, 2026-09-06.

**Numbers.**

| Arm | Catalogue tokens | Whole request |
| --- | --- | --- |
| unmasked | 4,409 | 9,677 |
| masked | 4,409 | 9,735 |

**The catalogue is byte-identical, and the guarantee holds under measurement.**
What a mask costs is 58 tokens in the system block, once per request — the
sentence that tells the model which tools it may not call. That is the whole
price, and it is roughly 0.6% of a request against a cache invalidation that would
have cost the entire prefix.

## What the tool catalogue costs, and what tiering saves (0.81.0)

**What is being measured.** The request floor. An io-cli field test on 2026-09-05
measured 7,311 tokens per request before the user had typed anything, 5,436 of
them the tool catalogue. This is the same measurement taken inside the crate,
against the descriptions alone, with and without tiered exposure.

**Method.** `f14_every_tool_description_fits_the_budget_or_is_a_named_exception`
in `tests/tool_tiers.rs` prints the pair as it runs. Description tokens only — the
schemas are counted by the `catalogue_cost` scorer above, and separating them is
what shows whether a description budget or a tier is the better lever.

```text
cargo test --all-features --test tool_tiers f14 -- --nocapture
```

**Machine.** Apple M1, macOS 26.5.2, debug profile, 2026-09-06.

**Numbers.**

| Build | Full catalogue | Core tier only |
| --- | --- | --- |
| default features | 2,140 | 1,860 |
| all features | 2,488 | 1,914 |

**Tiering takes 23% off the descriptions on a full build**, and 13% on the default
one — the difference being that the default build compiles no document tools to
withhold. The saving is per request and paid on every step of every turn.

**What it does not measure.** What tiering *costs*: an `expand_tools` call is one
extra turn, and expanding mid-run rewrites the cacheable prefix. The break-even
depends on how often a turn reaches beyond the core set, which this crate has no
case set for yet. Tiering therefore ships opt-in with the full catalogue as the
default.

## What `ModelApprover` catches (0.81.0) — not measured

**The question.** The field publishes a figure of roughly 89% against 13.6% for
the equivalent of `ModelApprover`. This crate has the primitive and no measurement
of it, and this release's contract named answering that as one of three
retroactive debts.

**It is not answered here, and the reason is structural rather than a shortage of
time.** `ApproverCatchRate` exists in `io_harness::eval` and scores what it says.
What the deterministic arm cannot supply is the evidence: an approver's decision is
a completion, and a *recorded* approver refusing an act is not evidence that a
differently prompted approver would refuse it. Replaying the recording measures the
recording.

Answering it needs a real model as the approver, which is the ignored live arm
(`n5_the_live_arm_scores_a_case_against_a_real_provider` in `tests/eval.rs`) and a
case set of injected acts that does not exist yet. **No default moves on the
strength of a number this crate has not taken**, which is why `ModelApprover` is
unchanged in this release.

**The live arm itself is real and was run.** Against `openai/gpt-4o-mini` through
OpenRouter on 2026-09-06, one case — read the readme, write a file, gated on the
file's contents — scored `prompt_tokens` 9,647 over two requests, `retention` 1.00,
`catalogue_cost` 4,409 over 25 tools, and decided as the case declared. A live
model took the same two steps the recorded one did, which is the smallest useful
statement the arm can make: the scorers read a real run, not only a replayed one.
What it does not yet have is a case set worth drawing a conclusion from.

## What one program costs against the chain of calls it replaces (0.79.0)

**What is being measured.** The token cost of one task done three ways against a
real provider — with `run_program` merely offered, with the model told to use it,
and with ordinary tool calls — from this crate's own `provider_calls` rows rather
than estimated.

**Why it is measured here at all.** CodeAct is usually introduced with a figure of
roughly 64% fewer tokens. That number was measured on a different harness, with a
different context assembler and a different tool catalogue. The roadmap entry for
this release said the capability must be argued from this crate's own evidence, so
this is that evidence, and it does not agree.

**The shape expected before it was measured.** Fewer provider round trips, because
control flow the model would otherwise simulate across several completions runs
inside the interpreter. Fewer prompt tokens, because a chain resends the whole
growing transcript on every step. More completion tokens, because writing a
program is more output than writing one tool call.

**Method.** `examples/codeact_live.rs`. Three arms over the same six-file
workspace, the same model and the same permissive policy, differing only in
whether `with_codeact` was called and whether the goal names the tool:

```text
set -a; . ./.env; set +a
cargo run --release --features codeact --example codeact_live
```

The task requires a loop with a branch and a write per iteration — append a line
to every `.txt` file under three lines, leave the rest alone, then write how many
were changed. Neither `exec`, which takes a fixed argv, nor `shell`, whose grammar
refuses control flow by name, can express that; a program can. The example reads
the `Program` events back and prints how many programs ran and how many callbacks
they made, so an arm that did not use the capability says so.

**Machine.** Apple M1, macOS 26.5.2, release profile, 2026-09-04, against
OpenRouter with `deepseek/deepseek-v4-flash-0731`.

**Numbers.**

| arm | steps | prompt | completion | total | programs / callbacks | answer |
| --- | --- | --- | --- | --- | --- | --- |
| offered | 6 | 38,303 | 1,492 | 39,795 | none — never used | correct |
| directed | 13 | 124,689 | 36,727 | 161,416 | 4 / 49, all finished | correct |
| tool calls | 5 | 30,148 | 2,009 | 32,157 | — | correct |

**Two results, and the second is not the one this capability is usually sold on.**

**The model does not choose it.** Offered the tool on a task shaped for it, this
model wrote no program — on this run and on the three before it. Once it wrote one
that reached for the workspace with Python's own file reads, got nothing because a
program runs in an empty scratch directory, and abandoned the approach; the other
times it never called it. The tool description was corrected after the first, and
the answer did not change.

**Told to use it, it costs five times more here.** 161,416 tokens against 32,157.
The direction of the completion term is what was predicted and its size is not:
36,727 completion tokens against 2,009, because the model wrote **four** programs
rather than one, iterating — 12, 10, 15 and 12 callbacks. Every one of them ran and
finished, and the answer was right, so this is a cost result rather than a
correctness one. The prompt term went the wrong way too: a program's output re-enters
the transcript, and four programs' worth of output is more than a chain of reads.

**What that does and does not settle.** It settles that on a small task, with this
model, a program is not cheaper — and that a model iterating on programs is the
expensive case rather than the happy one. It does not settle the capability's
ceiling: one task, one model, one sample per arm. Four earlier runs of the offered
arm produced ratios between 0.67x and 1.57x **while never using the capability at
all**, which is a fair measure of how little a single sample of this comparison
means.

The release does not rest on this number. What it rests on is that a program's acts
are gated exactly as a model's own calls are, which the suite proves and this run
demonstrates live across 49 callbacks. The saving is a hypothesis this repository
has now tested once and not found; it is recorded here rather than left as somebody
else's figure repeated.

## What Context Collapse carries that a stub does not (0.76.0)

**What is being measured.** How much of a turn's history survives into the
projection at one fixed budget, with the collapse rung off and at three settings,
and what the rung itself costs to compute.

**The shape to expect, stated before it was measured.** More observations carried
and fewer stubbed, at roughly the same token total — the budget is a ceiling and
the rung does not raise it, so the saving is not fewer tokens but *more of the run
inside the same tokens*. A smaller `keep_chars` should carry more entries, because
each shortened one leaves more room for the next. The rung's own cost should be
too small to matter beside a provider call, since it is one truncation per entry
that was going to be elided anyway.

**Method.** `n5_what_a_collapse_carries_that_a_stub_does_not` in
`tests/context_collapse.rs`, `#[ignore]`d and printed rather than asserted:

```text
cargo test --release --test context_collapse n5_ -- --ignored --nocapture
```

Forty grep observations of 2,000 characters each — 40,000 characters against a
4,000-token ceiling, so the ledger is far larger than the turn — over distinct
targets, so nothing is elided by supersession rather than by the budget.

**Machine.** Apple M1, macOS 26.5.2, release profile, 2026-09-03.

**Numbers.**

| `keep_chars` | carried | shortened | stubbed | est_tokens | assemble |
| --- | --- | --- | --- | --- | --- |
| 0 (off) | 7 | 0 | 33 | 3,554 | 289 µs |
| 300 | 11 | 4 | 29 | 3,947 | 54 µs |
| 600 | 9 | 2 | 31 | 3,901 | 42 µs |
| 1,200 | 8 | 1 | 32 | 3,877 | 35 µs |

At `keep_chars: 300` the same budget carries **eleven** observations instead of
seven, four of them shortened. The token total rises slightly — 3,554 to 3,947 —
which is the ceiling being used rather than a cost: what was being spent on
thirty-three elision lines is spent on content instead.

Assembly is *faster* with the rung on, which was not predicted and is worth
stating: an elision line is a `format!` with two allocations and a character
count over the entry it replaces, so shortening an entry is cheaper than
describing it. The 289 µs at `keep_chars: 0` is the same work 0.75.0 did.

**What it does not measure.** Whether either setting costs accuracy. That needs a
case suite and scorers this repository does not have; the instrument is `io-eval`,
built separately. Nor does it measure a tool mask's saving, which is not a token
saving at all: masking keeps the definitions in the request on purpose, so what it
buys is a prompt prefix that stays cached while the turn's permitted tools change.
The number for that is a cache hit rate on a live run over several turns, and this
release did not take it.

## What ranking a turn's recall costs now (0.75.0)

**What is being measured.** The same thing 0.57.0 measured, after 0.75.0 stopped
recomputing it. Recall used to normalise every entry of every scope into a token
set on every turn, and `remember`'s duplicate check did the same work again over
the same entries. Each entry's token sets are now written when its value is, so a
turn reads them where it used to recompute the whole store's worth twice per step.

**The shape to expect, stated before it was measured.** The tokenising is gone
from the turn and a read is added, so the curve should flatten rather than
disappear: what remains is one query per scope, the comparison itself, and the
clone the permutation makes. The gain should grow with the entry count, because
the entry count is what used to be paid twice.

**Method.** `memory_recall_cost` in `src/run.rs`, `#[ignore]`d because it prints
rather than asserts. Unchanged from 0.57.0 — same entry counts, same recall rows,
same medians of 20 — so the two columns are comparable. Run it with:

```text
cargo test --release --lib memory_recall_cost -- --ignored --nocapture
```

**Machine.** Apple M1, macOS 26.5.2, release profile, 2026-09-02. The 0.57.0
column was taken on the same machine on 2026-08-15.

**Numbers.** Medians of 20, against the 0.57.0 figures recorded below:

| Entries | Recall rows | ms/rank 0.57.0 | ms/rank 0.75.0 | ms/`remember` 0.57.0 | ms/`remember` 0.75.0 |
| --- | --- | --- | --- | --- | --- |
| 64 (the default) | 1,280 | 1.106 | **0.701** | 1.946 | **0.944** |
| 512 | 10,240 | 11.088 | **3.149** | 21.172 | **5.170** |
| 4,096 | 81,920 | 119.171 | **29.038** | 201.369 | **47.816** |

The signal set was 208 tokens. At the default the ranking is about a third
cheaper; at 4,096 entries it is roughly four times cheaper, and `remember` — which
paid the cost twice — is a little over four times cheaper. The curve is still
linear in the entry count, which is expected: what was removed is the tokenising,
not the walk.

**What it does not measure.** Whether `max_entries` should be raised, which is a
question about what fits in a *prompt* rather than what fits in a millisecond —
see `docs/guide/context-and-memory.md`. It also says nothing about a cold store: a
first turn against entries written by an older binary recomputes and pays roughly
the 0.57.0 figure once, after which the rows are there.

## What per-step latency attribution costs the step it measures (0.75.0)

**What is being measured.** 0.75.0 records where each committed step's wall clock
went, and the instrument has to be cheap enough that the number it reports is
about the work rather than about the measurement.

**The shape to expect, stated before it was measured.** Six `Instant` reads and
some arithmetic into a small struct per step, plus five nullable columns on an
`INSERT` that was already happening inside a transaction that was already open. No
allocation per phase, no extra statement, no extra round trip. So: too small to
see against the write it rides on.

**Method.** `what_step_attribution_costs_per_step` in `src/state/trace.rs`,
`#[ignore]`d because it prints rather than asserts. Forty committed steps against
a fresh in-memory store per round, twenty-one rounds, medians reported. Both arms
assert they wrote the same number of `steps` rows, and that the attributions are
present exactly when they were staged, before anything is reported — 0.63.0's
first facade measurement was itself the defect for want of that check. Run it with:

```text
cargo test --release --lib what_step_attribution_costs -- --ignored --nocapture
```

**Machine.** Apple M1, macOS 26.5.2, release profile, 2026-09-02.

**Numbers.** Four separate runs of the pair, medians of 21 rounds each, for 40
committed steps:

| Run | steps row only | steps row + attribution |
| --- | --- | --- |
| 1 | 335.833 µs | 285.291 µs |
| 2 | 277.750 µs | 279.166 µs |
| 3 | 300.083 µs | 283.000 µs |
| 4 | 278.959 µs | 285.250 µs |

**The attributed arm is faster in three of the four runs, which is the finding.**
Not that attribution is free in some interesting sense — that the difference
between the arms is smaller than the variation between runs of the same arm, and
the *sign* of the difference changes from run to run. The cost is below what this
measurement can resolve, which for 40 steps is a few microseconds, or well under a
microsecond per step. Four runs are reported rather than one for exactly that
reason: a single pair would have read as "attribution made it faster", which is
not a claim anybody should make.

**What it does not measure.** The cost of *reading* the attribution back, which is
a join against `provider_calls` and is paid by whoever asks rather than by the
run. Nor the event: `EventKind::StepAttributed` is emitted only when an observer
is attached, and an unobserved run pays for it exactly what it pays for every
other event, which is one `Ignore::event` call.

## What proving the boundary costs a run (0.74.0)

**What is being measured.** 0.74.0 stops taking a backend's word for its own
containment: before the first step, `BoundaryProbe::measure` attempts a write and
a dial outside the boundary and the run's claims are answered from what happened.
That is process spawns at run start, so the question a reader will have is what a
run pays for the evidence.

**The shape to expect, stated before it was measured.** A fixed cost per
*boundary*, paid once and never per step. At most three short-lived children: one
uncontained control, which is what separates "the boundary refused it" from "this
host could never have done it", and then one contained child per arm. Nothing
grows with the length of the run, the number of steps or the size of the
workspace. A flat run measures once; a tree measures once before its root agent
runs and every agent under it reads that measurement, so a twenty-agent tree pays
this once rather than twenty times. A run that asked for no containment pays
nothing — it is not probed at all.

**Method.** `n5_the_startup_probe_cost` in `tests/security_probe.rs`, `#[ignore]`d
because it prints rather than asserts. It times one whole
`BoundaryProbe::measure` against a default `SandboxConfig` — the control child and
both contained arms — and prints the elapsed time, the probe's trace label and the
backend it ran under. Seven runs, median reported. Run it with:

```text
cargo nextest run --run-ignored ignored-only --success-output immediate -E 'test(n5_)'
```

**Machine.** Apple M1, 8 cores, macOS 26.5.2, `curl` 8.7.1 — the probe spawns the
host's own `curl` rather than carrying a dependency — otherwise idle, `cargo test`
dev profile.

**Numbers.** Median of 7 runs:

| What | Median |
| --- | --- |
| one `BoundaryProbe::measure`, `macos-sandbox-exec` | 63 ms |

Once per run against a run whose first provider call is measured in hundreds of
milliseconds and whose whole life is measured in minutes. The number is recorded,
not asserted: no test gates on it, and a duration asserted on a CI runner is a
flake.

**What it does not measure.** The cost on Linux or Windows, where the rungs and
the spawn are different; a host with no `curl`, which is an unmeasured probe and
therefore no spawns at all; and the second and later agents of a tree, which reuse
the measurement and pay nothing.

## What the durable assistant turn costs (0.64.0)

**What is being measured.** 0.64.0 writes one extra row per committed step — the
assistant turn, so a resumed run can send it back — inside the transaction that
already writes the step. The question a reader will have is what a run pays per
step for it.

**The shape to expect, stated before it was measured.** One `INSERT OR REPLACE`
per committed step, in a transaction that is already open, against a table whose
only index is its primary key. Constant per step, not growing with the run: the
read side is one keyed `SELECT` per *resume*, not per step. So the expected shape
is a small fixed addition to what a checkpoint already costs, and nothing that
changes as a run gets longer.

**Method.** `what_the_durable_turn_costs_per_step` in `src/state/trace.rs`,
`#[ignore]`d because it prints rather than asserts. Forty committed steps against
a fresh in-memory store per round, twenty-one rounds, medians reported. Both arms
assert they wrote the same number of `steps` rows before anything is reported —
0.63.0's first facade measurement was itself the defect because its two arms were
not doing the same work. Run it with
`cargo test --lib what_the_durable_turn_costs -- --ignored --nocapture`.

**Numbers.** M1, `cargo test` (dev profile, unoptimized), medians of 21 rounds:

| Per committed step | 40 steps |
| --- | --- |
| `steps` row and checkpoint event only | 787.459 µs |
| the same, plus the assistant turn | 1.190167 ms |

About 10 µs per step, on an in-memory store in an unoptimized build, for a step
that in a real run is dominated by a provider call measured in hundreds of
milliseconds. The number is recorded, not asserted: no test gates on it.

## What binding the host once costs (0.63.0)

**What is being measured.** 0.63.0 adds a `Harness` that binds the provider, the
store, the boundary, the approver, the observer and a template `TaskContract`,
and then calls the same free function a caller would have called themselves. The
question a reader will have is whether the convenience is paid for per step.

**The shape to expect, stated before it was measured.** Constant, and paid once
at construction. The `Harness` assembles nothing the entry points do not already
assemble; it holds five references and a contract, and `Harness::run` is a call to
`run_with_observed` with them. There is no work inside the loop for it to add, so
the difference between the two paths should be indistinguishable from the noise of
running the same scripted run twice.

**Method.** `what_the_facade_costs_per_step` in `tests/harness.rs`, `#[ignore]`d
because it prints rather than asserts. Twenty-one rounds of a four-step scripted
run against an in-memory store, one fresh store and one fresh workspace per round,
medians reported. Run it with
`cargo test --test harness -- --ignored --nocapture`.

**Numbers.** M1, `cargo test` (dev profile, unoptimized), medians of 21 rounds:

| Path | Median, 4-step run |
| --- | --- |
| `run_with(&contract, &provider, &store, &policy, &ApproveAll)` | 4.845 ms |
| `harness.run(&contract)` | 4.834 ms |

The difference is smaller than the run-to-run spread of either arm, which is the
answer the shape predicted: the second call *is* the first call, with the
arguments read from a struct instead of from the stack.

**A number that did not match the shape was a defect in the measurement, and the
first version of this one reported the harness at 10.18 ms against 4.78 ms.** The
harness arm bound a template contract carrying the crate's default cap of 12
steps and was compared against a 4-step contract — it was timing three times the
work, not three times the overhead. The two arms now assert they carry the same
cap before either is timed. A measurement whose arms are not the same run
measures nothing, and a 2× that appears where the design says "constant" is a bug
report about one of the two.

**What it does not measure.** Provider latency, which dominates a real run by
orders of magnitude and is identical on both paths by construction — the same
`Provider` value is called by the same function.

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

**0.75.0 widened the eligible set and did not re-measure the window, deliberately.**
`list_dir` and the three git readers join `grep`, `find` and `read_file`, so four
more call shapes now qualify. What the number above measures is the *window* — the
tail after the tool call, against the time the read takes — and that is a property
of the completion and the call, not of which tool it is. Widening the set changes
how often the saving is available, not how large it is on a given call, and "how
often" is a property of a workload rather than of this crate. The one thing that
would be worth its own measurement is a **git reader**, because it spawns a process
where the other six read a file, and a spawn is the larger thing to overlap. That
is left unmeasured rather than guessed at, and it is the honest gap in this
release's numbers.
