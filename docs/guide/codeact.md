# CodeAct — one program instead of a chain of calls

A step that would have been six round trips — grep, read, read, read, edit, exec
— is one Python program the model writes once. The program runs as a child of
this process, and every act it takes comes back over a pipe and re-enters the
same `dispatch` a model's own tool call takes. What collapses is the number of
provider round trips, not the number of boundaries.

Behind the `codeact` feature, off by default. It adds **no crate**: `tokio`'s
`process` and `io-util` are already in the default build, and the interpreter is
a host binary rather than a dependency. It is a feature anyway for the reason
`browser`, `otel` and `mcp-server` are — a door onto this process's tools with a
program on the other side of it is something a build that did not ask for it
should not compile.

```rust
use io_harness::{run_with, ApproveAll, CodeActConfig, Policy, Store,
                 TaskContract, Verification};
use std::time::Duration;

let contract = TaskContract::workspace(
    "rename every handler in src/ that still takes a bare String",
    "/path/to/repo",
)
.with_verification(Verification::Command {
    argv: vec!["cargo".into(), "test".into()],
    expect_exit: 0,
})
.with_codeact(
    CodeActConfig::default()
        .with_max_callbacks(32)
        .with_timeout(Duration::from_secs(60)),
);

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

`with_codeact` is what turns it on for a run. Without it — the default —
`RUN_PROGRAM_TOOL` is absent from the catalogue entirely, and nothing about the
run changes.

## One tool, inside one ordinary step

The capability is a single built-in, `run_program`, whose one argument `source`
is a complete Python program. A program is a tool call inside one ordinary step,
so the shape of a step does not change: transcript pairing, the step cap, the
attribution columns, [`ToolMask`](tools-and-skills.md) and `before_tool` hooks
all keep working exactly as they did.

Whatever the program prints is the result, plus the `repr` of a global named
`result` if it sets one, and it comes back attached to that step's result. There
is no session between programs — interpreter state does not outlive the step, so
a second program starts from nothing.

## The interpreter is the host's

`[codeact] interpreter = "..."` names one. Otherwise `CODEACT_CANDIDATES` —
`python3`, then `python` — are resolved on `PATH` in that order. **Nothing is
downloaded, ever.**

**Every** candidate is version-probed, including one named in configuration, and
one reporting less than `CODEACT_MIN_PYTHON` — `(3, 8)` — is rejected. So a
`python` that answers 2.7 is rejected by number rather than trusted by name, and
the failure is "no usable interpreter" at discovery instead of a `SyntaxError`
inside a program the model wrote. A named interpreter that fails its probe is
**not** replaced by a candidate off `PATH`: the operator said which one, and
falling through to another would run a program on a binary nobody chose.

**No usable interpreter is a supported state, not an error.** The tool is not
advertised, and the turn is composed, sent and stepped exactly as it would have
been with the feature off. Discovery happens once per run, before the first step,
because the interpreter on `PATH` does not move under a run.

Either way the decision is on the record. One `EventKind::Program` is emitted
before the first step with `outcome` `available` or `withheld`, and its `detail`
names every candidate and what each one answered — "no interpreter" and "a
`python` that reported 2.7" are different facts about a host, and an operator
reading the trace needs the second one. A capability that quietly stops applying
is worse than one that is absent.

## Configuring it from a file

```toml
[codeact]
interpreter = "/usr/bin/python3"
max_callbacks = 32
timeout_secs = 60
```

**`[codeact]` is refused at project scope**, in `REFUSED_SECTIONS`, because
`interpreter` names a program on this machine and every program the model writes
is handed to it. An `io.toml` arrives with a `git clone`, and `io.local.toml` is
held to the same rule because a run's own agent can write it. Write it in the
user-scope file, which `Config::discover` also reads. See
[Configuration](configuration.md).

## Starting a program is itself gated

Starting the interpreter is an `Act::Exec` check taken before anything is
spawned, on the interpreter's own path and on `"<interpreter> program.py"` —
both spellings, exactly as `exec` checks them. The program alone is what
`deny_exec("python3")` names; the whole argv is what a narrower `allow_exec`
names. A run that denies execution therefore denies programs, and this tool is
not a second path around that gate.

`run_program` is refused while the plan gate is active, for the reason `remember`
is. The plan gate is a policy layer denying `Write` and `Exec`, and it works
because every mutating path in this crate is one of those two checks — starting
an interpreter is a third, so a run held still waiting for an approved plan would
have started programs while every act inside them was denied. It is denied rather
than filtered out of the catalogue, because this crate denies tools and never
hides them.

## Every act re-enters dispatch

There is one path, and it is the path a model's own call takes. Each act a
program takes goes back through `dispatch` with the arguments that arm already
holds — the same policy, the same gate, the same `policy_events` row, the same
journal attempt, the same `Observer` event. So a program reaches exactly what a
tool call reaches and nothing more. There is deliberately no shorter path: a
purpose-built one would compile more easily and pass every test in this release
while bypassing the gate.

The acts are **not** folded into the program's own event. Each arrives on the
observer channel as its own `EventKind::ToolCall`, which is the point — a program
is a shorter way of asking, so what it asked for is observed where every other
request is observed.

Inside the program, each callable tool is a function taking that tool's arguments
as keywords and returning an object with `.ok` and `.text`. `str()` of it is the
text, and it is falsy when the act was refused **or failed**:

```python
hit = grep(pattern="fn handler", path_glob="src/*.rs")
for line in str(hit).splitlines():
    edited = edit_file(path=line.split(":")[0], search="handler(s: String)",
                       replace="handler(s: &str)")
    if not edited:
        print("did not land:", edited.text)
```

`.ok` is false exactly when the observation came back as `ObsKind::Error`, and
`Dispatched::go` marks a policy refusal and a tool's own failure — a non-zero
`exec`, a file that is not there — with that same kind. So a false `.ok` says the
act did not land and not that the policy said no; `.text` is where the two are
told apart, and the tool's description tells the model to read it. A refusal is
therefore a value the program branches on rather than a hidden exception, and the
crate reads its own structural signal rather than its refusal text back out of a
string it just wrote.

## A program cannot see the workspace

The bindings are not a convenience over Python's own file access. They are the
only route to the workspace at all: the program runs in an empty scratch
directory, so `open("src/main.rs")`, `os.listdir(".")`, `glob` and `pathlib` find
nothing there — and they find nothing *quietly*, because an empty directory is a
legal answer to all four rather than an error.

That is the boundary working. Every effect a program has on the workspace is a
callback the policy sees, and a program that could read a file directly would be
an act with no `policy_events` row. But it is also the mistake a model makes
first, so the tool description says it outright rather than leaving it to be
inferred from where the program runs.

**This was found by the live arm and by nothing else.** The first live run
finished with the right answer while never using the capability: the model wrote
a program that counted lines with `open`, got zero, printed a confidently wrong
total, and then did the whole task as ordinary tool calls in the steps that
followed. The suite could not have caught it — every program in it was written by
somebody who already knew the answer. See
[MEASUREMENTS.md](../MEASUREMENTS.md).

## What a program may not call

`CODEACT_UNCALLABLE`, written out by name — three groups, and the reason differs
by group:

- `remember`, `forget` and `todo_write`, which are ungated inside `dispatch`
  because they land in the harness's own store rather than in the workspace.
  There is no `Act::Write` for a gate to check, their only boundary is the plan
  gate, and that is a property of the turn rather than of the program — so a
  program calling them would have no boundary at all.
- `ask_question`, `ask_questions` and `propose_plan`, which need a conversation,
  and `spawn_agent`, `send_message` and `read_messages`, which need a tree. A
  program is inside one step of one run: there is nobody to answer it and no
  sibling to address.
- `read_skill`, which hands a server-side document to the caller, and
  `run_program` itself — one program starting another would turn the callback
  bound into a bound per level rather than a bound.

The list is a literal rather than the catalogue minus the exclusions. Derived, it
is the same set today and makes every built-in added later callable silently;
written out, a new name is not callable until somebody classifies it.

It is checked at the boundary as well as being absent from the bindings a program
is given, so a program that builds a call by hand meets the same list.

A registered [`Tool`](tools-and-skills.md) or an MCP tool whose name is not a
Python identifier, or is a Python keyword, is left out of those bindings. Not
because it would fail to parse — the shim carries the names as data and injects
them into the program's namespace, so a keyword lands in a dictionary and breaks
nothing — but because the model calls a tool by writing `name(...)`, and a name
that is a keyword or is not an identifier cannot be written that way at all.
Advertising it would offer the model something it then cannot call. Such a tool
stays callable the ordinary way in the same turn.

## How the program is contained

`Sandbox::run` closes stdin and consumes its spec to completion, which is right
for every other contained execution in this crate and wrong for a program that
has to ask questions while it runs. So a program is contained the way
`shell_start` contains a living child: `wrap_argv`, `contain_command`,
`apply_rlimits` and `own_process_group` around an owned `tokio::process::Command`.
That selects the **same backend by the same rules** and applies the same
`SandboxLimits` — see [the sandbox](sandbox.md) for the backend table.

**That seam applies nothing at all on Windows, so a program that asked to be
contained is refused there rather than degraded.** `wrap_argv` has no Windows
branch, `apply_rlimits` is unix-only, `contain_command` answers `None` off Linux,
and the Job Object is created by the `Sandbox` runner and by `shell_start`'s own
suspended-spawn path — neither of which this is. Started anyway, the interpreter
would have had the full filesystem and the full network while the run reported a
backend granting neither, so the refusal names the backend the run resolved and
nothing is spawned. That is 0.74.0's rule, which `shell_start` already applies to
the narrower case of the AppContainer: a boundary named in the trace and not
applied to the process is worse than no boundary at all. A run that asked for no
containment is **not** refused there — it runs uncontained, by the caller's own
choice, exactly as `exec` does on such a run.

The program runs in an **ephemeral workdir of its own, never the workspace**, and
that workdir is the only writable root. It needs no workspace access, because
every effect it has on the workspace is a callback the policy sees — a program
that could edit a file directly would be an act with no `policy_events` row.
Under a backend that confines writes, a program that tries to edit a workspace
file directly cannot. Under the portable floor, which has no path rule, the honest
claim is only the ephemeral workdir.

Egress is denied and no proxy is named whatever the run itself was granted,
because a program that could open its own socket would be a second route out of a
run whose first one is gated. **How much that denial is worth is the backend's
answer, not this module's**: `Backend::denies_egress` is false for the portable
floor, so there a program can still open a socket, and a run with no containment
at all has nothing to deny it with. A program reaches the network the way anything
else does — through this crate's own network-governed tools, as callbacks checked
under `Act::Net` — see [network egress](mcp-and-network.md).

The shim owns the protocol descriptors before the program has run one
instruction. It dups file descriptor 1, then points descriptors 0, 1 and 2 at
devnull and replaces `sys.stdout` and `sys.stderr` with a buffer and `sys.stdin`
with an empty one. So nothing a program **prints** — a plain line, raw bytes
through `os.write`, or a line that is itself a well-formed callback frame — can
reach or forge the pipe this crate is reading, and a program calling `input()`
cannot eat a tool result.

That is a claim about printing, and deliberately not a claim about isolation. The
descriptors are closure variables rather than module globals, so the one-lookup
route through `_act.__globals__` is closed; a program that walks `__closure__`
can still find them. It gains nothing by it — a forged `call` frame is dispatched
under the same policy as an honest one, and a forged `done` only ends the program
early. A program is untrusted code running under a boundary, not code this crate
is trying to sandbox from itself.

Output is bounded twice. The shim truncates what the program printed before it
sends it, at the source and with the cut named, so the harness never has to hold
a frame this crate did not bound. The reader bounds it again: a frame larger than
4 MiB is reported as a failure rather than buffered, so a shim that did not
truncate — because somebody edited it, or because a program reached past it —
still cannot make this process allocate without limit.

## Two bounds beyond the sandbox's

The sandbox's caps bound what the child spends. These bound what the program
makes **this** process spend, which is what a tight callback loop exhausts and
what no rlimit can see:

- `max_callbacks`, default 64. It counts the calls actually served, and it is
  asked when a call arrives rather than before a frame is read — so a program
  that makes exactly its allowance and then finishes completes normally and keeps
  its output, and only the call past the bound stops one.
- `timeout`, default 120 seconds of wall clock.

Both are settable on `CodeActConfig` and in `[codeact]`. A breach is a typed
ending that reaches the model as feedback naming the bound it hit and what to do
instead — never a hang, and never a bare kill with no explanation. Ending the
program ends what it started: a program that spawns is a tree, and the kill
reaches the tree rather than only the interpreter.

**They are not belt-and-braces, because by default there is nothing underneath
them.** `TaskContract`'s `exec_sandbox` carries `SandboxLimits::none()` unless a
caller sets limits, so on a default contract no CPU, memory or wall bound applies
to a program at all — and on a run that never asked for contained exec there are
no rlimits either. That is the bound `exec` already has, and it is why `timeout`
is applied to the **wait for the next callback** rather than to the loop around
it: a program that spins without ever calling back produces no frame to check a
deadline between, so this bound is the only thing that stops it.

A program that raises comes back with its traceback and anything it printed
first, and the model may send a corrected program. That retry is a new attempt
and counts against the step cap like any other. A `sys.exit` is read by its code:
`None` or zero is a finish, and any other code is a failure carrying that code in
the message — an exit code is an outcome, and `sys.exit("boom")` reported as a
finish would tell the model a program that failed had succeeded.

**A deferral inside a program is a denial, not a pause.** `Decision::Defer`
records a pending action and parks the run so a human can answer after the
process has exited, which is a coherent answer to a model's own tool call and an
incoherent one to an act inside a program: the program is mid-flight with a pipe
open, the acts it has already taken have already happened, and a resumed run
writes the program from scratch and would re-execute them. Pausing there also
loses the run's own account of what the program changed, because the pause leaves
the arm before the `changed` and `remember` it accumulated are reported. So a
deferral becomes a denial **for a program's acts only**, in this crate's own
words, and the program branches on it like any other refusal. The caller's
`Approver` is untouched everywhere else, and an act the model makes itself can
still be deferred exactly as before. A question or a plan decision cannot arrive
at all — the tools that raise one are on the uncallable list — and a program that
built such a call by hand is told the act needs a decision it cannot wait for.
See [durable runs](durable-runs.md).

## Where it is not offered

- **A served MCP session does not offer it.** `run_program` is on
  `MCP_SERVER_UNSERVED`; see [MCP and network egress](mcp-and-network.md).
- **A sub-agent does not offer it.** The tool is advertised by the workspace loop
  only, so a tree's agents call the tools directly.

## What the saving actually is, measured here

The claim is fewer provider round trips for the same work, and this repository
measured the comparison itself rather than repeating a figure from another
harness. **It did not find a saving.**

On a task shaped for a loop, with `deepseek/deepseek-v4-flash-0731`: offered the
tool, the model wrote no program at all and did the work as ordinary calls. Told
to use one, it wrote four — iterating — for 161,416 tokens against the chain's
32,157. Every program ran, every one finished, and the answer was right each time,
so this is a cost result rather than a correctness one. A program's output
re-enters the transcript, and writing programs is expensive output.

One task, one model, one sample per arm — it settles that a program is not
automatically cheaper and that a model iterating on programs is the expensive
case, not that the capability cannot pay on larger work. The numbers, the method
and the machine are in [MEASUREMENTS.md](../MEASUREMENTS.md).

**So reach for a program when the work is a loop with a branch that the tools
cannot express, not to save tokens.** That is what it is for, and it is what the
release rests on: a program's acts are gated exactly as a model's own calls are.

## The limits, stated plainly

**A program is bounded by the run's containment, not by more than it.** A
contract whose `ExecMode` is not a contained one — `ExecMode::FullAccess` —
resolved no containment for `exec`, and there is none to compose from here
either. Such a program gets its own ephemeral working directory and otherwise
runs on the host with this process's privileges, exactly as that run's commands
do: no path rule, no rlimit, and nothing denying it a socket.

**On Windows, a program that asked to be contained is refused.** The living-child
seam applies no backend there at all, and a refusal naming the backend the run
resolved is what this crate does rather than start a program under a boundary the
trace claims and the process does not have. A Windows run that asked for no
containment is unaffected and runs uncontained.

**Denied egress is worth what the backend makes it worth.** No proxy is named and
egress is denied whatever the run was granted, but `Backend::denies_egress` is
false for the portable floor, so under it a program can still open a socket — and
a run with no containment has nothing to deny it with at all.

**The interpreter is the host's, and so is what it can import.** Nothing is
installed for a program: no `pip` step, and nothing is downloaded. But the
interpreter is spawned as itself — no `-I`, and no scrub of `PYTHONPATH` — so its
`site-packages` are importable, and what an `import` reaches is whatever that host
interpreter already carries rather than anything this crate decides. The tool's
description tells the model to prefer the standard library and to check an import
rather than assume it.

**A program that introspects the shim can reach the pipe, and gains nothing.**
The protocol descriptors are closure variables, so the reachable route through
`_act.__globals__` is closed and nothing a program *prints* can forge a frame; a
program that walks `__closure__` still finds them. What it can do with them is
send a `call` that is dispatched under the same policy as an honest one, or a
`done` that ends its own program early. This is a boundary around what a program
may reach, not an isolation of the shim from the program.

**A program cannot exceed a model's reach, and does not narrow it either.** Every
act takes the same gate, so a program reaches what the policy allows the model to
reach — no more, and no less. A `run_program` that a policy would refuse every
act of still runs, and comes back having been refused every time.

**Withholding is silent to the model and loud in the trace.** A run on a host
with no usable interpreter never sees the tool and cannot tell that the feature
was asked for; the `EventKind::Program` row at step 0 is where that is visible.

**A bound stops the program, not what it already did.** A callback bound, a
timeout or a broken pipe ends a program that has already made calls, and those
calls happened — each with its own gate decision, journal attempt and event. The
model is told how many were made before the stop.

## See also

- [Execution sandbox](sandbox.md) — the backends, the caps, and what each one
  does and does not confine
- [Permissions and approval](permissions.md) — the gate every callback takes
- [Command execution](command-execution.md) — `ExecMode`, and the containment a
  program composes from
- [MCP and network egress](mcp-and-network.md) — `Act::Net`, and the served set
  `run_program` is kept out of
- [Tools and skills](tools-and-skills.md) — the catalogue a program's bindings
  are generated from
- [Observability and replay](observability.md) — `EventKind::Program` and the
  `ToolCall` rows a program's acts land as
- [Configuration](configuration.md) — the scopes, and why this table is refused
  in one of them
- [Measurements](../MEASUREMENTS.md) — the numbers, none of them a gate
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
