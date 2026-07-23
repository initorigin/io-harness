<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Release history

## io-harness

| Version | Released | Mode | Delivered |
| --- | --- | --- | --- |
| 0.3.0 | 2026-07-24T00:00:00Z | release-ready | A developer hands io-harness a repository task: the agent greps and finds across a workspace, reads what it found, and edits several files in one run, verified together (WorkspaceTestPasses compiles the edited set with a test, so a partially-correct set fails). Two more providers ship behind the same Provider trait — Anthropic (/v1/messages stream) and OpenAI (OpenAI-style stream shared with OpenRouter) — selected only by which one is constructed and passed to run; the trace records which provider ran. The 0.2.0 single-file loop, budgets, retry, trace, resume, and execution-based verify are unchanged and still pass. Built, tested, and packaged (release-ready); crates.io publish deferred to the release/merge stage by owner decision. |
| 0.2.0 | 2026-07-24T00:00:00Z | published | A developer runs a longer, budgeted io-harness task and can trust it: step, time, and cost (token) budgets each stop the run with a distinct outcome; a failing provider/tool step is retried and then escalated with every attempt in the trace; each step's prompt, tool call, and token usage is persisted to rusqlite; an interrupted run resumes under its original id; and verification is execution-based — the produced file is compiled (and optionally tested) with rustc, so a substring stub cannot pass. Published to crates.io as 0.2.0. |
| 0.1.0 | 2026-07-23T16:11:56Z | release-ready | A developer embeds the io-harness crate, hands it a task contract to edit one file to a spec, and the harness runs the loop (observe, reason, act, verify, stop) with the filesystem tool and the OpenRouter provider, confirms the file meets the spec with a deterministic check, persists every step to rusqlite, and stops on success or the step cap. |

### 0.3.0 known limitations

- mode is release-ready, not published: 0.3.0 is built/tested/packaged but not yet uploaded to crates.io; publish deferred to the release/merge stage (owner decision). cargo publish --dry-run passes and CARGO_REGISTRY_TOKEN is available.
- Live provider proof is OpenRouter-only — the sole live key. The Anthropic and OpenAI request-build and stream-parse are unit-tested offline against synthetic wire fixtures; neither has been run against its live API. Cross-vendor live parity is unproven (matches the contract's Anthropic-only fallback and the logged key/quota risk).
- Agent working memory is an observation log folded into the user turn, not native multi-turn message history; long read_file/grep results are truncated to the last 4000 chars in that log.
- grep/find honor a fixed ignore list (.git, target, node_modules), not .gitignore; the walk is synchronous std::fs (fine for local repos, not tuned for huge trees).
- Multi-file execution-based verification is Rust-only (EachCompilesRust / WorkspaceTestPasses); other languages have no multi-file gate.
- Workspace resume reuses the files on disk as state and restarts the observation log; the Anthropic request caps max_tokens at a fixed 8192.
- Execution-based verification still compiles model-produced code locally with no sandbox (sandboxes are 0.5.0), inherited from 0.2.0.

### 0.2.0 known limitations

- 0.1.0 was never published to crates.io (it landed release-ready), so 0.2.0 is the crate's first version on the registry; a 0.1.0→0.2.0 upgrade was never exercised against crates.io itself.
- Execution-based verification runs model-produced code locally with no sandbox (sandboxes are 0.5.0); a hostile or buggy artifact could affect the workspace.
- The live model run covered one model (openai/gpt-5.6-luna) and one simple task; broader model and task coverage is unproven.
- The cost budget is counted in tokens, not currency — OpenRouter carries no reliable per-request price.
- Only Rust execution-based gates exist (CompilesRust, RustTestPasses); other languages fall back to content checks.
- Resume reuses the file on disk as the run's state; if the file is changed out of band between interruption and resume, resume continues from the changed file.

### 0.1.0 known limitations

- Not published to crates.io. target_mode was published; owner chose to stop at release-ready. Publish is the remaining step (`cargo publish` with CARGO_REGISTRY_TOKEN); crate name `io-harness` confirmed available.
- The end-to-end test uses a mock provider for deterministic offline CI. A real OpenRouter model run was not executed; the live path exists only in examples/edit_file.rs and is unproven against a real model.
- Verification is deterministic substring/exact-match only; no schema or model-judged verification.
- Single file per task, single agent, single provider (OpenRouter). No budgets beyond a step cap, no retry/recovery, no permissions, no human approval — all roadmap.
- No default OpenRouter model; OPENROUTER_MODEL must be set by the caller.


_Canonical sources: products/<id>/releases/<version>.yaml_
