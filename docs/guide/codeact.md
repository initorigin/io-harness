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
text, and it is falsy when the act was refused:

```python
hit = grep(pattern="fn handler", path_glob="src/*.rs")
for line in str(hit).splitlines():
    edited = edit_file(path=line.split(":")[0], search="handler(s: String)",
                       replace="handler(s: &str)")
    if not edited:
        print("refused:", edited.text)
```

`.ok` is false exactly when the observation came back as `ObsKind::Error`, which
is what `Dispatched::go` marks both a policy refusal and a tool's own failure
with. A refusal is therefore a value the program branches on rather than a hidden
exception, and the crate reads its own structural signal rather than its refusal
text back out of a string it just wrote.

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

It is checked at the boundary as well as being absent from the generated module,
so a program that builds a call by hand meets the same list.

A registered [`Tool`](tools-and-skills.md) or an MCP tool whose name is not a
Python identifier, or is a Python keyword, is left out of the generated module —
`def import(**kwargs):` is a `SyntaxError` that would take down every binding,
including the ones the program was going to use. Such a tool stays callable the
ordinary way in the same turn.

## How the program is contained

`Sandbox::run` closes stdin and consumes its spec to completion, which is right
for every other contained execution in this crate and wrong for a program that
has to ask questions while it runs. So a program is contained the way
`shell_start` contains a living child: `wrap_argv`, `contain_command`,
`apply_rlimits` and `own_process_group` around an owned `tokio::process::Command`.
That selects the **same backend by the same rules** and applies the same
`SandboxLimits` — see [the sandbox](sandbox.md) for the backend table.

**That seam has no Windows AppContainer branch.** A program on Windows is bounded
by the Job Object's memory, CPU and process-count caps, which have no path rule
and no socket rule. It is the same containment `shell_start` has there, stated
here rather than left to be inferred from the sandbox page.

The program runs in an **ephemeral workdir of its own, never the workspace**, and
that workdir is the only writable root. It needs no workspace access, because
every effect it has on the workspace is a callback the policy sees — a program
that could edit a file directly would be an act with no `policy_events` row.
Under a backend that confines writes, a program that tries to edit a workspace
file directly cannot. Under the portable floor and the Windows Job Object, which
have no path rule, the honest claim is only the ephemeral workdir and the
proxy-environment strip.

It is given no proxy environment of its own. A program reaches the network
through this crate's own network-governed tools, as callbacks checked under
`Act::Net` like any other — see [network egress](mcp-and-network.md).

The shim owns the protocol descriptors before the program has run one
instruction. It dups file descriptor 1, then points descriptors 0, 1 and 2 at
devnull and replaces `sys.stdout` and `sys.stderr` with a buffer and `sys.stdin`
with an empty one. So a program that prints ordinary text, writes raw bytes with
`os.write`, or prints a well-formed callback frame cannot reach or forge anything
on the pipe this crate is reading, and a program calling `input()` cannot eat a
tool result.

## Two bounds beyond the sandbox's

The sandbox's caps bound what the child spends. These bound what the program
makes **this** process spend, which is what a tight callback loop exhausts and
what no rlimit can see:

- `max_callbacks`, default 64.
- `timeout`, default 120 seconds of wall clock.

Both are settable on `CodeActConfig` and in `[codeact]`. A breach is a typed
ending that reaches the model as feedback naming the bound it hit and what to do
instead — never a hang, and never a bare kill with no explanation. Ending the
program ends what it started: a program that spawns is a tree, and the kill
reaches the tree rather than only the interpreter.

A program that raises comes back with its traceback and anything it printed
first, and the model may send a corrected program. That retry is a new attempt
and counts against the step cap like any other.

If an act a program takes returns an approval request, a question or a plan
decision, the program is **stopped and the run pauses** exactly as it would have
had the model made that call itself. None of the three can be answered while a
program is mid-flight, and answering one here would be this arm deciding
something the caller's `Approver` was asked. On resume the model writes the
program again, knowing the answer. See [durable runs](durable-runs.md).

## Where it is not offered

- **A served MCP session does not offer it.** `run_program` is on
  `MCP_SERVER_UNSERVED`; see [MCP and network egress](mcp-and-network.md).
- **A sub-agent does not offer it.** The tool is advertised by the workspace loop
  only, so a tree's agents call the tools directly.

## What the saving actually is, measured here

The claim is fewer provider round trips for the same work, and this repository
measures the comparison itself rather than repeating a figure from another
harness: a number measured on a different runtime, with different tools and a
different prompt, is not a result this crate can report as its own. What is found
is recorded in [MEASUREMENTS.md](../MEASUREMENTS.md), with the machine named and
the method stated, like every other number this repository publishes.

## The limits, stated plainly

**A program is bounded by the run's containment, not by more than it.** A
contract that asked for `ExecMode::FullAccess` resolved no containment for
`exec`, and there is none to compose from here either — such a program gets its
own ephemeral working directory and otherwise runs on the host with this
process's privileges, exactly as that run's commands do.

**The interpreter is the host's, and so is what it can import.** The tool's
description tells the model to write against the standard library, and nothing is
installed for a program: no `pip` step, and nothing is downloaded. What an
`import` of a third-party module does is decided by what that host interpreter
already carries, not by this crate.

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
