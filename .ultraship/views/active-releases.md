<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.7.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** A developer starts a long task and walks away. The harness runs unattended for a long horizon (24h+) with no user input, and it survives a crash or a full process restart without losing or corrupting the run. After every completed step of every agent, the harness durably checkpoints — the agent's loop position, its intermediate results, its draw against the 0.5.0 aggregate budget, and any pending policy decision — in one transactional rusqlite commit before the next step begins. On restart the harness reopens the store, reconstructs the whole 0.5.0 agent tree (parents, children, nesting, shared workspace, shared trace) from the checkpoint, and every agent continues exactly where it stopped.
Resume is idempotent by construction. A completed step is never re-run, the aggregate budget (agent count, tokens, cost, time) is never double-charged for work already done, and an irreversible action already applied — a file edit committed, an approval already consumed — is not applied a second time. A crash loses at most the one step that was in flight, and that step replays cleanly because nothing about it was committed. The 0.6.0 sandboxes are ephemeral and die with the process; on resume they are re-created per run as before, and an execution that was in flight at crash time is simply re-run inside a fresh sandbox — safe precisely because the sandbox is per-run and its result was never checkpointed.
Human approval spans the restart. When the 0.4.0 policy marks an action sensitive, the whole tree pauses, the pending action is persisted, and the process can exit entirely; when the human's decision arrives — minutes or hours later, in the same process or a fresh one — the tree resumes from the persisted pending action and continues. Unattended does not mean unsupervised: the run stops for a human exactly when policy demands and only then.
Every checkpoint, every resume, and every "step skipped because already done" is recorded in the rusqlite trace alongside the 0.2.0–0.6.0 records, so an operator can open the store after a 24h-with-two-crashes run and reconstruct the whole history — what ran, what was checkpointed, where each crash happened, and what replayed — from the store alone.


**Blockers:**

- OWNER APPROVAL — the git Release and `cargo publish` to crates.io (target_mode: published) are owner-gated and NOT done. Publishing is public and irreversible. The crate is release-ready and all package gates are green (cargo test 136 passed / clippy 0 / build --release / publish --dry-run for v0.7.0). CARGO_REGISTRY_TOKEN is in .env. This is the remaining implementation step, handled at /ultraship:complete.

- DEFERRED LIVE RUN (not a defect) — examples/durable_run.rs (the live unattended-then-resumed run against OpenRouter) compiles clean but is NOT executed here: it needs the API key and real spend, so it runs at /ultraship:complete, exactly as examples/sandbox_run did for 0.6.0. The literal 24h wall-clock run is out of scope by design (contract excluded scope); the horizon is proven by the real-SIGKILL resume test plus a 250-step long unattended run.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
