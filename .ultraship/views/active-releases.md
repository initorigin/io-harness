<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.24.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Give an agent a command line and have the boundary hold. Today the crate's `exec` tool takes an argv array and spawns `argv[0]` directly — no `sh -c`, no joining — which is what makes its permission check meaningful and also what makes half of real work unreachable: `cd infra && kubectl get pods | grep CrashLoop` is not a thing the model can express, and every pipeline, redirect and command sequence has to be smuggled in as a script the harness cannot see inside of. This release adds a `shell` tool that parses what the model wrote, walks the parse, and checks every sub-command's argv against `Act::Exec` and every redirect target against `Act::Write` or `Act::Read` — the same rules a fixed argv has always been checked against, applied to each element of the line rather than to a string. What it cannot resolve statically it refuses: command substitution, variable expansion in command position, `eval`, subshells, heredocs and control flow are named refusals with reasons, not guesses. Nothing in a refused line runs, and a pipeline whose second stage is denied does not execute its first.
Windows gains the containment the sandbox has been missing. Memory, CPU and active process count become real bounds on a Windows run rather than mapped fields nothing applies, the whole process tree dies with the job, and `SandboxLimits::max_processes` becomes enforced for the first time on any platform. What it does not become is "native" in the sense the macOS and Linux rows of the platform table mean: a Job Object contains resources and nothing else.
**Amended by `US-IO-HARNESS-0.24.0-I01`:** this paragraph promised that long-running work — a dev server, a log tail, a twenty-minute build — would be started, polled and killed through process handles instead of blocking a step until the timeout. Handles are deferred to 0.25.0 and that promise is withdrawn from this release rather than left standing. The Job Object stays, because it bounds every sandboxed run whether or not anything long-running is holding it.
And a directory can be listed. `find` walks the whole tree against a glob; there is no way to ask what is in one directory, which is the first thing anyone does in an unfamiliar repository.
Finally, the entry point stops describing a product this is no longer. `TaskContract::workspace` stops demanding a success criterion as a positional argument — it becomes `workspace(goal, root)` defaulting to `Verification::None`, with `.with_verification(v)` for the projects that have a test command — and the documentation leads with `Session`, the durable conversation, ahead of `run_with`, the one-shot verified run.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
