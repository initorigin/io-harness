<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Release history

## io-harness

| Version | Released | Mode | Delivered |
| --- | --- | --- | --- |
| 0.5.0 | 2026-07-24T00:00:00Z | release-ready | A developer hands io-harness one large task and a parent agent decomposes it at run time, spawning contained sub-agents on demand through a typed `spawn_agent` tool. Each child runs the same observe/reason/act/verify/stop loop from run.rs (not a second implementation) over the shared workspace root and the single rusqlite store, receives a parent-supplied context brief, and composes its RunOutcome plus a result summary back to the parent as the tool result. Children run as bounded concurrent tokio tasks under `max_concurrent` and may nest to `max_depth`, counted from the root. A `Containment` value handed in once at root construction caps the whole tree — max_total_agents, max_concurrent, max_depth, and an aggregate spend ceiling drawn down by the entire tree together — and no spawned TaskContract can raise it. The 0.4.0 Policy becomes the containment boundary: a child inherits its parent's effective policy and may only narrow it (denies union, allows intersect, sensitive tier tightens only), a separate code path from 0.4.0's widening layer merge, enforced in the harness and holding downward through arbitrary depth. A spawn breaching any cap returns a typed refusal the parent adapts to and is recorded, exactly as an out-of-policy action is in 0.4.0. One Approver serves the whole tree; a child Defer persists and resumes via resume_with_decision. Every spawn, parent/child edge, containment refusal, and budget draw is recorded in rusqlite (additive schema — a 0.4.0 database migrates in place), so the tree is a reconstructable graph. Sub-agents are opt-in: a 0.4.0 caller who constructs no Containment gets no spawn tool and the exact 0.4.0 surface and behaviour. Built, tested, documented, and packaged (release-ready); crates.io publish deferred to the release/merge stage by owner decision, as for 0.1.0-0.4.0. |
| 0.4.0 | 2026-07-24T00:00:00Z | release-ready | A developer runs io-harness on sensitive work under an explicit permission policy. A Policy is a stack of named layers evaluated deny-first across the whole stack, so an overlay adds capability but can never re-allow what a layer beneath it denied. Path rules govern read_file/write_file and filter grep/find so a denied path yields no results; they are evaluated on the resolved path, and a deny matches either a symlink's own path or its target. A command policy gates what verification may spawn, with every spawn's full argv traced. Enforcement lives in the tool and verification layers, not the prompt. Anything the policy marks Ask reaches one object-safe Approver trait with three decisions: Approve (optionally rewriting the action or remembering rules, both re-checked so neither can cross a deny), Deny, or Defer — which persists the pending action so a human can decide after the process exits, with resume_with_decision continuing the run under its original id and re-checking the policy. A denied action never reaches the approver and is reported to the model as an adaptable tool result. Every refusal and decision is in the rusqlite trace, attributed to the rule and layer. Policy is serde-serializable and composes via merge, so io-cli and io-studio can each keep their own config over one shared base. Built, tested, documented, and packaged (release-ready); crates.io publish deferred to the release/merge stage by owner decision, as for 0.1.0-0.3.0. |
| 0.3.0 | 2026-07-24T00:00:00Z | release-ready | A developer hands io-harness a repository task: the agent greps and finds across a workspace, reads what it found, and edits several files in one run, verified together (WorkspaceTestPasses compiles the edited set with a test, so a partially-correct set fails). Two more providers ship behind the same Provider trait — Anthropic (/v1/messages stream) and OpenAI (OpenAI-style stream shared with OpenRouter) — selected only by which one is constructed and passed to run; the trace records which provider ran. The 0.2.0 single-file loop, budgets, retry, trace, resume, and execution-based verify are unchanged and still pass. Built, tested, and packaged (release-ready); crates.io publish deferred to the release/merge stage by owner decision. |
| 0.2.0 | 2026-07-24T00:00:00Z | published | A developer runs a longer, budgeted io-harness task and can trust it: step, time, and cost (token) budgets each stop the run with a distinct outcome; a failing provider/tool step is retried and then escalated with every attempt in the trace; each step's prompt, tool call, and token usage is persisted to rusqlite; an interrupted run resumes under its original id; and verification is execution-based — the produced file is compiled (and optionally tested) with rustc, so a substring stub cannot pass. Published to crates.io as 0.2.0. |
| 0.1.0 | 2026-07-23T16:11:56Z | release-ready | A developer embeds the io-harness crate, hands it a task contract to edit one file to a spec, and the harness runs the loop (observe, reason, act, verify, stop) with the filesystem tool and the OpenRouter provider, confirms the file meets the spec with a deterministic check, persists every step to rusqlite, and stops on success or the step cap. |

### 0.5.0 known limitations

- No execution sandbox — children still compile model-produced code directly on the host (the risk carried since 0.2.0), now multiplied by the fan-out factor. 0.5.0 bounds what the tree may touch and spend, not where code runs; per-run isolation is the very next release, 0.6.0.
- No whole-tree crash-resume. 0.5.0 reuses 0.4.0's single-pending-action persistence; a process restart does not resume an in-flight fleet. Full tree checkpointing is 0.7.0.
- Concurrent siblings share one workspace with last-write-wins — a decomposition that fans out onto overlapping files can corrupt state before 0.6.0's per-run sandbox brings real write isolation. The trace records both writes; the parent is expected to decompose to avoid collisions.
- The aggregate ceiling overshoots slightly by design: when the tree budget is exhausted mid-flight, in-flight children finish their current step before stopping, so no child is left with a half-applied edit. The small overshoot is recorded honestly in the trace rather than prevented.
- The spend ceiling is denominated in tokens as the hard, provider-agnostic ceiling; max_total_cost is an optional derived estimate only, and max_total_duration is optional wall-clock.
- No per-agent provider or model selection — the whole tree runs under the provider given at root construction (deferred, not committed for 0.5.0).
- No inter-agent coordination beyond parent<->child spawn and compose-back — no sibling messaging, shared mutable blackboard, or agent-to-agent RPC. Coordination is through the parent and the shared workspace only.
- The aggregate budget ledger is a single shared lock — deliberately the one point of serialization, adequate for one-host in-process fan-out and not tuned for throughput beyond that.
- mode is release-ready, not published: 0.5.0 is built, tested, documented, and packaged but not uploaded to crates.io; publish is deferred to the release/merge stage by owner decision, as for 0.1.0-0.4.0. cargo publish --dry-run passes and CARGO_REGISTRY_TOKEN is available.
- `Containment` field names are now a cross-repo serde contract the moment io-cli and io-studio adopt it, like `Policy` in 0.4.0; neither app has adopted it yet, so the composition value is unrealised until they do.

### 0.4.0 known limitations

- Single-file mode is not policy-enforced. A policy only applies to workspace contracts (TaskContract::workspace). Passing a non-permissive policy with a single-file contract now returns an error rather than running unenforced — this was found by the completion gate, where it had been a silent no-op, and fixing it was release-blocking. Extending enforcement to the single-file loop is outstanding.
- A refused action can still waste a run. Refusals count against the step budget (a decided trade-off), so a model that repeatedly retries a denied action is bounded by the step cap but achieves nothing. Confirmed live: with no guidance the model retried a denied read on steps 2, 4, 7 and 8 and hit StepCapReached without doing the work. The mitigation today is a TaskContract constraint telling the model not to retry refused actions, which turned the same task into Success{steps:4}. A repeated-refusal early stop is the named follow-up.
- Rules remembered before a Defer are not persisted with the pending action. They are returned on RunResult::remembered, so a caller can merge them into the policy it passes to resume_with_decision, but the crate does not do this for it — a caller that ignores the field silently loses them on resume.
- The model is not told when approve-with-changes altered its action (matching Claude Code's behaviour). The loop re-reads the workspace each step so it observes reality eventually, but it may waste a step believing it wrote a path it did not.
- The command policy gates the binary name only, not arguments. Sound today because every argv element is harness-constructed (src/verify.rs) and no model or caller output reaches it; the full argv is traced. Argument gating becomes required in 0.6.0 when plugins/MCP can supply argv.
- Which form is remembered when an approver both rewrites and remembers is the approver's choice — the crate applies exactly the rules handed to it and does not derive them from either the requested or the performed action.
- The default is permissive: a caller who passes no policy gets no enforcement. Deliberate for 0.3.0 compatibility, but it means 0.4.0 ships a boundary nobody gets unless they opt in, and neither io-cli nor io-studio has adopted it yet — the release's value is unrealised until they do.
- Policy layering is composed, not discovered: the crate merges a stack it is handed and never reads a config file. Where the shared base lives, and in what precedence, is unsettled across io-cli and io-studio.
- mode is release-ready, not published: 0.4.0 is built, tested, documented and packaged but not uploaded to crates.io; publish is deferred to the release/merge stage by owner decision, as for 0.1.0-0.3.0. cargo publish --dry-run passes and CARGO_REGISTRY_TOKEN is available.
- Execution-based verification still compiles model-produced code locally with no sandbox (sandboxes are 0.5.0), inherited from 0.2.0. The command policy constrains which binary runs, not what it may do.
- Policy glob matching compiles a regex per check and matches full relative path or basename; adequate for a handful of rules per tool call, not tuned for large rule sets.

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
