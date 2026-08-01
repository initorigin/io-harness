# Hooks — reacting to a run from `io.toml`

An application has been able to watch a run since 0.12.0 by implementing
[`Observer`](observability.md): one method, one enum, and a `Flow` that can stop
the run. That is the right shape for a Rust program and the wrong shape for an
operator, who has a config file and a shell script and no reason to own a
compiler.

A `[[hook]]` table is the same capability reached from the file. Name the events
you want, name a path to append them to or an argv to run, and the audit log, the
notification, the formatter and the local policy check stop being code.

```toml
# io.local.toml — gitignored, and yours. A project file may not declare a hook.
[[hook]]
append = "audit.jsonl"
```

That is a complete hook: no `on`, so every event, one JSON line each. Nothing
about the run loop changed to make it work. `Config::hooks()` returns a `Hooks`,
which *is* an `Observer`, and the caller installs it exactly as it installs its
own.

## The keys

| Key | What it takes | Default |
| --- | --- | --- |
| `on` | the event names this hook wants | absent means **every** event |
| `append` | a path to append one JSON line per matching event to | — |
| `run` | an **argv array** to spawn with the event JSON on its stdin | — |
| `on_failure` | `"continue"` or `"cancel"` | `"continue"` |
| `timeout_ms` | the wall-clock ceiling on `run` | `5000` |

**Exactly one of `append` and `run`.** A table with neither and a table with both
are each an error naming the table by index — `key hook[2]` — because "the second
one" is the only way an operator finds a table in an array of them. A `run`
that names no program at all is refused the same way.

A key the table does not know is an error too, so a misspelled `timout_ms` is
reported rather than accepted and quietly ignored. `[[hook]]` differs from
`[[mcp]]` here, which cannot reject an unknown key at all.

Paths are relative to the **discovery root** — the directory `Config::discover`
was given — and not to the file that wrote the table. An operator who writes
`append = "audit.jsonl"` in their user-scope file means the project they are
pointing the harness at, not their own home directory. A `run` hook is spawned
with that same directory as its working directory.

There is **no shell** anywhere. `run` is a TOML array and stays an array, which is
the discipline `${cmd:}` and the `exec` tool already hold: a `;`, a `|` or a
backtick in an argument is an argument. A hook has no metacharacter surface beyond
its own argv.

## Four hooks, worked

### An audit log

```toml
[[hook]]
on = ["refused", "approval_requested", "approval_decided", "spawned"]
append = "audit.jsonl"
```

One JSON line per matching event, in the shape `RunEvent` has serialised since
0.12.0 — flat, tagged on `event`, and documented in
[Observability](observability.md#forwarding-events-to-another-process). No format
was invented for this.

The file is created, empty, when the hooks are built — before any event exists. So
an **empty file means the filter matched nothing** and **no file means the hook
was never installed**, and an operator debugging a silent hook can tell those two
apart by looking. (A path that cannot be created is warned about at build time and
left to the first append to report properly.)

### A notification

```toml
[[hook]]
on = ["stalled", "finished"]
run = ["/usr/local/bin/notify", "--channel", "ops"]
timeout_ms = 2000
```

The event JSON arrives on the child's stdin, one line, followed by EOF. An event
is small enough that it cannot fill the pipe, so a child that never reads its
stdin is not a deadlock — it is just a child that ignored what it was given.

The child's `stdout` and `stderr` go to `null`. A library must not write to its
caller's terminal, so a hook that has something to say says it by **exiting
non-zero**. A non-zero exit, a program that could not be spawned, and a child
still running at `timeout_ms` (which is killed) are the three ways a `run` hook
fails.

### A formatter

```toml
[[hook]]
on = ["step"]
run = ["cargo", "fmt"]
```

A formatter-on-write is a hook, and that is deliberate rather than incidental. The
obvious place to put one is beside 0.25.0's diagnostics pass, which already runs
after a successful `edit_file` or `write_file` — and it does not belong there,
because that pass **reads** and a formatter **writes**.

Two things break if a writer runs at the edit site. The observation the model is
about to read says `[wrote src/x.rs] (412 chars)`, and after a reformat those are
no longer the bytes on disk — the model would be reading a report of a file that
has since changed under it. And the write has already been classified:
`Wrote::moved_the_workspace` (`src/tools/workspace.rs:53`) is computed by comparing
the bytes written against the bytes that were there, and it is the signal 0.11.0's
stall detection rests on. A formatter running after that comparison cannot affect
it; a formatter running before it would make every reformat look like progress.

At the `step` boundary both of those are already settled: the step is committed
and `moved_the_workspace` has been decided, so no reformat can be mistaken for
progress. That is where a formatter belongs, and the file is how you put it there.

### A local policy check that stops the run

```toml
[[hook]]
on = ["tool_call"]
run = ["./scripts/check-tool-call"]
on_failure = "cancel"
```

`on_failure = "cancel"` is the whole of "a local policy check". The mechanism
already existed — an `Observer` returning `Flow::Cancel` has been the supported
way to stop a run from outside since 0.12.0 — and this is the key that reaches it
from the file. A script that exits non-zero ends the run.

Cancellation is honoured at the **next step boundary**, not immediately: the run
finishes the step it is on, records `cancelled`, and returns
`RunOutcome::Cancelled { steps }`, resumable like any other ending. A hook that
refuses a `tool_call` does not un-call the tool. It ends the run that called it.

`on_failure` governs any failure of the hook, including an `append` that cannot
open its file — a hook whose audit log stopped being writable is a hook that is no
longer auditing.

## Which events you may name

The names are the wire tags `EventKind` serialises to, and the full set is:

```
started              step                 tool_call            refused
approval_requested   approval_decided     spend_draw           retry
fell_back_to         replan               stalled              spawned
spawn_refused        memory_wrote         todo_wrote           question_asked
question_answered    server_tool_used     token                sandbox
mcp                  handle_started       handle_polled        handle_killed
handle_exited        handle_orphaned      finished
```

A name this crate does not emit is an **error at load**, naming the name and
listing the ones that exist. A misspelled tag would otherwise be a hook that
loads, installs, and never fires — a silence, which is the failure this feature
can least afford. The list is checked against the enum by a test rather than
maintained by hand, so a variant added in a later release is a name a hook may use
in that release.

## Installing it

```rust
use io_harness::{run_with_observed, ApproveAll, Config};

let config = Config::discover(&root)?;
let hooks = config.hooks();

let result = run_with_observed(
    &contract, &provider, &store, &policy, &ApproveAll, &hooks,
).await?;
```

One line to build it and one argument to pass it, to `run_observed`,
`run_with_observed`, `resume_observed` or any of the tree forms — the same
observed twins any other observer uses. Nothing in this crate installs a hook on
its own, which is the rule every projection in `io.toml` obeys: the file describes
it, the caller loads it, and nothing happens implicitly.

`Hooks::is_empty()` says whether the file declared any, so an embedder that would
rather not install an observer that does nothing can ask first.

One `Hooks` covers a whole [tree](composition.md): a child's events reach it with
the child's own `run_id` and a non-zero `depth`, like any other observer's.

## A project file may not declare a hook

**`[[hook]]` is refused in the project scope**, and the whole array is refused
rather than its executing half:

```
io.toml: key `hook`: a project-scoped file may not declare hooks, because a hook
runs or writes on this machine and `io.toml` arrives with a `git clone`. Write it
in `io.local.toml` or the user-scope file instead.
```

0.27.0 refused `${cmd:}` in `io.toml` because parsing a file must not be able to
run a command, and `io.toml` is the file a `git clone` delivers. A hook that runs
an argv is that primitive arriving one release later. A hook that *appends* is a
write to a path a stranger chose, which is the same hazard by a shorter route — so
refusing the executing half and allowing the writing half would be a rule a reader
has to hold two halves of.

`io.local.toml` and your user-scope file take hooks unchanged. `Config::from_toml`
is the project scope too, and refuses them for the same reason. A `[[hook]]` hidden
inside a `[profile.<name>]` body of a project file is refused as well; a profile is
applied later, and a check that only looked at the base would let it reach the same
place by a different path.

This is the [configuration guide's](configuration.md#a-project-file-may-narrow-and-may-never-widen-0270)
narrow-never-widen rule applied to a new key rather than a new rule, and it comes
with the same sentence it is not.

**What this does not claim.** Not that a cloned repository is safe. `[[mcp]]` still
names a command, `[toolchain]` still names an argv, and a `[[policy.layers]]` entry
can still allow what the defaults did not. This is a specific narrowing of a
specific hazard — four keys, `${cmd:}` and `[[hook]]`, no more — and it is the file
half of a boundary whose enforcing half is still the `Policy` you loaded.

## What a hook costs

A hook runs **inside the run loop**. `Observer::event` is synchronous, so the step
that emitted the event is stopped until every matching hook has finished. That is
what makes `on_failure = "cancel"` possible at all — an asynchronous hook could
not refuse anything in time — and it is why `run` is bounded by `timeout_ms` and
`append` is a plain write.

So the event you name is a cost decision:

| Event | How often | A `run` hook there means |
| --- | --- | --- |
| `token` | once per **streamed token** | a process spawn per token |
| `step`, `tool_call` | once per step | a process spawn per step |
| `started`, `finished` | once per run | a process spawn per run |
| `refused`, `stalled`, `approval_requested` | when it happens, rarely | nothing measurable |

An `append` hook is an open, a write and a close per event, which is cheap enough
to point at `step` and not cheap enough to point at `token` without meaning to.
The five-second default `timeout_ms` is chosen to be noticeable: an operator who
wires a spawning hook to a hot event finds out rather than wonders.

## The limits, stated plainly

**A hook blocks the run.** `Observer::event` is synchronous and returns a `Flow`
the loop acts on immediately, so the step that emitted the event waits for every
matching hook. That is not a defect to be fixed later — it is the property that
makes `on_failure = "cancel"` mean anything, since a hook that answered after the
run had moved on could not refuse anything. It also means hooking `token` with a
`run` action is a decision to spawn a process per streamed token, and the run will
be exactly as slow as that sounds.

**A hook is refused in the project scope, whole — and that does not make a cloned
repository safe.** `[[mcp]]` still names a command, `[toolchain]` still names an
argv, and a `[[policy.layers]]` entry can still allow what the defaults did not.
This is a specific narrowing of a specific hazard, and the boundary against the
agent is still the `Policy` the caller loaded.

**A hook is not accumulated across scopes.** Unlike `[[policy.layers]]` and
`[[agent]]`, a later scope **replaces** the array whole — so one `[[hook]]` in
`io.local.toml` silently replaces every hook in the user-scope file. The hooks that
run are the hooks of one file, never a pile assembled from three. `Config::sources()`
says which file won.

**An `append` hook opens its file once per event rather than holding a handle**,
and appends are serialised by a lock. Within this process that is enough: a
sub-agent tree emits from several tasks at once, and no reader will ever see half
of one event followed by half of another. A **second process** appending to the
same log is not coordinated by anything, and there is no file lock, no rotation and
no size cap.

**A hook's failure is only fatal when the operator asked for it.** The default is
`continue`: a failed hook is logged through `tracing` at warn level, naming the
hook's index and the event tag and never the event itself — a `started` carries the
goal — and the run goes on. A notification that could not be delivered is not a
reason to abandon work. `on_failure = "cancel"` is how you say otherwise, per hook.

**A hook receives events and grants nothing.** It is not a permission mechanism. It
cannot approve an action, deny one, alter an argument, or answer an
`approval_requested` — the `Approver` is the other channel, and it is unchanged.
The only thing a hook can do to a run is end it. Its output is discarded, `stdout`
and `stderr` go to `null`, and it talks by exiting non-zero rather than by
printing.
