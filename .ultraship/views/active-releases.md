<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.25.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Start something that does not finish, and stay in control of it. Every tool this crate has ever had is a call that returns: `exec` and, since 0.24.0, `shell` run a command line to completion or to a timeout, and one step of the loop is blocked for the whole of it. So a dev server, a log tail, a watch build and a twenty-minute compile are not slow here — they are unrepresentable. The only way the model can express one is to run it in the foreground and lose the loop until a 900-second timeout decides for it, which is not a decision anyone made. This release adds three tools: `shell_start` returns a handle instead of a result, `shell_poll` reads the output produced since the last poll, and `shell_kill` ends the process and everything it spawned. The line is parsed and checked by exactly the machinery 0.24.0 built — same allowlist lexer, same per-stage `Act::Exec` check, same path-resolved redirect targets, all before the first spawn — so a handle is a different lifetime for a command line, not a second way to run one.
A handle recorded by a previous process is orphaned on resume and never re-attached, polled or signalled. This is the one place this crate could damage something outside its own workspace: a PID recorded before a crash may since have been reused by an unrelated process, and a resumed run that signals it kills a stranger's program. So orphaning is unconditional rather than a best-effort re-attach, and the handle is readable in the store with a reason afterwards rather than silently dropped.
And the agent stops paying a step to find out what it just broke. When an edit changes a file in a project whose toolchain the crate already detects, the project's own checker runs against it and its diagnostics come back attached to the edit — file, line, span, message, and the exact rendered error a human would see in a terminal. Today a type error costs a step, a build and a provider call to discover; attached to the edit result it costs nothing, and the model corrects it inside the same turn it made it.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
