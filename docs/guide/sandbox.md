# Execution sandbox

Every external command the harness runs — the verification gate's compile and
test spawns, and any command an agent runs as a tool — goes through a `Sandbox`,
so there is exactly one place model-produced code leaves the harness.

The verification gate compiles and runs model-produced code. Running that
directly on the host was the crate's "compiles locally, no isolation"
limitation, made sharper by many concurrent agents in a tree. The sandbox routes
every such execution through a boundary:

- **Ephemeral workdir** — created per run and **destroyed** on every exit path
  (success, failure, cap kill), so nothing it writes or spawns outlives it.
- **Resource caps that kill, not throttle** — `SandboxLimits` caps CPU time,
  wall-clock, and memory; a breach returns a *typed* cap hit, never a hang. The
  unix mechanisms behind the CPU and memory caps are `RLIMIT_CPU` and an RSS
  monitor; on Windows the Job Object caps memory, CPU and the active process
  count instead, and a cap that was not applied is never reported as hit.
- **Network denied by default** — `SandboxConfig::allow_network` is `false`
  unless you set it. How strong that denial is depends entirely on the backend:
  a kernel boundary on Linux, an SBPL rule on macOS, and only a proxy-env strip
  on the portable floor. The configurable egress *allow-list* is a separate
  mechanism at the policy layer — see
  [MCP and network egress](mcp-and-network.md).
- **Trace** — every create, the argv and the backend that ran it, each cap hit,
  and each teardown land in the rusqlite trace, so an operator can audit *where*
  each piece of code ran and *how* it was isolated.

The default caps are sized so an ordinary `rustc`/`cargo` verification passes
out of the box — a default that failed real compiles would push callers to
disable the sandbox entirely: 60 CPU seconds, 120 wall-clock seconds, 2 GiB
resident, 512 open files, no process cap. Tighten them via the fields for
untrusted work.

## OS-native and OS-neutral

One trait, a real native backend per platform, over a portable floor that runs
everywhere — so a task isolates the same way on mac, linux, and windows:

| Backend | Isolation |
| --- | --- |
| **macOS `sandbox-exec`** | a generated profile confines writes to the workdir and **denies network**; rlimits cap CPU and open files; an RSS monitor caps memory (macOS does not enforce address-space rlimits) |
| **Linux namespaces** | user/mount/pid/**net** namespaces via `unshare` — a *hard* network boundary and a private root — plus the same rlimits and RSS monitor. Requires `setpriv` (util-linux, beside `unshare`) since 0.74.0: the setup runs as uid 0 and the payload would otherwise inherit `CAP_SYS_ADMIN` over the mount namespace it had just made read-only. A host without it skips this rung *(cfg-gated; compiled + unit-tested, not live-run on the macOS build host)* |
| **Windows Job Object** (the default) | a **resource** boundary and only that: caps committed memory, user-mode CPU and the active process count, and kills the whole process tree when the job handle closes. A Job Object has no path rule and no socket rule to set, so filesystem scoping is still the floor's ephemeral workdir and egress denial is still the proxy-env strip (see the note under this table) |
| **Windows AppContainer** (0.59.0, opt-in) | the **access** boundary that job has no facility for: a low-box token that is default-deny on every securable object and reaches only the paths this run resolved, with no capability granting it a socket unless the policy permits egress. Selected by `SandboxConfig::with_access_confinement()`, and it refuses rather than degrading |
| **Portable floor** | the guaranteed minimum on every OS: fresh subprocess, ephemeral workdir, resource caps, network env stripped. Deliberately the **weakest** backend — filesystem-scoped and resource-capped, *not* a full syscall jail |

**Reads, stated plainly (0.80.0).** Every unix rung confines **writes** and does
not confine **reads**. Landlock grants read and execute over the whole
filesystem; the two mount rungs bind the whole tree read-only and then bind the
writable roots back over it, which grants the same thing in a different
vocabulary. So a contained command on macOS or Linux can read anything the
embedding process can read — the operator's home directory, their shell history,
their SSH keys — and cannot write outside the roots the run declared.

That is a deliberate ceiling and not an oversight. A read set narrow enough to be
worth having would have to name every path a process needs to start — the loader,
the shared libraries, the interpreter, the certificate store, the toolchain's own
installation — on every distribution this crate supports, and a list like that is
wrong on the first host it has not seen and fails as an unreadable loader rather
than as a refusal anyone can act on. **What bounds reads is the policy, not the
sandbox**: `Act::Read` is checked for every path a tool opens, and a command a
run executes is bounded by `Act::Exec` on the command rather than by a read rule
on what that command then opens. If a run must not be able to read something, it
belongs outside the machine the run is on.

The two mount rungs grant a temporary directory the run owns; **the Landlock rung
still grants the system one**, and since `sandbox::workdir` puts every run's
ephemeral workspace inside it, two concurrent runs on that rung can read and
rewrite each other's workspace from inside their own sandboxes. 0.80.0
implemented the narrowing and withdrew it: a `git worktree` child's object store
lives in the parent repository, outside its workdir, so the narrowed grant let
the child write its file and refused its commit. Closing it needs a run to be
able to declare a writable root of its own.

**Windows, stated plainly.** Since 0.24.0 a Windows run is contained by a real
Job Object and reports `Backend::WindowsJobObject`. Memory, CPU and the active
process count are real bounds there, and closing the job handle kills the whole
process tree, so nothing the run spawned outlives it. What a Job Object has **no
facility for** is the filesystem and the network — there is no path rule and no
socket rule to set on one. So by default **"sandboxed" on Windows still means
resource-capped rather than access-confined**, and the two must not be read as
the same claim.

**Since 0.59.0 a caller can ask for the other half.**
`SandboxConfig::with_access_confinement()` selects the AppContainer, and a run
that gets it reports `Backend::WindowsAppContainer`, has its writes confined to
the paths the run resolved, and holds no capability to reach the network unless
its policy permits egress. It stays opt-in because the grant set is derived from
the run's own facts and derived is not complete — a toolchain reading a
machine-wide file outside it is refused, and a default boundary that cannot run
an arbitrary payload is worse than one a caller reaches for deliberately. And it
is the one backend here that **does not degrade**: a boundary asked for by name
that cannot be applied is an error naming the grant that failed, because a run
that quietly took the Job Object instead would have no boundary at all while
every assertion about it still passed.

**One column it does not deliver, and 0.26.0 wrote down the reason without
noticing it.** The note below has said since then that loopback is refused inside
an AppContainer, "a stronger result than was asked for". It stopped being a
bonus in 0.48.0, when per-host egress became a loopback proxy the run owns: a
contained command that cannot reach loopback cannot be scoped by it. Measured
again in 0.59.0 under every capability set, and against the host's own network
address as well, with the same answer. So egress under Windows access
confinement is the capability itself — all of the network or none of it — a
contained command there is given no proxy at all, and the agent's own boundary
section tells it so rather than claiming the per-host rules it cannot enforce.

### AppContainer — what 0.26.0 shipped, and what it is not yet

The access half is an AppContainer, and 0.26.0 built it: `io_harness::sandbox::appcontainer`
creates a container profile, derives its SID, grants a path to it with an explicit
ACE, and spawns a process into it through `CreateProcessW` with a process-thread
attribute list carrying two attributes —
`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, which puts the child in the
container, and `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (0.74.0), which decides what
crosses into it. The second is not decoration: without it `bInheritHandles` is a
blanket grant of every inheritable handle this process holds, carrying the access
each was opened with, and no ACL sees it, because a handle does not go back
through an access check. It is proven on the
Windows CI runner, and each claim carries a negative control — the identical
payload outside the container, which must succeed at the thing the container is
refused:

- **Filesystem** — default-deny. A payload inside reads only what was granted;
  a file in an ungranted directory is refused, and the same payload outside reads
  it fine.
- **Network** — the capability array is **empty**, so there is no `internetClient`
  and no socket the token may open. Outbound fails inside and succeeds outside.
  Loopback is refused as well, which is a stronger result than was asked for.

**It is what `Sandbox::select` chooses on Windows when the caller asks for it,
and not otherwise** (0.59.0). Without `access_confinement` a run still gets the
Job Object. The reason is the grant set
rather than the mechanism: an AppContainer is default-deny for *reads*, so
everything a payload legitimately needs has to be named — the workspace is easy,
and the executed binary, the toolchain, the redirected temporary directory and
every language's install tree are the rest. Naming those for arbitrary ecosystems
is a discovery problem this release did not close, and a boundary that is on by
default and cannot run the payload is worse than one a caller opts into
deliberately.

So: the mechanism exists, is tested, and is reachable. The platform table above is
unchanged, because the table describes what a run gets by default and that has not
changed. See [the contract](../CONTRACT.md).

Two differences from the unix mechanisms are worth naming rather than leaving to
be discovered. The job's CPU limit
counts user-mode time only, where `RLIMIT_CPU` counts kernel time as well, so the
cap is weaker on Windows for a kernel-heavy payload. And the memory limit makes
the offending allocation *fail* rather than terminating the process: the payload
is never allowed to hold more than the cap and typically dies of its own failed
allocation. That one is reported from the job's peak-memory accounting at ninety
percent of the cap, because the job records the peak a process reached and never
the allocation it was refused — a run that got that close and then failed, failed
because of the ceiling.

Two further caveats belong beside it. The Linux backend applies no seccomp
filter of its own: whatever syscall tightening a Linux run gets is what the
kernel layers by default under an unprivileged user namespace, not something the
crate installs. And `SandboxLimits::max_processes` is enforced by the **Windows
Job Object alone** — its `ActiveProcessLimit` is the one mechanism this crate
has that bounds the process count per sandbox, and like the job's memory cap it
is not a kill: the job refuses the `CreateProcess` that would cross the limit, so
the run fails because its own spawn failed rather than because it was shot.
macOS and Linux enforce nothing here. Neither maps it to `RLIMIT_NPROC`, because
that limit is per-real-uid rather than per-sandbox and capping it would throttle
the operator's whole login session; the other backend that could scope it
properly is the Linux pid namespace's active-process limit, which is not wired
up. Setting it on a unix host has no effect.

`select` picks the strongest backend the host can actually deliver and records
which ran. The *candidate* is chosen at compile time by cfg, but a native
backend whose primitive turns out to be unavailable degrades to the floor and
**reports the floor**, rather than naming an isolation it did not apply. The
Linux backend probes its `unshare` wrapper once per process by really spawning
it: Ubuntu 24.04 ships
`kernel.apparmor_restrict_unprivileged_userns=1`, and on such a kernel every
wrapped spawn used to fail and the caller was told its code had failed
verification. Since the backend that ran is in the trace, a degraded run is
auditable rather than silent — and a wrapper that fails for some other reason is
an `Error::Sandbox`, never a failed verification.

Use `Sandbox::backend()` on what `select` returns to see what will really run.
`SandboxConfig::floor_only()` forces the floor everywhere, which is how the
selection ladder is exercised in tests.

**Since 0.74.0 the boundary is measured rather than declared.**
`sandbox::BoundaryProbe` attempts a write and a dial outside the boundary before
the run's first step, and `confines_writes()`, `denies_egress()` and the boundary
sentence the agent is given answer from what happened rather than from what the
backend was built to do. Three of that release's findings were a backend reporting
a containment it had not applied, and twice before — 0.40.0 and 0.48.0 — the code
was right about the design and wrong about the host.

Every arm fails closed, and the effect on an ordinary run is worth knowing before
it surprises you: **an arm that could not be attempted answers `false`**, so a host
with no probe program, or none with a directory outside the boundary to aim at, is
told the boundary could not be established even under a backend that would have
confined it. It is worded as what this run could not establish rather than as an
absence of confinement, because only the first is known to be true. One **control**
child runs outside the boundary first, so a host that could never have performed an
arm reports it unmeasured rather than refused.

The probe is taken once per boundary — once per flat run, and once per tree before
the root agent runs, since every agent in a tree shares one containment — so a
child's trace carries no probe row of its own. A run that asked for no containment
is not probed and records nothing: it makes no claim, and a row naming the backend
that *would* have applied is the same misattribution the probe exists to catch.
What lands is one `sandbox_events` row of kind `boundary_probe` at step 0, whose
`detail` names both attempts and how each ended, announced on the event stream like
every other row in that table.

The warning that a backend named a containment this host did not apply is measured
against **this run's** claim — `BoundaryProbe::claimed_confinement` and
`claimed_egress_denial` — and not against the backend's unconditional properties,
because a run that permits network is supposed to see its dial land and a
`FullAccess` run on macOS or Linux still names a confining backend without being
wrapped by it. A **proxied** run claims neither arm: its egress is scoped by the
proxy rather than denied by the boundary, and a tree that will be proxied is
recorded unmeasured, because on Linux the proxy decides which rung the chain picks.
`BoundaryProbe::measure(config, &writable_roots, proxy)` takes that proxy address —
`None` where there is no proxy.

Sandboxing is the **default** for the verification gate and is transparent to it
— the same code passes or fails as before — and a caller who wants direct-host
execution can opt it off with `Verification::no_sandbox()`, so the behaviour is
additive and reversible. `Verification::sandboxed(config)` supplies your own
caps instead of the defaults.

Run it live: `cargo run --example sandbox_run`.

## What else runs in here (0.40.0)

Until 0.40.0 the sandbox had exactly one caller: the verification gate. The
project's own commands — `exec` and the `shell` tools — ran on the host, which was
a decision rather than an omission, and it still is the default. A contract can now
opt them in with `TaskContract::with_contained_exec`; see
[Running commands](command-execution.md) for what that costs and what it buys.

Two consequences land here rather than there:

**Linux confines filesystem writes now, and did not before.** The backend unshared
a mount namespace and then remounted nothing into it, so the namespace existed
while the filesystem view stayed the host's. Only the network namespace was real.
It now remounts the tree read-only and binds back the working directory and the
system temporary directory — the same two places the macOS profile has always
allowed.

**That change reaches the verification gate too**, because the gate uses the same
backend. A gate command that wrote outside its working directory on Linux
succeeded before this release and fails after it. That is the boundary this module
always claimed, arriving late rather than a new restriction — but it is a real
behaviour change and worth knowing if a gate starts failing on Linux only.

## See also

- [Running commands](command-execution.md) — the `ExecMode` that decides where a
  contained command may write, and `ExecMode::ALL` for listing the modes
- [Verification](verification.md) — what the gate the sandbox confines actually proves
- [Permissions and approval](permissions.md) — `Act::Exec` decides *whether* a command runs at all
- [Agent composition](composition.md) — why concurrent agents made isolation urgent
- [Durable runs](durable-runs.md) — why a sandbox is re-created and never resumed
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
