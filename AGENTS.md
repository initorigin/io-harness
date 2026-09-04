# AGENTS.md

Guidance for any coding agent working in this repository — Claude Code, Codex, Cursor, Gemini
CLI, Copilot, or whatever comes next. This file is the single source of truth; harness-specific
files such as `CLAUDE.md` point here rather than restating it.

`io-harness` is an embeddable agent runtime for Rust: a library crate, no binary, no daemon,
no CLI. It runs an agent loop under a layered permission boundary, inside an OS sandbox, with
every step in a SQLite trace. MSRV is `rust-version` in `Cargo.toml` (1.95 today) and the
default build compiles no optional dependency at all.

## Commands

```bash
# What CI's test matrix runs, both feature polarities (it uses cargo-nextest).
cargo nextest run --lib --tests --no-fail-fast
cargo nextest run --all-features --lib --tests --no-fail-fast
cargo test --lib --tests            # nextest is not required; cargo test works

# One test file / one test
cargo test --test verify_gate
cargo test --test verify_gate f3_          # substring match on the test name
cargo nextest run -E 'test(f3_)'

# Doctests — a separate CI job, and `--lib --tests` above does NOT run them.
cargo test --doc
cargo test --all-features --doc

# Lint gate (all six run in CI, all with -D warnings)
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features documents -- -D warnings
cargo clippy --all-targets --features media -- -D warnings
cargo clippy --all-targets --features otel -- -D warnings
cargo clippy --all-targets --features mcp-server -- -D warnings

# Examples compile-check (CI does not link them)
cargo check --examples
cargo check --examples --all-features

# Platform code this macOS host cannot compile: Landlock, seccomp, the Windows half.
./scripts/cross-check.sh

# Measurements. #[ignore]d deliberately — they print timings and assert nothing,
# because a duration asserted on a CI runner is a flake.
cargo nextest run --run-ignored ignored-only --success-output immediate -E 'test(n5_)'
cargo test -- --ignored --nocapture

# Live runs against a real provider: source .env first (see .env.example), then
cargo run --example edit_file
cargo run --features browser --example browser_live
```

## Architecture

**One loop, two engines.** Everything funnels into `src/run.rs` — `run_with_extras` for a flat
run, `run_tree_with_extras` for a tree with sub-agents. `tests/one_runtime_path.rs` parses the
file and *derives* that every driving entry point reaches one of the two; a new entry point
with its own inline loop fails that test. `src/harness.rs` (`Harness`) is a facade that binds
provider/store/policy once and delegates; it adds no loop.

**Two shapes over the same loop.**
- `run_with(&TaskContract, ...)` — unattended one-shot, optionally gated by `Verification`.
- `Session::turn(...)` (`src/session.rs`) — a durable conversation; each turn is its own run,
  its own budget, its own trace. `Session::drive` is where a turn's contract meets the loop,
  and it is the only place `TurnExtras.classify` is set — a *classifying* turn (one that may
  answer instead of acting) is reachable only through a session.

**Module map (`src/`).**
- `run/` — `step.rs` (one step), `dispatch.rs` (tool dispatch), `tree.rs` (sub-agent trees),
  `gate.rs` (verification evaluation), `prompts.rs` (system-prompt composition),
  `read.rs` (speculative/parallel reads), `mailbox.rs`, `memory.rs`, `outcome.rs`, `record.rs`.
- `policy.rs` — deny-first layered rules over `Act::{Read,Write,Exec,Net}`. Every mutating path
  resolves through `Policy::explain`; tools are *denied*, never filtered out of the list.
- `approve.rs` — approve / deny / defer-past-process-exit; `state/approvals.rs` persists a defer.
- `sandbox/` + `containment.rs` — backend rungs per host (`macos.rs` seatbelt, `linux.rs`
  namespaces, `landlock.rs`, `seccomp.rs`, `windows.rs` Job Objects, `appcontainer.rs`), over a
  portable floor. What actually applied is recorded in the trace and in the agent's prompt.
  `proxy.rs` is the per-run egress proxy. `containment.rs` also holds the shared budget `Ledger`.
- `state/` — the SQLite store: `schema.rs`, `trace.rs`, `runs.rs`, `sessions.rs`, `agents.rs`,
  `memory.rs`, `accounting.rs`, `approvals.rs`, `leases.rs`. Trace, budget draw and checkpoint
  commit in **one transaction** per completed step — that is what makes resume safe. A run is
  driven under a lease (`leases.rs`); a second driver gets `Error::Conflict`.
- `provider/` — `anthropic.rs`, `openai.rs`/`openai_wire.rs`, `openrouter.rs`, `compatible.rs`
  (any OpenAI-shaped endpoint), `catalog.rs` (vendor presets), `fallback.rs`,
  `record.rs`/`replay.rs` (deterministic offline replay, used heavily by tests).
- `tools/` — `workspace.rs`, `fs.rs`, `exec.rs`, `shell.rs` (a whole command line parsed *here*,
  never handed to a shell), `handles.rs` (backgrounded `shell_start`), `git.rs` (fixed argv),
  `custom.rs` (the `Tool` trait), `diagnostics.rs`, `browser.rs`, `documents/`.
- `config.rs` — one `io.toml` over four scopes projected onto the typed API; a project scope may
  narrow and never widen.
- Others: `context.rs` (per-turn assembly, compaction), `verify.rs`, `contract.rs`, `mcp.rs`,
  `lsp.rs`, `skills.rs`, `plugin.rs`, `hooks.rs`, `observe.rs`, `pricing.rs`, `resilience.rs`,
  `template.rs`, `toolchain.rs`, `diff.rs`, `web.rs`, `attach.rs`, `agent.rs`.

**Feature flags.** `default = []`. `media`, `browser` (implies `media`), `documents` (umbrella
over `xlsx`/`docx`/`pptx`/`pdf`/`barcode`), `otel` (export a run as OpenTelemetry spans),
`mcp-server` (serve this crate's tools over MCP on stdio). The last three add no crate at all —
they are features because a build that did not ask for a browser, an outbound telemetry writer or
a door onto its own tools should not compile one. The canonical list lives in `docs/CONTRACT.md`
and is drift-checked. Adding a dependency to the default tree is a deliberate, argued act — read
the comments in `Cargo.toml` before you do.

## The tests that gate documentation

Several tests are checkers over prose and config, not over behaviour. They fail merges when docs
drift, and each carries a negative control so it cannot pass by matching nothing.

- `tests/public_api.rs` — the crate-root surface must match `docs/public-api.txt`. **Edit that
  file by hand; there is deliberately no `--bless`.** A removed or renamed item is a break: keep
  the old name with `#[deprecated]` naming its replacement, and add a CHANGELOG migration note.
  A new item needs a doc comment with a worked example.
- `tests/changelog.rs` — every entry starting with `**BREAKING` must contain a literal
  `Migration:` saying what to write instead. See `docs/CHANGELOG_STRUCTURE.md`.
- `tests/docs_drift.rs` — the MSRV, the feature list, relative links and the README's
  `io-harness = "X.Y"` snippet must agree with `Cargo.toml`.
- `tests/readme.rs` — a runnable code fence and the MSRV inside the first 60 README lines; no
  heading named after the release that introduced it.
- `tests/guide_pages.rs` — every named capability keeps its `docs/guide/*.md` page, and the six
  "stated plainly" limits blocks must survive rewrites.
- `tests/ci_workflow.rs` — every example a test spawns as a child must be in the CI matrix's
  `cargo build --example` list. `--lib --tests` does not build `examples/`.
- `tests/state_error.rs` — no `pub` item's surface may name `rusqlite` again.
- `tests/determinism.rs` — the same case run twice produces the same canonical trace.

## Conventions

**Prose register** — `docs/STYLE.md` is the rule, and it is not linted, so it is on you.
Present tense, no diary (history goes in `CHANGELOG.md`; a version number in a sentence is a
citation, not a story). Name the reason for a non-obvious decision *where the decision is*, once.
No first person, no "powerful"/"robust"/"simply". A claim a test asserts is stated flatly; one
nothing asserts says what is actually known instead of hedging.

**Comments carry the argument.** This codebase deliberately keeps long "why" comments —
`Cargo.toml`'s dependency notes, `.github/workflows/ci.yml`'s matrix rationale,
`scripts/cross-check.sh`'s header. Match that density; do not strip them.

**Test names are sentences about behaviour** — `a_plan_gated_classifying_turn_may_still_answer`,
not `test_planning_directive_2`. Prefixes map to a release's acceptance criteria:
`f1_`/`f2_`… functional facts, `nf3_` non-functional, `n5_` measurements (`#[ignore]`d).

**Release contracts.** `.ultraship/products/io-harness/releases/X.Y.Z.yaml` is where a version's
outcome, scope and numbered acceptance criteria are written before the code, and where the
evidence is recorded after. A released one is `immutable: true` — do not edit it. The F-numbers
in test names point back into it.

**Branch and release flow** (`CONTRIBUTING.md`, `docs/RELEASE_PROCESS.md`): work branches are
`feat/<version>` cut from `develop`; PRs go into `develop`; `main` holds released versions only
and a `develop` → `main` PR *is* the release. Conventional Commits, subject under ~50 chars.
Every user-facing change updates `CHANGELOG.md` under `## [Unreleased]` in the right category —
those entries become the GitHub Release notes verbatim. `cargo publish` is run by hand, last,
and is never added to a workflow.

**Measurements are never gates.** No test asserts a duration. New timing work prints, is
`#[ignore]`d, and its method goes in `docs/MEASUREMENTS.md`.
