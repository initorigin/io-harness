<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.6.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** A developer runs a task whose model-produced code no longer touches the host directly: every execution the harness performs — the execution-based verification gate that has compiled and run model output since 0.2.0, and any command an agent runs as a tool — happens inside an ephemeral local sandbox created for that one run, given an isolated working directory seeded from the task workspace, run under resource caps (CPU time, wall-clock, memory, process count, file descriptors), with network denied by default, and then torn down so nothing it wrote or spawned outlives it. Output (stdout, stderr, exit status, produced files copied back under policy) is captured and returned exactly as the un-sandboxed path returned it, so verification still passes or fails on real execution.
The sandbox is both OS-native and OS-neutral. A single `Sandbox` trait has one real native backend per platform — macOS via a `sandbox-exec` profile plus rlimits, Linux via user+mount+ net+pid namespaces with seccomp and rlimits over a tmpfs, Windows via a Job Object (kill-on-close, memory/CPU/active-process caps) with a restricted token — over a portable floor backend (fresh subprocess, ephemeral tempdir workspace, resource caps, network env stripped) that compiles and runs on all three so isolation is never absent on any OS the crate builds for. The crate selects the strongest backend available at runtime and records which one ran. A caller who wants the exact 0.5.0 behaviour can opt the sandbox off, but sandboxed execution is the new default for the verification gate and the agent command tool.
Every sandbox lifecycle event — create, the exact argv and backend used, any resource cap hit, a denied network attempt where the backend can observe it, and destroy — is recorded in the rusqlite trace alongside the existing step, refusal, and spawn records, so an operator can audit where each piece of code ran and why it stopped. A sandbox that fails to start, exceeds a cap, or is denied an action returns a typed result the agent adapts to, exactly as an out-of-policy action does in 0.4.0 — it never panics or takes down the run or the tree.


**Blockers:**

- OWNER APPROVAL — the git Release and `cargo publish` to crates.io (target_mode: published) are owner-gated and NOT done. Publishing is public and irreversible. The crate is release-ready and all package gates are green (test/clippy/build/dry-run). CARGO_REGISTRY_TOKEN is in .env. This is the only remaining implementation step (T10).

- TEST GAP (minor, flagged) — T08's "a child sandbox start-failure does not take down siblings" is proven indirectly (typed start-failure unit test + run_tree's existing error isolation shown by the subagents suite) but has no single tree+start-failure injection test, because making one child's rustc unspawnable is not reachable through the public API. Owner to accept or request a test hook before /ultraship:complete.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
