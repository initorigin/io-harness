# Durable, unattended runs

Start a long task and walk away: a run survives a crash or a full process
restart, so the harness can run unattended for a long horizon (24h+) with no
user input and pick up exactly where it stopped.

- **Checkpoint after every step, transactionally** — each completed step's trace
  row, its budget draw, and a checkpoint marker are committed in one rusqlite
  transaction. The committed checkpoint *is* the step's completion marker: a crash
  mid-commit leaves either a whole step or none of it, never a torn half.
- **Resume the whole tree** — `resume` (or `resume_with`, for a run started under
  a policy) continues a single or workspace run;
  `resume_tree` reconstructs a crashed tree and continues **every** agent from
  its own last committed step. A parent *adopts* the children it had already
  spawned and resumes each from its checkpoint, rather than duplicating or
  restarting them.
- **Idempotent by construction** — a completed step is skipped (recorded as a
  `skipped` event), the aggregate `Ledger` budget is restored from durable totals
  (never reset, never double-charged), the time budget counts real wall-clock
  elapsed across the downtime, an already-applied edit is re-observed rather than
  repeated, and re-running a resume is a no-op.
- **Approval survives a restart** — a sensitive action that pauses the tree
  outlives the process; a fresh process delivers the decision with
  `resume_tree_with_decision` and the tree continues.
- **Sandboxes are re-created, never resumed** — an ephemeral sandbox is never
  checkpointed, so an exec in flight at crash time simply re-runs in a fresh
  sandbox; a committed sandboxed step is skipped.
- **Typed failure** — a resume against a newer-format or missing checkpoint is an
  `Error::Resume`, not a panic or a half-resume. `check_resumable` refuses a
  store written by a newer checkpoint format rather than misreading a layout it
  does not understand, and refuses a run id that is not in the store. An
  already-completed run is *not* refused: it resumes as a no-op. A run another
  live process is already driving is refused as `Error::Conflict` (0.62.0), a
  separate type because it answers a separate question — the checkpoint *can* be
  continued, by somebody else, right now — and it names the holder and when its
  lease lapses. See [one driver per run](#one-driver-per-run-0620).

The 24h horizon is proven by a real `kill -9`-then-resume test plus a time-scaled
long unattended run; a literal 24h wall-clock run is noted, not gated on.

## Which resume to call

Plain `resume` is for a run that had **no** permission boundary. It drives the
loop permissively, and this is not a preference — a run that *was* started under
a non-permissive policy is **refused** with `Error::Resume` rather than resumed
without it. Through 0.12.0 `resume` substituted a permissive policy for every
workspace run it resumed, so a caller who ran under a deny-by-default policy and
crashed came back with no boundary and nothing said so. Refusing is the only
behaviour that cannot silently widen what an agent may do. A run with no
recorded policy at all — which is every run checkpointed before 0.13.0 — resumes
exactly as it did then.

That leaves the caller needing a policy to hand, which a process coming up after
a crash somewhere else may not have. The policy is recorded against the run, so
there are entry points that read it back:

| Function | Boundary comes from |
| --- | --- |
| `resume` | none; refuses a run that had one |
| `resume_with` | the policy you pass, which is recorded as the one the resumed run executed under |
| `resume_from_stored_policy` | the store |
| `resume_tree` | the policy you pass |
| `resume_tree_from_stored_policy` | the store |

The stored-policy forms fail with `Error::Resume` when the store holds no policy
for the run, rather than substituting a permissive one — that substitution is
the exact defect the recorded policy exists to close. It is sharper for a tree,
where every child inherits the root's policy through `Policy::contain`: a
guessed-at root boundary is guessed at for the whole tree, which may already
have taken an irreversible action under the real one.

Passing `Policy::permissive()` to `resume_with` deliberately downgrades a run
that had a boundary. That is a caller's decision to make explicitly, and it is
exactly what `resume` will not do on the caller's behalf.

Every one of these has an `_observed` twin taking an `Observer` last; see
[observability and replay](observability.md).

## What is restored, and what is not

Restored: the run id, the step it reached, its token and wall-clock budgets, the
observation ledger it had assembled, and — since 0.64.0 — the assistant turns
that go with it: what the model wrote each step and the calls it made. Those two
are the two halves of one request, so the resumed run does not merely ask the
model what the interrupted one would have asked, it asks it the same way: the
same roles, the same assistant turns, the same result batches. Before 0.64.0 the
results were restored and the calls they answered were not, so everything before
the crash arrived as one block of user prose — the model read a third-person
account of its own actions, on exactly the runs that had already cost the
operator a crash. A run recorded before 0.64.0 has no turns stored and still
resumes that way. A resumed tree also restores the shared spend
ledger from the tree's durable totals, so it draws against one continuous
ceiling rather than a reset one, and an adopted child is not counted against the
agent cap a second time.

Not restored: a permission policy the store does not hold, which the crate
refuses to guess at; and a sandbox, which is ephemeral by design.

The tree's wall clock is measured from the **root's** start time, not from
whichever agent notices, so a child spawned twenty hours into a run does not get
a fresh 24 hours.

Run it live: `cargo run --example durable_run` (kills itself mid-run and resumes).

## One driver per run (0.62.0)

A run is driven under a **lease**. Every `run_*` and `resume_*` takes one on the
run it is about to drive and gives it back on the way out, whichever way it
leaves — an ordinary finish, an early return, an error. Nothing in your code
changes: no entry point grew a parameter.

What changes is what a *second* process gets. While the first driver holds the
run, a second `resume` is refused before it drives anything:

```rust,ignore
match resume(&contract, &provider, &store, run_id).await {
    Err(Error::Conflict { owner, expires_at, .. }) => {
        // Somebody else is driving it. Back off, or wait until `expires_at`
        // and take it over — the decision is yours, and you can make it
        // without parsing a message.
        eprintln!("held by {owner} until {expires_at}");
    }
    other => { other?; }
}
```

Before this release both processes proceeded. Their steps interleaved into a
single trace under one run id, each numbered from its own in-memory counter, and
the result read as a coherent run that neither process had performed — no error,
no event, and nothing in the store afterwards that told it from a real trace.

**A crash is not a lock.** An acquire is refused only while the lease is held by
another owner, has *not* lapsed, and that owner's process is still running. So
`kill -9` a driver and its run is takeable at once, not half an hour later — on
Windows as well as on unix. The platform is asked directly about the pid in the
owner id: `kill(pid, 0)` on unix, `OpenProcess` plus `GetExitCodeProcess` on
Windows, neither of them a dependency this crate did not already have. A process
that is there but somebody else's — `EPERM` on unix, a handle refused for lack of
rights on Windows — counts as alive. The liveness check errs towards "alive": an
owner id with no readable pid, a platform that is neither unix nor Windows, and a
Windows process that exited with code 259, which cannot be told from a running one
because 259 is `STILL_ACTIVE`, all report the owner as running, and there the ttl
governs. That is the safe direction — a dead
owner believed alive costs a wait, while a live owner believed dead would hand
its run to a second driver. Either way the takeover raises the generation by one,
and the previous owner is refused at its next durable commit, writing neither a
step row nor a checkpoint event.

Set the ttl with `TaskContract::with_lease_ttl`. The default is
`DEFAULT_LEASE_TTL`, twice `DEFAULT_EXEC_TIMEOUT`: the lease is renewed by every
step commit, so what it has to outlast is one step — a completion plus at most one
tool execution — and not a whole run. Shorten it if your steps are small and you
want a run recoverable sooner in the cases liveness cannot answer.

Ask who holds a run with `Store::run_lease`. `runs.status = 'running'` has never
distinguished a live process from a crashed one and still does not; the lease row
is the answer to that question.

**The bound: one machine.** The owner id is process-scoped and the lease is a row
in a SQLite file. Two hosts sharing that file over a network filesystem is outside
what SQLite itself promises, and this release does not claim it.

**A session head is not leased.** A conversation is something many processes may
legitimately read and branch, so locking one would forbid the `branch_from` this
crate ships. Instead the head advances by compare-and-swap: a turn taken against a
head that has since moved gets `Error::Conflict` back, and its turn row is left
intact to be read or rebased. That reports a dropped turn rather than making both
turns land — which it cannot, because a conversation has one head by definition.

## Putting a file back (0.28.0)

A resume restores the run. `rewind` undoes it.

```rust
use io_harness::{rewind, Rewind};

match rewind(&workspace, &store, run_id, "src/lib.rs")? {
    Rewind::Restored(_)      => println!("put back"),
    Rewind::Removed          => println!("the run created it; it is gone"),
    Rewind::NotKept(why)     => println!("the run changed it and we cannot undo that: {why}"),
    Rewind::NotRecorded      => println!("this run never wrote it"),
}
```

Every `write_file` and `edit_file` already read the file's previous contents to
measure the edit. Since 0.28.0 the **first** one it sees per path is kept, as a row
in the store — so the restore point is the state of the file before the run first
touched it, and it survives a crash, a restart and a resume exactly the way the
step count does. A run that edited one file five times rewinds to where it started,
which is the only definition under which "undo what this run did" is one operation
rather than five.

It is a new table, so `CHECKPOINT_FORMAT` stays 7 and a store written by 0.27.0
opens, resumes and replays unchanged. It simply has no snapshot rows, and `rewind`
on a run from before this release is `NotRecorded` — the honest answer, and the
reason that is a variant rather than an empty restore.

Restoring writes through `Workspace::write_file`, so an undo cannot put bytes
anywhere the run could not have written them. Removing is stricter: it refuses
anything that is not an outright allow, because a write is content a human can
inspect afterwards and a delete is not.

**What one restore point per file per run does not offer.** There is no redo. A
previous file over 1 MiB or one that is not valid UTF-8 is `NotKept` — reported,
and never guessed at, and never truncated. Only `write_file`, `edit_file` and
`patch_file` take a snapshot: a file changed by `shell`, `exec` or a git built-in
has no restore point and answers `NotRecorded`. And `rewind` takes one path per
call; `rewind_run` below is the whole-run form. Per-step undo was in this list
until 0.51.0 and is now `rewind_step`, further down.

## Putting a whole run back (0.36.0)

`rewind` answers "undo this edit". `rewind_run` answers "undo this run", which
is not the same question. A run that wrote three files, recorded two decisions
in memory and queued four children leaves three of those five effects in place
after you have restored every file — and the two that remain are the ones that
change what the *next* run does. Memory is read into context, so a wrong fact a
rewound run learned outlives the files it was learned from; a queued backlog is
adopted on resume, so work you undid is re-admitted. A partial undo is worse
than none, because it looks complete.

```rust
use io_harness::rewind_run;

let done = rewind_run(&workspace, &store, run_id)?;
for (path, verdict) in &done.files {
    println!("{path}: {verdict:?}");   // the same four verdicts as `rewind`
}
println!("{} notes put back, {} removed", done.memory_restored.len(), done.memory_removed.len());
println!("{} queued children dropped", done.queue_cleared.len());
# Ok::<(), io_harness::Error>(())
```

Each memory entry goes back to the value that was there before this run's
**first** write to that key — the same definition, and the same first-write
guard, that files have had since 0.28.0. An entry the run created is removed,
because "the way it was" for an entry that did not exist is not existing.

**Nothing in the trace is deleted.** The steps, the event stream, the spawn
records and the ledger are untouched: the spend happened, and an undo that
erased the rows would make the ledger disagree with the invoice and make "this
agent has tried this three times" unanswerable. What the rewind took is written
down before it goes, and `Store::rewinds` reads it back — so the work and its
undoing are both answerable long afterwards. `EventKind::Rewound` reports the
same three numbers to an observer.

**What it does not undo, stated rather than implied.** A commit the run made is
still there; `git reset` is unreachable from this crate by construction, and
undoing history is a decision about a branch rather than about a run. A push is
not recalled, a migration is not reversed, a provider call is not un-billed, and
a worktree is never removed. It is one run and not a tree — a caller wanting a
subtree loops over it, which is honest about what "a rewind" means rather than
inventing an ordering over children whose files may overlap.


## Undoing one step (0.51.0)

`rewind` undoes a file and `rewind_run` undoes a run. Neither can undo step
eighteen of twenty, because the restore point is the state of a file before the
run *first* wrote it. Since 0.51.0 every write also records the change itself, as
a unified diff of the whole file, and `rewind_step` reverse-applies one step's:

```rust
use io_harness::{rewind_step, Reverted};

for step in (1..=last).rev() {
    for (path, what) in rewind_step(&workspace, &store, run_id, step)? {
        match what {
            Reverted::Applied(_) => println!("{step} {path}: put back"),
            Reverted::Stale(why) => println!("{step} {path}: not touched — {why}"),
            Reverted::NoHunk(why) => println!("{step} {path}: nothing to undo with — {why}"),
        }
    }
}
```

**Walk backwards.** Reverse-application is order-sensitive: a step reverted while
a later step's change still sits on top of it finds context that has moved, and
the answer is `Stale` and an untouched file — never a fuzzy match that quietly
corrupts it. Reverting the newest step first is what makes each one fit.

`NoHunk` is the other way nothing happens, and it is a different fact: the row
predates 0.51.0, or the file's previous contents were not kept, so there is
nothing to undo with and reverting the later steps first will not change that.
`rewind` is what puts such a file back.

This does not replace `rewind`. A snapshot is a stronger restore than a chain of
reverse-applies, and a run whose hunks are absent must still be fully undoable.
And it undoes files only — memory and the queue are `rewind_run`'s, because a
step did not create them.

The revert is in the trace like everything else: `Store::rewinds` reports it with
`undid_step` set, which is what distinguishes it from a whole-run rewind, and
`rewind_step_observed` emits one `EventKind::Reverted` carrying how many paths
were actually put back.

## Reading a run's change as a patch (0.51.0)

The same hunks answer a question that has nothing to do with undo. `Store::patch`
renders a run's whole change as a step-ordered patch series — one
`--- a/path` / `+++ b/path` header pair per edit, in the order the run made them —
so an unattended run can be reviewed as a diff rather than as a list of line
counts.

It is a series, not one diff. Two edits to the same file take their line numbers
from that file as it stood at each of them, so it applies as a sequence the way a
multi-commit diff does; joining the hunks under one pair of headers would look
like a patch and would not apply.

## See also

- [Permissions and approval](permissions.md) — the boundary a resume must not drop
- [Agent composition](composition.md) — the tree a `resume_tree` reconstructs
- [Execution sandbox](sandbox.md) — why an in-flight exec is re-run, not resumed
- [Observability and replay](observability.md) — the checkpoint events in the trace
- [Resilience](resilience.md) — surviving a provider failure without a restart
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
