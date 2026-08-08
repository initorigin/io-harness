# Verification

Verification is how the harness decides a task is done: a typed criterion carried
on the [`TaskContract`](../../README.md), checked after every step, that ends the
run when it is satisfied.

This is the narrowest guarantee in the crate, and the most misread one. The two
sections that matter most are [What a passing gate proves](#what-a-passing-gate-proves)
and [The subject cannot defeat its own gate](#the-subject-cannot-defeat-its-own-gate);
read them before you rely on a gate anywhere downstream.

## The criteria

| Variant | Mode | What it checks |
| --- | --- | --- |
| `FileContains(String)` | single-file | The file's text contains this substring |
| `FileEquals(String)` | single-file | The file's text equals this exactly |
| `Command { argv, expect_exit }` | either | A command runs and exits with this status — in the workspace root, or in the edited file's own directory in single-file mode |
| `None` | either | No gate. The run ends on an assistant turn that calls no tool |
| `WorkspaceFileContains { file, needle }` | workspace | A named file under the workspace root contains this substring |
| `DocumentContains { file, needle }` | workspace | A document's *extracted* text contains this substring — see [Documents](documents.md) |
| `EachCompilesRust(Vec<PathBuf>)` | workspace | Every listed file compiles on its own as Rust; one wrong file fails the set |

Single-file criteria are checked with `Verification::passes`; workspace criteria
with `Verification::passes_in`. Using one in the other mode is a typed
`Error::Config`, not a silent skip.

`Verification::describe` renders the criterion into the prose the model is shown
as its success condition, so the agent is told the same thing the gate will
check.

## Content checks and their limit

Content checks (`FileContains`, `FileEquals`, `WorkspaceFileContains`) confirm
the file *says* the right thing but not that it *works*. The 0.1 live run passed
`FileContains("fn hello")` by writing the literal string `fn hello`, which does
not compile. They are cheap, deterministic and language-agnostic, and they are
the right tool when the outcome is genuinely about content — or as a checkpoint a
parent agent reads off a child (see [Agent composition](composition.md)). They
are not evidence that anything runs.

## Execution-based verification

An execution gate runs something. Since 0.18.0 there is one general way to write
it and one Rust-specific survivor:

- `Verification::Command { argv, expect_exit }` — runs the command and requires
  that exit status. The project's own runner decides, so this is the same
  criterion for `cargo test`, `npm test`, `go test ./...`, `pytest` or
  `make check`.
- `Verification::EachCompilesRust(files)` — compiles each listed Rust file on its
  own, with the probe described below. The one gate the harness still spawns
  `rustc` for itself.

```rust
use io_harness::{TaskContract, Verification};

let contract = TaskContract::workspace(
    "make `parse` reject an empty input instead of panicking",
    "/path/to/repo",
)
   .with_verification(Verification::Command {
       argv: vec!["cargo".into(), "test".into()],
       expect_exit: 0,
   });
```

`argv` is an array, program first. **There is no shell**: `;`, `&&`, `$( )` and a
backtick are bytes inside one argument rather than syntax. `argv[0]` is what the
exec [policy](permissions.md) is asked about, and verification cannot prompt, so
the spawn happens only when a rule allows it outright.

In workspace mode the command runs in the workspace root; in single-file mode it
runs in the edited file's own directory, which is the only root a single-file
contract has.

### Removed in 0.18.0

`CompilesRust`, `RustTestPasses` and `WorkspaceTestPasses` were deprecated in
0.17.0 and are **gone**. Each has a `Command` criterion that replaces it:

| Removed | Write instead |
| --- | --- |
| `CompilesRust` | `Command { argv: vec!["cargo".into(), "build".into()], expect_exit: 0 }` |
| `RustTestPasses { test_src }` | `Command { argv: vec!["cargo".into(), "test".into()], expect_exit: 0 }`, with the test in the project's own suite |
| `WorkspaceTestPasses { files, test_src }` | the same `cargo test` criterion, which runs the whole crate rather than a concatenation of files you listed |

The `test_src` string has no replacement and does not need one: put the test in
the repository, where the project's own tooling runs it, reviews it and keeps it.
A criterion that lived only in a `TaskContract` was invisible to everyone except
the run that used it.

## A criterion a model answers (0.34.0)

Every criterion above is a fact: the command exited 0, the file holds the text, the
crate compiled. That is the right default and it has one blind spot — a change that
compiles, passes the suite, and is not the change that was asked for. An agent
optimising against an exit status produces exactly that.

`Verification::Review` is the criterion for it:

```rust,no_run
use io_harness::{ModelReviewer, TaskContract, Verification};
use std::sync::Arc;

# fn demo<P: io_harness::Provider + std::fmt::Debug + Send + Sync + 'static>(reviewing: P) {
let contract = TaskContract::workspace("split the parser into two modules", "/repo")
    .with_verification(Verification::Review {
        rubric: "the split is by responsibility, and every public item kept its docs".into(),
        allow_self_review: false,
    })
    // The reviewer holds its OWN provider and its own model. That is the point.
    .with_reviewer(Arc::new(ModelReviewer::new(reviewing, "a-different-model")));
# let _ = contract; }
```

The reviewer is handed the goal, the rubric and the files the run wrote — with
their current contents — and returns `Review { passed, reasons }`. The reasons
reach the trace and every `Observer` as `EventKind::Reviewed`.

**A model may not review its own work.** With `allow_self_review: false`, a
reviewer whose model equals the model under review is refused with `Error::Config`
*before a request is built*. A model grading its own answer reports what the run
already believes; the refusal is not a warning, and it costs nothing because it
happens before the wire.

**But the refusal can only fire for a provider that names its model.** It compares
`Provider::model_hint`, which is a defaulted trait method returning `None` — the
default every out-of-tree provider gets for free, and the reason adding the method
did not break the crate's one extension point. For a provider that does not
override it the comparison has nothing to compare, so **the rule is a no-op, not a
failure**: the review runs, and it may well run on the model that wrote the change.
The three providers this crate ships override it. If you have implemented
`Provider` yourself and you want this rule to hold, override `model_hint` — nothing
will tell you that it is silently not holding.

**What a review does not prove.** It is one model's opinion of one change against
one rubric, at one moment: not deterministic, not reproducible across model
versions, and not a proof. It does not replace an execution gate — run the suite
*and* have the change read. The full bounds, including why the reviewer never sees
the run's conversation, are in [the public contract](../CONTRACT.md).

### The reviewer reads the change (0.42.0)

A rubric like *"no public item lost its doc comment"* cannot be answered from the
files as they stand, because what was deleted is not in the text that remains.
`ModelReviewer` is therefore shown each written file **before and after**: the
state before the run first touched it, from the restore point the store already
keeps, and the state now. A file the run created carries no before; a file it
never wrote is not shown at all.

Your own reviewer opts in by overriding one method:

```rust
use io_harness::{ChangeReview, Review, Reviewer, ReviewRequest, Reviewing};

impl Reviewer for NothingWasDeleted {
    fn review<'a>(&'a self, _: ReviewRequest) -> Reviewing<'a> {
        Box::pin(async { Ok(Review::failed(["this rubric needs the change"])) })
    }

    fn review_change<'a>(&'a self, request: ChangeReview) -> Reviewing<'a> {
        let shrank = request.changes.iter().any(|c| {
            c.before.as_ref().is_some_and(|b| b.len() > c.after.len())
        });
        Box::pin(async move {
            Ok(if shrank { Review::failed(["a file lost content"]) } else { Review::passed() })
        })
    }
}
```

`review_change` defaults to forwarding the `ReviewRequest` a reviewer has always
received, so nothing written before 0.42.0 needs an edit — and, stated plainly: a
reviewer that does not override it keeps judging the outcome, not the change.

## Retrying one gate, without re-running the run (0.34.0)

A review is the first criterion that can fail for a reason that is not about the
work — a 529, a dropped socket, an answer nobody could parse. Before 0.34.0 every
gate outcome was a `bool` the run discarded, so "the review said no" and "the
review never happened" were the same ending, and the only way back was to run the
whole task again.

Each evaluation now writes a `gate_attempts` row: `Passed`, `Failed`, or
`Errored`. Only the last is retryable, and `retry_gate` is how:

```rust,no_run
use io_harness::{retry_gate, GateOutcome, Store, TaskContract};

# async fn demo(contract: &TaskContract, store: &Store, run_id: i64) -> io_harness::Result<()> {
if store.last_gate_attempt(run_id)?.is_some_and(|a| a.outcome.is_retryable()) {
    // The criterion runs again. The forty steps that produced the work do not.
    let outcome = retry_gate(contract, store, run_id).await?;
    assert!(matches!(outcome, GateOutcome::Passed | GateOutcome::Failed));
}
# Ok(()) }
```

It re-runs the criterion and nothing else: no step is re-executed, no tool is
called, and the run's steps rows, token ledger and files are what the run left
them. It grades the workspace **as it now stands** rather than a snapshot from gate
time, and it refuses a gate that `Failed` — that criterion ran and answered, and
asking again over an unchanged tree is asking until the answer is convenient.

## What a passing gate proves

**A pass means the stated criterion was satisfied under the harness's compile and
run. That is all it means.**

What a pass does **not** prove:

- **Not that the artifact is correct.** A criterion checks what it checks. A gate
  asserting `hello() == 42` is silent about everything else in the file — every
  other function, every panic path, every side effect.
- **Not that the file is safe to run, ship, or merge.** Verification executes
  model-produced code to decide whether it passes; it makes no judgement about
  what that code does. Containment of that execution is the
  [sandbox](sandbox.md)'s job and the exec [policy](permissions.md)'s, not the
  gate's.
- **Not that a compile-only gate type-checked anything interesting.**
  `EachCompilesRust` proves each file is well-formed Rust whose items exist. It
  says nothing about behaviour.
- **Not a guarantee against a hostile author.** The 0.8.1 work below is a
  boundary against *the file under verification* — the artifact the model
  produced. It is not a boundary against a human adversary with other tools; a
  workspace run has file, exec and network surface that the gate is not
  responsible for.

Execution gates are **stronger** than content gates because a stub cannot compile
and run — that is why 0.2.0 introduced them — but stronger is not sufficient, and
"the gate passed" is not "the work is right".

## The subject cannot defeat its own gate

Until 0.8.1 the file under verification could change how the criterion resolved,
because the two were appended into one compilation with nothing between them.
Two working bypasses:

1. **Macro shadowing.** A file defining

   ```rust
   #[macro_export] macro_rules! assert { ($c:expr $(, $r:tt)*) => {{ let _ = &$c; }}; }
   ```

   made `assert!(false, "this gate can never pass")` — a criterion no honest file
   can meet — report a pass. An agent found this unprompted during io-cli 0.1.0's
   live runs. It is not specific to `assert`: `assert_eq!` and every other
   prelude macro were open the same way, so a fix that blocklisted one name would
   have left the class open.

2. **Criterion deletion.** A file opening with `#![cfg(any())]` strips the whole
   crate — the appended criterion with it — and a test binary that runs zero
   tests exits 0, which the gate read as a pass. The compile-only gates had the
   same hole by a different route and needed no criterion at all: `#![cfg(any())]`
   strips a file's items *before* rustc type-checks them, so
   `pub fn hello() -> u32 { "not a u32" }` compiled clean and passed.

### What stops each, now

**Shadowing is structurally impossible since 0.18.0.** It needed a criterion the
harness compiled *into the subject's crate*, and the three variants that did that
are gone. A `Command` criterion is a command: it runs in its own process, through
the project's own runner, and never shares a compilation with the file the agent
edited. Nothing in the subject can shadow a name the criterion resolves, because
the criterion resolves its names somewhere the subject is not.

Where you put the criterion now matters, and it is the same discipline any test
suite already has: keep it somewhere the agent's policy does not let it write, or
somewhere it has no reason to touch. A gate the agent can edit is not a gate,
whatever the harness does.

**Deletion** is still stopped by a probe, for `EachCompilesRust` — the one gate
the harness still compiles itself. A reserved item is appended to the subject,
the subject is compiled to an rlib on its own, and a second tiny crate is then
type-checked that references that item through `extern crate subject`. A subject
that deleted its own contents deleted the probe too, and the reference fails to
resolve.

The probe is used rather than the more obvious harness-authored root that
`include!`s the subject, because that construct rejects crate-level inner
attributes outright — which would fail an honest file opening with
`#![allow(dead_code)]` or `#![no_std]`. Legitimate inner attributes are
unaffected; only *deleting the crate's contents* is caught.

### Which phase failed

`SandboxEvent::gate_phase_failed` in the trace says where a compile gate died:

| Phase | Meaning |
| --- | --- |
| `subject-compile` | The file does not compile. The ordinary failure, unchanged since 0.8.0. |
| `subject-emptied` | The file compiled, but its items are gone — the probe could not be found. |

There were two more phases before 0.18.0 — `criterion-compile` and `test-run` —
and they belonged to the removed variants. A `Command` gate that fails records
its own exit status and the command's output instead, which is strictly more
useful: it is the runner's own diagnosis rather than the harness's guess at one.

## Governing what the gate is allowed to spawn

Verification spawns whatever the criterion names — the command's `argv[0]`, or
`rustc` for `EachCompilesRust`. Every spawn goes through `ExecGuard`, which checks
the exec [policy](permissions.md) first and records the full argv against the
run.

Verification **cannot prompt** — there is no approver on this path — so a command
is spawned only when the policy explicitly *allows* it. Anything else, including
`Effect::Ask`, is refused. A refusal is `Error::Refused`, which is
distinguishable from a verification that ran and returned `false`.

```rust
use io_harness::{ExecGuard, Policy};

// The gate may run the project's test runner and nothing else. `Policy::default()`
// allows `rustc` by name, so a `Command` gate naming anything else needs saying.
let policy = Policy::default().layer("gate").allow_exec("cargo test*");
let guard = ExecGuard::new(&policy);
```

`TEST_BINARY` still exists, so a policy written against it still compiles, but
**nothing spawns it since 0.18.0**: it was the logical name of the test binary the
removed variants built, and no criterion builds one now. Denying it changes
nothing. A compile-only gate today is `EachCompilesRust`, or a `Command` naming
the compiler rather than the test runner.

By default the compile runs inside an ephemeral [sandbox](sandbox.md).
`ExecGuard::sandboxed` supplies a different config; `ExecGuard::no_sandbox` opts
back to direct host execution.

## Reading the trace, and resuming

Every step's prompt, decision, tool call, and token usage is persisted — as are
the gate-phase rows above. Read the full trace back, and resume an interrupted
run under its original id instead of restarting:

```rust
use io_harness::{resume, Store};

// After a crash or a hit budget, continue the same run from where it stopped.
let store = Store::open("runs.db")?;
let result = resume(&contract, &provider, &store, run_id).await?;

for step in store.steps(result.run_id)? {
    println!("step {}: {} ({} tokens)", step.step, step.decision, step.tokens);
}
```

A resumed run keeps the step it reached, what it spent, how long it has been
alive, and the observation ledger it had assembled — it continues the same run
rather than re-deriving one.

If the run was started under a permission policy, plain `resume` refuses it
rather than continuing without a boundary. Use `resume_from_stored_policy`, which
reads back the policy the run actually executed under, or `resume_with` if you
are supplying it yourself:

```rust
// The policy is durable, so the resuming caller does not have to reconstruct it.
let result = io_harness::resume_from_stored_policy(
    &contract, &provider, &store, run_id, &approver,
).await?;

// Or name it explicitly.
let result = io_harness::resume_with(
    &contract, &provider, &store, run_id, &policy, &approver,
).await?;
```

`resume_from_stored_policy` fails with `Error::Resume` when the store holds no
policy for the run, rather than substituting a permissive one. See
[Durable runs](durable-runs.md) for checkpoints and approvals that survive a
restart.

Or run it live end to end: `cargo run --example edit_file`.

## See also

- [Permissions and approval](permissions.md) — the exec policy the gate's spawns pass through
- [Execution sandbox](sandbox.md) — what isolates the compile and the test binary
- [Durable runs](durable-runs.md) — checkpoints, resume, approvals across a restart
- [Documents](documents.md) — why `DocumentContains` exists and what it gates on
- [Observability and replay](observability.md) — watching gate phases as they happen
- [The contract](../CONTRACT.md) — the crate's stability and API promises
- [README](../../README.md)
