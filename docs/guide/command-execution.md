# Command execution

The agent can run a command in the workspace — the project's own build, tests,
linter, formatter, or package manager — through an `exec` tool authorized by the
same permission check as every other tool.

```rust
use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};

# async fn demo() -> io_harness::Result<()> {
let contract = TaskContract::workspace(
    "the test suite is failing; find out why and fix it",
    "/path/to/repo",
    // The gate is the project's own command, in whatever language it is written.
    Verification::Command { argv: vec!["npm".into(), "test".into()], expect_exit: 0 },
);

// What the agent may run, decided before it runs anything.
let policy = Policy::permissive()
    .layer("app")
    .allow_exec("npm*")
    .allow_exec("node*")
    .deny_exec("rm*")
    .deny_exec("git push*");

let result = run_with(&contract, &OpenRouter::from_env()?, &Store::open("runs.db")?,
                      &policy, &ApproveAll).await?;
# let _ = result;
# Ok(()) }
```

## There is no shell

`exec` takes an **array of strings**, program first. That array is handed to the
operating system as an array. Nothing on the path parses it, so `;`, `&&`,
`$( )`, a backtick, `|` and `>` are ordinary bytes inside whichever argument
contains them:

```jsonc
// One command with an oddly-named file. Not two commands.
{ "argv": ["rustc", "a;b && c $(id).rs"] }
```

There is deliberately no shell-string form. It is the whole injection surface and
nothing here needs it. If a real task later needs a pipeline, it arrives as its
own named tool with its own argument, not as a mode on this one.

The other half of the same discipline is that the model supplies the **whole**
argv here, which is the opposite of how the git built-ins work — those construct
their own argv and let the model supply only paths. Both are deliberate. `git`
has subcommands that fetch code and options that execute it, so its safe set is a
closed one worth enumerating. `exec` exists precisely to run commands this crate
has never heard of, so the boundary moves from the argv's *shape* to the policy.

## What a rule can say

Every call is checked twice, and the second check is what makes a useful rule
writable:

- on the **program** — `deny_exec("rm")` holds whatever the arguments are, and
  matches by basename too, so it also covers `/bin/rm`;
- on the **whole argv, joined by spaces** — `allow_exec("cargo test*")` beside
  `deny_exec("cargo publish*")` means what it reads.

A deny in any layer beats an allow in any other, so an operator base is safe to
hand out: nothing an application stacks on top can take it back. An `Effect::Ask`
routes to the `Approver` exactly as a write does; a `Deny` never reaches one.

A refusal, and an approver's decision, are recorded in `policy_events` with the
rule and layer that produced them — the same table, and the same shape, as a
refused read or a denied host. A *silent allow* is not: `exec` behaves here
exactly as `read_file` and `write_file` do, and none of the three writes a row
when the policy simply permits the action. What every call does leave, allowed or
not, is the step's own trace row, whose `tool_call` column holds the argv the
model asked for. So "what was refused" is one query against `policy_events`, and
"what actually ran" is one query against `steps`.

```rust
use io_harness::{Act, Effect, Policy};

let policy = Policy::permissive()
    .layer("ops")
    .allow_exec("cargo*")
    .deny_exec("cargo publish*");

assert_eq!(policy.check(Act::Exec, "cargo test").effect, Effect::Allow);
assert_eq!(policy.check(Act::Exec, "cargo publish --token x").effect, Effect::Deny);
```

## The bounds on a running command

Two, and both are reported to the model rather than only logged.

**A wall-clock timeout.** `DEFAULT_EXEC_TIMEOUT` is 15 minutes, overridable with
`TaskContract::with_exec_timeout`. It is generous on purpose: it is a ceiling on
a *wedged* command, not a budget, and a timeout tight enough to be interesting is
one that kills honest cold builds. What it prevents is a command that will never
return quietly eating the contract's whole time budget and being reported as a
budget stop, which sends whoever reads the trace to the wrong diagnosis.

**An output ceiling, applied head and tail.** Oversized output keeps its start
and its end and elides the middle, and the result the model reads states how much
went. A build log's two useful pieces are at opposite ends — what ran, at the
top; what failed, at the bottom — so a tail-only cut loses the command and a
head-only cut loses the error. The ceiling is the run's own per-observation cap,
derived from `ContextBudget`, so a command obeys the same bound as every other
tool result rather than one of its own.

A program the machine does not have is reported as unavailable and the run
carries on, exactly as it does when `git` is missing. A capability the host
lacks is not a failed run.

## The boundary, stated plainly

**A command runs in the workspace root with the embedding program's privileges.
It is not sandboxed.** The policy decides what may *start*; it does not decide
what a started process then does. This is the same bound this crate already
states for a registered `Tool` and for a stdio MCP server, and it is the widest
capability the crate grants.

That is a deliberate trade, not an oversight. The `Sandbox` denies network egress
by default and confines writes to its workdir, which is right for a verification
gate and makes `npm install`, `pip install` and `cargo fetch` impossible. A
sandboxed `exec` would have made this release's own purpose unreachable.

Four consequences worth writing down:

- **A policy written for file access does not constrain command execution.**
  `allow_read("*")` and `deny_write("secrets/*")` say nothing about `exec`. If
  you want a boundary on commands, write `Act::Exec` rules; the tier default
  decides everything you did not name, and `Policy::default()` sets it to
  `Effect::Ask`.
- **A command can do things the policy would have refused the agent directly.**
  `cat secrets/prod.env` is a command, not a read, and the exec rules are what
  govern it. Deny the programs you do not want run.
- **A timeout kills the direct child only.** A process that child started itself
  may outlive it, on every platform this crate supports. See below.
- **The environment is inherited.** A build needs `PATH`, `HOME` and the
  toolchain's own variables, so stripping it would break the capability. `stdin`
  is the null device — a command that prompts fails rather than waiting forever
  on a terminal that will never answer — and both output streams are piped.

If none of that is acceptable for your workload, the answer today is not to allow
`exec` at all: leave the tier default at `Deny`, or deny the programs by rule.
Sandboxed execution as an opt-in is later work.

## Where it shows up afterwards

| Question | Where |
| --- | --- |
| what commands did this run ask to run | `steps.tool_call`, and the `ToolCall` observer event |
| which were refused, or went to a human | `policy_events`, `act = "exec"`, with the rule and layer |
| what a gate command printed when it failed | `sandbox_events`, `kind = "gate_output"` |
| which phase of a gate failed | `sandbox_events`, `kind = "gate_phase_failed"` |
| what the agent saw | the step's observation, and the next step's prompt |

## See also

- [Permissions and approval](permissions.md) — layers, tiers, and attribution
- [Language support](language-support.md) — what the harness already knows about
  a project's commands
- [Verification](verification.md) — making a command the run's definition of done
- [Execution sandbox](sandbox.md) — what *is* sandboxed, and how strongly
