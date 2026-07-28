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
  already-completed run is *not* refused: it resumes as a no-op.

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

Restored: the run id, the step it reached, its token and wall-clock budgets, and
the observation ledger it had assembled — so the resumed run asks the model what
the interrupted one would have. A resumed tree also restores the shared spend
ledger from the tree's durable totals, so it draws against one continuous
ceiling rather than a reset one, and an adopted child is not counted against the
agent cap a second time.

Not restored: a permission policy the store does not hold, which the crate
refuses to guess at; and a sandbox, which is ephemeral by design.

The tree's wall clock is measured from the **root's** start time, not from
whichever agent notices, so a child spawned twenty hours into a run does not get
a fresh 24 hours.

Run it live: `cargo run --example durable_run` (kills itself mid-run and resumes).

## See also

- [Permissions and approval](permissions.md) — the boundary a resume must not drop
- [Agent composition](composition.md) — the tree a `resume_tree` reconstructs
- [Execution sandbox](sandbox.md) — why an in-flight exec is re-run, not resumed
- [Observability and replay](observability.md) — the checkpoint events in the trace
- [Resilience](resilience.md) — surviving a provider failure without a restart
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
