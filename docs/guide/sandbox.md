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
| **Linux namespaces** | user/mount/pid/**net** namespaces via `unshare` — a *hard* network boundary and a private root — plus the same rlimits and RSS monitor *(cfg-gated; compiled + unit-tested, not live-run on the macOS build host)* |
| **Windows Job Object** | a **resource** boundary and only that: caps committed memory, user-mode CPU and the active process count, and kills the whole process tree when the job handle closes. A Job Object has no path rule and no socket rule to set, so filesystem scoping is still the floor's ephemeral workdir and egress denial is still the proxy-env strip (see the note under this table) |
| **Portable floor** | the guaranteed minimum on every OS: fresh subprocess, ephemeral workdir, resource caps, network env stripped. Deliberately the **weakest** backend — filesystem-scoped and resource-capped, *not* a full syscall jail |

**Windows, stated plainly.** Since 0.24.0 a Windows run is contained by a real
Job Object and reports `Backend::WindowsJobObject`. Memory, CPU and the active
process count are real bounds there, and closing the job handle kills the whole
process tree, so nothing the run spawned outlives it. What a Job Object has **no
facility for** is the filesystem and the network — there is no path rule and no
socket rule to set on one. macOS confines writes to the workdir and denies
outbound network, Linux does the same through mount and network namespaces, and
Windows does neither, so "sandboxed" on Windows means resource-capped rather than
access-confined and the two must not be read as the same claim. The access half
is `AppContainer`, scheduled for 0.26.0.

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

Sandboxing is the **default** for the verification gate and is transparent to it
— the same code passes or fails as before — and a caller who wants direct-host
execution can opt it off with `Verification::no_sandbox()`, so the behaviour is
additive and reversible. `Verification::sandboxed(config)` supplies your own
caps instead of the defaults.

Run it live: `cargo run --example sandbox_run`.

## See also

- [Verification](verification.md) — what the gate the sandbox confines actually proves
- [Permissions and approval](permissions.md) — `Act::Exec` decides *whether* a command runs at all
- [Agent composition](composition.md) — why concurrent agents made isolation urgent
- [Durable runs](durable-runs.md) — why a sandbox is re-created and never resumed
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
