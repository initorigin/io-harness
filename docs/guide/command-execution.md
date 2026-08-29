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
)
// The gate is the project's own command, in whatever language it is written.
.with_verification(Verification::Command {
    argv: vec!["npm".into(), "test".into()],
    expect_exit: 0,
});

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

There is deliberately no shell-string form *on this tool*. It is the whole
injection surface, and nothing `exec` does needs it. An earlier version of this
page said that if a real task later needed a pipeline it would arrive as its own
named tool with its own argument rather than as a mode on this one. That is what
0.24.0 did: see [`shell`](#running-a-command-line) below. `exec` is unchanged,
and a caller who only wants a fixed argv keeps exactly what they had.

## Running a command line

`shell` takes a **line** instead of an array, and is the tool to reach for when
the work genuinely is `a | b`, `a && b`, `a; b` or `a > file`.

```jsonc
{ "line": "cd infra && kubectl get pods | grep CrashLoop" }
```

The line is parsed by this crate. Every sub-command in it is checked against
`Act::Exec` — on the program and on that stage's own joined argv, the same two
targets `exec` checks — and every redirect target is path-resolved and checked
against `Act::Write` or `Act::Read`, the way `write_file` and `read_file` are.

**All of that happens before the first process starts.** A line whose second
stage is denied does not run its first, which is the property that makes the
tool worth having rather than a convenience: partial execution of a refused line
would leave the workspace in a state nobody authorised.

Each stage is then spawned as an argv, exactly the way `exec` spawns one, and
this crate wires the pipes between them. There is no `sh -c` and no `cmd /c` at
any point after the parse — handing the original string to a shell after parsing
it would make the parse decorative.

### What it admits, and what it refuses

Admitted: single and double quotes, backslash escapes, `|`, `;`, `&&`, `||`, the
redirects `>` `>>` `<` `2>` `2>>` `2>&1`, and `cd`, which applies to the rest of
the line.

Refused, each by name and with a reason the model can act on: command
substitution `$( )` and backticks, parameter expansion `$VAR` and `${VAR}`,
arithmetic `$(( ))`, process substitution `<( )`, subshells `( )`, brace groups
`{ }`, here-documents `<<`, background `&`, the `if`/`for`/`while`/`case`
keywords, and the glob characters `*` `?` `[` `]` outside quotes.

The refusals are not a list of known-bad constructs. The lexer permits an
enumerated set of characters and refuses everything else, so a construct nobody
anticipated is refused rather than absorbed into a word. Quoting is the escape
hatch: `grep '$HOME' f` searches for the literal text, because a quoted `$` is
not an expansion.

Globs are worth their own sentence, because refusing them is the surprising
choice. Expanding one here would make the argv depend on the state of the
filesystem at expansion time, so the argv the policy checked and the argv that
ran could differ by anything created in between. Passing it through unexpanded
would hand the model a `*` that a real shell would have expanded, which silently
means something else. Refusing is the only one of the three that cannot be
wrong — use `find` or `list_dir` to choose paths and then name them.

The other half of the same discipline is that the model supplies the **whole**
argv here, which is the opposite of how the git built-ins work — those construct
their own argv and let the model supply only paths. Both are deliberate. `git`
has subcommands that fetch code and options that execute it, so its safe set is a
closed one worth enumerating. `exec` exists precisely to run commands this crate
has never heard of, so the boundary moves from the argv's *shape* to the policy.

### A backslash is an escape, not a path separator

`\` means here what it means in POSIX: outside quotes it escapes the next
character and is itself removed. Nothing about that changes on Windows, so an
unquoted absolute Windows path is taken apart before anything is spawned.

```text
C:\repo\server.exe --port 8080    →  argv[0] = C:reposerver.exe
'C:\repo\server.exe' --port 8080  →  argv[0] = C:\repo\server.exe
"C:\repo\server.exe" --port 8080  →  argv[0] = C:\repo\server.exe
```

**On Windows, quote absolute paths.** Single quotes are the simplest form,
because inside them every character is literal, `\` included; double quotes work
too, since a backslash there escapes only `"`, `\`, `$` and a backtick and is
kept as itself otherwise. This is the grammar rather than a property of one tool,
so it holds for `shell`, for `shell_start`, and for a redirect target as much as
for a program or an argument: `> C:\logs\out.txt` writes to a file called
`C:logsout.txt`.

The failure it produces is worth recognising, because the error names a place the
caller never typed — the spawn fails on `C:reposerver.exe`, a program nobody
wrote. `shell_start` is worse: the handle registers, reports itself running, and
produces nothing.

The grammar is one grammar on all three platforms, and that is the reason there
is no Windows special case for this. A parser that decided which backslashes were
escapes and which were path separators would be guessing, and it would be
guessing about the one thing this tool exists not to guess about — what the
policy checked and what then ran.

## Leaving a command running

`shell` holds the step until its line finishes or times out, which is the wrong
shape for a dev server, a log tail or a watch build — the things whose whole
purpose is not to finish. `shell_start` takes the same single `line` and returns
a handle id instead of a result; `shell_poll` reads what that line has printed,
and `shell_kill` ends it.

```jsonc
{ "line": "npm run dev" }  // shell_start → handle 1
{ "handle": 1 }            // shell_poll  → what it printed since the last poll
{ "handle": 1 }            // shell_kill  → it, and every process it spawned
```

The line is parsed and checked by **exactly** the machinery `shell` uses — the
same parser, the same planner, the same per-stage `Act::Exec` check and the same
per-redirect path check, in the same order and all of it before the first spawn.
The grammar above is the grammar here, a construct `shell` refuses is refused
here with the same reason, and a line with a denied stage still runs no stage. A
handle is a different *lifetime* for a command line, not a second way to run one;
a second way would be a second place for the parse and the processes to disagree.

What changes is only what happens after the check passes. A handle has no
wall-clock timeout at all: the timeout exists on `shell` because a foreground
call has no other way to be told to stop, and a dev server has nothing to be
killed at. What a handle has instead is `shell_kill` and the end of the run.

### What a poll returns

A poll answers two questions — what the line has printed since the previous poll,
and whether it is still running. It is a byte cursor into the line's output
rather than a re-read of it, so polling in a loop shows progress instead of
returning the same log ten times, and a poll before the process has written
anything is empty rather than an error. Bytes and not lines: a half-written line
is a real thing a running process produces, and waiting for the newline would
hide it.

One poll returns at most 16 KiB, and when there is more than that the newest
bytes are the ones kept, because the newest is what a poll is asking about. It
then says how many bytes it skipped rather than eliding them quietly, and the
cursor advances past what was skipped as well as what was read, so the next poll
does not re-deliver the middle of the log. Nothing is lost by the window: every
byte a poll reads is written to the trace as it is read, so "what did that dev
server actually print" is answerable after the process is gone.

The output goes to a capture file rather than to a buffer in memory, and that is
the whole streaming design. The criterion is that a handle nobody polls must not
be able to exhaust memory. A buffer needs a bound, a bound needs a policy for
what to discard, and discarding the middle of a log is how an operator loses the
line that mattered. A file has none of those problems — the kernel writes it, a
poll reads a window of it, the whole of it is still there afterwards, and the
memory cost is the size of one poll rather than the size of the output. The file
lives outside the workspace, because a handle's output is not a file the agent
wrote and must not appear to be one.

### The bounds on a handle

**A run may have eight live handles at once.** The ninth `shell_start` is refused
with a reason naming the cap and telling the model to kill one, rather than
queued: a queue is a leak with a delay, and a process started and then rejected
is a process that ran. Ending a handle makes room for another, so this bounds how
many run at once and not how many a run may start.

**Every live handle is killed when the run ends, however it ends** — a finish, a
budget stop, a cancellation, an error carried out of the loop, or a panic. That
is the property that matters for the operator's machine, and it is why
`shell_kill` is for finishing with something early rather than for tidying up at
the end. Killing a handle that has already ended is not an error either: it
reports how it ended and the run carries on, because a model that lost track of a
handle should be told rather than stopped.

A kill walks every process the handle spawned, in reverse spawn order, so a
pipeline's later stages go before the ones feeding them — killing only the last
stage of `a | b` leaves `a` writing into a closed pipe rather than dead.

**It reaches the whole tree, and that is a guarantee rather than a best effort.**
A dev server starts a package manager which starts a runtime, and then the
package manager exits; the runtime is still the run's responsibility and the
parent/child link that would have led to it is gone. Walking the process table
cannot recover it. So the handle's processes are put in containment at the moment
they are spawned, and the kill closes the containment instead of chasing links:

| | Mechanism | Since |
|---|---|---|
| macOS, Linux | each stage leads its own process group; the kill signals the group | 0.25.0 |
| Windows | each stage is created suspended, assigned to a per-handle Job Object, then resumed; the kill closes the job | 0.26.0 |

Both survive the death of every intermediate process, which is the only property
that matters here. The Windows half was open in 0.25.0 and is closed now; see
[the contract](../CONTRACT.md).

### A handle does not survive the process that started it

If a run is resumed in a new process, every handle the previous process left
running is reported **orphaned**: it is never re-attached, never polled and never
signalled. A poll or a kill naming one is answered from what was recorded, and
the model is told to start again whatever it still needs.

That is a stated guarantee rather than a gap, and the reasoning is in
[the contract](../CONTRACT.md) because a consumer has to be able to depend on it.
The short version: the only thing a checkpoint records about a live process is
its pid, a pid is not an identity, and the operating system may have handed that
number to something unrelated in between.

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

**A command runs in the workspace root, contained, unless the contract says
otherwise (0.46.0).** It may write to the workspace, the system temporary
directory and the detected toolchain's own caches, and nowhere else. The policy
still decides what may *start*; the `ExecMode` decides where what started may
write. `TaskContract::with_full_access()` is how a run asks for the embedding
program's own privileges — the widest capability the crate grants, and up to
0.45.0 what every run got without asking.

Up to 0.45.0 this section said the opposite, and the trade it described was real:
the sandbox denied egress and, as the verification gate used it, threw its working
directory away, which is right for a gate and makes `npm install`, `pip install`
and `cargo fetch` impossible. Both halves are answered now — a contained command
keeps the workspace root, and the toolchain's own cache directories are writable —
so containment no longer costs the capability this module exists to provide.

Four consequences worth writing down, and the first three are unchanged:

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

If none of that is acceptable for your workload, the answer is not to allow
`exec` at all: leave the tier default at `Deny`, or deny the programs by rule.

## The three modes (0.40.0, 0.46.0)

```rust
use io_harness::sandbox::SandboxConfig;
use io_harness::{ExecMode, TaskContract};

// Contained, without asking: the default since 0.46.0.
let default = TaskContract::workspace("run the test suite", "/repo");

// Nothing this run starts may write into the workspace at all.
let audit = TaskContract::workspace("summarise the architecture", "/repo")
    .with_exec_mode(ExecMode::ReadOnly);

// The host's own privileges — a decision, on the page.
let wide = TaskContract::workspace("upgrade the toolchain", "/repo")
    .with_full_access();

// Contained *and* capped: the mode is a boundary, the config adds ceilings.
let bounded = TaskContract::workspace("run untrusted code", "/repo")
    .with_contained_exec(SandboxConfig::new());
```

The default carries **no resource cap at all**. Containment says where a command
may write; a 120-second wall clock would say how long your build may take, and
that is not a default this crate can set for you. `with_contained_exec` is where
the ceilings live.

Every command `exec` and `shell` start is wrapped by the backend this host
offers. The working directory stays the **workspace root**, which is the part
that makes it usable: nothing is copied to a temporary directory and nothing is
discarded, so `target/`, `node_modules/` and an incremental build survive from one
command to the next.

What a contained command gives up:

| | Contained (the default) | `with_full_access()` |
| --- | --- | --- |
| working directory | the workspace root | the workspace root |
| writes outside the workspace | refused on macOS and Linux, except the toolchain's own caches | allowed |
| outbound network | only if the policy permits `Act::Net`, and then to every host | whatever the host allows |
| resource caps | the config's, and a kill names the cap | none |
| audit | `sandbox_events` records the backend that applied | nothing |

Three things are worth knowing. Egress is a single boolean, so a policy naming one
host opens all of them *at the sandbox wall* — the crate's own tools are still
checked per host. A toolchain that writes to a user-level cache
(`~/.cargo/registry`, `~/.npm`) **works**, because since 0.46.0 the detected
toolchain's cache directories are writable roots; a build that writes somewhere
else the caller configured needs `with_full_access()`. And the `shell_start`
handles are not contained at all, because a handle outlives the call that made
it.

[The contract](../CONTRACT.md) states the per-platform table: a Windows run gets
the resource caps and neither a filesystem nor a network boundary by default,
because a Job Object has neither facility — and gets both when the caller asks
for access confinement, which selects an AppContainer instead (0.59.0). Per-host
egress is the one thing that does not follow: a process inside that container
cannot reach the loopback proxy the rules are enforced by, so its network answer
is the capability alone.

### Listing the modes (0.71.0)

`ExecMode::ALL` is the three modes as data, widest confinement first, for an
application that offers them to an operator — a `--exec-mode` flag, a settings
screen, a config validator. `ExecMode::as_str` is the label each one carries in
the trace, in the agent's own boundary prompt, and as the `[sandbox] mode` key in
a config file.

```rust
use io_harness::ExecMode;

assert_eq!(
    ExecMode::ALL,
    [ExecMode::ReadOnly, ExecMode::WorkspaceWrite, ExecMode::FullAccess]
);
assert_eq!(ExecMode::ReadOnly.as_str(), "read-only");
```

**The list has to come from here, because a hand-written one is not
self-policing.** `ExecMode` is `#[non_exhaustive]`, so a match on it outside this
crate needs a wildcard arm; a mode added in a later release lands in that arm
rather than in your menu, and your build keeps passing while the menu quietly
stops offering the whole boundary. Only *removing* a mode would break you. That
is the opposite of [`Effect`](permissions.md#listing-the-three-effects-0710),
which is not `#[non_exhaustive]`: a stale hand-written list of effects stops
compiling the day it goes stale. The enum that decides where model-produced code
may write is the one whose list cannot be policed downstream.

`ALL` is kept complete in-crate by an exhaustive `match`, not by asserting its
length — a length check against a literal `3` is the same stale hand-written list
moved inside the fix.

Only the *menu* half was ever at risk. Reading a mode back has always been
strict: `[sandbox] mode` deserializes through `ExecMode`'s own kebab-case labels
with no `#[serde(other)]` arm, so an unrecognised string in a config file is a
hard error rather than a silent fall back to the default.

## Where it shows up afterwards

| Question | Where |
| --- | --- |
| what commands did this run ask to run | `steps.tool_call`, and the `ToolCall` observer event |
| which were refused, or went to a human | `policy_events`, `act = "exec"`, with the rule and layer |
| what this run started and left running | `process_handles`, one row per handle with the line, its processes and how it ended |
| what a handle printed | `handle_output`, appended as each poll read it |
| what a gate command printed when it failed | `sandbox_events`, `kind = "gate_output"` |
| which phase of a gate failed | `sandbox_events`, `kind = "gate_phase_failed"` |
| what the agent saw | the step's observation, and the next step's prompt |

## See also

- [Permissions and approval](permissions.md) — layers, tiers, and attribution
- [Language support](language-support.md) — what the harness already knows about
  a project's commands
- [Verification](verification.md) — making a command the run's definition of done
- [Execution sandbox](sandbox.md) — what *is* sandboxed, and how strongly
