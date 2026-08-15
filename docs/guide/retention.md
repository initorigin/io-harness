# Retention: what a store holds, and deciding what it keeps

A store accumulates. Every step, every observation, every provider call, every
snapshot and every edit is a row, and through 0.57.0 nothing in this crate ever
removed one. That position has not changed and this release does not change it:
**nothing expires on its own.** There is no background job, no default retention
window, and no age at which a run becomes eligible for anything. How long an
audit record survives is not a library's decision — the trace lives in the file
you opened, and the program that opened it owns the policy.

What 0.58.0 adds is the instrument that carries the policy out. Before it, a
program that wanted to keep ninety days of history had to write its own SQL
against thirty-three tables this crate owns and versions. The schema carries
exactly one declared foreign key and never enables `PRAGMA foreign_keys`, so
there is no cascade to lean on and no error when a table is missed — the result
is a silent orphan, discovered months later as a file that will not shrink. And
the first release that moved a table broke that program without saying so.

Six methods, all on `Store`:

| Method | What it does |
| --- | --- |
| `session_size` | What one session is holding, in rows and content bytes |
| `store_size` | What the file is holding, in pages, plus a per-table breakdown |
| `delete_session` | Removes one session whole, in one transaction |
| `sweep_sessions` | The same removal for every session older than a cutoff |
| `archive_session` | Keeps every row and empties every column that holds words |
| `compact` | `VACUUM`, returning the bytes the file shrank by |

**None of them is a tool.** They are reachable by the embedding program and not
by the model. A model that could delete its own trace is a model that can hide
its work, and no acceptance criterion in this crate's history would survive
that.

## Measuring, before removing anything

There are two honest numbers here and they measure different things. Neither is
dressed up as the other.

**`session_size` reports content bytes.** It is the summed `length()` of the
session's own text and blob columns, beside the count of turns, the count of runs
in its tree, and the total rows those runs hang off. It is exact and it is
reconstructible: seed a store with known content and the number is the sum of
what you seeded.

It is *not* the pages the session occupies on disk, and that is a limit rather
than an approximation. `dbstat` is available — the bundled `libsqlite3-sys`
compiles with `-DSQLITE_ENABLE_DBSTAT_VTAB` — but it attributes a page to a
b-tree, which is to say to a *table*. A page holds rows from one table and those
rows may belong to any number of sessions. There is no arithmetic that turns that
into a per-session page count, so this crate reports the number it can compute
correctly rather than one that would look more like an answer.

```rust
use io_harness::Store;

# fn demo() -> io_harness::Result<()> {
let store = Store::open("runs.db")?;

match store.session_size(7)? {
    Some(size) => println!(
        "session {}: {} turns, {} runs, {} rows, {} content bytes",
        size.session_id, size.turns, size.runs, size.rows, size.bytes,
    ),
    None => println!("no such session"),
}
# Ok(()) }
```

`None` is a session id the store does not hold. Asking the size of nothing has no
answer, and a `Some` full of zeroes would claim it does.

**`store_size` is where the file's real page arithmetic lives.** `file_bytes` is
`page_size × page_count` and `free_bytes` is `page_size × freelist_count` — the
space already free *inside* the file. Beside them, `sessions` and `runs` count
what the store holds, and `tables` carries the per-table `SUM(pgsize)` from
`dbstat`, which is where a store that grew unexpectedly tells you which table
grew.

Read it before a prune and after one. The two readings are what make the next
section legible.

## Deleting a session

```rust
use io_harness::Store;

# fn demo(store: &Store) -> io_harness::Result<()> {
let pruned = store.delete_session(7)?;
println!(
    "{} sessions, {} turns, {} runs, {} rows, {} bytes, {} restore points",
    pruned.sessions, pruned.turns, pruned.runs, pruned.rows, pruned.bytes,
    pruned.restore_points,
);
# Ok(()) }
```

One transaction. A failure partway through leaves the store exactly as it was.

**The unit is a session, never a turn.** A turn drives a run, and that run may
have spawned children, which may have spawned their own. The run set removed is
the transitive closure of the session's turns under `runs.parent_run_id` — the
same recursive walk a tree resume already performs — so a spawned child goes with
the parent that spawned it. Removing a turn on its own would leave a subtree
reachable from no session, which is exactly the orphan state this release exists
to prevent.

Everything keyed to that run set goes with it: the steps, the events, the ledger
observations, the summaries, the provider calls, the edits, the snapshots, the
spawn records, the queue rows, the citations. Then the turns, then the session
row.

`Pruned::bytes` is the content bytes those rows held — the same figure
`session_size` reported for the session immediately before. `restore_points`
counts the snapshots that went, and it is reported rather than implied because an
undo you can no longer perform is a promise the store stopped being able to keep.

A `delete_session` for an id the store does not hold is a `Pruned` of zeroes and
not an error. Deleting nothing succeeds.

### Notes are not taken

A `memory` entry carries the run that wrote it and **outlives it**. A note is a
workspace asset — 0.56.0 made that explicit by adding a scope above the workspace,
for facts true wherever you run — so removing the session that first learned
something does not unlearn it. No `memory` row is ever removed by anything in
this page.

`memory_recalls` rows for removed runs **do** go, because they name a run that no
longer exists, and there is a consequence you should read here rather than
discover later. Since 0.56.0 eviction ranks candidates by `COUNT(DISTINCT
run_id)` over that table: the entry the fewest separate runs have carried is the
one dropped at a cap. Pruning a session therefore **lowers the standing of every
note that session had drawn on**, and a note that had earned its place mostly
through runs you have now removed can be evicted by a later write that would
previously have evicted something else.

That is the honest reading — evidence from a run that no longer exists is not
evidence — and it is the behaviour, not a defect. `Store::memory_pin` is what
holds a note regardless.

### What is out of reach

A run reachable from **no** session is not removed by anything here, because
nothing names it. A run started by `run` or `run_with` rather than by a session
turn is such a run. Removing one is not in this release.

## Sweeping to a date

```rust
use io_harness::Store;

# fn demo(store: &Store) -> io_harness::Result<()> {
let pruned = store.sweep_sessions("2026-05-17")?;
println!("{} sessions removed, {} refused", pruned.sessions, pruned.refused.len());
# Ok(()) }
```

`sweep_sessions` applies `delete_session`'s removal to every session whose
`created_at` is **strictly before** the cutoff, in one transaction. A session
whose `created_at` equals the cutoff survives.

**The cutoff is a string, and the comparison is a string comparison**, because
that is what the storage actually does. `sessions.created_at` is a text column
written by `strftime('%Y-%m-%dT%H:%M:%fZ','now')`, so a value looks like
`2026-05-17T09:41:12.318Z`. That shape sorts lexicographically in the same order
it sorts chronologically, which is what makes a plain `<` correct — and it means
a date alone is a usable cutoff: every timestamp recorded on 2026-05-17 begins
with `2026-05-17T`, which is greater than `2026-05-17`, so the bare date means
"before midnight UTC beginning that day". Pass the full timestamp when you want
the boundary finer than a day.

A duration would have been friendlier and would have needed a clock inside the
store, which is the one thing this crate's tests are forbidden from gating on.
The caller has a clock; the store has a column.

### A resumable run is never swept

A session holding any run whose status is `Running` or `Paused` is **refused**.
It is not deleted, not partly deleted and not reported as an error: its id is in
`Pruned::refused` and the session is left byte-identical.

The refusal exists because a date is a policy applied to sessions nobody looked
at. A crash-resumable tree that vanished for being old is the worst thing this
release could ship — the run is *still resumable*, that is what the status says,
and the whole point of a durable checkpoint is that the process that comes back
days later finds it.

`delete_session` has **no** such refusal, and the asymmetry is deliberate.
Naming one session id is somebody's decision about that session. A cutoff is a
rule about sessions nobody has considered individually. Removing a refused
session is done through the call that takes one id, which is what makes it a
decision somebody made rather than a policy that ran.

`refused` carries ids and no reasons. There is exactly one reason a sweep
refuses, it is stated in the method's documentation, and a reason string on a
single-cause refusal is a field that is wrong the first time a second cause
appears.

## Worked: measure, sweep to ninety days, read the refusals, compact

```rust
use io_harness::Store;

# fn demo(ninety_days_ago: &str) -> io_harness::Result<()> {
let store = Store::open("runs.db")?;

// 1. What the file holds before anything is removed.
let before = store.store_size()?;
println!(
    "{} bytes on disk, {} of them already free — {} sessions, {} runs",
    before.file_bytes, before.free_bytes, before.sessions, before.runs,
);
for (table, bytes) in &before.tables {
    println!("  {table}: {bytes} bytes of pages");
}

// 2. Ninety days ago, in the shape `sessions.created_at` is written in:
//    `2026-05-17`, or `2026-05-17T09:14:22.031Z` if the cutoff is a moment
//    rather than a day. This crate takes no date library and has no opinion
//    about which one the calling program uses — the comparison is on the
//    string, so any clock that prints an ISO-8601 timestamp will do, and a
//    bare date is a valid cutoff because it sorts before every time on it.
let cutoff = ninety_days_ago;

// 3. Sweep. One transaction, whatever the sweep's size.
let pruned = store.sweep_sessions(cutoff)?;
println!(
    "before {cutoff}: removed {} sessions, {} turns, {} runs, {} rows, \
     {} content bytes, {} restore points",
    pruned.sessions, pruned.turns, pruned.runs, pruned.rows, pruned.bytes,
    pruned.restore_points,
);

// 4. What it would not take. Each of these holds a Running or Paused run.
//    They are still here and still resumable; deciding about them is a
//    `delete_session` call naming the id.
for session_id in &pruned.refused {
    match store.session_size(*session_id)? {
        Some(size) => println!(
            "kept {session_id}: {} turns, {} runs, {} bytes — has a resumable run",
            size.turns, size.runs, size.bytes,
        ),
        None => println!("kept {session_id}"),
    }
}

// 5. The prune freed pages into the file, not out of it. Read that, then
//    return them to the filesystem.
let freed = store.store_size()?;
println!("still {} bytes on disk, {} now free inside it", freed.file_bytes, freed.free_bytes);

let reclaimed = store.compact()?;
let after = store.store_size()?;
println!("compaction returned {reclaimed} bytes; the file is now {}", after.file_bytes);
# Ok(()) }
```

## Archiving: every row stays, every word goes

`archive_session` keeps the session and empties it of language.

```rust
use io_harness::Store;

# fn demo(store: &Store) -> io_harness::Result<()> {
let archived = store.archive_session(7)?;
println!(
    "{} turns, {} rows, {} bytes of text cleared",
    archived.turns, archived.rows, archived.bytes,
);
# Ok(()) }
```

What survives: the rows themselves, and every column that carries a count, a
timing, a token figure, a cost, a path, a verdict, a status or a kind. The
session still answers what it cost, how long it took, which files it touched and
how many lines it changed, and `provider_calls` still totals the same tokens.

What goes: the prompts, the replies, the tool results, the summaries, the
snapshot contents, the edit hunks — the user's words and the model's.

**It is not just the conversation table, and it must not be.** `provider_calls`
is the only pure-accounting table in this schema. The user's own words are in
`steps.prompt`; every tool's output is in `ledger_observations.text`; whole file
contents are in `snapshots.before`; the change itself is in `edits.hunk`. An
archive that emptied only `session_turns` would leave the transcript sitting in
four other tables while reporting a removal it had not performed — which, for an
operator running this to satisfy a privacy obligation, is the worst possible
failure, because it is silent and it is reported as a success. Every text-bearing
column across the session's runs is emptied, and the test that guards it sweeps
every `TEXT` and `BLOB` column named by `PRAGMA table_info` for a phrase seeded
into six different tables, rather than checking the tables somebody remembered.

**This is what lets an audit obligation and a privacy obligation be satisfied at
once.** They pull in opposite directions on the same rows, and until this release
the only instrument that satisfied either was deleting the file, which satisfies
exactly one of them.

Archiving is idempotent: archiving an already-archived session clears zero rows
and zero bytes and says so.

It does **not** refuse a session holding a resumable run, and the asymmetry with
the sweep is deliberate. The sweep's refusal exists because a date is applied to
sessions nobody looked at; archiving names one session. A resumable run whose
words are gone can still be resumed — it loses its transcript, which is what the
caller asked for.

### An archived restore point says so rather than restoring nothing

A snapshot's content is words, so archiving empties it. The row stays and is
marked as archived in `snapshots.state`, and a restore path reaching such a row
reports the existing `Reverted::Stale` with a reason naming the archive.

This is what makes archiving safe to offer at all. An emptied snapshot restored
naively writes an empty file over a real one, which is the single way this
release could destroy something *outside* the database. The file is left
byte-identical and the caller is told why.

## Compaction

SQLite frees pages **into** the file, not out of it. A deletion of ten thousand
rows leaves the file exactly the size it was and raises
`store_size().free_bytes`. An operator who prunes and watches the file stay the
same size concludes the prune did not work, so the reclamation is here rather
than left as a note.

```rust
use io_harness::Store;

# fn demo(store: &Store) -> io_harness::Result<()> {
let reclaimed = store.compact()?;   // bytes the file shrank by
# Ok(()) }
```

`compact` runs `VACUUM` and returns the difference between the file's size before
and after. Three things about it, all of which decide *when* you call it:

- **It rewrites the whole database.** Cost is proportional to what is left, not
  to what was removed.
- **It needs free disk space of roughly the file's own size while it runs**,
  because the rewrite is a second copy. `docs/MEASUREMENTS.md` carries the
  measured time and peak extra space on a named machine. A `VACUUM` that fails
  leaves the original file intact — that is SQLite's own guarantee, not a claim
  this crate adds on top.
- **It cannot run inside a transaction**, which is SQLite's rule, so it is its
  own call and cannot be folded into a prune.

**`PRAGMA incremental_vacuum` is not an option here.** It requires the database
to have been created with `auto_vacuum` enabled, and every store this crate has
ever created was created without it. On any existing file the pragma does
nothing. Turning `auto_vacuum` on for *new* stores would split the crate's
behaviour between old files and new ones and would still do nothing for the
stores an operator already has — which are the ones with the problem.

## The limits, stated plainly

- **A deletion cannot be undone by this crate.** There is no trash, no tombstone
  and no recovery path. An operator whose recovery position matters copies the
  file first; that is the whole of the answer, and it is the reason the sweep
  refuses a resumable session and the reason every call reports what it did.
- **This instrument removes what is in the database.** It can say nothing about
  the operator's own logs, their provider account, or their filesystem. A prompt
  your program logged to stdout, a request body the vendor retains, and a file
  the run wrote into the workspace are all outside it and stay where they are.
- **Nothing expires on its own.** Every call above must be made by name. A
  program upgraded to 0.58.0 and left alone behaves exactly as it did on 0.57.0.
- **No model can call any of it.** The retention surface is the embedding
  program's, not the agent's.
- **A session's size is content bytes, not pages**, for the reason given above;
  the file's own size is `store_size`'s.
- **The unit is a session.** There is no removal of a single run, a single turn
  or a named string, and archiving is not redaction — it takes a whole session.
- **A run reachable from no session is not reached.**
- **A deletion's cost is linear in the rows removed and in the number of
  tables**, with a full scan of each table carrying no `run_id` index, which is
  most of them. The measured numbers are in `docs/MEASUREMENTS.md` with the
  method and the machine; none of them is a gate.

## See also

- [Sessions](sessions.md) — the unit everything on this page operates on
- [Context and memory](context-and-memory.md) — the eviction ranking a prune
  shifts, and the pin that holds a note through it
- [Durable runs](durable-runs.md) — restore points, and the `Reverted::Stale` an
  archived snapshot reports
- [Accounting](accounting.md) — the rows an archive deliberately keeps
- [Observability and replay](observability.md) — what a run recorded, before you
  decide how long to keep it
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
