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
| `at` | the lifecycle point this hook attaches to — `"before_tool"` (0.42.0) | absent means an event hook |
| `tools` | the tool names an `at` hook wants | absent means **every** call |
| `append` | a path to append one JSON line per matching event to | — |
| `run` | an **argv array** to spawn with the event JSON on its stdin | — |
| `on_failure` | `"continue"`, `"cancel"`, or `"refuse"` (0.42.0) | `"continue"`, or `"refuse"` for an `at` hook |
| `timeout_ms` | the wall-clock ceiling on `run` | `5000` |

**Exactly one of `on` and `at`.** An event hook watches what happened; a lifecycle
hook decides whether something happens. A table claiming both is an error naming
its index, as is `tools` on a table with no `at`, an `at` value this crate does not
have, and a lifecycle hook whose only action is `append` — appending a line cannot
stop a call, so that table would be a check that always passes. Every one of them
is refused when the file is read, because a check that loads and never fires looks
exactly like a check that approved everything.

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

### A rule that stops a call before it runs (0.42.0)

An event hook is told what happened. `on_failure = "cancel"` is the strongest
answer it has, and it lands at the next step boundary — after the call it objected
to has already run. A hook that must stop a call attaches to the lifecycle
instead:

```toml
# Nothing publishes from this repository, whatever the agent decides.
[[hook]]
at = "before_tool"
tools = ["exec", "shell"]
run = ["./checks/no-publish"]
timeout_ms = 2000
```

The child is spawned before the call executes, in the discovery root, with the
pending call on its stdin:

```json
{"at":"before_tool","run_id":41,"step":3,"depth":0,"tool":"exec",
 "arguments":{"argv":["cargo","publish"]}}
```

Exit 0 and the call proceeds. Exit non-zero and — by default — **that call does
not happen**: the run continues and the model is told why, in the hook's own
words, so it retargets rather than retrying the same refused call to the step cap.
The reason is the child's first non-empty line of stdout, bounded at 4096
characters; a hook that prints nothing gets a reason naming its program and its
exit status. This is the one place a hook's stdout is read at all — an event hook's
is discarded, as it has been since 0.28.0.

```sh
#!/bin/sh
# ./checks/no-publish — refuse, and say why in one line.
if grep -q '"cargo".*"publish"' -; then
  echo "releases are cut by a person in this repository"
  exit 1
fi
```

`on_failure = "cancel"` ends the whole run instead, at the next step boundary, and
`on_failure = "continue"` makes the hook advisory — it runs, its failure is logged,
and the call proceeds.

A refusal appears in the trace as a `refused` event with the hook's program where
a rule's pattern would be and `io.toml hook` where a layer would be, so an
observer already watching refusals sees it without being rebuilt.

**A lifecycle hook is a process spawn per matching call**, so `tools` is not
decoration: filtered to `exec`, a completion full of `read_file` calls costs
nothing at all, and unfiltered it costs one spawn per read. The gate is serial and
runs on the loop's own thread; the read work it approves still runs concurrently.

**It is installed like everything else in this file, and it is not implicit.**
`Config::hooks()` builds the same `Hooks` you install as an observer;
`TaskContract::with_tool_hooks(Arc::new(hooks))` is what makes the `at` tables
decide anything. As an observer it ignores the `at` tables; as a gate it ignores
the `on` ones.

There is no `after_tool`. It needs a result shape this crate does not have yet,
and naming a point that cannot fire is the mistake the validator above exists to
prevent.

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
can least afford. The list is written down in the crate and then *checked against
the enum* by a test that reads the source, so a variant added in a later release
cannot ship without becoming a name a hook may use — which is the half that had
been missing since 0.21.0 and that 0.28.0 closed.

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

## Reading the hooks that are installed (0.71.0)

`is_empty()` is a count, and a count is the wrong answer to the question an
application layer actually has. A settings screen, a `doctor` command, or
anything that installs hooks on an operator's behalf needs to say *what* is
configured — otherwise the operator is told "3 hooks" about a file they were
trying to debug. `Hooks::declarations()` returns the tables themselves, in
declaration order, and `Hook` is public with an accessor per key:

```rust
use io_harness::{Config, OnFailure};

let hooks = Config::discover(&root)?.hooks();
for hook in hooks.declarations() {
    let what = match hook.at() {
        Some(point) => format!("before {point}, tools {:?}", hook.tools()),
        None if hook.on().is_empty() => "every event".to_string(),
        None => format!("events {:?}", hook.on()),
    };
    let action = match (hook.append(), hook.run()) {
        (Some(path), _) => format!("append {}", path.display()),
        (_, Some(argv)) => format!("run {argv:?}"),
        _ => unreachable!("the loader refuses a table with neither"),
    };
    println!("{what}: {action} ({:?}, timeout {:?})", hook.on_failure(), hook.timeout_ms());
}
```

Two of those accessors are not plain field reads, and the difference is the
point:

- **`on_failure()` resolves the kind's default.** A table that wrote nothing
  still has an answer, and it is not the same answer for both kinds — an event
  hook continues, a lifecycle hook refuses. There is deliberately no accessor for
  the raw `Option`, because a reader who took it would have to re-derive that
  rule to say anything true, and `OnFailure::default()` is `Continue`, which is
  right for only one of the two.
- **`timeout_ms()` is the key rather than a resolved number.** It answers "did
  the operator choose this?", which is what a caller *showing* the table needs;
  the module's own default fills in when they did not.

`OnFailure` is `#[non_exhaustive]`: match it with a `_ =>` arm. The obvious next
lifecycle point takes an answer these three do not — an `after_tool` hook decides
about a result rather than about a call — and paying for the arm once now is what
keeps that from being a break.

This is the configuration half of the answer. A [capability bundle](plugins.md)
can contribute hooks too, and `Plugin::hooks()` is the bundle half; after
`Plugins::apply_to_hooks`, `declarations()` returns both, the file's first.

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
