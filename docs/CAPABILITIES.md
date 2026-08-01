# Capabilities — IO Harness

The index of the guide pages, and the map of what the harness holds.

If you arrived here from docs.rs rather than from the README, this is the way in.
[README.md](../README.md) is the landing page and
[CONTRACT.md](CONTRACT.md) is what you may depend on.

## Guides

One page per capability. Each carries the depth the README does not, including
the limits that capability actually has.

| Guide | What it covers |
| --- | --- |
| [Permissions and approval](guide/permissions.md) | Layered deny-first rules, what asks and what is refused, deferring a decision past process exit |
| [Command execution](guide/command-execution.md) | Running a project's own toolchain under an `Act::Exec` check on the whole argv, and the bound that is not there |
| [Language support](guide/language-support.md) | Toolchain detection, a criterion in any language, and migrating off the Rust-specific gates |
| [Verification](guide/verification.md) | The criteria, execution-based gates, and exactly what a pass proves |
| [Agent composition](guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [Durable runs](guide/durable-runs.md) | Checkpoints, resume, the stored-policy entry points, approvals that survive a restart |
| [MCP and network egress](guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy stops governing |
| [Tools and skills](guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery, and the boundary |
| [Context and memory](guide/context-and-memory.md) | Per-turn assembly, compaction, invalidation, durable cross-run memory |
| [Resilience](guide/resilience.md) | Failure classification, kind-aware retry, provider fallback, stall detection |
| [Observability and replay](guide/observability.md) | Observers, event kinds, outcome records, deterministic replay |
| [Sessions](guide/sessions.md) | A durable, branchable conversation: turns that are runs, token streaming, steering and interruption |
| [Agency](guide/agency.md) | A plan you can watch, a question about intent, named agents with their own model and boundary, and prompt templates |
| [Web search and fetch](guide/web.md) | Provider-executed search and fetch, what each vendor takes and what it refuses, citations, and the boundary the vendor enforces rather than this process |
| [Configuration](guide/configuration.md) | One `io.toml`, four scopes, `${env:}` and `${file:}` substitution, projected onto the typed API |
| [Accounting](guide/accounting.md) | One row per provider call, the cache and reasoning breakdown, latency and TTFT, and cost derived from a price table you own |
| [Documents](guide/documents.md) | Spreadsheets, Word, PowerPoint, PDF, barcodes — and what was cut, with the reasoning |
| [Images and git](guide/images-and-git.md) | Image passthrough and the fixed-argv git built-ins |
| [Hooks](guide/hooks.md) | Reacting to a run from `io.toml`: an audit log, a notification, a formatter, a check that stops the run — and why the whole array is refused in the project scope |
| [Providers](guide/providers.md) | One `Compatible` provider over any OpenAI-shaped endpoint, the 21 vendor presets, the local runtimes, and what `Provider::models()` reports |

## The twelve pillars

A complete harness holds all twelve. All twelve hold. The release column is
history, not structure — a pillar is *shipped* when a release contract accepted
it, not when a release merely touched it.

| Pillar | Release |
| --- | --- |
| **Task contract** — goal, constraints, expected output, success criteria | 0.1.0 |
| **Orchestration loop** — observe, reason, act, check, stop | 0.1.0 |
| **Verification layer** — execution-based gates confirm the task is done | 0.2.0, hardened 0.8.1 |
| **Stop conditions and budgets** — cap steps, time, tokens, retries | 0.2.0, tree-wide 0.5.0 |
| **Permissions and guardrails** — what the agent may read, write, exec, dial | 0.4.0, network 0.8.0 |
| **Human approval layer** — review before sensitive or irreversible actions | 0.4.0, durable 0.7.0 |
| **Tool layer** — narrow, typed actions the agent invokes | 0.3.0, 0.8.0, completed 0.9.0 |
| **Context construction** — feed the model only relevant, current, trusted info | 0.10.0 |
| **State and memory** — progress within a run, durable recall across runs | 0.2.0, completed 0.10.0 |
| **Recovery and retry** — retries, fallbacks, replanning, escalation | 0.2.0, completed 0.11.0 |
| **Observability and tracing** — prompts, decisions, tool calls, cost, outcomes | 0.2.0, completed 0.12.0 |
| **Evaluation layer** — success, reliability, safety, latency, cost across cases | 0.12.0 |

## Beyond the pillars

- **Providers** — OpenRouter, Anthropic and OpenAI, plus one `Compatible`
  provider reaching any OpenAI-shaped endpoint and 21 vendor presets behind named
  constructors — 13 hosted and 8 local runtimes — all over the crate's own
  HTTP+SSE client, with fallback between them
- **Provider-executed web search and fetch** — one `WebAccess` declaration each
  vendor translates into its own shape, with the sources cited recorded in the
  trace; the provider dials the URL, so the domain filter is the vendor's
- **Agent composition** — spawn and nest many agents under one shared budget
- **Ephemeral code-exec sandboxes** — OS-native per platform over a portable floor
- **Long-running autonomous tasks** — unattended and crash-resumable
- **Extensibility** — an MCP client over stdio and streamable HTTP, in-process
  `Tool` implementations, and markdown skills
- **Built-in tools** — filesystem, grep and find over a policy-scoped workspace
- **Documents** — spreadsheets, Word, PowerPoint text, PDF and barcode decoding,
  each behind its own cargo feature, all off by default
- **Images** — passthrough to any provider whose model accepts one
- **Git** — status, diff, log, add and commit as fixed-argv built-ins

## What is off the roadmap

Cut, not deferred. None appears in any planned release, and each was decided on
evidence rather than on capacity.

**OCR** (2026-07-27). Tesseract binds a C++ library needing vcpkg on the Windows
runner, and the standing rule forbids a system package on any runner. The
pure-Rust alternative requires a newer MSRV than this crate's floor, fetches
models over the network at runtime, and reads Latin script only.

**PowerPoint authoring** (2026-07-27). The one credible Rust crate is a 46-star,
single-maintainer, pre-0.3 project. Generating a deck otherwise means
hand-rolling layouts, masters, theme parts and the relationship graph that ties
them together, and a file PowerPoint may or may not open is not a capability.
Reading a deck's text is easy and is shipped.

**Video** (2026-07-27). The Anthropic Messages API and the OpenAI Chat
Completions API accept images and no video at all. OpenRouter, the only one of
the three carrying a `video_url` content part, says support varies by model and
gives no way to ask which. "Passthrough to any provider whose model accepts it"
would have meant one provider out of three, with no way to know when.

**In-place Word editing.** The mature crate's round trip silently drops the OOXML
it does not model. For an agent editing someone's real document that is data
loss, so it is not claimed. Read and generate are shipped.

Spreadsheet edits do round-trip a workbook the harness did not create, but
preservation is not a guarantee — charts, drawings, pivots and macros are where
it is likeliest to cost something.

Each of these returns only as a new roadmap entry argued on its own merits.
