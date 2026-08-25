# Capabilities — IO Harness

The index of the guide pages, and the map of what the harness holds.

If you arrived here from docs.rs rather than from the README, this is the way in.
[README.md](../README.md) is the landing page and
[CONTRACT.md](CONTRACT.md) is what you may depend on.
[MEASUREMENTS.md](MEASUREMENTS.md) holds the numbers this repository has actually
measured, with the machine named — none of them a gate.

## Guides

One page per capability. Each carries the depth the README does not, including
the limits that capability actually has.

| Guide | What it covers |
| --- | --- |
| [Driving a browser](guide/browser.md) | Opening a page, using it, reading what it rendered and looking at it — with every document navigation decided by the run's own policy at the paused request |
| [Permissions and approval](guide/permissions.md) | Layered deny-first rules, what asks and what is refused, deferring a decision past process exit |
| [Command execution](guide/command-execution.md) | Running a project's own toolchain under an `Act::Exec` check on the whole argv, and the bound that is not there |
| [Language support](guide/language-support.md) | Toolchain detection, a criterion in any language, and migrating off the Rust-specific gates |
| [Verification](guide/verification.md) | The criteria, execution-based gates, the review criterion a second model answers, per-gate retry, and exactly what a pass proves |
| [Agent composition](guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [Durable runs](guide/durable-runs.md) | Checkpoints, resume, the stored-policy entry points, approvals that survive a restart |
| [MCP and network egress](guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy stops governing |
| [Tools and skills](guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery, the boundary, and what declaring a tool read-only opts it into — running beside its siblings, and starting before the model has finished asking for it |
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
| [Providers](guide/providers.md) | One `Compatible` provider over any OpenAI-shaped endpoint, the 21 vendor presets, the local runtimes, what `Provider::models()` reports, the routing rules that change which model answers mid-run, and the optional method that reports a finished tool call while the completion is still streaming |
| [Capability bundles](guide/plugins.md) | A directory that contributes skills, templates, agents, MCP servers, hooks and deny-only policy at once; what a project-scoped declaration may not hand you; how a contribution names the bundle it came from; and why a broken bundle is dropped rather than fatal |
| [Retention](guide/retention.md) | What a session and a store are holding, removing a session whole, sweeping to a date and what that refuses, keeping every row while emptying every word, and returning the freed pages to the filesystem |
| [The mailbox](guide/mailbox.md) | Giving each agent in a tree an address, sending a finding to a named sibling, reading an inbox oldest-first exactly once, and a bounded wait that returns on the message or on the sender finishing |

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
| **State and memory** — progress within a run, durable recall across runs | 0.2.0, completed 0.10.0, kept by evidence and scoped above the workspace 0.56.0, selected by what the turn is about 0.57.0 |
| **Recovery and retry** — retries, fallbacks, replanning, escalation | 0.2.0, completed 0.11.0 |
| **Observability and tracing** — prompts, decisions, tool calls, cost, outcomes | 0.2.0, completed 0.12.0 |
| **Evaluation layer** — success, reliability, safety, latency, cost across cases | 0.12.0 |

## What shipped when

[README.md](../README.md) states what the crate does now, in the present tense
and with no release numbers in it. This is where "which release introduced what"
lives instead: one line per version, linking the entry that holds the rest.
[CHANGELOG.md](../CHANGELOG.md) is the history — this table is only the index
into it.

| Version | What it introduced | Entry |
| --- | --- | --- |
| 0.69.0 | An operator folds a running turn — `Steer::fold()` beside `say` and `interrupt`, honoured at the next step boundary and before that step's own request — and a fold outlives the turn that made it, because a session's seed now carries the paragraph an earlier turn folded instead of the conversation it replaced | [2026-08-25](../CHANGELOG.md#0690---2026-08-25) |
| 0.68.0 | The conversation folds when the operator says so as well as when the threshold notices — `TaskContract::fold_now` folds a turn's history before its first request, beside the automatic fold that is unchanged — and an MCP server says how many tools it offered | [2026-08-25](../CHANGELOG.md#0680---2026-08-25) |
| 0.67.0 | A session turn can be steered whatever contract it carries — the caller's `TaskContract`, an `Observer` and a `SteerInbox` on one call, on the flat turn and on a fan-out whose root reads the operator's correction while its children never do | [2026-08-25](../CHANGELOG.md#0670---2026-08-25) |
| 0.66.0 | A session turn that may fan out takes the caller's `TaskContract` — a plan gate, registered tools, a budget or a verification gate on a turn that can decompose — and the bound `Harness` gains a contained turn at all | [2026-08-19](../CHANGELOG.md#0660---2026-08-19) |
| 0.65.0 | A run killed in the middle of a call the harness cannot inspect pauses for an operator's decision instead of repeating it — a durable journal of what was started, a recovery classification on the `Tool` trait, and a resume that refuses to drive past an open attempt | [2026-08-18](../CHANGELOG.md#0650---2026-08-18) |
| 0.64.0 | A resumed run sends the model its own past turns rather than a third-person account of them — the assistant half of a transcript becomes durable and is restored beside the ledger, in both loops | [2026-08-17](../CHANGELOG.md#0640---2026-08-17) |
| 0.63.0 | The host is bound once in a `Harness` — provider, store, boundary and the configuration that is not a property of any task — the storage library leaves the public contract, and a turn's framing becomes settable | [2026-08-17](../CHANGELOG.md#0630---2026-08-17) |
| 0.62.0 | One driver per run: a lease with a generation, a typed conflict for a second live owner, takeover of a lapsed lease, and a session head that advances by compare-and-swap | [2026-08-17](../CHANGELOG.md#0620---2026-08-17) |
| 0.61.0 | Every name the harness answers is reserved, and a test derived from the crate's own tool constants keeps the set complete | [2026-08-17](../CHANGELOG.md#0610---2026-08-17) |
| 0.60.3 | Every block a classifying turn is composed from is true of that turn — the plan gate, the boundary in force, and a preset that shapes rather than replaces | [2026-08-16](../CHANGELOG.md#0603---2026-08-16) |
| 0.60.2 | The contract tells one truth about the sandbox boundary — twenty-two claims a release had outlived, corrected and given tests | [2026-08-16](../CHANGELOG.md#0602---2026-08-16) |
| 0.60.1 | The landing page rewritten to the present tense, with measured cost, a sourced comparison, and the drift tests that keep them honest | [2026-08-16](../CHANGELOG.md#0601---2026-08-16) |
| 0.60.0 | The mailbox — every agent in a tree has an address, and a message reaches one named sibling | [2026-08-16](../CHANGELOG.md#0600---2026-08-16) |
| 0.59.0 | The browser runs on Windows, and Windows confines access when the caller asks for it | [2026-08-16](../CHANGELOG.md#0590---2026-08-16) |
| 0.58.0 | Retention — what a store is holding, removing a session whole, sweeping to a date, emptying words while keeping rows | [2026-08-15](../CHANGELOG.md#0580---2026-08-15) |
| 0.57.0 | Recall selects by what the turn is about, while the print order stays fixed | [2026-08-15](../CHANGELOG.md#0570---2026-08-15) |
| 0.56.0 | `forget`, memory kept for its value, marked unlearnable, and scoped above one workspace | [2026-08-15](../CHANGELOG.md#0560---2026-08-15) |
| 0.55.0 | A read is the whole file, the line range it asked for, or a refusal — and a wider door in front of the image wire | [2026-08-14](../CHANGELOG.md#0550---2026-08-14) |
| 0.54.0 | A read-only call starts off the provider's stream, before the completion has finished | [2026-08-14](../CHANGELOG.md#0540---2026-08-14) |
| 0.53.0 | Driving a real browser: six tools over a pipe transport, gated at the paused request | [2026-08-13](../CHANGELOG.md#0530---2026-08-13) |
| 0.52.0 | LSP navigation — five tools answered by a language server named in `io.toml` or on the contract | [2026-08-13](../CHANGELOG.md#0520---2026-08-13) |
| 0.51.0 | Every write records its change as a unified diff, and `rewind_step` reverse-applies one step's | [2026-08-12](../CHANGELOG.md#0510---2026-08-12) |
| 0.50.0 | A parent chooses how a child comes back — detached, or waited for | [2026-08-12](../CHANGELOG.md#0500---2026-08-12) |
| 0.49.0 | A request carries an ordered transcript instead of one appended string | [2026-08-11](../CHANGELOG.md#0490---2026-08-11) |
| 0.48.0 | A backgrounded `shell_start` handle runs inside the boundary — the last full-privilege path closed | [2026-08-11](../CHANGELOG.md#0480---2026-08-11) |
| 0.47.0 | Linux containment as a chain: Landlock first, bubblewrap next, the strongest rung the host delivers | [2026-08-09](../CHANGELOG.md#0470---2026-08-09) |
| 0.46.0 | `ExecMode` — a run's own commands are contained by default, `WorkspaceWrite` being that default | [2026-08-09](../CHANGELOG.md#0460---2026-08-09) |
| 0.45.0 | The agent is told the boundary it works inside instead of discovering it one refusal at a time | [2026-08-09](../CHANGELOG.md#0450---2026-08-09) |
| 0.44.0 | A second cache breakpoint, in the transcript, with the print order fixed to keep it | [2026-08-09](../CHANGELOG.md#0440---2026-08-09) |
| 0.43.0 | Compaction — the ledger past a share of the turn's budget becomes one model-written paragraph | [2026-08-09](../CHANGELOG.md#0430---2026-08-09) |
| 0.42.0 | `ModelApprover` — a model answers the pending act, told the rule and the layer that produced it | [2026-08-08](../CHANGELOG.md#0420---2026-08-08) |
| 0.41.0 | Several read-only calls in one completion run at the same time | [2026-08-08](../CHANGELOG.md#0410---2026-08-08) |
| 0.40.0 | `with_contained_exec` puts the project's own commands inside this host's sandbox backend | [2026-08-07](../CHANGELOG.md#0400---2026-08-07) |
| 0.39.0 | A session turn can be a tree, so a conversation fans out into contained sub-agents | [2026-08-06](../CHANGELOG.md#0390---2026-08-06) |
| 0.38.0 | A cache breakpoint at the end of the system block, so a long conversation stops paying full price for what never changes | [2026-08-06](../CHANGELOG.md#0380---2026-08-06) |
| 0.37.0 | A conversation answers without opening a run | [2026-08-06](../CHANGELOG.md#0370---2026-08-06) |
| 0.36.1 | CI answers in a fraction of the time, proving exactly what it proved before | [2026-08-04](../CHANGELOG.md#0361---2026-08-04) |
| 0.36.0 | A run lands as a branch in its own worktree, and `rewind_run` puts the whole run back | [2026-08-03](../CHANGELOG.md#0360---2026-08-03) |
| 0.35.0 | Capability bundles — a directory contributing skills, templates, agents, MCP servers, hooks and deny-only policy | [2026-08-03](../CHANGELOG.md#0350---2026-08-03) |
| 0.34.0 | A review criterion a second model answers, per-gate retry, and routing that changes which model answers mid-run | [2026-08-02](../CHANGELOG.md#0340---2026-08-02) |
| 0.33.0 | Two processes, one run — the landing call reports whether it is the one that landed | [2026-08-02](../CHANGELOG.md#0330---2026-08-02) |
| 0.32.0 | A tree-wide, per-tier concurrency ceiling the fleet holds instead of hits | [2026-08-02](../CHANGELOG.md#0320---2026-08-02) |
| 0.31.0 | The agent proposes before it acts, and a caller says how hard the model may think | [2026-08-01](../CHANGELOG.md#0310---2026-08-01) |
| 0.30.0 | The store answers *why*: which file decided a setting, whether a fact was decided, and how runs actually ended | [2026-08-01](../CHANGELOG.md#0300---2026-08-01) |
| 0.29.0 | One `Compatible` provider over any OpenAI-shaped endpoint, with hosted vendors and local runtimes behind named constructors | [2026-08-01](../CHANGELOG.md#0290---2026-08-01) |
| 0.28.0 | `[[hook]]` tables in `io.toml`, refused whole in the project scope — and `rewind`, putting a file back | [2026-08-01](../CHANGELOG.md#0280---2026-08-01) |
| 0.27.0 | `io.toml` names the provider and model, plus profiles, an unvalidated application section and discovered project instructions | [2026-08-01](../CHANGELOG.md#0270---2026-08-01) |
| 0.26.0 | The Windows tree kill, through a Job Object — the guarantee macOS and Linux got in 0.25.0 | [2026-08-01](../CHANGELOG.md#0260---2026-08-01) |
| 0.25.0 | Handles — a run starts something that does not finish, reads it over many steps and ends it | [2026-07-31](../CHANGELOG.md#0250---2026-07-31) |
| 0.24.0 | A command line the agent writes and the harness checks, and a Windows run the kernel actually bounds | [2026-07-31](../CHANGELOG.md#0240---2026-07-31) |
| 0.23.0 | The dependency release: nothing the crate does changes, only who is allowed to call it (MSRV 1.88 → 1.95) | [2026-07-31](../CHANGELOG.md#0230---2026-07-31) |
| 0.22.0 | Provider-executed web search and fetch, with what the agent read recorded in the run | [2026-07-30](../CHANGELOG.md#0220---2026-07-30) |
| 0.21.0 | Agency — a plan you can watch, and a question about intent instead of a guess | [2026-07-30](../CHANGELOG.md#0210---2026-07-30) |
| 0.20.0 | Sessions — a durable conversation with token streaming, steering, interruption and branching | [2026-07-30](../CHANGELOG.md#0200---2026-07-30) |
| 0.19.0 | `io.toml`: one file, four scopes merging in a fixed order, every key landing in a type the crate had | [2026-07-29](../CHANGELOG.md#0190---2026-07-29) |
| 0.18.0 | Accounting — one row per provider call, with its model, tokens, latency and TTFT, and cost derived from a price table you own | [2026-07-29](../CHANGELOG.md#0180---2026-07-29) |
| 0.17.0 | Any language: the project's own toolchain runs, and a run's definition of done stops requiring a Rust gate | [2026-07-29](../CHANGELOG.md#0170---2026-07-29) |
| 0.16.2 | A docs.rs build fix behind the `docsrs` cfg; no public item and no behaviour moved | [2026-07-28](../CHANGELOG.md#0162---2026-07-28) |
| 0.16.1 | A documentation correction; no code changed | [2026-07-28](../CHANGELOG.md#0161---2026-07-28) |
| 0.16.0 | The documented public contract — what a caller may depend on, and what may change | [2026-07-28](../CHANGELOG.md#0160---2026-07-28) |
| 0.15.0 | Images the model can see, and work handed back as commits through the git built-ins | [2026-07-27](../CHANGELOG.md#0150---2026-07-27) |
| 0.14.0 | Documents — spreadsheets, Word, PDF, barcodes and slide text, under the rules that already govern source | [2026-07-27](../CHANGELOG.md#0140---2026-07-27) |
| 0.13.0 | A resumed run restores its policy and its assembled context, not only its counters | [2026-07-27](../CHANGELOG.md#0130---2026-07-27) |
| 0.12.0 | Observability and evaluation — the twelfth and last pillar | [2026-07-27](../CHANGELOG.md#0120---2026-07-27) |
| 0.11.0 | Failure classification, kind-aware retry, provider fallback and stall detection | [2026-07-26](../CHANGELOG.md#0110---2026-07-26) |
| 0.10.0 | Per-turn context assembly and invalidation replace the appended string, plus durable memory across runs | [2026-07-26](../CHANGELOG.md#0100---2026-07-26) |
| 0.9.1 | The three-OS matrix goes green — defects shipping since 0.3.0 that no local run could see | [2026-07-26](../CHANGELOG.md#091---2026-07-26) |
| 0.9.0 | The tool layer closed: an in-process `Tool` trait offered to the model beside the built-ins | [2026-07-26](../CHANGELOG.md#090---2026-07-26) |
| 0.8.1 | The execution gate could be defeated by the file it was verifying; fixed | [2026-07-25](../CHANGELOG.md#081---2026-07-25) |
| 0.8.0 | An MCP client, and `Act::Net` as the permission model's fourth act | [2026-07-25](../CHANGELOG.md#080---2026-07-25) |
| 0.7.0 | Durable unattended runs — a step and its checkpoint committed together, resumed from the last one | [2026-07-25](../CHANGELOG.md#070---2026-07-25) |
| 0.6.0 | The execution sandbox: gate commands run in an ephemeral per-run sandbox, OS-native over a portable floor | [2026-07-24](../CHANGELOG.md#060---2026-07-24) |
| 0.5.0 | Sub-agent composition, bounded by an operator-held containment ceiling | [2026-07-24](../CHANGELOG.md#050---2026-07-24) |
| 0.4.0 | The permission policy — deny-first named layers — and a human approval pause a run resumes from | [2026-07-24](../CHANGELOG.md#040---2026-07-24) |
| 0.3.0 | Workspace-wide search and multi-file edits, and a choice of three providers behind one surface | [2026-07-24](../CHANGELOG.md#030---2026-07-24) |
| 0.2.0 | Budgets, retry, a full trace, resumable runs, and verification that compiles what the run produced | [2026-07-24](../CHANGELOG.md#020---2026-07-24) |
| 0.1.0 | One agent, from a typed task contract to a verified file edit, in-process | [2026-07-23](../CHANGELOG.md#010---2026-07-23) |

## Beyond the pillars

- **Providers** — OpenRouter, Anthropic and OpenAI, plus one `Compatible`
  provider reaching any OpenAI-shaped endpoint and 21 vendor presets behind named
  constructors — 13 hosted and 8 local runtimes — all over the crate's own
  HTTP+SSE client, with fallback between them
- **Provider-executed web search and fetch** — one `WebAccess` declaration each
  vendor translates into its own shape, with the sources cited recorded in the
  trace; the provider dials the URL, so the domain filter is the vendor's
- **Sessions** — a durable conversation over a workspace where every turn is a
  run with its own steps, budgets, policy boundary and checkpoint; tokens stream
  as the model produces them, a turn is steered mid-flight or interrupted, and
  any earlier turn is a branch point rather than a restart
- **Agent composition** — spawn and nest many agents under one shared budget
- **The agent mailbox** — every agent in a tree has an address and sends one
  named sibling a line of text, read oldest-first exactly once and optionally
  waited on for a bounded number of seconds; an address reaches inside its own
  tree and nowhere else, and a message grants nothing — the boundary stays the
  `Policy` a child inherited and narrowed
- **Ephemeral code-exec sandboxes** — OS-native per platform over a portable floor
- **Long-running autonomous tasks** — unattended and crash-resumable
- **Extensibility** — an MCP client over stdio and streamable HTTP, in-process
  `Tool` implementations, and markdown skills
- **Built-in tools** — filesystem, grep and find over a policy-scoped workspace,
  where a read returns the file, the line range it asked for, or a refusal naming
  the size and the ceiling — never a shortened file that looks like a whole one
- **Speculative read-ahead** — a provider that reports a finished tool call while
  its completion is still streaming lets the harness start the read-only ones
  then; the call is used only if the completion finally returned carries it
  byte-identical at the same position, so an over-eager provider costs a wasted
  read and never a wrong action
- **Driving a browser** — open a page, click, type, scroll, read what it rendered
  and look at it, on every supported platform and behind its own cargo feature;
  every document navigation is an `Act::Net` check against its `host:port`
  decided at the paused request rather than at the URL a tool was handed, so a
  click, a redirect and a script assigning `location` are gated by the same code
  as the URL the model typed
- **LSP navigation** — definition, references, symbols, hover and rename answered
  by a language server named in `io.toml` or on the contract, offered only to a
  run that configured one; `lsp_rename` returns a patch you apply yourself, so
  every byte reaching the workspace still passes an `Act::Write` check on where
  it lands
- **Documents** — spreadsheets, Word, PowerPoint text, PDF and barcode decoding,
  each behind its own cargo feature, all off by default
- **Images** — passthrough to any provider whose model accepts one, with BMP,
  TIFF, ICO, TGA and PNM converted to PNG at the door, and the formats needing a
  decoder this crate does not carry refused by name with the conversion that
  fixes them
- **Configuration** — one `io.toml` across four scopes merged in a fixed order,
  with `${env:}` and `${file:}` substitution, every key landing in a type this
  crate already had rather than in a second configuration model beside the API
- **Hooks** — `[[hook]]` tables in `io.toml` turn an audit log, a notification, a
  formatter and a local policy check that can stop the run into a path or an
  argv instead of Rust, reaching the run through the `Observer` the crate already
  had; the whole array is refused in the project scope, because a committed file
  that runs a command on a teammate's machine is not something to inherit
- **Retention** — the store answers what it and each session are holding, removes
  a session whole or every session older than a date, and empties a session of
  words while keeping every row an audit rests on; nothing expires on its own,
  and no model can call any of it
- **Undo** — `rewind` puts one file back to the state before the run first
  touched it, `rewind_run` does that for a whole run together with the memory it
  wrote and the children it queued, and `rewind_step` reverse-applies one step's
  recorded diff; nothing in the trace is deleted, because the spend happened
- **Git** — status, diff, log, add, commit, branch and worktree as fixed-argv
  built-ins, so a run ends as a reviewable commit on a branch of its own

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
