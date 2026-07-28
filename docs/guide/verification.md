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
| `CompilesRust` | single-file | The file compiles as a Rust library, and its items survived to be type-checked |
| `RustTestPasses { test_src }` | single-file | The file compiles, and `test_src` compiles against it and passes |
| `WorkspaceFileContains { file, needle }` | workspace | A named file under the workspace root contains this substring |
| `DocumentContains { file, needle }` | workspace | A document's *extracted* text contains this substring — see [Documents](documents.md) |
| `EachCompilesRust(Vec<PathBuf>)` | workspace | Every listed file compiles on its own; one wrong file fails the set |
| `WorkspaceTestPasses { files, test_src }` | workspace | All listed files, concatenated in order, compile and pass `test_src` together |

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

Execution gates run the artifact:

- `Verification::CompilesRust` — passes only if the file compiles
  (`rustc --crate-type lib`) **and** its items survived to be type-checked.
- `Verification::RustTestPasses { test_src }` — compiles `test_src` against the
  file and passes only if the test binary passes.

```rust
use io_harness::{TaskContract, Verification};

let contract = TaskContract::new(
    "write a hello function returning 42",
    "hello.rs",
    Verification::RustTestPasses {
        test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
    },
);
```

Compilation runs locally in a throwaway temp dir (removed afterwards) and touches
no network.

`test_src` calls the file's items **unqualified**, and reaches the file's
**private** items — an implementation does not have to be `pub` to pass. A
subject writing an idiomatic `fn hello() -> u32 { 42 }` with no `pub` passes the
same gate a `pub fn hello` passes.

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
  `CompilesRust` proves the file is well-formed Rust whose items exist. It says
  nothing about behaviour.
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

**Shadowing** is stopped by where the criterion sits. The subject and the
criterion are still **one crate** — that is what lets a criterion call a private
`fn hello`, and making them two crates broke exactly that during 0.8.1's
development, because privacy is a wall between crates. Instead the criterion is
wrapped in a child module of the subject's own crate that re-imports the prelude
macros *explicitly* and then does `use super::*`. A subject defining
`macro_rules! assert` — exported or not — now makes the name ambiguous (rustc
E0659) rather than capturing it, so the gate **fails to compile instead of
passing**. A macro the subject exports under any *other* name still reaches the
criterion through the glob, which is what keeps this a fix rather than a
restriction.

**Deletion** is stopped by a probe. A reserved item is appended to the subject,
the subject is compiled to an rlib on its own, and a second tiny crate is then
type-checked that references that item through `extern crate subject`. A subject
that deleted its own contents deleted the probe too, and the reference fails to
resolve. This runs on both paths — compile-only and test — so
`CompilesRust`, `EachCompilesRust`, `RustTestPasses` and `WorkspaceTestPasses`
all confirm the file's items survived.

The probe is used rather than the more obvious harness-authored root that
`include!`s the subject, because that construct rejects crate-level inner
attributes outright — which would fail an honest file opening with
`#![allow(dead_code)]` or `#![no_std]`. Legitimate inner attributes are
unaffected; only *deleting the crate's contents* is caught.

Nothing changed in what you write. `test_src` is the same source a 0.8.0 caller
wrote, it still calls the file's items unqualified, a macro the file legitimately
exports still reaches it, and a private implementation still passes.

### If a run stopped passing at 0.8.1

The gate was being defeated. `SandboxEvent::gate_phase_failed` in the trace says
which phase failed:

| Phase | Meaning |
| --- | --- |
| `subject-compile` | The file does not compile. The ordinary failure, unchanged since 0.8.0. |
| `subject-emptied` | The file compiled, but its items are gone — the probe could not be found. |
| `criterion-compile` | The file compiles, but the criterion will not compile beside it. A subject shadowing `assert!` lands here, as an E0659 ambiguity. |
| `test-run` | Everything compiled and the test binary ran and failed. The honest failure. |

`criterion-compile` and `subject-emptied` are the two shapes a pre-0.8.1 bypass
takes. `test-run` is a test that ran and said no.

## Governing what the gate is allowed to spawn

Verification spawns `rustc` and then the test binary it built. Both go through
`ExecGuard`, which checks the exec [policy](permissions.md) before every spawn
and records the full argv against the run.

Verification **cannot prompt** — there is no approver on this path — so a command
is spawned only when the policy explicitly *allows* it. Anything else, including
`Effect::Ask`, is refused. A refusal is `Error::Refused`, which is
distinguishable from a verification that ran and returned `false`.

```rust
use io_harness::{ExecGuard, Policy, Verification, TEST_BINARY};

// Compile-only verification: the produced code is type-checked but never run.
let policy = Policy::default().layer("no-exec").deny_exec(TEST_BINARY);
let guard = ExecGuard::new(&policy);
```

`TEST_BINARY` is the logical name of the binary verification builds. Denying it
while allowing `rustc` gives compile-only verification: `RustTestPasses` type-
checks the criterion against the subject and then refuses to execute it.

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
