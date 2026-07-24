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
