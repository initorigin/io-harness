# Changelog

All notable changes to **IO Harness** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**This file is the single source of truth for release notes.** When a release is
cut, the notes for that version are taken verbatim from its section below, so
keep every entry clear, user-facing, and complete. See
[docs/CHANGELOG_STRUCTURE.md](docs/CHANGELOG_STRUCTURE.md) for the required
structure and [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md) for how release
notes are produced from it.

## [Unreleased]

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.7.0] - 2026-07-25

Durable, unattended runs. A run can be left alone for a long horizon and survive
a crash or a full process restart: after every completed step the harness commits
that step and a checkpoint marker in one rusqlite transaction, and on restart it
resumes every agent — a single run or a whole 0.5.0 tree — from its own last
committed step, without re-running finished work, double-charging the budget, or
re-applying an edit already made.

### Added

- **Durable step-level checkpoint.** After every completed step, the step's trace
  row and a `checkpoint` event are written in one rusqlite transaction, so the
  committed checkpoint *is* the step's completion marker: a crash leaves either a
  whole step or none of it, never a torn half recorded as done. Backed by an
  additive `checkpoint_events` table and a `PRAGMA user_version` format stamp; a
  0.6.0 database migrates in place.
- **Whole-tree resume — `resume_tree`.** Reconstructs a crashed 0.5.0 tree from
  the store (parent/child edges, shared workspace, shared trace) and re-drives
  every unfinished agent from its own checkpoint. On replay a parent *adopts* the
  children it had already spawned — keyed by (parent, step, goal) and persisted in
  a new `spawns` table — and resumes each from its own last step instead of
  duplicating it.
- **Durable aggregate budget.** The shared `Ledger` is restored on resume from the
  tree's durable totals (`Ledger::from_state`), so a resumed run draws against one
  continuous ceiling rather than a reset one. The time budget counts real
  wall-clock elapsed across the downtime (from a stored `started_at`), not just the
  current process's uptime.
- **`RunStatus` + `Store::run_status`.** A durable `running` / `paused` /
  `completed` / `failed` status, so a caller can tell a crashed run (still
  `running`, the resume target) from one paused for a human or already finished.
- **Approval across a full restart — `resume_tree_with_decision`.** A 0.4.0
  sensitive action that pauses a tree now survives the process exiting entirely; a
  fresh process delivers the decision and resumes the whole tree from the persisted
  pending action.
- **`Error::Resume`.** A resume against a newer-format or missing checkpoint returns
  a typed error the caller handles — never a panic and never a silent half-resume.
- **`examples/durable_run.rs`.** A live unattended run against OpenRouter that is
  killed mid-run and resumed in a fresh process to a verified result.

### Changed

- **Checkpointing is on by default and is idempotent.** A completed step is skipped
  on resume (recorded as a `skipped` event), an irreversible edit is re-observed
  rather than repeated, and re-running a resume is a no-op. Ephemeral 0.6.0
  sandboxes are never checkpointed — an exec in flight at crash time simply re-runs
  in a fresh sandbox. Existing 0.6.0 callers compile unchanged and reach the same
  verified result.

### Security

- **A run can now be left unattended safely.** The whole tree pauses for a human
  only when the policy demands and continues once a decision arrives, even across a
  restart; nothing about a crashed run is lost or silently re-executed.

## [0.6.0] - 2026-07-24

Execution sandbox. Every command the verification gate runs — the `rustc`
compile and the test binary it has run since 0.2.0 — now executes inside an
ephemeral, per-run sandbox, so model-produced code no longer runs on the host
directly. The sandbox is OS-native and OS-neutral: one trait, a native backend
per platform over a portable floor that runs everywhere.

### Added

- **`Sandbox` trait + `select`.** One OS-neutral execution abstraction (RPITIT,
  no OS-specific type in its signature) that every external command routes
  through. `select` picks the strongest backend available on the running OS and
  records which ran, so an audit shows not just what code ran but how it was
  isolated.
- **A native backend per OS, over a portable floor.**
  - **macOS `sandbox-exec`** — a generated profile confines filesystem writes to
    the run's workdir and denies outbound network; `RLIMIT_CPU` caps CPU and an
    RSS monitor caps memory (macOS does not enforce address-space rlimits). Live-run.
  - **Linux namespaces** — user/mount/pid/**net** namespaces (a hard network
    boundary) plus rlimits. cfg-gated; compiled and unit-tested, not live-run.
  - **Windows Job Object** — kill-on-close plus memory / active-process / CPU
    limits and a restricted token. cfg-gated; compiled and unit-tested, not live-run.
  - **Portable floor** — the guaranteed minimum on every OS: fresh subprocess,
    ephemeral workdir, resource caps, network env stripped. Deliberately the
    weakest backend (filesystem-scoped + resource-capped, not a syscall jail).
- **`SandboxLimits` — resource caps that kill, not throttle.** CPU time (SIGXCPU
  via `RLIMIT_CPU`), memory (RSS poll-and-kill), and wall-clock (timeout). A breach
  returns a typed `Cap` hit, never a hang. Serde-serializable like `Policy` and
  `Containment` so io-cli and io-studio load it from config.
- **Default-deny network.** Every backend denies outbound network by default,
  enforced by the sandbox and not the prompt. A configurable egress allow-list is
  deferred to 0.8.0.
- **Guaranteed per-run teardown.** The workdir is a `tempfile::TempDir` removed on
  every exit path (success, failure, cap kill), and the child is `kill_on_drop`, so
  no directory and no orphan process leak.
- **Policy-filtered copy-back.** Files a sandboxed command produces are copied back
  to the task workspace only where the 0.4.0 write policy allows, so isolation
  composes with the permission layer instead of bypassing it.
- **Sandbox trace.** An additive rusqlite `sandbox_events` table records create,
  the argv and backend that ran it, cap hits, and destroy — reconstructable from a
  reopened store. A 0.5.0 database migrates in place.
- **`examples/sandbox_run.rs`.** A live run showing a model's code compiled inside
  the sandbox, a resource cap killing a runaway, network denied, and teardown
  leaving nothing behind.

### Changed

- **Sandboxed execution is the new default for the verification gate**, and it is
  transparent — the same code passes or fails as before. A caller who wants the
  exact 0.5.0 direct-host execution opts it off with `ExecGuard::no_sandbox()`, so
  the change is additive and reversible. In a 0.5.0 tree, each child's verification
  runs in its own sandbox, so isolation stacks on top of containment.

### Security

- **Model-produced code no longer executes directly on the host** — the
  "compiles locally, no isolation" limitation carried since 0.2.0 and made sharper
  by 0.5.0's concurrent agents is closed. A sandbox that fails to start returns a
  typed `Error::Sandbox`, so one failed child never takes down its siblings.

## [0.5.0] - 2026-07-24

Sub-agent composition: a parent decomposes a task at run time and spawns
sub-agents on demand, bounded by an operator-held containment ceiling. This is
the release that turns io-harness from a single-agent harness into an
agent-composition engine.

### Added

- **`spawn_agent` tool.** A typed action any agent may invoke to launch a
  sub-agent with its own goal, target, verification, and optional narrowing
  constraints. The child runs the same observe/reason/act/verify/stop loop from
  `run.rs` — not a second implementation — over the shared workspace and the
  single rusqlite store, so the whole tree is one auditable run.
- **Shared context and compose-back.** A child receives the shared workspace
  root, the shared trace, and a parent-supplied context brief. When it finishes,
  its `RunOutcome` and a result summary (produced paths, verified/failed, steps,
  spend used) return to the parent as the `spawn_agent` tool result, so the
  parent's next model call reasons over what the child actually did.
- **Concurrent fan-out to 100+.** A parent may request many children in one step;
  they run as bounded concurrent tokio tasks under `max_concurrent`. Spawns
  beyond `max_concurrent` queue; spawns beyond `max_total_agents` are refused. A
  stress test exercises the 100+ simultaneous-agent target without deadlock or
  overspend.
- **Bounded nesting.** A child may spawn its own children; `max_depth` caps how
  deep, counted from the root so a long chain cannot reset it.
- **`Containment` value.** Handed in once at root construction, carrying
  `max_total_agents`, `max_concurrent`, `max_depth`, and an aggregate spend
  ceiling (`max_total_tokens`, optional `max_total_cost`, optional
  `max_total_duration`). Serde-serializable like `Policy`, so io-cli and
  io-studio load it from config.
- **Containment merge — inherit-and-narrow only.** A child's effective policy is
  derived from the parent's: denies union, allows intersect, sensitive tier
  tightens only. A child can never read, write, or execute anything its parent
  could not. This is a separate code path from 0.4.0's `Policy::merge` (which
  widens via allows-union) precisely so the two are never confused. Enforced in
  the harness, never the prompt; holds downward through arbitrary depth.
- **Tree-wide spend ceiling above the task contract.** The aggregate budget is
  drawn down by the whole tree together. No spawned `TaskContract` can raise it —
  a child contract may set a tighter per-child budget but never a looser one than
  the tree has remaining. When the aggregate is exhausted the tree halts as a
  whole; in-flight children finish their current step, then stop.
- **Spawn refusal semantics.** A spawn breaching any cap (agents, depth,
  remaining budget, or a widened policy) returns a typed refusal to the
  requesting agent as its tool result, does not panic or abort the tree, and is
  recorded — the requesting agent can adapt, exactly as with an out-of-policy
  action in 0.4.0.
- **One approver for the tree.** Sensitive actions in any child route to the same
  `Approver` the root run was given; `Approve`/`Deny`/`Defer` are unchanged, and
  a child's `Defer` persists and is resumable via `resume_with_decision`.
- **Deterministic aggregate accounting.** The shared budget ledger is updated
  under a single lock, so many concurrent agents cannot overspend past the
  ceiling through a race. A concurrent-overspend stress test asserts total
  recorded spend never exceeds `max_total_tokens`.
- README and crate docs covering `spawn_agent`, `Containment` and every cap, the
  containment merge versus 0.4.0 layer merge, and the tree-wide spend ceiling;
  `examples/subagents.rs` drives a live run where a parent spawns children under
  a `Containment`, showing compose-back and one containment refusal end to end.

### Changed

- The rusqlite schema gains a `parent_run_id` on runs (null at root), spawn-event
  records, containment-refusal records, and budget-draw records, so the tree is a
  reconstructable graph and the aggregate spend is auditable after the fact.
  Additive only — a 0.4.0 database migrates in place and a 0.4.0 binary still
  reads a migrated database.

### Security

- **Sub-agents are opt-in.** The `spawn_agent` tool exists only when the root run
  is constructed with a `Containment`. A 0.4.0 caller that constructs none gets
  no spawn tool and the exact 0.4.0 surface and behaviour — `run_with`, `resume`,
  `resume_with_decision`, `Policy`, and `Approver` are unchanged.
- **Containment is enforced in the harness, not the prompt.** A child requesting
  a widened policy or an over-cap spawn is refused even when the model asks for it
  directly. No child at any depth can hold an effective allow, or a looser budget,
  than the root granted.
- Spawn, refusal, and budget-draw records carry agent ids, paths, commands,
  rules, layers, decisions, and token counts only — never file contents or
  credentials, consistent with 0.4.0.
- **Not isolated: children still compile model-produced code directly on the
  host** (the execution risk carried since 0.2.0), now multiplied by the fan-out
  factor. 0.5.0 bounds what the tree may touch and spend, not where code runs;
  per-run sandboxing is the next release (0.6.0).

## [0.4.0] - 2026-07-24

### Added

- **Permission policy.** `Policy` is a stack of named layers plus a per-action
  default, evaluated deny-first across the whole stack, so a layer can add
  capability but can never re-allow what a layer beneath it denied. Rules cover
  reads, writes, and command execution.
- **Enforcement in the tool layer, not the prompt.** `grep`, `find`, `read_file`,
  and `write_file` consult the policy before touching the filesystem, so a model
  that ignores its instructions still cannot act outside it. Denied paths produce
  no search results, so they cannot be exfiltrated into the model's context.
- **Canonical path and symlink rules.** Paths are evaluated after `..`
  resolution, and a deny matches when either a symlink's own path or its resolved
  target matches. A link resolving outside the workspace root is refused.
- **Secret paths denied by default.** `.env`, `*.pem`, `id_rsa`, `id_ed25519`,
  and `*.key` are denied on read and write under `Policy::default()`, even inside
  an otherwise readable tree.
- **Command execution policy.** `ExecGuard` gates what verification may spawn.
  `rustc` and `TEST_BINARY` are allowed by default; denying `TEST_BINARY` while
  allowing `rustc` type-checks produced code without ever running it. Every spawn
  is recorded with its full argv.
- **Human-approval gate.** `Approver` is one object-safe trait with three
  decisions — approve, deny, defer. The decision future may stay pending
  indefinitely; the run waits rather than timing out. Built-ins: `ApproveAll`,
  `DenyAll`, `StdinApprover`.
- **Approve-with-changes and approve-and-remember.** An approval may rewrite the
  action or remember rules for the rest of the run. Both are re-checked against
  the policy: an approval cannot move an action across a deny, and a remembered
  allow cannot override one. Remembered rules are returned on
  `RunResult::remembered` for the caller to persist.
- **Deferred approval across processes.** `Decision::Defer` stops the run with
  `RunOutcome::AwaitingApproval { request_id, steps }` and persists the pending
  action, including the content the human was shown. `resume_with_decision`
  continues the run under its original id once a decision arrives, re-checking
  the policy so a deny that landed while it waited still holds.
- **Policy in the trace.** Refusals record the action, target, rule, and the
  layer that rule came from; decisions record their value, source, and the
  performed form when an approval rewrote the action. An action auto-approved by
  a remembered rule is distinguishable from a fresh approval.
- **`Policy::explain`** returns the decision for a path with its rule and layer.
  It *is* the enforcement function, so an explanation can never describe a
  boundary different from the one enforced.
- **Serde-serializable policy and `Policy::merge`**, so io-cli and io-studio read
  one format and compose their own config over a shared base. The crate composes
  a stack it is handed; it does not discover config files.
- `run_with`, and `examples/policy_run.rs` driving a live run under a
  restrictive policy.
- `Policy::is_permissive`. Passing a non-permissive policy together with a
  single-file contract now returns an error instead of running unenforced —
  single-file mode has no policy-aware tool layer in this release, and silently
  ignoring a policy would leave a caller believing a boundary existed.

### Changed

- The rusqlite schema gains `policy_events` and `pending_approvals`. Additive
  only — a 0.3.0 database migrates in place and a 0.3.0 binary still reads it.
- `RunResult` gains a `remembered` field; `RunOutcome` gains `AwaitingApproval`
  and `Denied`.

### Security

- A refused action is reported to the model as a tool result it can adapt to and
  consumes a step, so a model repeatedly requesting a denied action reaches the
  step cap rather than looping.
- Refusal and decision records carry paths, commands, rules, and decisions only —
  never file contents or credentials.
- **The default is permissive.** A caller who passes no policy gets no
  enforcement and the exact 0.3.0 behaviour. The boundary is opt-in; this is a
  deliberate backward-compatibility trade-off, and existing 0.3.0 callers compile
  and behave unchanged.

## [0.3.0] - 2026-07-24

Repository-wide work and provider choice: the agent can search a whole workspace
and edit several files in one run, and you pick OpenRouter, Anthropic, or OpenAI
at run construction — behind the same provider-agnostic surface.

### Added

- Workspace tasks: `TaskContract::workspace(goal, root, verify)` runs a
  multi-tool loop where the agent uses `grep` (regex/substring over file
  contents), `find` (name/path glob), `read_file`, and a path-taking
  `write_file` to edit several files under one root. All tools are confined to
  the root — an absolute path or a `..` that escapes it is refused. The grep/find
  walk skips `.git`, `target`, and `node_modules`.
- Multi-file verification: `Verification::EachCompilesRust(files)` (every listed
  file compiles on its own) and `Verification::WorkspaceTestPasses { files,
  test_src }` (the files, concatenated, compile and pass a test together) — the
  run only succeeds when the whole edited set meets its spec.
- Anthropic provider (`Anthropic`, `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL`) over
  the own HTTP + SSE client, parsing Anthropic's `/v1/messages` streaming format.
- OpenAI provider (`OpenAi`, `OPENAI_API_KEY` / `OPENAI_MODEL`) sharing the
  OpenAI-style chat/completions transport with OpenRouter.
- The run trace now records which provider ran (`Store::provider(run_id)`); the
  `Provider` trait gained a defaulted `name()` for the label.

### Changed

- `Provider` gained a `name()` method with a default, so existing implementers
  keep compiling; the built-in providers override it.

### Migration

- 0.2 callers are unchanged: `TaskContract::new`, `run`, `resume`, and the
  single-file loop behave exactly as before. A 0.2 rusqlite database gains a
  `provider` column in place on open (additive; a 0.2 binary still reads it).

## [0.2.0] - 2026-07-24

Trust a longer run: budgets, retry, a full trace, resumable runs, and
execution-based verification that compiles the produced file so a substring stub
cannot pass.

### Added

- Execution-based verification: `Verification::CompilesRust` (the produced file
  must compile) and `Verification::RustTestPasses { test_src }` (it must compile
  and pass an appended test). Compilation runs `rustc` in a throwaway temp dir
  with no network, closing the 0.1.0 hole where a substring stub passed
  `FileContains`.
- Step, time, and cost budgets on `TaskContract` — `with_time_budget`,
  `with_token_budget` (cost is counted in tokens), and the existing
  `with_max_steps` — each with a distinct stop reason:
  `RunOutcome::TimeBudgetExceeded` and `RunOutcome::CostBudgetExceeded`.
- Retry with escalation: `with_max_retries` retries a failing provider/tool
  step, records every attempt in the trace, then escalates the error.
- Full trace: each step now persists its prompt, tool call, and token usage
  alongside the decision and result (`StepRecord`).
- `resume(contract, provider, store, run_id)` continues an interrupted run from
  its persisted state under the original run id instead of restarting.
- `Usage` (prompt/completion/total tokens) on `CompletionResponse`, populated
  from the OpenRouter stream usage summary.

### Changed

- `Store::record_step` replaced by `Store::record(run_id, &StepRecord)`; the
  `steps` table gains `prompt`, `tool_call`, and `tokens` columns and migrates a
  0.1.0 database in place on open.
- `Verification::check` (sync) replaced by `Verification::passes(path, contents)`
  (async), since execution-based gates run a compiler.
- `CompletionResponse` gained a `usage` field; construct it with
  `..Default::default()` for forward compatibility.

### Deprecated

### Removed

### Fixed

### Security

## [0.1.0] - 2026-07-23

First working slice: run one AI agent from a typed task contract to a verified
file edit, in-process.

### Added

- `TaskContract` — typed goal, target file, constraints, verification criterion,
  and step cap.
- Orchestration loop (`run`) — observe, reason, act, verify, stop; stops on the
  first passing verification or when the step cap is reached
  (`RunOutcome::Success` / `RunOutcome::StepCapReached`).
- Deterministic verification layer — `Verification::FileContains` and
  `Verification::FileEquals`. Deterministic (no model in the loop), so results
  are reproducible. Note: these are **content checks only** — they confirm the
  expected text is present, not that the artifact compiles or is semantically
  correct, so a model can satisfy a substring without meeting full intent.
  Execution-based verification is planned for 0.2.
- Filesystem tool — reads the target file into context, writes the agent's edit
  back; a missing file reads as empty so the agent can create it.
- Provider-agnostic `Provider` trait with no vendor type in the public API, and
  an `OpenRouter` implementation over an own HTTP + SSE client that parses
  streamed tool-call fragments. Credentials read from `OPENROUTER_API_KEY`;
  model from `OPENROUTER_MODEL` (no default guessed).
- Run state in rusqlite (`Store`) — steps, decisions, and intermediate results
  persisted and read back for audit.
- End-to-end integration test (mock provider) and a live OpenRouter example.

### Security

- OpenRouter API key is read from the environment and never logged or committed.


<!--
Cut a release by renaming [Unreleased] to [X.Y.Z] - YYYY-MM-DD, then start a
fresh [Unreleased] block above it. Keep versions newest-first. Example:

## [0.1.0] - 2026-01-01

### Added

- First working slice: ...
-->
