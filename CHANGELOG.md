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

## [0.80.0] - 2026-09-05

**Nothing new ships. Everything known to be wrong is fixed.** Two defects an
io-cli field test found in its first hour of ordinary use, the seven security
residuals 0.74.0 named and did not close, the seven more it recorded while
fixing the fifty-one it did close, two filed correctness defects, and the
security advisory that release still owed. Every item is a correction of
behaviour this crate has already published.

**Three of these corrections narrow what a run may reach.** If you rely on any of
them, widen deliberately — each is named under *Security* with the versions it
was wrong in.

**One residual is carried rather than closed.** The Landlock rung still grants
the whole system temporary directory. Narrowing it was implemented and withdrawn
in this release: a `git worktree` child's object store lives in the parent
repository, outside its workdir, so a narrowed grant let the child write its file
and refused its commit. Closing it needs a way for a run to declare a writable
root of its own, which is 0.81.0's work.

### Added

- A CI leg that runs the Linux mount rungs. `MOUNT_SETUP` carries three shipped
  fixes and **the shell had never executed on any machine**: the security suite
  returns early unless the chain picks a mount rung, and neither Linux leg does.
  The new leg makes Landlock unavailable *at the syscall* — a seccomp profile
  answering `ENOSYS`, which is what a kernel built without it answers — so the
  rung is not selected, it is made unavailable. An environment variable pinning
  the rung would be a downgrade reachable by anything that can set one.
- A draft security advisory for the four critical issues 0.74.0 closed, at
  `docs/advisory-0.74.0-draft.md`, with each issue's mechanism, how it is
  reached, and a workaround for an operator who cannot upgrade.

### Changed

- **`sandbox.allow_network` and `policy.defaults.net` now widen the sandbox, as
  documented.** They did not. `ExecContainment::with_egress` replaced the
  operator's `[sandbox]` answer with the policy's instead of combining them, and
  every spawn site calls it — so the key was overwritten before a command was
  wrapped and changed nothing at any scope. No dev server could bind a port and
  no package install could resolve a host. The two answers combine now. This
  does not re-open what 0.74.0 closed: the key is refused in any file inside the
  workspace, so it is the user scope's to write.
- **The prompt describes the containment each command actually gets.** It was
  built from the raw `[sandbox]` section with the egress answer never applied,
  and its network sentence said outbound was "permitted only where this run's
  policy permits it" whether the sandbox granted the network or denied it
  outright — wrong in both directions, because a contained command's network is
  all or nothing at that layer. Three sentences now, one per state.
- **A `reasoning` event is emitted before the tokens it produced.** It was
  emitted after the completion resolved while tokens came from the streaming
  loop, so a consumer drawing the stream in arrival order drew the thought below
  the answer.
- A **named** local provider endpoint — `http://localhost:11434/v1` for a local
  model — is refused unless `IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` is set. A run
  already required that; driving a `Provider` directly did not, and now the two
  agree. An IP literal is unaffected.
- Windows AppContainer profile names carry the creating process id. The
  unpredictable half keeps all sixty-four of its bits.

### Fixed

- **A step carrying gate feedback kept its assistant turn.** Feedback was
  classified as a tool result, so on a step with N calls it took ordinal N, the
  transcript's pairing check failed for that step's whole run of results, and
  every one of them was flattened into prose — the step's assistant turn and its
  native tool-call blocks were never emitted, and on the Anthropic wire the step
  became plain user text. Nothing reported it; a warning does now. (#246)
- **A step carrying `propose_plan` beside a refused call no longer goes out
  malformed.** The plan-revise arm answered that call without taking its
  position, leaving a `tool_use` with no `tool_result`. (#246)
- `Config::is_empty()` accounted for twelve of the twenty sections the file
  format carries, so a configuration holding only `[[hook]]`, `[[lsp]]`,
  `[[plugin]]`, `[browser]`, `[memory]`, `[routing]`, `[otel]` or `[codeact]`
  reported empty — and an empty configuration is one that needs no review.
- `read_file` and `read_typed` are capped like `read_bytes`. All three refuse a
  file over the limit in the same words.
- A write through a symbolic link inside the workspace root gets its
  re-decision on FreeBSD, NetBSD and DragonFly, which report `EMLINK` where
  Linux and macOS report `ELOOP`. The write already failed closed there; what
  was lost was a documented capability.
- `scripts/cross-check.sh` compiles again. It has not since 0.74.0, so the local
  Linux gate has been silently unavailable for five releases — and a run that
  skipped every target printed its skips and exited 0. Nothing checked is a
  failure now. (#255)

### Security

- **An absolute read or write target is containment-checked.** It went straight
  to the policy with the containment check skipped, for one consumer's benefit —
  `read_skill`, whose bundle lives outside the root by design — and every
  consumer of an absolute target inherited it. That consumer asks for its
  allowance by name now. *An absolute target outside the root is now refused.*
- **A provider endpoint is dialled at the address it was graded at.** The
  endpoint was graded before the run's first step and then resolved again by the
  provider's own client, so a name that answered with a local address only the
  second time reached it.
- **`Skills::discover` does not follow a symbolic link out of the root it was
  given.** A link inside a legitimate skills directory yielded a `SKILL.md`
  whose frontmatter reached the system prompt. *A skill symlinked in from
  elsewhere no longer loads, and says so.*
- **A permitted page's subresources are gated.** Request interception covered
  documents only, so an uncontained run under a narrow `allow_net` still let an
  `<img>` element carry bytes to a host the policy never allowed. One row per
  document is unchanged: a permitted subresource is decided and not recorded, a
  refused one is recorded like any other refusal.
- **The browser child is contained** on runs that own an egress proxy, which is
  the signal that the run asked for containment. It was spawned with nothing at
  all while every other child of the same run was contained.
- **An approver's rewritten target is refused rather than discarded.** Both a
  provider authorization and a shell redirect threw the rewrite away and
  performed the original, so an approver *narrowing* a host or a path was
  overruled in silence. That is audit M4's decision for `Act::Exec` reaching the
  two sites shaped like it.
- **`write_leaf` no longer follows a directory swapped for a symbolic link.**
  The parent chain was checked and then resolved again by the write, with
  `O_NOFOLLOW` covering the final component only — so `root/a/b/x` with `a`
  replaced by a link to `/etc` created `/etc/b` and wrote `/etc/b/x` past a gate
  that had allowed a path inside the root. Every writing entry point routes
  through it. Each component is now opened from the descriptor of the one above
  it.
- `send_message` and `read_messages` are refused during the plan phase by rule.
  They were unreachable only because a refused `spawn_agent` leaves no siblings
  to talk to — unreachable by accident rather than by rule.
- A hard-killed run's Windows AppContainer profile is reaped by the next run.
  `Drop` cannot run on `TerminateProcess`, and each leftover adds ACEs to a
  workspace DACL that caps at sixty-four kilobytes. Only profiles whose creating
  process is gone are deleted; a pid that has been reused reads as alive and is
  left alone.

## [0.79.1] - 2026-09-05

**The README names what the crate actually ships.** Four capabilities that
shipped between 0.76.0 and 0.79.0 were named nowhere on the landing page a reader
opens: structured output and `OutputSchema`, `run_program`, `ToolMask` and
`Collapse`. Every drift checker in this repository catches a documented sentence
that became false, and none of them catches a capability that was never
documented — so this release closes that asymmetry with a gate as well as with
prose, and re-reads the comparison tables, whose stated date had not moved in
nineteen releases.

No behaviour changed. The only lines this release adds under `src/` are doc
comments, and `docs/public-api.txt` is unchanged.

### Added

- A drift gate over omission: every release row in `docs/CAPABILITIES.md`'s
  register at or after 0.73.0 must either be represented in the README by a name a
  reader can search for, or be listed as having introduced none, with its reason.
  A release can no longer pass by silence, and the failure names the version.
- A staleness bound on the README's comparison tables — 180 days behind the newest
  release the register records — because the date those tables carry is a claim
  about work done rather than a disclaimer.

### Changed

- The README names `OutputSchema` and the `output_schema` key, `run_program`,
  `ToolMask`, `Collapse` and `read_skill`, each in the section that already covers
  its area and each linked to the guide carrying its depth.
- Both comparison tables were re-read against each project's own current
  documentation on 2026-09-05 and the stated date restated. Five cells moved:
  Claude Code documents a denial record (`/permissions` → *Recently denied*, and a
  `PermissionDenied` hook) and a headless spend cap (`--max-budget-usd`, which a
  subagent's spend counts against); Codex CLI's `approval_policy` gained a
  `granular` table and `sqlite_home` names a database for resumable runtime state;
  Goose removed its macOS seatbelt sandbox outright; and rig, swiftide and
  langchain-rust all document per-call token accounting the table reported as
  absent. Three links that had died or moved were corrected.
- The paragraph beneath the tables no longer claims that nothing else documents a
  tree-wide spend ceiling; it says what is still unmatched instead.
- `docs/CONTRACT.md` states what an output schema guarantees and what it cannot —
  the closed keyword subset checked at declaration, `response_format` with
  `strict: false` on the OpenAI-shaped wire against nothing at all on the Anthropic
  Messages API, the local check that decides either way, and the bounded re-prompt
  that ends in `RunOutcome::SchemaUnsatisfied`.

## [0.79.0] - 2026-09-04

**A sequence of tool calls collapses into one contained program.** A step that
would have been six round trips — grep, read, read, read, edit, exec — is one
Python program the model writes once, and control flow it currently has to
simulate across six completions becomes control flow an interpreter runs.

The program is a tool call, not a new kind of step, so the transcript pairing,
the step cap, the attribution columns, the tool mask and the `before_tool` hooks
all keep working. It runs inside the containment a backgrounded command line
already gets, in a scratch directory of its own rather than in the workspace, and
every act it takes re-enters the same dispatch a model's own call takes — same
policy, same gate, same `policy_events` row, same journal attempt, same observer
event. Starting the interpreter is itself one of those checks, and on a host where
the containment the run asked for cannot be applied the program is refused rather
than started under a boundary it does not have. A program is a shorter way of
asking, not a wider door.

The interpreter is the host's, found the way a browser is found, and **nothing is
downloaded, ever**. A machine without a usable one is a supported machine: the
tool is not offered, the turn runs exactly as it did at 0.78.0, and the decision
is on the record rather than left to be inferred.

**Reach for a program when the work is a loop with a branch the tools cannot
express — not to save tokens.** CodeAct is usually introduced with a figure of
roughly 64% fewer tokens, measured on another harness. This release measured the
comparison here instead and did not find a saving: offered the tool on a task
shaped for it, the model wrote no program at all; told to use one, it wrote four
and spent five times the tokens the same work took as ordinary calls. Every
program ran and every answer was right, so that is a cost result rather than a
correctness one. One task, one model, one sample per arm — the numbers, the
method and the machine are in `docs/MEASUREMENTS.md`, and they are recorded
rather than replaced with somebody else's.

### Added

- `codeact`, an off-by-default feature letting a run write one contained Python
  program instead of a chain of tool calls. It adds **no crate**: the interpreter
  is a host binary, not a dependency, and a build that does not ask for the
  feature resolves the same dependency graph it did at 0.78.0.
- The `run_program` built-in, whose one argument is a complete Python program —
  source, not a command line. Inside it, the tools the run already has are
  ordinary functions taking the same arguments by keyword; each returns an object
  that is falsy when the act was refused **or failed** — a non-zero `exec` and a
  file that is not there are both `.ok` false — so a program branches instead of
  stopping, and reads `.text` to tell a refusal from a command that ran and
  failed. A step that would have been six round trips — grep, read, read, read,
  edit, exec — is one program written once.
- Every act a program takes re-enters the same dispatch a model's own tool call
  enters, so the policy, the gate, the `policy_events` row, the journal attempt
  and the observer see a program's act on exactly the terms they see a model's. A
  program is a shorter way of asking, not a wider door: it reaches nothing the
  model could not have reached by asking one call at a time. What collapses is
  the number of provider round trips, not the number of boundaries.
- Starting the interpreter is an `Act::Exec` check in its own right, taken before
  anything is spawned, against the interpreter's path and against the whole argv —
  both spellings, exactly as `exec` checks them. A run whose policy denies
  execution denies programs too. `run_program` is refused while the plan gate is
  active for the same reason `remember` is: the gate is a layer denying `Write`
  and `Exec`, and starting an interpreter is a third check that layer cannot see.
- A program that asked to be contained is **refused on Windows**, naming the
  backend the run resolved, because the living-child seam applies no backend
  there — `wrap_argv` has no Windows branch, `apply_rlimits` is unix-only,
  `contain_command` answers `None`, and the Job Object belongs to the `Sandbox`
  runner and to `shell_start`'s own suspended spawn. Started anyway it would have
  had the full filesystem and the full network while the trace reported a boundary
  granting neither, which is 0.74.0's rule. A Windows run that asked for no
  containment is unaffected and runs uncontained, exactly as its `exec` does.
- Egress is denied and no proxy is named whatever the run itself was granted, and
  the claim is exactly what the backend delivers: `Backend::denies_egress` is
  false for the portable floor, so under it a program can still open a socket, and
  a run with no containment at all has nothing to deny it with.
- A deferral inside a program is a **denial for that program's acts only**. A
  pause is coherent for a model's own call and not for an act inside a live
  program — the acts already taken have happened, and a resumed run writes the
  program from scratch and would re-execute them — so `Decision::Defer` becomes a
  denial the program branches on, in this crate's own words. The caller's
  `Approver` is untouched everywhere else and the model's own calls can still be
  deferred.
- A program's output is bounded at the source: the shim truncates what was printed
  before sending it, and a frame larger than 4 MiB is refused by the reader rather
  than buffered, so a program cannot make the harness allocate without limit. A
  `sys.exit` is read by its code — `None` or zero is a finish, any other code is a
  failure carrying that code.
- `TaskContract::with_codeact` and `CodeActConfig`, naming the interpreter to use
  and the two bounds on how far one program may reach: `max_callbacks`, 64 by
  default, and `timeout`, 120 seconds by default. Both bound what a program makes
  *this* process spend, which is what a tight callback loop exhausts and what no
  sandbox rlimit can see. `max_callbacks` counts the calls actually served, so a
  program that makes exactly its allowance and then finishes completes normally
  and keeps its output; the call past the bound is the one that stops it. Asking
  is a request rather than a guarantee — the tool is offered only if a usable
  interpreter was also found.
- Host-interpreter discovery, once per run. `CODEACT_CANDIDATES` names what is
  looked for and in what order — `python3`, then `python` — and every candidate is
  version-probed against `CODEACT_MIN_PYTHON`, `(3, 8)`, so a `python` that
  answers 2.7 is rejected by what it reports rather than trusted by its name.
  **Nothing is downloaded, ever**, and nothing is installed for a program either —
  the interpreter is spawned as itself, so what a program can `import` is whatever
  that host interpreter already carries. A host with no usable interpreter is a
  supported host: `run_program` is simply not advertised, and the turn is
  composed, sent and stepped exactly as it would have been with the feature off.
- The `[codeact]` configuration section — `interpreter`, `max_callbacks` and
  `timeout_secs` — read from the user-scope file. It is refused at project scope,
  and in `io.local.toml` too: the table names a program on this machine that every
  program the model writes is handed to, and a project-scope file arrives with a
  clone.
- `EventKind::Program`, emitted once before the first step with outcome
  `available` or `withheld` and what each candidate answered, and once per program
  afterwards with the callback count and the outcome — `finished`, `failed`,
  `bound` or `timeout`. The acts a program took are not on this event; each
  arrives as its own `ToolCall`, where every other request is observed.
- `CODEACT_UNCALLABLE`, naming the built-ins a program may not call: `remember`,
  `forget`, `todo_write`, `ask_question`, `ask_questions`, `propose_plan`,
  `read_skill`, `run_program`, `spawn_agent`, `send_message` and `read_messages`.
  It is a literal rather than a derivation, pinned by a test, so a built-in added
  later fails that test until somebody classifies it instead of becoming callable
  silently.

### Changed

- `run_program` is a reserved built-in name, so a registered tool may no longer
  claim it. The name is reserved whether or not `codeact` is compiled, so the set
  of names a custom tool can take does not depend on a feature flag.
- `MCP_SERVER_UNSERVED` names `run_program`. A served session lends the boundary,
  not a way to drive it in a loop the operator at the far end cannot watch — the
  client writes its own loop and makes its own calls.

### Deprecated

### Removed

### Fixed

### Security

## [0.78.0] - 2026-09-04

**The best trace in the field becomes readable by things that are not this crate,
and the boundary becomes borrowable by other agents.** Two absences with one root:
everything this crate knows about a run has been recorded with unusual precision
and has been legible only to this crate.

A run can now be exported as OpenTelemetry spans, following the GenAI semantic
conventions, to any OTLP/HTTP collector — so an agent appears in the dashboard an
operator already runs, beside the services it called. The exporter is an
`Observer`, the door that already existed, so the run loop does not move.

And this crate's own tools can be served over MCP on stdio, so another harness
can call `grep`, `read_file`, `edit_file` and `exec` through **this** crate's
policy rather than through its own. Every served call goes through the same
dispatch a model's call goes through, so the gate decides it, a `policy_events`
row records the decision and the journal keeps it. What is being lent is the
boundary, not the tool.

Both halves are off unless asked for, and **neither adds a dependency**. A caller
who enables neither feature compiles the same code and resolves the same
dependency graph as at 0.77.0 — the unique package count is 154 on the default
build with either feature on, with both on, and unchanged at 327 with
`--all-features`.

### Added

- `otel`, an off-by-default feature exporting a run as OpenTelemetry spans over
  OTLP/HTTP with a JSON body. `OtelExporter` implements `Observer`, so it attaches
  through `Harness::with_observer`, any `*_observed` entry point, or an observed
  session turn. `OtelConfig` names the collector, the service, the headers, the
  per-request deadline and the queue bound.
- The GenAI span tree: an `invoke_agent` span per run, a span per committed step,
  an `execute_tool <name>` span per tool call, and a `chat <model>` span per
  provider call carrying the model, the token split and the latency that call
  actually cost. The per-call facts come from the store, because the event channel
  carries no provider-call event, no end for a tool call and no timestamp.
- `mcp-server`, an off-by-default feature serving this crate's own tool catalogue
  over MCP on stdio. `serve_mcp` is the unattended door and uses `DenyAll`;
  `serve_mcp_with` takes an `Approver`. `McpServerConfig` names the workspace root,
  the store, the policy and any registered tools the operator chooses to include.
- `MCP_SERVER_UNSERVED`, naming the tools a served session does not offer —
  `ask_question`, `ask_questions`, `propose_plan`, `spawn`, `send_message`,
  `read_messages`, `read_skill`, `remember`, `forget` and `todo_write`. The
  served set is written out by name and pinned by a test, so a tool added in a
  later release fails that test until somebody decides which side it belongs on.
- `GENAI_CONVENTIONS` and `OTEL_DEFAULT_ENDPOINT`, naming the convention revision
  this crate follows and the port the specification names.
- Two clippy polarities in CI, `--features otel` and `--features mcp-server`,
  beside the existing `documents` and `media` runs.

### Changed

- Nothing in the run loop. The only edits under `src/run/` are `pub(super)` to
  `pub(crate)` visibility promotions, so `src/mcp_server.rs` can reach the tool
  catalogue and the dispatch a served call routes through. No public item moved
  and `docs/public-api.txt` gained lines without losing any.

### Deprecated

### Removed

### Fixed

### Security

- A served MCP session grants access deliberately and under an explicit boundary:
  the feature is off by default, the policy defaults to the tiered
  `Policy::default()` where a write or an exec is an `Ask` and egress is denied,
  and the default approver is `DenyAll` — so a rule that would ask a human refuses
  instead, because there is no human at the far end of a pipe. A stdio pipe carries
  no identity, so whoever can spawn the server can call whatever the policy allows;
  the operator who starts it is the one who decides what that is.
- The three tools the policy deliberately does not see — `remember`, `forget` and
  `todo_write`, which write to the harness's own store rather than to the
  workspace — are **not served**. Their only boundary is the plan gate, and a
  served session has none, so serving them would have put two disabled boundaries
  behind a write that reaches the durable memory recalled into every later run
  over the same workspace.
- The exporter sends no transcript content. `gen_ai.input.messages`,
  `gen_ai.output.messages` and `gen_ai.system_instructions` are opt-in in the
  convention and are not implemented here at all — absent rather than defaulted
  off, so no flag can include them by accident. The `[otel]` configuration section
  is refused at project scope for the same reason `[routing]` is: a cloned
  repository must not choose where a run's metadata is sent.

## [0.77.0] - 2026-09-03

**The transcript says what kind of thing each part of it is, and a caller can demand
a shape.** Two absences with one root: this crate recorded what happened with unusual
precision and said almost nothing about the *kind* of what happened.

Every piece of content in the transcript now carries where its bytes came from —
an operator's instruction, the agent's own words, the harness's prose, or external
content by source: a file, a shell, a web page, an MCP or LSP server, a skill body,
a child agent. The mark is set at the construction site rather than inferred later,
stored beside the observation, readable on the `Observer` channel, and **framed in
the prompt** so the model reads external content as content rather than as
instruction. That is the middle of the field's three-part answer to prompt
injection: the boundary shipped in 0.4.0 and was proven in 0.74.0; neutralization at
the subagent boundary is still not shipped and is named for a later release.

Alongside it, a caller can declare a JSON Schema for a run's final output. The
schema rides the wire natively where a wire has a place for it, the model's final
text is validated locally before the run may report success, and a failure
re-prompts with the validation error — bounded, traced, and drawing against the
budgets the run already has.

**What this release does not claim.** Whether a marked and framed transcript changes
what a model actually does is a scoring question, and the instrument for it is
`io-eval`, built separately. This release delivers a structural distinction and
declines to imply the measurement.

### Breaking changes

- **BREAKING** — `context::Observation::new` takes a fifth argument, the
  `context::Origin` of the content. There is deliberately no defaulting form and no
  `new_unmarked`: a construction site that does not say where its bytes came from is
  a compile error, which is the only enforcement that survives the next release
  adding a tool.
  *Migration:* pass the origin. Use `Origin::Operator` for what a human said,
  `Origin::Agent` for the model's own words, `Origin::Prose` for text your own code
  wrote, and the matching external variant for anything that arrived from a file, a
  process, the network, a server, a skill or a child. Do **not** pass
  `Origin::Unmarked` — it exists only as what a row written before 0.77.0 reads back
  as.

  ```rust
  use io_harness::context::{ObsKind, Observation};
  use io_harness::Origin;

  // Before
  let obs = Observation::new(step, ObsKind::Read, Some(path), text);
  // After
  let obs = Observation::new(step, ObsKind::Read, Some(path), text, Origin::File);
  ```

- **BREAKING** — `context::Observation` and `context::Emitted` each gained an
  `origin` field, so a struct literal naming every field no longer compiles.
  *Migration:* `Observation` is built through `Observation::new` above. For
  `Emitted`, name the field; carry it through from the entry it came from rather
  than deriving it from the `piece` beside it, which answers a different question
  (see below).

- **BREAKING** — `provider::CompletionRequest` gained `output_schema`, so an
  exhaustive struct literal no longer compiles. This is the same break `media`
  (0.15.0), `model` (0.21.0), `web` (0.22.0) and `effort` (0.31.0) each took, and
  for the same reason: the type's whole ergonomic is a struct literal with
  `..Default::default()`.
  *Migration:* construct with `..Default::default()`, which the type has derived
  since it existed.

- **BREAKING** — `observe::EventKind::ToolCall` gained an optional `origin` field.
  `#[non_exhaustive]` covers new *variants*, not new *fields* on a variant, so a
  pattern naming every field no longer compiles. Same breakage class as 0.68.0's
  `EventKind::Mcp::tools`.
  *Migration:* end the pattern with `..`.

  ```rust
  // Before
  EventKind::ToolCall { name, target } => { /* … */ }
  // After
  EventKind::ToolCall { name, target, .. } => { /* … */ }
  ```

- **BREAKING (behaviour)** — the prompt bytes moved for every run. External content
  in the observation section is wrapped in an `external_content` tag, and the user
  block carries one note saying what the tag means. **The tag is emitted for every
  prompt family, not only Anthropic**, because a boundary made of prose alone can be
  forged by the quoted content itself, which is the attack the framing exists to
  stop — an Anthropic-only delimiter would ship the defence off for most of the
  fleet. The note is unconditional, so runs that never call a tool see it too; a
  constant never moves, and a sentence that appeared only on the turn a run first
  read a file would withdraw the cache marker for nothing.
  *Migration:* none required. If you compare prompt bytes against a stored baseline,
  regenerate it. Nothing about what the agent may do changed — a mark is a statement
  about the record, never a grant.

### Added

- `context::Origin`, re-exported at the crate root: where a piece of transcript
  content came from. Twelve variants, `#[non_exhaustive]`, with `is_external()` —
  the one place that decides what "untrusted" means — and `as_str()`.
- `schema::OutputSchema`, re-exported at the crate root: a JSON Schema validated
  against a stated, closed keyword subset. Construction is fallible and refuses any
  keyword the crate does not implement, **naming it**, so a schema that exists has
  been understood in full. Deserialization goes through the same constructor, so a
  config file cannot smuggle in an unchecked schema.
- `TaskContract::with_output_schema` and a `[run] output_schema` config key.
- `provider::CompletionRequest::output_schema`, emitted to the OpenAI-shaped wire as
  `response_format` with `strict: false`. Anthropic's body is deliberately
  untouched: that API has no `response_format`, its native route to a shape is a
  forced tool call, and the schema is enforced locally on that path regardless.

### Changed

- An observation's origin is persisted in a new nullable `ledger_observations.origin`
  column, added by `ALTER TABLE` with `CHECKPOINT_FORMAT` **unmoved**. A store written
  by 0.76.0 opens unchanged and no migration runs; a row written before this release
  reads back as `Origin::Unmarked` rather than as an error or as a guess. Proven
  against a real 0.76.0 binary from crates.io, in both directions, by
  `tests/cross_version_0_76_0.rs`.
- `context::Piece` and `context::Origin` are **independent on purpose**, and this is
  worth knowing if you consume either. A `Piece` is a *layout role* — `Piece::Result`
  is what makes an entry occupy a tool call's position — while an `Origin` is a
  *provenance fact*. Deriving one from the other was tried during this release and
  reverted: it forces every entry answering a tool call to claim an external origin
  whatever its bytes are, so an operator's own answer to `ask_question` would have to
  be recorded as tool output to keep the transcript well formed.

### Security

- Tool output, a fetched page, an MCP server's reply and a child agent's conclusion
  are now structurally distinguishable in the transcript from an operator's
  instruction, both on the record and in the prompt the model reads.
- **A resumed pre-0.77.0 run is not framed.** Rows written before the origin column
  read back `Unmarked`, which is not external, so their content carries no frame.
  Unmarked is a fact rather than an error, and nothing infers a provenance for a
  historical row — but the defence does not reach a ledger restored from an older
  store.
- **Known ceiling:** external text containing a closing `external_content` tag can
  end its own frame early. The body passes through byte for byte because this
  release marks content and explicitly does not transform it, and an escaping scheme
  is a second silent thing to get wrong. Neutralization at the subagent boundary,
  which is where transformation belongs, is named for a later release.

## [0.76.0] - 2026-09-03

**A turn can withhold a tool without moving the catalogue, and an observation that
will not fit whole can be carried shortened rather than thrown away.** Both are the
same decision made twice: the part of a request a caller wants to vary per turn is
the part the cached prefix must not cover. A mask leaves the tool array
byte-identical and refuses the call at the gate instead; a collapse shortens an
entry while the turn is assembled and buys no summary to do it. Beside them, the
two ways this repository's own suite could report a pass that meant nothing are
closed.

### Breaking changes

- **BREAKING** — `context::Assembly` gained a `collapse` field, so a struct literal
  naming every field no longer compiles. The type holds borrows and has no
  `Default`, so the field has to be written out.
  *Migration:* name it, and name it `Collapse::default()` unless you want the new
  rung — off assembles exactly what 0.75.0 assembled.

  ```rust
  use io_harness::context::{assemble, Assembly, Collapse};

  // Before
  let out = assemble(&ledger, budget, &notes, &global, Assembly {
      ws, policy, store, run_id, step,
  }).await?;
  // After
  let out = assemble(&ledger, budget, &notes, &global, Assembly {
      ws, policy, store, run_id, step,
      collapse: Collapse::default(),
  }).await?;
  ```

- **BREAKING** — `context::Assembled` gained a `shortened` counter, so a struct
  literal naming every field no longer compiles. It counts the observations a
  collapse carried shortened rather than stubbed, and it is zero on every run that
  configures no collapse.
  *Migration:* construct with `..Assembled::default()`, which the type has derived
  since it existed, and read the new counter beside `stubbed` wherever you report
  on a turn's projection. An entry counted in both `carried` and `shortened` was
  carried short rather than whole; `carried` alone still means whole.

  ```rust
  use io_harness::context::Assembled;

  // Before
  let a = Assembled { text, carried, stubbed, reread, recalled, recalled_keys,
                      collapsed, est_tokens, emitted };
  // After
  let a = Assembled { text, carried, stubbed, ..Assembled::default() };
  ```

### Added

- `ToolMask` and `TaskContract::with_tool_mask`: the tools a turn may not call,
  withheld by name. **It withholds availability, not membership.** The catalogue a
  masked run sends is byte-identical to an unmasked one's — the same `ToolSpec`
  values in the same order, the same schemas, the same tokens paid for them.
  Nothing is removed from the request. What changes is that the turn's prompt names
  the withheld tools after the observation section, and a call to one of them is
  refused before anything is started. Removing the definitions instead would save
  each one's tokens once and pay a cache *write* on every later turn of the run,
  because the tool array sits ahead of 0.38.0's breakpoint at the end of the system
  block and any byte changed in it invalidates that entry and everything after it
  in the same ordering.
- Masking is enforced at both places a tool call can begin — the head of `dispatch`
  and `read_batch`'s per-call loop — because a batched read-only call does not route
  through `dispatch`. A refusal is the existing `EventKind::Refused` with the tool's
  name where a rule's pattern would be and `turn tool mask` where a layer would be;
  there is no new event kind and no new error variant.
- A mask is a deny set rather than an allow set, for the reason every other refusal
  in this crate is deny-first: a list of permitted names silently withholds the next
  tool anyone adds, and a caller's list written against this version would quietly
  narrow their agent on the next one. A withheld name that no tool answers to is
  kept rather than rejected, so a mask stays portable across feature flags and
  across runs that configured different toolboxes.
- A mask is a property of a turn, not of a run and not of a tree. It is read where
  `fold_now` is read, and it does not reach a spawned child: a child's contract is
  built fresh and carries none.
- Context Collapse — `Collapse` and `TaskContract::with_collapse`: an observation
  that will not fit the turn's budget is carried shortened rather than replaced by a
  one-line stub. It is a read-time projection and not a rewrite, so it costs no
  provider call, writes no `summaries` row, and leaves the ledger exactly as long as
  it was. Turning it off on a later turn assembles every entry it shortened whole
  again, which a fold cannot do.
- A shortened entry keeps its `ObsKind` and its `target`, so assembly's
  invalidation and re-read rules still find the write that supersedes an earlier
  read of the same path. Behind a fold those entries have become one prose paragraph
  with no path and no kind, and the stale-read machinery cannot see them — which is
  why the ladder takes this rung before a fold's.
- The shortening reuses `bound`, the same helper that caps a single oversized
  observation, so a collapsed entry carries the marker a truncated one carries and a
  reader learns one convention rather than two: an `ObsKind::Read` keeps its tail,
  every other kind keeps its head.
- `Collapse { keep_chars: 0 }` is off and is the default, so a caller who configures
  nothing gets 0.75.0's projection byte for byte. `fold` remains the last rung and
  remains the default trigger; a collapse only ever changes what happens to an entry
  that was going to be stubbed anyway.

### Changed

- The per-turn assembly trace line gained `shortened=` and renamed its existing
  `collapsed=` to `stubs_collapsed=`. The two are different events and the release
  notes make the words collide: `stubs_collapsed` has meant "the elision lines were
  merged into one to hold the ceiling" since 0.10.0 and has nothing to do with
  Context Collapse. Both are printed so neither has to be inferred, and each names
  the other where it is defined. This is a trace string rather than a public type,
  so nothing stops compiling; a reader parsing that line by key will need the new
  spelling.

### Fixed

- Two tests reported a slow host as a broken product (#232). Both asserted against a
  fixed wall-clock budget under a runner whose scheduling they do not control, so a
  missed deadline and a real defect arrived at the same assertion with the same
  message. The sandbox test that proves a forked child does not outlive the
  wall-clock kill now waits on the child process disappearing — an ordering the
  scheduler cannot reverse — and separates three outcomes: alive at the ceiling is a
  leak, gone with its marker written is a child that outran the kill, gone without
  one is a pass. A diagnostic CI round on the leg where it failed established that
  the group kill is sound there, so this is a test being made honest rather than a
  containment defect being hidden.
- The fleet queue poll asserted "the fixture never filled its queue" on whatever a
  fixed poll budget had found, naming a cause it had no evidence for. Its ceiling is
  a liveness bound now, and every failed read is kept rather than discarded — the
  fixture writes to the database while the test reads it, so a busy store was being
  swallowed and then blamed on the fixture.
- Three more cut-offs waited out a budget instead of waiting for a state. The
  crash-mid-fan-out test kept "no child has finished" true with a 500 ms child
  delay, which is a bet on the scheduler; a child parked for an hour cannot finish,
  and the cut-off above it was already a condition. The recovery matrix cut off at
  400 ms, which had to cover opening SQLite, the startup boundary probe's child
  spawns, composing a prompt, answering it and journalling an attempt — and when it
  did not, the run parked in the probe rather than at the kill point, the timeout
  still elapsed, the "was it cut off" assertion still passed, and the failure
  surfaced far away as an index panic on an empty attempts list. Each arm now waits
  for its own observable, and that suite runs in 0.34 s rather than 30.2 s.
- Six socket-absence assertions passed over a genuinely leaked socket. Each read a
  counter incremented by a separate accept thread with nothing ordering that thread
  against the assertion, and a count of zero is true whenever the thread has not been
  scheduled yet. `Sink` gains `wait_for` and `assert_only` in both copies that lacked
  them: the test dials its own sink and waits for that connection, and the OS cannot
  deliver a later dial ahead of an earlier one, so once the control is accepted
  anything the run opened has been accepted too. The same fix already existed in
  `tests/replay.rs` and had not been applied to the other two copies — nor, at one
  site, to the file that defined it.
- Two bare wall-clock thresholds asserted a duration on a shared runner and proved
  nothing about the code when they failed, which is the rule this repository already
  applies to every other duration it records. Both print now, and the property each
  stood in for is asserted structurally instead: that assembly is bounded by the
  turn's budget rather than by the ledger's length, and that skill discovery is paid
  once per run, which the test below it already proves by changing the directory
  mid-run.
- None of the changes in this section alters shipped behaviour. They change what
  this crate's suite is able to report.

## [0.75.0] - 2026-09-02

**A slow or expensive run is diagnosable from the trace, and the two costs the
crate paid silently stop being silent.** This crate measures more than any of its
peers and measured almost nothing about itself: the cache counters it has
recorded since 0.18.0 were never divided by anything, a step's wall clock was not
attributed at all, and the memory ranking re-tokenised the whole store on every
turn — which is why `memory.max_entries` was a knob nobody could raise.

### Breaking changes

- **BREAKING** — `Usage::cache_write_tokens` is now `Option<u64>`. `None` means
  the wire carries no cache-write counter at all, which is every OpenAI-shaped
  endpoint and therefore OpenRouter; `Some(0)` means a call that measured zero.
  Collapsing the two made a hit rate computed on that wire read as a complete
  accounting of a call that also paid to write the cache. This is the same
  distinction `CompletionResponse::ttft_ms` has always drawn — a provider that
  measured nothing reports nothing rather than zero.
  *Migration:* read the counter as an option, and treat `None` as unknown rather
  than as zero. Where you previously summed it:

  ```rust
  // Before
  let written = usage.cache_write_tokens;
  // After — an unreported write is not a free one
  let written = usage.cache_write_tokens.unwrap_or(0);
  // Better: say which it was
  if !usage.cache_writes_reported() { /* this wire cannot tell you */ }
  ```

  Pricing is unchanged: an unreported write is billed as fresh input exactly as
  it was before.
- **BREAKING** — `pricing::Spend` gained `unreported_cache_writes`, so a struct
  literal naming every field no longer compiles.
  *Migration:* construct with `..Default::default()`, as the type's siblings
  already document. The field counts calls in the group whose write cost the wire
  never reported, in the same "this group is a floor, not a total" role
  `unpriced_calls` already has.
- **BREAKING (trace)** — `EventKind::StepAttributed` is a new variant. The enum
  has carried `#[non_exhaustive]` since 0.24.0, so a `match` with a catch-all is
  unaffected.
  *Migration:* there is nothing to write for a consumer that already has a
  wildcard arm. A consumer that wants the new fact matches the variant; one that
  does not, ignores it.

### Added

- The achieved prompt-cache hit rate is readable from the accounting surface:
  `Usage::cache_hit_rate()` and `Spend::cache_hit_rate()` return the share of the
  prompt served from the provider's cache, and `Usage::cache_writes_reported()`
  says whether the wire reported a write cost at all. A call with no prompt has
  no rate rather than a rate of zero.
- `Store::spend_by_session`, beside `spend_by_model`, `spend_by_day` and
  `spend_by_run`. A session's cost was previously reachable only by folding its
  turns by hand, because every turn is its own run and the session tables carry
  no token columns. A run belonging to no session is absent from the grouping
  rather than bucketed under a sentinel.
- Per-step latency attribution: each committed step records where its wall clock
  went — the provider, tool execution, the policy gate of which, and the durable
  write that ended the step before — read with `Store::step_attributions`, which
  carries the step's time to first token beside it. Written inside the checkpoint
  transaction, so a driver that lost its lease cannot write one.
- `EventKind::StepAttributed` announces that attribution beside `EventKind::Step`,
  so a live run can be diagnosed without waiting for it to end and reading the
  store.
- `Routing::mechanical` names the model that answers the completions the crate
  makes on its own behalf. There is exactly one today — the summary a fold writes
  when the context is compacted, which until now was answered by whatever model
  was doing the work. Unset, nothing changes.
- A `[routing]` section in `io.toml`, projecting `escalate_after`/`escalate_to`,
  `downshift_under`/`downshift_to`, `require_primary` and `mechanical`. Routing
  has been able to change which model answers mid-run since 0.34.0 and was
  reachable only from Rust. A rule is a threshold and a model, and naming one
  without the other is refused at load rather than half-applied.

### Changed

- **BREAKING (behaviour)** — `list_dir`, `git_log`, `git_status` and `git_diff`
  are read-only calls now, so they overlap with each other and with `grep`,
  `find` and `read_file` inside one completion, and can start before the
  completion finishes streaming. 0.41.0 deferred exactly these four pending
  evidence; each is asked `Act::Exec`, `Act::Read` on `.git` and `Act::Read` on
  every path it names before anything starts, and runs under the same containment
  it would have had serially. `git_add`, `git_commit`, `git_branch`,
  `git_worktree` and `view_image` are unchanged.
  *Migration:* there is nothing to write for most callers — the results and the
  observations are identical, and a run whose policy denies `Act::Exec`
  speculates none of them. Two things move for a caller that inspects the trace:
  these calls may now run concurrently rather than in call order, and a
  *speculated* git reader's sandbox rows are written when the call is collected
  rather than around its spawn. `TaskContract::with_max_parallel_reads(1)`
  restores 0.40.0's fully serial execution shape, as it always has.
- The memory ranking and `remember`'s duplicate check read each entry's
  normalised token sets from the store instead of recomputing them every turn.
  The ranking is unchanged — the same notes in the same order — and
  `memory.max_entries` is now a number an operator can raise. See
  `docs/MEASUREMENTS.md` for the before and after.

### Fixed

- The containment probe added in 0.74.0 no longer re-measures a boundary this
  process has already proven for the same configuration, and can no longer
  *un*-prove one. Its arms spawn short-lived children, and under load an arm that
  fails left the run reporting that it could not establish confinement — which
  changes a sentence in the system prompt, and the system prompt is the cached
  prefix. A host that failed one probe therefore paid a cache *write* on every
  turn after it, invisibly. The result is now kept per backend, config, writable
  roots and proxy; a configuration that differs anywhere is measured for itself,
  and a probe that reached no conclusion is retried rather than remembered.

### Security

- `[routing]` is refused in a file inside the workspace, `io.toml` and
  `io.local.toml` alike, and inside a `[profile]` body. A table that decides
  which model answers a run, and which model reads the whole transcript when the
  context is folded, is one a cloned repository must not write. It names no
  program and no endpoint, so it is refused on the principle 0.74.0 wrote down
  rather than by the same clause. Declare it in the user-scope file instead.

## [0.74.0] - 2026-09-01

**The boundary this crate is built to be is now actually on the path, and it
proves itself rather than declaring itself.** An internal audit read the whole
crate under the threat model that matters — an LLM-driven agent following
prompt-injected instructions from a hostile repository, a hostile PR branch, or
a config the agent wrote itself — and returned 51 findings. Two of them were
this product's own thesis failing rather than a bug beside it: the gate the
design rests on was not on the post-edit path at all, and three separate
backends reported a containment that was not in force.

Every change here removes permission or adds a check. Nothing in this release
grants an act that 0.73.0 refused. Several of the narrowings are visible, and
each one below names what to do instead — a refusal that does not teach is
half a fix.

### Breaking changes

- **BREAKING (behaviour)** — a file inside the workspace may no longer declare
  anything that names a program to run, names an endpoint a credential is sent
  to, or opens a route out of the boundary the `Policy` does not gate.
  `[[provider]]`, `[[mcp]]`, `[[lsp]]`, `[[hook]]`, `[browser]` and `[web]` are
  refused in both `io.toml` and `io.local.toml`. `[web]` is the third clause's
  entry: a provider-executed search or fetch is dialled by the provider, so
  `Act::Net` never sees it, no sandbox is on the path, and the domain lists
  beside it are a filter this crate states rather than enforces. `${cmd:...}`
  and `${file:...}` are refused in both files too — the first runs a program at
  load, and the second joins its argument onto the file's own directory, where
  an absolute one names any path on this host. A plugin bundle **declared from**
  either file may not contribute a `[[hook]]`, an `[[mcp]]` or a `[[bin]]`
  either — and a bundle needs **two** things now to contribute all seven kinds,
  not one: it must be declared from the user scope *and* sit outside the
  workspace. A `[[plugin]]`'s `path` resolves against the discovery root, so
  `path = "bundles/tools"` in the operator's own file names a directory the
  run's own agent can write — one ordinary `write_file` of
  `bundles/tools/plugin.toml` carrying a `[[hook]]` was a program to run,
  installed with no refusal anywhere on the path. The user scope's exemption
  rests on its file being outside every workspace; that premise holds for the
  declaring file and does not transfer to a directory it points at.

  *Migration:* move the table to the **user-scope** file — `$IO_CONFIG`, else
  `$IO_CONFIG_HOME/io.toml`, else `$XDG_CONFIG_HOME/io/io.toml` or
  `~/.config/io/io.toml`; on Windows `%IO_CONFIG%`, else
  `%IO_CONFIG_HOME%\io.toml`, else `%APPDATA%\io\io.toml`. Every refusal names
  that path. `io.local.toml` was the documented home for per-project hooks and
  is no longer an option for them: it sits at the workspace root, which is a
  path the agent can write and a clone can ship, and that is the whole finding.
  **For a plugin bundle, moving the declaration is not enough — move the
  bundle.** A `path` relative to the discovery root still lands inside the
  workspace however it was declared, so a bundle that contributes a `[[hook]]`,
  an `[[mcp]]` or a `[[bin]]` has to live outside the project and be named
  absolutely from the user-scope file (`~/.config/io/bundles/<name>`). One
  declared into the workspace keeps loading and keeps contributing its skills,
  templates, agents and deny policy; only the three program-running kinds are
  dropped, each with its reason.
  For per-project web access, set it on the `TaskContract` with `with_web` —
  that is the application deciding rather than the repository. Note that
  `[web] search = false` is a *narrowing* sentence a workspace file can no
  longer write either; a whole section is refused whole, and the feature is off
  unless the user scope turns it on. A workspace file may still **narrow** the
  boundary, which is what the project scope is for.

- **BREAKING (behaviour)** — a `[[policy.layers]]` entry in a file inside the
  workspace may carry `deny` rules and nothing else. An `allow` rule, an `ask`
  rule, or a rule whose `effect` is missing or is not a string, is refused
  naming the layer, the effect, the act and the pattern.

  *Migration:* write the layer in the user-scope file. A layer was
  `policy.defaults.exec = "allow"` reached by a different door — rules append to
  the shipped defaults with no effect filter, `Policy::explain` resolves
  deny → ask → allow, and the default contributes no `Act::Exec` or `Act::Net`
  deny for an allow to lose to — and the `net` spelling of it also switched the
  sandbox's own `allow_network` back on, because every spawn site resolves that
  flag from the policy rather than from `[sandbox]`. An `ask` is refused beside
  an `allow` because it hands capability out as well: it converts an act the
  operator's own default denied into one an approver is asked to wave through.
  This is the rule a `plugin.toml` — the *more* trusted file — has been held to
  since 0.35.0.

- **BREAKING (behaviour)** — the widening-key list grew from five keys to
  twelve. `policy.defaults.read = "allow"` and `policy.defaults.write = "allow"`
  are now refused in a workspace file beside `exec` and `net`;
  `sandbox.mode = "workspace-write"` is refused beside `"full-access"`; and each
  of `sandbox.limits.max_cpu_secs`, `max_wall_secs`, `max_memory_bytes`,
  `max_processes` and `max_open_files` is refused when written as `0`.

  *Migration:* write the narrowing value, or move the key to the user-scope
  file. `read` and `write` were the omission that mattered: the shipped defaults
  are `read = "allow"` and `write = "ask"`, a later scope overrides an earlier
  one and the project scope outranks the user scope, so a cloned file writing
  `write = "allow"` turned every unmatched write from a question into a silent
  grant whatever the operator's own file said. `"workspace-write"` is refused
  for the same merge reason — written in a workspace file it raised an
  operator's `read-only` back to the crate's default — and `"read-only"`, the
  one value that narrows in every direction, stays legal. `0` in a
  `[sandbox.limits]` key means *no cap*, so it removes a wall rather than
  setting one; a merely large finite cap is still a cap and is still accepted.

- **BREAKING (behaviour)** — `.git/*`, `*/.git/*`, `io.toml` and `io.local.toml`
  are deny-by-default to `write_file`, `edit_file` and `patch_file`. They live in
  a layer of their own, `builtin-config`, rather than among the secret patterns:
  these are not secrets and are not denied for reading, they are the files
  something *else* reads back, so a refusal names a different reason and a trace
  can tell the two apart.

  *Migration:* none for ordinary work. `git_add`, `git_commit` and `git_branch`
  cover every legitimate reason to write inside `.git`, and configuration is not
  agent-writable by design. `.gitignore`, `.gitmodules` and `.gitattributes` are
  unaffected — the pattern is `.git/*`, not `.git*`.

- **BREAKING (behaviour)** — `git` refuses to run at all in a repository whose
  own `.git/config` defines a `filter` or `merge` driver.

  *Migration:* remove the driver from that repository's config. There is no
  narrower answer available: a driver is keyed by a name the repository chooses,
  so there is no wildcard `-c` to neutralise it and no environment variable that
  suppresses a repo-local config file at all. The refusal names the section, the
  key and the file.

- **BREAKING (behaviour)** — under a policy whose `exec` tier is not `allow`,
  the automatic post-edit check no longer runs. `Policy::default()` has
  `exec: Ask`, so this is the default.

  *Migration:* `allow_exec` the checker for the projects you want checked. The
  write itself is unaffected and still cannot fail; the diagnostics are simply
  absent. Before this release that reflex spawned the project's compiler with no
  `Act::Exec` gate and no containment, which for a cargo project means running
  the workspace's own `build.rs`.

- **BREAKING (behaviour)** — an `Effect::Allow` rule is matched against the
  target's full relative path only. The basename retry — the half that lets a
  bare `.env` deny `config/.env` at every depth — no longer applies to allows,
  in any act. `allow_write("out.txt")` covers `out.txt` and not `logs/out.txt`;
  `allow_read("notes.md")` covers `notes.md` and not `docs/notes.md`;
  `allow_exec("cargo")` covers `cargo` and not `./target/debug/cargo`, a binary
  the agent had just built for itself, which is the shape the audit named.
  Denies and `Effect::Ask` rules keep the retry.

  *Migration:* write the reach you meant. `*` spans `/`, so the recursive form
  of the rules above is `allow_write("*out.txt")` and `allow_read("*notes.md")`;
  name the directory — `allow_write("logs/out.txt")` — where only one location
  was ever intended. A rule that misses now falls to the tier default, which
  asks or refuses rather than granting, so the failure mode is a prompt and not
  a silent grant.

- **BREAKING (behaviour)** — on Windows, `shell`, `shell_start` and the `git`
  built-ins refuse to spawn when the run selected the AppContainer backend.

  *Migration:* run those tools without `access_confinement`, or use the `exec`
  tool, which is contained. An AppContainer is entered at `CreateProcessW`
  through a process-thread attribute list, so it cannot be expressed as an argv
  wrapper — which is why these tools were previously spawning completely
  unwrapped while the run reported `windows-appcontainer`.

- **BREAKING (behaviour)** — a path that escapes the workspace root is now
  denied rather than graded on its collapsed spelling, a write no longer follows
  a symlink at the leaf out of the root, and `Workspace::read_bytes` refuses a
  file over 64 MiB.

  *Migration:* none for a path that stays inside the root, including one that
  uses `..` to get there and one that goes through a symlink pointing inside.
  A caller that relied on `check_path` allowing `../../x` was relying on a bug:
  it graded that path as `x`.

- **BREAKING (behaviour)** — the gate refuses an approver's `modified` request
  on an `Act::Exec` action instead of discarding it, and a resumed approval
  whose persisted act cannot be replayed is refused instead of performed.

  *Migration:* an approver that rewrote an exec action was never having its
  rewrite applied — the original argv ran and the trace recorded the rewrite.
  Approve or deny instead. Rewrites of reads and writes are unaffected.

- **BREAKING** — `sandbox::BoundaryProbe::measure` takes a third argument, the
  run's egress proxy address, and `BoundaryProbe` carries two more public fields:
  `claimed_confinement` and `claimed_egress_denial`.

  *Migration:* `measure(config, &writable_roots, None)` for a run with no egress
  proxy, and `Some(addr)` for one that has it; a struct literal needs the two new
  fields, and `BoundaryProbe::unmeasured(backend)` sets both to `false`. The
  parameter is a defect fix rather than an API tidy-up. `contradicts_claim()`
  compared the measurement against `Backend::confines_writes()` and
  `Backend::denies_egress()`, which are unconditional properties of the backend
  with nothing of the run in them, so this release's own "the backend named a
  containment this host did not apply" warning fired on two correct
  configurations: any run that permits network — where the dial is *supposed* to
  land — and every `ExecMode::FullAccess` run on macOS and Linux, where `select`
  reads the platform rather than the mode, so an unwrapped run still names a
  backend that declares confinement (`macos-sandbox-exec write-outside=landed
  dial-outside=landed`). The comparison is now against what this run actually
  asked for, and a proxied run claims neither arm: its egress is scoped by the
  proxy rather than denied by the boundary. For the same reason a tree that will
  be proxied is recorded **unmeasured** rather than measured against a boundary it
  will not have — on Linux the proxy decides which rung the chain picks.

- **BREAKING (behaviour)** — the Linux `LinuxNamespaces` rung requires `setpriv`.
  Its mount setup now ends `exec setpriv --no-new-privs --inh-caps=-all
  --bounding-set=-all -- "$@"`, and a host without the program fails the setup
  rather than running the payload.

  *Migration:* install `setpriv`, which ships in util-linux beside `unshare`
  itself. A host without it answers `false` to the rung's own probe, so the chain
  skips that rung and the run is reported under the backend it actually got —
  degrading loudly rather than running under a name whose guarantee was not
  applied. Without the drop, `unshare --user --map-root-user` ran the setup as
  uid 0 and a root process exec'ing a file with no file capabilities keeps a full
  permitted set, so the payload arrived holding `CAP_SYS_ADMIN` over the very
  mount namespace the script had just remounted read-only: one
  `mount -o remount,bind,rw /` undid all of it, `ExecMode::ReadOnly` was a label
  on that rung, and `Backend::confines_writes()` answered `true` for it.

- **BREAKING (behaviour)** — `sandbox::copy_back` refuses a path that leaves
  either root rather than following it. Every entry must be relative and made of
  ordinary components: a `..`, a root, a Windows prefix, or a symbolic link at
  the source is `Error::Refused`, and the resolved destination is re-checked
  against `dest_root` before anything is written.

  *Migration:* pass workspace-relative paths, which is what a sandbox outcome
  already hands you. There is no opt-out, and the reason is that the predicate
  is given the *relative* path: a caller consulting a write policy was being
  asked about `../../etc/authorized_keys` and answering about a file somewhere
  else entirely, and a symlinked source copied the contents of whatever it
  pointed at. A listed file the sandbox never produced is still **skipped**, as
  before; only a path that cannot be under the root is an error.

### Added

- **The containment a run reports is now measured rather than declared.**
  `BoundaryProbe` attempts a write and a dial outside the boundary before the
  first step, and `confines_writes()`, `denies_egress()` and the boundary
  sentence the agent is given all answer from what happened rather than from the
  backend's own claim. Three findings in this release were a backend reporting a
  containment it had not applied; this is the guard that makes the fourth one
  loud.

- Every arm of the probe fails closed. One **control** child runs outside the
  boundary first, so a host that could never have performed an arm reports it
  unmeasured rather than refused, and an attempt that could not be made claims
  nothing. A run on a host that cannot measure is told the boundary could not be
  established — not that there is none.

- The probe is measured **once per boundary**: once per run for a flat run, and
  once per **tree**, before the root agent runs, for a tree. So a child agent's
  run carries no probe row of its own, which is worth knowing before reading a
  child's trace; an agent whose contract asks for a different `SandboxConfig`
  measures that one. A run that asked for no containment is not probed and
  records nothing — it makes no claim, and a row naming the backend that *would*
  have been chosen is the same misattribution the probe exists to catch. What is
  recorded is one `sandbox_events` row of kind `boundary_probe`, at step 0, and
  it is announced on the event stream like every other row in that table.

### Changed

- `deny_net` matches a host case-insensitively and ignores one trailing root
  dot, so `deny_net("evil.example")` covers `EVIL.example:443` and
  `evil.example.`. The fold applies to denies only: folding an allow would
  widen it, and this release adds no widening.

- An `Act::Exec` deny matches its target case-insensitively and splits a
  Windows path on `\`, so `deny_exec("rm")` covers `RM` and
  `deny_exec("kubectl.exe")` covers `C:\...\kubectl.exe`. Both are a deny's
  alone, for the reason the fold above is. The basename retry's narrowing to
  non-granting rules is under **Breaking changes**, since it takes reach away
  from allows that already exist.

- The egress proxy resolves a permitted host **once** and dials exactly the
  addresses that resolution produced, closing the window in which the answer that
  was checked and the answer that was dialled did not have to be the same one. A
  resolution that fails is therefore reported as a refusal attributed to the
  local-address floor — a 403 naming the reason, and a dial row marked not
  allowed — where it used to surface as a 502 from the connect. A command whose
  dependency fetch fails can still tell "this host is not permitted" from "the
  network is down": both say which.

- On Windows a run killed hard leaves its AppContainer profile and the ACEs that
  named it behind, and what that costs is now written down. They are **inert for
  access** — the SID names a container that no longer exists, so nothing can
  enter it — but not for DACL capacity: each orphaned run leaves one ACE per
  granted path plus a traverse ACE per ancestor, and a DACL has 64 KB of room,
  past which every later grant on that object fails. It fails loudly rather than
  silently, and an ordinary teardown revokes every ACE naming the SID.

### Fixed

- `spawn_agent` is refused while a proposed plan is unapproved, and a child that
  does spawn inherits the policy its parent is running under rather than the
  contract's. A plan gate could previously be stepped around by emitting
  `spawn_agent` instead of `propose_plan`.

- `before_tool` hooks fire for `spawn_agent`, `send_message` and
  `read_messages`. An operator's hook on those three loaded, validated,
  installed and never ran.

- A remembered approval survives the tree loop, so "approve and stop asking" is
  no longer re-asked at every step inside `run_tree`.

- On Windows, teardown no longer revokes grants that failed. On a host where the
  harness is not an administrator, every run ended with a security-descriptor
  tree walk of `%SystemRoot%` to remove an ACE it had never written.

### Security

- **The gate is on the post-edit path.** The reflex that runs a project's own
  checker after every successful write called `Exec::new` directly, with no
  `gate(Act::Exec)` and no containment. `CHECKERS` maps `Cargo.toml` to
  `cargo check`, which compiles and runs `build.rs`, so a repository the agent
  can write reached arbitrary host execution while the approver saw two
  `Act::Write` prompts and nothing else. The same fix closed a second spawn
  found beside it: the model-callable `check` tool gated its checker and then
  ran it outside the run's sandbox.

- **An address-level floor sits under every network decision.** Until this
  release every net decision in the crate was a hostname glob, so
  `Policy::permissive()` handed the model cloud metadata, localhost admin ports
  and the internal network. Loopback, link-local, cloud metadata, carrier-grade
  NAT, unique-local and RFC 1918 addresses are refused whatever the policy says.
  `IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` lifts it for the local-model case — an
  environment variable and not an `io.toml` key, because a key that widens is one
  a cloned repository could set, and the environment of a process that has
  already started is the one thing a hostile repository cannot write. The
  metadata hostnames, `169.254.169.254` and `100.100.100.200` stay refused even
  then. A refusal names the floor as its layer, so a trace tells "your rules
  refused this" apart from "the floor underneath your rules refused this".

  **Where the check is made differs by call site, and the difference matters.**
  A floor that graded only names is one `http://169.254.169.254.nip.io/` walks
  through, so the floor resolves — but resolving is only worth what the dial is
  bound to:

  - The **HTTP MCP transport** and the **egress proxy** resolve once and dial
    exactly the addresses that were graded. Check and dial are the same answer.
  - A **provider endpoint** is resolved and graded before the run's first step,
    but a `Provider` owns its own HTTP client and resolves the name again when it
    dials. A name that always answers with a local address is refused; a name
    that answers differently the second time is not.
  - **`browser_navigate` is graded by name only.** Every literal spelling of a
    local address is refused and the trace row names the floor, but a *name* that
    resolves onto one is not: Chrome resolves each URL itself, so an address
    graded at the gate is not the address the browser dials unless the navigation
    is pinned to it, and pinning breaks SNI and certificate validation. The close
    is to route the browser through the run's egress proxy — which already
    resolves once and dials what it graded — and that wiring is not in this
    release. `browser_navigate("http://169.254.169.254.nip.io/")` under a policy
    that allows every host reaches cloud metadata.

- **A short-form IPv4 host is resolved before it is graded.** `2130706433` and
  `127.1` are `127.0.0.1` to `getaddrinfo`, and `2852039166` is
  `169.254.169.254`; none of them parses as an `IpAddr` and none matches a policy
  glob written against the dotted form, so a floor that graded only what it could
  parse as a literal let all three through. They are answered from `inet_aton`
  without a DNS query and graded like any other resolved address.

- **A backslash ends the URL authority**, in `net::target` and at the browser's
  navigation gate. The WHATWG URL parser and Chrome's GURL both treat `\` as a
  path separator for `http`, `https`, `ws` and `wss`, so
  `http://127.0.0.1:11434\@example.com/v1` was *checked* as `example.com:80` and
  would have been *dialled* at `127.0.0.1:11434` — the checked host and the
  dialled host were not the same host. Reachable by an operator pasting an MCP or
  provider URL rather than from a workspace file, since this release refuses
  `[[provider]]` at project and local scope.

- **A browser navigation that reaches no host is decided by its scheme**, before
  the navigation is issued. Only `http`, `https`, `ws` and `wss` reduce to a
  `host:port`, so nothing else was ever decided: `browser_navigate` to a `file:`
  URL read a local file past `Act::Read` and past every secret deny, and recorded
  nothing. `about:blank` is permitted and every other scheme is refused —
  an allowlist, so a scheme nobody has considered is refused too. Each decision
  is a `BrowserNavigated` event whose `host` is the **scheme**: a `data:` URL is
  its own payload and a `javascript:` URL is a program, and neither belongs in
  the trace.

- **A language server is told nothing about a path the policy has not cleared.**
  The `path` a navigation names is an `Act::Read` check taken before anything
  reads it, and it is `Workspace::check_path`, so an absolute path and a `..`
  that climbs out are refused as they are everywhere else — until now the
  argument was joined onto the root, read from disk and shipped to the server, so
  `lsp_hover {"path": "../../../../etc/shadow"}` crossed the boundary
  `read_file` would have refused and left no row. A refusal is written through
  the same gate every other refusal is, which has a consequence worth stating: a
  policy whose read tier is `Ask` now **prompts** on a navigation where it passed
  silently. `Policy::default()` allows reads, so the common configuration is
  unchanged.

- **`lsp_rename` renders a file only if the policy allows reading it outright.**
  A `WorkspaceEdit` names files the *server* chose, and rendering one puts every
  removed line in the model's context, so each path is resolved under the
  workspace root and must be an outright allow. An `Ask` no longer passes: there
  is no approver on that path, and an unanswered question is not permission. The
  cost is stated rather than hidden — under such a policy a rename renders no
  patch and says so, and a run that wants one grants `allow_read` over the tree
  it is renaming in.

- **A shell redirect is contained the way a write is.** `shell::resolve` was
  purely lexical, so `echo x > docs/ext/.bashrc` through a checked-in
  `docs/ext -> $HOME` symlink created a file in the home directory. Every
  redirect target now takes the same containment check `write_file` takes. In the
  same pass, a `cd` target may not carry a double quote, a backslash or a control
  character: that directory becomes the workdir a later stage is contained in,
  and on macOS it is interpolated into an SBPL profile. `cd 'My Project'` is
  still ordinary and still allowed.

- **A workspace file cannot aim the system prompt at a directory outside the
  workspace.** `run.skills`, `run.templates` and a `[[plugin]]`'s own `path` were
  each resolved by joining onto the discovery root, where an absolute value
  replaces that root and a `..` climbs out of it — and the frontmatter of every
  `*.md` under a skills or templates directory is composed into the model's
  system prompt on every turn, read-only, unasked, before any `Policy` exists to
  have an opinion about the read. The two `run` keys are refused in `io.toml` and
  `io.local.toml`; a `[[plugin]]` path that resolves outside the root — absolute,
  `..`, or through a symbolic link — is dropped with its reason; and a bundle's
  own `skills` and `templates` are held to the rule `[[bin]]` already had, in
  every scope. The user scope still points wherever the operator wants, which is
  what keeps a shared skills directory outside the project working.

- **`git_worktree` cannot create a checkout outside the workspace.** It is the
  one git built-in that creates the path the model names, and the gate routes an
  *absolute* read or write target to the policy alone — a relaxation that exists
  so `read_skill` can reach a bundle outside the root. `{"path":"/tmp/escaped"}`
  therefore wrote a full checkout outside the workspace with an allow-shaped
  trace row to match. The path is now asked of `Workspace::check_path` directly,
  so an absolute spelling and a `..` that climbs out are the same refusal, with
  the same row, as every other path in this crate.

- **A path can no longer rewrite the macOS sandbox profile.** `workdir` and the
  writable roots were interpolated into the `sandbox-exec` profile with no
  escaping, and SBPL is last-matching-rule-wins, so a directory whose name
  closed the profile's own string literal appended rules to it — regranting
  write and network on `/` while the backend still answered
  `confines_writes() = true`. Paths carrying a quote, a backslash or a control
  character are refused; a profile that cannot be built grants nothing rather
  than silently losing one line.

- **The Linux namespace rung drops its capabilities before the payload runs.**
  The mount setup made the root read-only and then handed the payload a uid-0
  process with a full permitted set, so `CAP_SYS_ADMIN` over that same mount
  namespace survived the `exec` and one `mount -o remount,bind,rw /` undid every
  remount above it. `setpriv --no-new-privs --inh-caps=-all --bounding-set=-all`
  empties the bounding set so nothing on the far side can add a capability back,
  and escaping into a nested user namespace does not recover it — mounts
  propagated into a new mount namespace arrive locked and their read-only flag
  cannot be cleared from there. Until this release `ExecMode::ReadOnly` was a
  label on that rung while `Backend::confines_writes()` answered `true`, which is
  the same shape as the three other findings in this release.

- **The probe's contradiction warning compares against the run's own claim.** It
  was compared against the backend's unconditional properties, so it fired on a
  run that permits network — where the dial is supposed to land — and on every
  `FullAccess` run on macOS and Linux, where `select` reads the platform rather
  than the mode. A warning that fires on correct configurations is a warning an
  operator learns to ignore, which would have cost this release the guard it is
  built around.

- **Windows stops naming a containment it never applied**, the AppContainer
  profile is per-run with an unguessable SID and is revoked at teardown, and the
  spawn's handle list names only the capture file and the handles it was given.
  A fixed profile name meant `ExecMode::ReadOnly` was not enforced after any
  earlier writable run on the same tree.

- **A cloned repository can no longer redirect the provider endpoint** or name
  an MCP or LSP server to spawn at run start. This is the same door
  `plugin.rs` already refused for a project-scoped plugin; it is now one
  allowlist in one place, and the rule is written down rather than enumerated:
  anything that names a program to run, or names an endpoint a credential is
  sent to, is refused outside the user scope, without exception.

- **A repository's own git config is neutralised or refused.** `--no-textconv`
  joins `--no-ext-diff`, `core.fsmonitor` joins `core.hooksPath` as a `-c`
  override, and `GIT_ATTR_NOSYSTEM` stops the system attributes file selecting a
  driver. What `-c` cannot reach is refused outright.

- **A credential file other accounts on the host can reach is named.** On unix,
  `io.local.toml`, the user-scope file and every `${file:}` target are warned
  about through `tracing` when any group or other permission bit is set, naming
  the file, its mode and the `chmod 600` that fixes it. A warning rather than
  `ssh`'s refusal, deliberately: this is a library inside somebody else's binary
  and `0644` is what a `umask 022` host produces, so refusing would turn an
  upgrade into a startup failure for the common case. The committed `io.toml` is
  not checked — it is world-readable by design, and a warning on every run is one
  an operator learns to ignore.

- **The store is created readable only by the user that created it** on unix,
  and an existing store is tightened on open. The trace holds whatever the run
  saw — a step that read a credentials file puts those credentials in
  `steps.result` — and that is now stated in the crate's own documentation
  rather than left to be discovered.

- **A column name read from a store's own schema is quoted** before it reaches a
  statement. A database with an unusual or foreign column name could previously
  not be sized, archived or swept.

- **A provider response is bounded rather than read until the deadline.** One
  completion may accumulate 8 MiB across its text, its reasoning and its
  tool-call arguments; one SSE line may reach 1 MiB without a newline; one
  response may open 1024 tool-call blocks, since the block index is the sender's
  and a map keyed on it grows even when every block is empty; and a non-success
  body is read to 8 KiB before it becomes an error message. Past any of them the
  response is **refused, not truncated** — a tool call cut mid-JSON either fails
  to parse or parses into something the model did not ask for — as a retryable
  `ProviderErrorKind::Malformed`. Until now the only bound on any of these was
  the 600 s request deadline, which is not a memory bound: a host that wrote
  steadily for ten minutes decided how much of this process's address space it
  took. Both wire formats draw against the same budget, so this covers
  `Anthropic`, `OpenAI`, `OpenRouter` and every `Compatible` endpoint including
  the vendor presets.

## [0.73.0] - 2026-08-31

A skill can open the file it points at, a bundle can say which program it ships,
and one ordinary shell idiom no longer costs a whole session. A skill was one
markdown file and nothing else: `read_skill` returned its body, so a skill whose
instructions said "the full checklist is in `references/review.md`" was naming a
file the model had no way to open, and the author's choice was to inline
everything or to be ignored. A bundle could contribute six kinds of thing and not
the one an operator most often wants from it — the executable it built. And
`ls 2>&1 | head -50`, which every shell on the planet accepts, was an
`Error::Config` raised at spawn time that propagated out and ended the run, where
every other construct this parser does not admit is a refused step the model
simply writes around.

### Breaking changes

- **BREAKING (source)** — `Skill` gained a `pub root: PathBuf` field and is now
  `#[non_exhaustive]`, so it can no longer be built with a struct literal from
  outside this crate.

  *Migration:* a caller that only **reads** a `Skill` needs no change at all —
  `name`, `description` and `path` are the fields they always were, and `root` is
  one more beside them. A caller that **built** one with a struct literal cannot,
  and there is deliberately no constructor to move to: a `Skill` is what
  `Skills::discover` found on disk, and a hand-built one describes a skill that is
  not there.

  ```rust,ignore
  // before — a struct literal, typically in a test or a fake toolbox
  let skill = Skill { name, description, path };
  // after — discover the directory the skill actually lives in
  let skills = Skills::discover(&dir)?;
  let skill = skills.get("review").expect("review");
  ```

  `#[non_exhaustive]` goes on in the same edit on purpose. The break is being
  taken once, for the field; adding the attribute in a later release would be a
  second break bought for nothing.

- **BREAKING (behaviour)** — `Plugin::contributions()` returns a **seventh** name,
  `"bin"`, ordered after `"hooks"` and before `"policy"`. The same vector is
  `EventKind::PluginLoaded`'s `contributions` field, so an observer, an installer
  or a snapshot test matching on it sees a name it has not seen before.

  *Migration:* nothing to write for a consumer that renders the vector or asks it
  `contains("hooks")`. A consumer that matches it **exhaustively** — comparing
  against a fixed slice, or switching on its length — adds the `"bin"` case:

  ```rust,ignore
  // before
  assert_eq!(plugin.contributions(), ["skills", "hooks", "policy"]);
  // after — a bundle with a [[bin]] now reports it, between the two
  assert_eq!(plugin.contributions(), ["skills", "hooks", "bin", "policy"]);
  ```

  A manifest with no `[[bin]]` returns exactly what it returned before, so only
  bundles that declare one change shape.

- **BREAKING (format)** — a `plugin.toml` declaring `[[bin]]` does **not** load on
  an older harness. `[[bin]]` is additive **forward only**: a manifest written
  before 0.73.0 loads on 0.73.0 exactly as it always did. The other direction does
  not hold. `Manifest` carries `#[serde(deny_unknown_fields)]`, so a manifest
  declaring `[[bin]]` is refused by io-harness 0.72.0 and earlier as an unknown
  field, and the bundle is dropped **whole** — every skill, template, agent, hook,
  MCP server and deny layer in it, not just the `[[bin]]`.

  *Migration:* a bundle that only ever loads on 0.73.0 or later needs nothing. A
  bundle that must load on both ships **two manifests** — one bundle directory per
  harness range — or requires `io-harness >= 0.73.0` and says so where the people
  installing it will read it. There is no forward-compatible spelling of the key
  to write instead: `deny_unknown_fields` is what makes an unknown key an error
  rather than a shrug in every other manifest, and that is the property being
  relied on here.

### Added

- **`read_skill` takes an optional `path`** — a file the skill's own instructions
  point at, named relative to the skill's root. `name` is unchanged and still the
  only required property, and a call with no `path` behaves byte for byte as it
  did in 0.72.0. For a **plugin-contributed** skill the root is the *bundle's*
  root rather than its `skills/` directory, so a bundle keeping `shared/` beside
  `skills/` is in reach; for a standalone skill discovered through `with_skills`
  it is the skill's own directory.
- **A `path` naming a directory returns a sorted listing** of its entries, one per
  line, under the same result cap a body is subject to. Deliberate: it saves the
  model the turn it would otherwise spend guessing a filename.
- **`Skill::root`** — the directory a companion path resolves beneath, and the
  boundary the resolver refuses to cross.
- **`plugin.toml` `[[bin]]`** — an array of tables, each a `name` and a `path`
  relative to the plugin root, matching the shape of `[[agent]]`, `[[mcp]]` and
  `[[hook]]`. It joins `[[hook]]` and `[[mcp]]` as a contribution a **project**
  `io.toml` may not make — it names a program this machine would run, and
  `io.toml` arrives with a `git clone` — so a manifest declaring one from that
  scope is refused whole and the bundle lands on `Plugins::dropped()`. Declaring a
  `[[bin]]` is **not** permission to execute it: the harness says what a bundle
  contributes, and where a host places it, and whether the policy lets the agent
  invoke it, stay the host's decisions.
- **`Plugin::bin()`** — each declared entry's name beside its path joined onto the
  plugin root, absolute, in declaration order. The path is validated **lexically
  only**: absolute, or climbing out with `..`, is refused at load, and nothing is
  ever stat'd — an executable a bundle ships is ordinarily produced by the
  bundle's own build, and a manifest must not be valid on Tuesday and dropped on
  Wednesday because somebody cleaned a build directory. The `bin` name is **not**
  namespaced, unlike every skill, template, agent, layer and MCP id, because it is
  the program a human or a model actually invokes.

### Changed

- A companion read goes through the **same `Act::Read` gate** the skill body
  already passes, against the resolved absolute path. A policy that denies the
  bundle's directory denies the companion file too; there is no second door.
  Escape is refused and never resolved — an absolute path, any `..` component, and
  a symlink whose target canonicalises outside the root are each refused with an
  observation rather than an error, and no read happens. A path that simply does
  not exist is reported as **not there**, distinctly from a refusal, and does not
  enumerate the directory: a skill pointing at a file it no longer ships is a typo,
  and calling it a refusal would send an operator hunting for a breach.

### Fixed

- `2>&1` on a stage whose stdout is piped is now refused in `parse`, by name, as
  the construct **`a stream merge on a piped stage`**, and the run continues —
  `run::dispatch` returns a decision beginning `shell refused:` and the model
  writes something else, exactly like every other construct this tool does not
  admit. Up to 0.72.0 it was an `Error::Config` from `apply_redirects` at spawn
  time, which propagated out and ended the run: a malformed redirect cost a whole
  session instead of one refused step. The refusal keeps the sentence that explains
  the fix — put the redirect on the last stage of the pipeline. **A redirect on the
  last stage is still legal and always was**: `ls 2>&1`, `ls | head -50 2>&1` and
  `ls | head -50 2>&1 > out.txt` all run. A `cd` stage inside a pipeline is exempt,
  because its redirects are never applied at run time, so `cd x 2>&1 | y` still
  runs.

## [0.72.0] - 2026-08-30

An agent can ask everything it needs in one breath, and each offer can explain
itself. `ask_question` took one question and the run blocked inside
`Responder::answer` before the model could ask a second, so a model that needed
five facts spent five round trips and an interface downstream could not gather
them into one surface — it can only render what reaches it, and they reached it
one at a time. A choice was a bare `String`, so an operator picked between five
labels with nothing saying what any of them cost. Both are the same gap: the
harness recorded what was asked and almost nothing about what was offered.

### Breaking changes

- **BREAKING (source)** — `Question::choices` is now `Vec<Choice>` rather than
  `Vec<String>`, and `Question` is `#[non_exhaustive]`.

  *Migration:* a caller *building* choices needs no change at all:
  `with_choices(["a", "b"])` still compiles and means what it did, through
  `From<&str> for Choice`. A caller *reading* them adapts by `.label` —
  `question.choices.first().cloned()` becomes
  `question.choices.first().map(|c| c.label.clone())`, and
  `assert_eq!(q.choices, ["a", "b"])` becomes a comparison over
  `q.choices.iter().map(|c| c.label.as_str())`. A caller constructing `Question`
  with a struct literal moves to `Question::new` plus the builders, which is what
  `#[non_exhaustive]` now requires.

  **No data migration.** Every `pending_questions` row written by 0.71.0 and
  earlier holds `choices` as a JSON array of plain strings, and `Choice`'s
  deserializer reads both spellings — a string becomes a label with no
  description, an object is read by field. `CHECKPOINT_FORMAT` stays 7, the two
  new columns are nullable, and a 0.71.0 binary still opens and resumes a store
  0.72.0 wrote. Both directions are proven against a real 0.71.0 from crates.io
  rather than asserted.

### Added

- **`Choice`** — an offered option as a label plus an optional `description` (one
  sentence naming what taking it means) and an optional `preview` (a short
  concrete block showing what it would actually do). A preview is bounded at
  twelve lines or eight hundred bytes, cut at a line boundary with the model told
  what was cut, and stripped of control characters and escape sequences — a model
  writes this value and every consumer draws it into a terminal.
- **`Question::multiple`** and the `Question::multiple()` builder — whether more
  than one of the offers may be taken. An offer of several, not a demand for
  several; default `false`, so every existing question keeps its meaning. A
  `multiple` question with no choices is a parse error.
- **`Question::answer_of`** — one spelling for a several-part answer, stated by
  the harness rather than by each interface, so two interfaces answering the same
  question produce the same text. The answer stays a `String`.
- **`ask_questions`** — a second built-in tool taking an array of question
  objects, parsed strictly per index with the failing index named, at most ten per
  call. `ask_question` is unchanged and is still the right tool for one question;
  questions whose answers depend on each other belong in separate calls.
- **`Responder::answer_all`** and **`AnswersFuture`** — a second trait method with
  a default body that loops `answer` in order, so the trait stays dyn-compatible
  and no existing implementor changes. An interface that wants one overlay for
  five questions overrides it; `StdinResponder` does, printing the whole batch
  before reading the first answer.
- **`EventKind::QuestionsAsked`** — one event carrying the whole batch, so an
  observer can tell three questions asked together from three asked in sequence.
  `QuestionAsked` is not also emitted for a batch; `QuestionAnswered` stays
  singular and is emitted once per answer.
- **`Store::put_questions`**, and two nullable JSON columns on
  `pending_questions`, so a batch is one durable row.

### Changed

- A batched ask parks the run exactly as a single one does: one row, one
  `question_id`, one `RunOutcome::AwaitingAnswer`, one `Waiting::Question`, and
  the same four `resume_*_with_answer` functions with the signatures they have.
  The row's question text is the whole ask rather than the first of it, so a
  reader that predates batching still sees all of it.

## [0.71.0] - 2026-08-29

The crate answers for its own schema, and stops answering with the operator's
key. 0.70.0 made every fact this crate records reachable; its schema stayed
private, so a consumer built a copy — and one of those copies fails open, while
the crate itself printed credentials through a derived `Debug`.

### Breaking changes

- **BREAKING (behaviour)** — `{:?}` and `{:#?}` no longer print resolved secrets
  for `Config`, `File`, `ProviderSpec`, `McpServer`, `McpTransport`, `LspServer`,
  `Hook` or `Toolchain`, and their output is different text than it was. Each
  type now has a hand-written `Debug` instead of a derived
  one: a provider's `api_key` renders as `<redacted>` when set and `None` when
  not, a `Compatible` `base_url` goes through the same endpoint redaction the
  provider types have used since 0.70.0, and `Config`'s `raw` table renders as
  key names, nesting and leaf *kinds* rather than leaf values. Structure an
  operator needs is intact — the scopes read, the sections present, the model
  ids, the MCP server ids. *Migration:* nothing to write, and nothing was
  parseable before — `Debug` is not a stable format. If you logged a `Config` to
  see a configured value, read it through the typed accessor instead:
  `config.provider_spec()` for a provider, `config.origin(key)` for which file
  decided a setting. `Serialize` is deliberately unchanged and still round-trips
  an `api_key` verbatim, so a tool that writes a configuration file back still
  writes the operator's real key.

- **BREAKING (behaviour)** — a plugin manifest is no longer substituted at all.
  `${env:}` and `${file:}` join `${cmd:}` in being refused, in every scope, and
  the manifest is now parsed by a reader that does not resolve substitutions at
  all rather than by the resolving parser with a check after it. Only `${cmd:}`
  was refused before, which was sufficient while a manifest could be reached only
  after an operator had written a `[[plugin]]` entry naming it — a trust act.
  `Plugins::inspect`, new in this release, is pointed at directories nobody has
  agreed to yet. *Migration:* write the value out literally. A manifest is a
  third party's file, and there is deliberately no opt-out: if a bundle needs a
  value from the host environment, the operator supplies it in their own
  configuration, where substitutions still resolve normally. No manifest in this
  repository or its fixtures used either form.

### Added

- `Effect::ALL` and `ExecMode::ALL`, so an application reads an enum's legal
  values instead of hand-writing a menu that a later variant makes silently
  stale, plus `Effect::as_str` for the wire word of a held effect. Completeness
  is guarded in-crate by an exhaustive `match`, which is the only mechanism that
  makes a new variant break a build rather than quietly shrink a list — a length
  check against a literal would not. (#218)
- `DEFAULT_MAX_STEPS`, `DEFAULT_WORKSPACE_MAX_STEPS` and `DEFAULT_MAX_RETRIES`,
  the defaults behind `run.max_steps` and `run.max_retries`, which were bare
  literals no caller could name. Both step budgets are named and both are kept —
  a repo task takes more turns than a single one, and collapsing them would have
  changed the budget of every caller of one constructor. The constructors read
  the constants, so the name cannot drift from the value. (#219)
- `PriceTable::models()`, `len()` and `is_empty()`, matching the shape `Agents`
  already had. `models()` lists what the table can actually price: a model given
  tiers but no base price is unpriced, and is excluded rather than listed as
  something `price()` will then decline to answer for. (#220)
- `net::target` is public, and `net` is a public module exporting it and nothing
  else. It normalises a URL to the `host:port` the runtime will check, and its
  documentation states the half a reimplementation gets wrong: **`None` is a
  refusal, not "nothing to check"**. (#221)
- `Hook` and `OnFailure` are public, with an accessor for each of a hook's seven
  fields, so a hook can be displayed rather than only counted. `Plugin::hooks()`
  completes the accessor set that `contributions()` already advertised, and
  `Hooks::declarations()` is public so an operator's own configured hooks are
  enumerable too — the plugin half alone would have left the configuration half
  blind. (#223)
- `Plugins::inspect`, which reads and fully validates a plugin bundle on disk
  without a declaration file being written first. Every check the normal load
  path runs still runs, including the scope asymmetry that makes it useful: at
  user scope a bundle's hooks and MCP servers are returned, at project scope they
  are refused, and a manifest carrying a `${cmd:}` substitution is refused at
  either. (#224)

### Fixed

- `McpServer`'s documentation named `Error::Mcp` for a server the policy refuses.
  The refusal is `Error::Refused` — `act: "exec"` for a stdio server's binary,
  `act: "net"` for a remote server's host — and `Error::Mcp` is returned only
  once the policy has allowed the server and the process will not start. A
  consumer had already written its error mapping against the wrong sentence and
  would have missed every policy refusal. The corrected text is now asserted by a
  test rather than merely edited to match a reading of the code. (#221)
- `net::target` returned `Some(":443")` for a URL whose authority was nothing but
  userinfo (`https://user@/x`), because dropping the credentials can empty the
  authority and the empty host fell through to the scheme's default port. A
  hostless target that a permissive policy would then allow. It is `None`, and
  therefore a refusal, like every other unresolvable URL.
- `net::target` fail-opened on four more shapes, all of them bracketed. An empty
  IPv6 host (`https://[]/x`), an empty IPv6 port (`https://[::1]:/x`), a tail
  after the bracket that is not a port at all (`https://[::1]evil.com/x`), and an
  unclosed bracket (`https://[/x`) each produced a target instead of a refusal —
  while the identical shapes without brackets correctly produced none. The IPv6
  branch funnelled every tail it could not read into the scheme's default port,
  and the unclosed case escaped that branch entirely and was read as a bare host.
  The documented contract already said an empty host or an empty port is a
  refusal, so a consumer written from the documentation was stricter than the
  crate it was copying from.
- The Windows containment test's "can execute a program" row probed an arbitrary
  program on `PATH`, which on a CI runner is a rustup shim that starts, fails to
  read the host's own toolchain home, and exits non-zero — so the row reported a
  containment failure for a reason that was the host's, not the container's. It
  now runs the toolchain binary the same report table already uses, and the test
  refuses to pass at all if no program row could be executed.

### Security

- **Every release up to and including 0.70.0 prints the operator's resolved
  configuration secrets on `{:?}` of a `Config`, a `File` or a `ProviderSpec`.**
  All three derived `Debug`. `ProviderSpec` carries `api_key` in all four
  variants and is handed out by `Config::provider_spec()`; worse, `Config` holds
  the merged `raw` TOML table, which configuration parsing has *already*
  substituted every `${env:}`, `${file:}` and `${cmd:}` into — so a single
  `{:?}` printed each secret twice, through the typed field and through the raw
  table, including MCP `Authorization` headers that never touch `ProviderSpec`
  at all. Nothing inside the crate printed one, so no io-harness run leaked a key
  on its own; the exposure is an embedder that logs or dumps its effective
  configuration, which is an ordinary thing to build. **Upgrading is the fix**,
  and there is no workaround on an earlier version short of never formatting
  those types. Found by sweeping for the class 0.70.0 closed one instance of,
  rather than from a report.
- **The same leak reached one call further out, through the accessors.** Hiding a
  secret in `Config`'s own `Debug` while `Config::mcp_servers()` handed back a
  type that still derived `Debug` would have closed the reported shape and left
  the reachable one open — which is exactly what 0.70.0 did when it fixed four
  provider types and missed the configuration layer beneath them. So `McpServer`
  and `McpTransport` (an `Authorization` header, a stdio child's environment and
  argv), `LspServer` (a child's environment and argv), `Hook` (an argv and an
  append path) and `Toolchain` (six argv vectors from `[toolchain.*]`) all have
  hand-written `Debug` impls too. Each shows what an operator needs to recognise
  the thing — the id, the program, the header and environment *key names* — and
  hides every value a substitution could have filled. `Serialize` is untouched on
  every one of them.

## [0.70.0] - 2026-08-29

The harness stops keeping things to itself. Every entry below is the same shape:
a fact this crate already recorded, or an act it could already perform, that no
caller could reach — and in one case, a decision the operator made that four code
paths did not honour.

### Added

- `enabled` on an `[[mcp]]` server and on a `[[plugin]]` bundle, defaulting to
  `true`, so an operator turns one off without deleting its declaration. A
  disabled server contributes no tools and is never started; a disabled bundle
  contributes nothing to skills, templates, agents, MCP servers, hooks or policy.
  Both stay readable as configured-and-off — `Plugins::disabled()` lists the
  bundles — because a capability missing from a listing cannot be told apart from
  one that was never declared. A switched-off bundle claims no id, so switching
  one off and declaring its replacement beside it works: two bundles sharing an
  id collide only when both are switched on.
- A near-miss check for the `enabled` key inside an `[[mcp]]` table. That table is
  the one section exempt from `deny_unknown_fields`, so `enabld = false` would
  otherwise be swallowed in silence and the server the operator disabled would
  run. An unrelated unknown key in the same table is still accepted: the exemption
  stays, and it is load-bearing for forward compatibility.
- A public probe for a configured MCP server — start it, report whether it
  answered and what it offered, shut it down — so a consumer can tell "the policy
  would refuse this" from "this command is wrong" from "this host is
  unreachable". Bounded by the server's own `timeout_secs`, handshake included.
- `Store::session_created_at`, and a preview of `sweep_sessions` that returns the
  `Pruned` it would produce without deleting anything. The preview and the sweep
  resolve and measure through one code path, so the preview is the receipt rather
  than an estimate of it. (#216)
- `Store::run_file`, a reader for the path `start_child_run` records — which for a
  child spawned under `worktree = true` is the child's own worktree, not its
  parent's root. The column had been written since 0.36.0 and read by nothing.
  (#215)
- `RunOutcome::VerificationFailed { steps }`, for a run that reached its step cap
  having failed its criterion. `StepCapReached` had meant both "ran long" and
  "the work does not hold up", and a caller reasonably read it as "raise
  `max_steps`" when the real cause was a criterion that was not being met. (#212)

### Changed

- **`Effect::Ask` on `Act::Exec` now raises an approval instead of refusing.** It
  was compared against `Allow` and refused anything else, so `Ask` behaved as
  `Deny` — and `Policy::default()` sets `exec = Ask`, so out of the box every git
  built-in was refused and no approver was ever consulted, with the error naming
  the program so it read as a missing binary. Fixed at all four sites carrying
  that comparison, not only the one that was reported: the git spawn, every MCP
  tool invocation and a spawned agent's worktree write. **This changes what a
  default-policy run does** — consumers get a pause where they previously got an
  error. A `Deny` posture still refuses without asking. (#214)

  Two boundaries on that, both deliberate. **A configured MCP server must still
  be allow-listed to start at all**: `Ask` on the server's own binary remains a
  refusal, because connecting is configuration rather than an action a human is
  standing by to approve — so "MCP tool calls now ask" only applies once the
  server itself may run. And **a verification gate still refuses rather than
  asking**, because it has no approver and `Verification::passes_guarded` returns
  a `bool` with nowhere to put a pause; what changed there is only the reason
  given, which used to say the policy forbade a command it had merely asked
  about.
- **BREAKING (behaviour): a run that reached its step cap having failed its
  criterion now reports `RunOutcome::VerificationFailed { steps }` and persists
  `"verification_failed"`.** `StepCapReached` now means only that nothing judged
  the work — a run with no `Verification` still answers it, and so does one that
  never reached its gate. **Migration:** match the new variant beside
  `StepCapReached`; a downstream arm reading `StepCapReached` as "raise
  `max_steps`" will silently stop firing for the runs where that reading was
  wrong, which is the fix. The new outcome is deliberately **not** terminal to
  `resume`: a gate is re-run from scratch on the next step, so a repaired machine
  or a raised budget can still turn it green.
- A failing verification gate's phase and its recorded output are appended to the
  next step's request, so a retry is informed rather than blind. Bounded by a line
  count and a character cap together, and the bound is asserted rather than
  assumed. A failure that repeats is carried **once**, not once per step: the
  ledger accumulates for the whole run, so appending a near-identical block per
  failing step would re-send all of them on every request thereafter. A failure
  that changes is carried again. (#211)
- `McpServer` gains a public `enabled` field and is not `#[non_exhaustive]`.
  **Migration:** a struct-literal construction of `McpServer` stops compiling; use
  `McpServer::stdio` or `McpServer::http`, which every construction in this
  workspace and in io-cli already does, or add `enabled: true` to the literal.
- **Downgrade hazard, and the two halves point opposite ways.** `[[mcp]]` is
  exempt from `deny_unknown_fields`, so a 0.69.0 binary reading a file that sets
  `enabled` on a server **ignores the key and runs the server the operator
  disabled** — silently. `[[plugin]]` is not exempt, so a 0.69.0 binary reading a
  file that disables a bundle **refuses the whole file**. Neither can be fixed
  from here; 0.69.0 is already published. An operator who downgrades should remove
  the `enabled` keys first.

### Deprecated

### Removed

### Fixed

- `ModelReviewer` is constructible over the providers this crate ships. It
  required `P: Debug` through the `Reviewer` supertrait, which none of
  `OpenRouter`, `Anthropic` or `OpenAi` satisfied, leaving the only shipped
  `Reviewer` for the only model-judged criterion unreachable from any downstream
  crate using our own providers. The bound is gone rather than merely satisfied,
  so an out-of-tree provider works too. (#213)

### Security

- **`Compatible` no longer prints the operator's API key.** It derived `Debug`
  while holding the credential as a plain `String`, so `format!("{:?}", provider)`
  emitted the key verbatim — and it leaked transitively through the derived
  `Debug` on `Record<P>` and `Fallback<A, B>`. All four shipped providers now
  carry a hand-written `Debug` that prints the endpoint and the model and never
  the credential. Found while fixing #213, which had described the same code as a
  missing implementation.
- **A credential carried in the endpoint URL is not printed either.** Withholding
  the `api_key` field is not enough on its own: a base URL is caller-supplied, and
  gateway and Azure-style deployments routinely carry the key in it, as
  `https://user:sk-…@host/v1` or `https://host/v1?api-key=sk-…`. The four `Debug`
  impls now strip userinfo and the query string, keeping scheme, host, port and
  path — which is what someone debugging a misconfiguration is looking at.

## [0.69.0] - 2026-08-25

An operator can fold a running turn, and a fold outlives the turn that made it.

`Compaction` decides the folds nobody asked for and `TaskContract::fold_now` asks
for one before a turn's first request. Neither is reachable by an operator watching
a turn that is already long and already running: a contract is fixed when the turn
starts, and the threshold lands where it lands. The other half is that a fold in a
session bought exactly one turn of relief. `summaries` is keyed on `run_id`, every
session turn is its own run, and the next turn's seed rebuilt the conversation from
the turn rows — so whatever a fold had just replaced came back whole at the first
step of the next turn, on every trigger.

### Added

- `Steer::fold()` — fold the conversation at the next step boundary, beside
  `Steer::say` and `Steer::interrupt`. It is a third trigger for the one machinery:
  the same summariser, the same durable `summaries` row, the same
  `EventKind::Compacted`, and the same "what it never loses". The step that reads
  the request folds before it assembles its own request, so the summary reaches the
  model on the next thing it is sent rather than the one after that, and the
  request itself is in the trace as a `ContextEvent::steered` line at the step that
  read it.

  Five boundaries, each of them a reading somebody would otherwise implement. It is
  **not immediate**: like a message and an interrupt it lands at the next step
  boundary, because a tool call in flight is not a safe place to change the
  conversation out from under. It does **not** override an off setting —
  `Compaction { at_share: 1.0, .. }` never folds, this trigger included. It does
  **not** reach a spawned child, whose ledger is its own work with no conversation
  seeded into it. It **loses to an interrupt** sent before the same boundary: the
  turn is cancelled and no summariser call is spent on a turn nobody is going to
  read. And it does **nothing when there is nothing to fold** — a conversation
  shorter than `keep_recent` has no prefix a paragraph could stand in for, so the
  request is spent and the turn goes on. `EventKind::Compacted` is what says a fold
  happened; having sent the request is not.

  One request, one fold, and the unit is the boundary rather than the call: two asks
  that reach the same boundary are one fold, two asks separated by a boundary are
  two, and asking once does not put the turn into a mode where every step folds.

### Changed

- **BREAKING** — `SteerInbox::pending` returns `Steering` instead of
  `(Vec<String>, bool)`. The third thing an operator can send had to either grow
  the tuple — the same break again at the fourth — or be dropped from it silently,
  which loses a request the operator was told had been sent. `Steering` is
  `#[non_exhaustive]` for exactly that reason, so the next field costs a caller
  nothing.

  *Migration:* bind the struct and read its fields.

  ```rust
  // Before
  let (messages, interrupted) = inbox.pending();
  // After
  let steering = inbox.pending();
  let (messages, interrupted) = (steering.messages, steering.interrupted);
  // and steering.fold, which is what the tuple had no room for
  ```

- **A fold now survives the turn that made it.** `Session::seed` asks for the
  newest turn on the path whose run folded, and seeds that turn's newest summary
  paragraph in place of the conversation entries the fold actually consumed —
  rather than rebuilding every prompt and reply and undoing the fold. The paragraph
  carries the same `[earlier work, summarised]` framing an in-turn fold writes, so
  a folded span reads identically whether it was folded three steps ago or three
  turns ago, and it reaches the model as narration rather than as either party's
  words.

  It does not replace every earlier turn. A fold keeps the newest `keep_recent`
  entries whole, so the seed keeps them too, and the turns after the folding one
  are seeded as they always were.

  Nothing is stored that was not stored before: it is a join over
  `session_turns.run_id` and `summaries.run_id`, the same join `Session::transcript`
  already makes. The transcript is untouched — every prompt and reply is still
  there whole and the folded observations are still in the trace. Only what the
  model is seeded with gets shorter.

### Deprecated

### Removed

### Fixed

### Security

Nothing else moves: no schema change, `CHECKPOINT_FORMAT` stays at 7, no
dependency was added, and no signature other than the one marked above changed.

## [0.68.0] - 2026-08-25

The conversation folds when the operator says so, as well as when the threshold
notices.

`Compaction { at_share, keep_recent }` has decided every fold since 0.43.0, and it
decides them by watching the ledger cross a share of the window. An interface that
wanted to fold *now* — the `/compact` an operator reaches for when they know a long
thread is finished — had no call to make. It could only lower `at_share` for a turn
and hope the ledger crossed it: a caller's own setting mutated to fake a request,
landing at a point nothing could predict, and possibly not landing at all.

### Added

- `TaskContract::fold_now` and `TaskContract::with_fold_now` — fold this turn's
  history at its first step, before that step assembles its first request.

  Automatic compaction is untouched and stays the default. This is a second
  trigger for the same machinery, the same summariser and the same durable
  `summaries` row, so a caller who never sets the flag sees exactly the behaviour
  they had.

  Three boundaries are deliberate and each is asserted. The request is consumed
  **once**, at the turn's first step, so a contract reused for every turn does not
  fold every turn. It does **not** override an off setting — `Compaction { at_share:
  1.0, .. }` never folds, and one trigger reversing that would make "off" mean two
  things. And it does **not** reach a spawned child: a contract reaches the whole
  tree, but a child's ledger is its own work with no conversation seeded into it,
  so only the root turn honours the request.

- `EventKind::Mcp` carries `tools` — how many tools a server offered, on the event
  that announces it reaching the run, and `None` on every other form. A server that
  came up offering nothing announces `Some(0)`, which is the fact this exists to
  separate from "this event does not carry the count".

### Changed

- **BREAKING** — `EventKind::Mcp` gained a field, which an exhaustive match does
  not survive. `EventKind` itself has been `#[non_exhaustive]` since 0.24.0, but
  that covers the variant *list* rather than a variant's fields, so this one is
  paid rather than free.

  *Migration:* add `..` to the pattern.

  ```rust
  // Before
  EventKind::Mcp { server, tool, ok, millis } => { /* ... */ }
  // After
  EventKind::Mcp { server, tool, ok, millis, .. } => { /* ... */ }
  ```

  The count was previously argued to be implied by the `discovered` events that
  follow a connect. It is derivable — by counting N events and telling them apart
  by which of their fields are set — but only by an observer attached for the whole
  of connect, and never as a number the event itself states. That reasoning is
  reversed here rather than left standing beside the change.

### Fixed

- **A session turn seeded with a long conversation could not fold at its first
  step — on the threshold or on the overflow recovery.** A fold may only replace
  entries the store already holds, and the seeded conversation sat above that
  watermark for the whole of step one: it became durable at the *end* of that step,
  while the fold is attempted at the start of it. So the check returned early
  before it ever asked whether a fold was forced, and a turn whose conversation
  exceeded the window re-sent the same refused request and escalated — the one turn
  the overflow recovery exists for was the one it could not help.

  The seed is now made durable before the first step. That is not a relaxation of
  the rule that an observation must not outlive a step that never committed: the
  seed belongs to no step of the run, and is a copy of conversation rows that are
  already durable.

Nothing else moves: no schema change, `CHECKPOINT_FORMAT` stays at 7, no dependency
was added, and no other signature changed.

## [0.67.0] - 2026-08-25

A turn can be steered whatever contract it carries.

An operator who wanted to correct an agent mid-run and an operator who wanted that
run to carry skills, MCP servers, registered tools, a plan gate, a step or token
budget or a verification gate were told to pick one. `Session::turn_steered` takes
a `SteerInbox` and builds its contract internally from the operator's text;
`turn_bounded_observed` and `turn_contained_bounded_observed` take the caller's
contract and take no inbox at all. There was no third option, so an interface that
wanted both dropped steering.

Nothing in the loop was missing. `Session::drive` has taken the contract, an
optional `Containment` and the steer inbox as three orthogonal parameters since
they existed, and the tree loop has drained the inbox at its own step boundary all
along — that call had simply never executed from a real entry point, because no
contained turn could be given an inbox to drain.

### Added

- `Session::turn_bounded_steered` — the caller's `TaskContract`, an `Observer` and
  a `SteerInbox` on one call. The contract's `root` is replaced by the session's,
  as `turn_bounded` replaces it.
- `Session::turn_contained_bounded_steered` — the same for a turn that may fan out,
  with the `Containment` in `turn_contained_bounded_observed`'s position and the
  inbox appended last. The operator's correction reaches the root of the fan-out;
  a spawned child is handed no inbox at all, because a sub-agent is never steerable
  by an operator it has not spoken to.

Steering itself is unchanged. A message is drained at the step boundary and nowhere
else, so the step in flight completes whole and the agent reads the correction
before choosing its next action; an interrupt ends the turn as
`RunOutcome::Cancelled` on a whole step. On the contained path that boundary is the
root's own step, which is the one point at which no child of the root is in flight —
so a correction typed while children are running lands after that step's children
have finished rather than interrupting one of them. That is now stated in both
methods' rustdoc, in `docs/CONTRACT.md` and in `docs/guide/sessions.md`.

Nothing else moves: every existing signature is unchanged, no schema moved,
`CHECKPOINT_FORMAT` stays at 7, no dependency was added, and a caller who never
calls the new methods gets exactly the crate they had.

## [0.66.0] - 2026-08-19

A turn that may fan out takes the caller's contract, on the session and on the
bound harness alike.

`Session::turn_contained` built its own contract from the operator's text, so a
turn that could decompose was the one turn shape with no way to carry a plan gate,
a preset or a replaced system prompt, repository instructions, registered tools,
MCP servers, skills, a step or token budget, or a verification gate — every one of
which `Session::turn_bounded` has accepted since 0.36.0. A caller who wanted a
fan-out *and* a contract was told to pick one. Nothing in the tree loop was
missing: `run_tree` has taken a full contract since 0.39.0 and the loop reads all
of it. What was missing was a way to reach it as a turn.

`Harness` had no contained turn at all, so an embedder who bound their host once
had to unbind the moment a conversation needed to decompose.

### Added

- `Session::turn_contained_bounded` and `Session::turn_contained_bounded_observed`
  — a contained turn under a `TaskContract` the caller shaped, taken beside the
  `Containment`. The contract bounds the agent answering the turn; the containment
  bounds the tree it may grow. As with `turn_bounded`, the contract's `root` is
  replaced by the session's, because a turn is about the conversation's workspace.
- `Harness::turn_contained` and `Harness::turn_contained_with` — the same two
  shapes with the provider, store, policy, approver and observer bound once.

- What the tree loop reads differently from the flat one is now stated in the new
  methods' rustdoc, in `docs/CONTRACT.md` and in `docs/guide/sessions.md`: the
  tree's one shared spend ceiling comes from the `Containment` rather than from the
  contract, `Routing`'s `escalate_after` and `downshift_under` do not move the model
  per step, the preflight checks a flat run makes before its first request (a
  `Verification::Review` with no reviewer, a reviewer that is the model under
  review, `Routing::require_primary` against `Provider::reachable`) are not made
  though a model approving its own call is still refused at the root, and
  `max_parallel_reads` bounds a batch only the flat loop builds. None of this is new
  in 0.66.0 — all of it has been true of `run_tree` since 0.39.0 — but a contract
  reaching that loop from a session turn puts it in front of callers who have never
  read it.

Nothing else moves: every existing signature is unchanged, no schema moved,
`CHECKPOINT_FORMAT` stays at 7, and a caller who never calls the new methods gets
exactly the crate they had.

## [0.65.0] - 2026-08-18

A run killed in the middle of a call the harness cannot inspect pauses for a
decision instead of silently making the call twice.

Everything a run records is written at the step boundary that commits, so that an
observation belonging to a step that never committed does not outlive it. That is
the right rule for a ledger, and it is exactly what makes an interrupted external
call invisible: the call ran, the process died before the step committed, and the
store holds no evidence that anything was attempted. Resume then replays the step
— correct for a workspace edit, which can be read back, and wrong for a charge, a
deployment, a posted message or any registered tool reaching a service the crate
cannot see. This release gives those calls a durable journal of their own, written
before the call and closed after it, outside every step transaction, and a resume
that refuses to drive a run holding an open one.

### Breaking changes

- **BREAKING** `RunOutcome` gained the variant `AwaitingRecovery { attempt_id,
  steps }` and is now `#[non_exhaustive]`. The variant alone already breaks an
  exhaustive `match`; the attribute lands in the same release so that no later
  addition breaks one again. *Migration:* add a wildcard arm.

  ```rust
  match result.outcome {
      RunOutcome::Success { steps } => println!("done in {steps}"),
      // Handle the pause if you register tools with effects the crate cannot
      // see; otherwise this arm is unreachable and the wildcard is enough.
      RunOutcome::AwaitingRecovery { attempt_id, .. } => decide(attempt_id),
      _ => {}
  }
  ```

- **BREAKING (behaviour)** A resumed run whose journal holds a call that was
  started and never finished returns `AwaitingRecovery` instead of driving. This
  can only happen for a registered `Tool` reporting
  `ToolRecovery::Indeterminate` — which is every tool declaring, or defaulting
  to, `ToolEffect::Mutating` — or for an MCP call. A run of built-in tools, and a
  tool declaring `ToolEffect::ReadOnly`, are unaffected and journal nothing.
  *Migration:* to keep the old behaviour for a tool you know is safe to repeat,
  say so — `fn recovery(&self) -> ToolRecovery { ToolRecovery::Replayable }` —
  which is also what makes the claim reviewable. To act on the pause, call
  `resume_with_recovery` with `RecoveryDecision::Retry`, `Completed { observation }`
  or `Abort`.

### Added

- `ToolRecovery`, and `Tool::recovery`, a defaulted trait method: whether a call
  that was in flight when the process died is safe to make again. Defaulted from
  `Tool::effect`, so every existing implementation compiles unchanged.
- `RecoveryDecision` and `resume_with_recovery` / `resume_with_recovery_observed`:
  retry the call, record it as already completed with the account the model is
  given, or abort the run.
- `ToolAttempt`, `Store::open_attempt`, `Store::close_attempt`,
  `Store::open_attempts` and `Store::resolve_attempt` — the journal, readable by
  an operator directly.
- `EventKind::RecoveryPaused`, emitted by the resume that finds an open attempt,
  so a caller driving many runs learns which attempt is holding which run without
  opening the store.

### Changed

- A read-only call is never speculated ahead of its completion unless it is also
  replayable. The two were already the same set — speculation requires
  `ToolEffect::ReadOnly` and that derives `ToolRecovery::Replayable` — and the
  requirement is now stated where the work is built rather than left to hold by
  agreement between two files.

## [0.64.0] - 2026-08-17

A resumed run sends the model its own past turns rather than a third-person
account of them.

Until this release, everything a run did before it was interrupted arrived in the
next request as one flat block of user prose, and only the steps driven after the
resume point were role-tagged. The cause was narrow and had been recorded in the
source since 0.49.0: tool *results* are durable and their per-step positions are
recomputed on restore, but the assistant turn they answer — what the model wrote
and the calls it made — was held in memory by the run loop and died with the
process. With nothing to pair the results against, they were flattened into the
user message.

That half is now durable. Given the same committed state and the same responses,
a resumed run assembles the same `messages` an uninterrupted run assembles: same
roles, same assistant turns, same result batches.

### Added

- **`AssistantTurn`, `Store::record_step_turn` and `Store::step_turns`** — what
  one step asked for, kept. `text` is `Option<String>` so that *wrote nothing* and
  *wrote the empty string* stay different facts; `calls` is the ordered
  `Vec<ToolCall>` the model made, stored as this crate's own JSON rather than any
  vendor's wire form.

  ```rust,ignore
  for turn in store.step_turns(run_id)? {
      println!("step {} called {:?}", turn.step,
               turn.calls.iter().map(|c| &c.name).collect::<Vec<_>>());
  }
  ```

  This is the first durable, structured record of what a step asked for.
  `StepRecord::tool_call` is unchanged and stays what it is — one human-readable
  string, `name:args` joined with ` | `, which a trace dump prints and the stall
  signature compares, and which cannot be split back apart when a tool name
  contains `:` or an argument contains ` | `.

- **One additive table, `step_turns`**, keyed `(run_id, step)` with no index of
  its own — the primary key is the index every read searches. `CHECKPOINT_FORMAT`
  is unchanged at 7 and no existing table, column or statement is altered. A store
  written by an earlier release gains the table empty on first open; a store
  written by this one is read by an earlier binary, which never names it.

### Changed

- **A resumed run's requests carry a conversation where they used to carry
  prose.** This is a change to what goes on the wire, not to any signature: a
  caller diffing raw requests across versions will see it, and it is the release.
  The transcript a resumed run now sends is the one the uninterrupted run would
  have sent.

  Two things deliberately do not change. Tool-call ids are still minted from
  position and still never stored, so the same transcript still assembles to the
  same bytes and a cache prefix still holds. And a step whose results do not line
  up with the calls it made still falls back to prose — correlating them
  positionally would answer the wrong call, and that case is correct as it stands.

- **A run resumed out of a store written before this release behaves exactly as it
  did before it.** There are no turns to restore, so its earlier history is prose,
  as it was. Absent is not empty: a step with no stored turn falls back, while a
  step whose stored turn made no call and wrote no text is a real turn that did
  nothing and is sent as one.

## [0.63.0] - 2026-08-17

Build the harness once and run against it. A `Harness` binds the provider, the
store, the permission boundary, the approver, the observer, and the host
configuration that is not a property of any one task — the toolbox, MCP and LSP
servers, the browser, the skills directory, the plugin bundles, the agent roster,
the responder and web access. Two tasks then run through it without restating any
of them.

Before this release a session turn read
`turn(text, &provider, &store, &policy, &approver)` and the steered variant took
seven arguments with `#[allow(clippy::too_many_arguments)]` in the source beside
it; `src/run.rs` exposed thirty-five top-level public functions, every one of them
taking `&Store` and most taking `&Policy` and `&dyn Approver` too. A caller with
twenty tasks built the same ten host settings twenty times, and every one of those
was a place they could be built differently by accident.

The storage library also stops being part of the published contract, and `Error`
becomes `#[non_exhaustive]`.

### Breaking changes

- **BREAKING (compile)** — `Error` is now `#[non_exhaustive]`. A program matching
  it exhaustively outside the crate stops compiling until it adds a wildcard arm.
  *Migration:* add `_ => ...`. A caller that already has one is unaffected.

  This is late rather than new, and saying so is the point: every variant added
  since 0.23.0 — `Lsp`, `Browser`, `Resume`, and 0.62.0's `Conflict` — was a
  compile break of exactly this shape, because the attribute was never on this
  enum. It is paid once here and no future variant costs anyone anything.

### Added

- **`Harness`** — the host, bound once. It borrows rather than owns, because
  `rusqlite::Connection` is `Send` and not `Sync` and every existing entry point
  already takes these by reference; it is generic over the provider, because
  `Provider::complete` returns `impl Future` and the trait is not dyn-compatible,
  so there is no `Box<dyn Provider>` to be had.

  ```rust,ignore
  let harness = Harness::new(&provider, &store)
      .with_policy(policy)
      .with_approver(&ApproveAll)
      .with_defaults(TaskContract::workspace("", "/repo").with_skills("/repo/.io/skills"));

  harness.run(&harness.workspace("bring the docs up to date", "/repo")).await?;
  let mut session = harness.session("/repo")?;
  harness.turn(&mut session, "what does this crate do?").await?;
  ```

  The template it holds is a source for `workspace()` and `task()` and for
  nothing else — a contract handed to `run()` is used verbatim. The rejected
  alternative, filling in whatever a contract still holds at its default, cannot
  tell a caller who set a field to its default value from one who never set it,
  and a rule a caller cannot evaluate at the call site is worse than typing the
  setting twice.

- **`Error::Storage { kind, message }` and `StorageErrorKind`** — an owned
  storage failure. `kind` is `Busy`, `Constraint`, `Corrupt` or `Other`, with
  only `Busy` retryable; `message` is what the storage layer said, kept whole.

- **`TaskContract::with_conversational_turns`** — say outright whether a session
  turn may decide it was conversation and answer, rather than having it inferred
  from `Verification::None`. Unset is the default and infers exactly as before.

### Changed

- Nothing a caller wrote before this release behaves differently. The
  thirty-five free functions and all seven `Session::turn*` methods keep their
  exact signatures, asserted by a derived test rather than by review, and a
  `Harness` call reaches the loop by calling the same function a caller would
  have called themselves — asserted as canonical-trace equality, because outcome
  equality would pass against a facade that quietly ran a different loop.

### Deprecated

- **`Error::State(rusqlite::Error)`** — replaced by `Error::Storage { kind,
  message }`. **It is removed in 0.65.0**, the minimum cycle this crate's
  contract allows, named by version so a caller reading the warning knows exactly
  how long they have. Nothing in the crate constructs it as of this release, so no
  failure arrives as that variant any more.

  *Migration:*

  ```rust,ignore
  // Before
  Err(Error::State(e)) => report(e.to_string()),

  // After
  Err(Error::Storage { kind, message }) if kind.is_retryable() => retry(),
  Err(Error::Storage { message, .. }) => report(message),
  ```

  From 0.23.0 until now, `rusqlite` was a *public* dependency of this crate: its
  error type was in `Error`, so a `rusqlite` major bump was a breaking change
  here whether or not anything behaved differently — which is exactly what
  0.23.0 itself was. That is over. What it does **not** change is that
  `libsqlite3-sys` declares `links = "sqlite3"`, so a consumer's graph still
  holds one version of it; the upgrade stops being a type-level break, not a
  graph-level constraint.

## [0.62.0] - 2026-08-17

Two processes can no longer both drive one run. A driver holds a lease with a
generation, every durable step commit carries it, a second live owner is refused
with a typed conflict instead of racing, an expired lease can be taken over, and
a session's head advances by compare-and-swap so a lost update is a returned
conflict rather than a silently dropped turn.

Before this release nothing in the crate detected a live owner: two processes
calling `resume` on one run both passed every check and interleaved their steps
into a single trace, under one run id, each numbered from its own in-memory
counter. The result read as a coherent run that neither process had performed —
no error, no event, and nothing in the store afterwards to tell it from a real
one. For a runtime whose durable trace is the thing it is for, that was the
sharpest correctness boundary left open.

### Breaking changes

- **BREAKING (compile)** — `Error` gains a `Conflict { run_id, owner,
  expires_at }` variant. A program matching `Error` exhaustively outside the
  crate stops compiling until it handles or ignores the new arm. *Migration:*
  add the arm, or a `_ =>` catch-all. A caller that already has one is
  unaffected.
- **BREAKING (behaviour)** — a second process driving a run its owner still
  holds now gets `Error::Conflict` where it previously got a trace. For every
  caller that did this by accident — a supervisor restarting a worker before the
  old one exited, a resume issued from a CLI while a service still holds the
  run — the previous behaviour was a corrupted trace, so this is a silent
  failure becoming a loud one. A caller that deliberately drove one run from two
  processes and tolerated the interleaving must now wait for the holder to finish
  or its lease to lapse and take the run over. Only a *live* owner refuses a
  resume: a run whose driver was killed is taken over at once, on unix and on
  Windows, so `kill -9` and resume is unchanged. *Migration:* handle `Error::Conflict`; it
  names the holder and when its lease expires.
- **BREAKING (behaviour)** — a session-head write that lost a race used to
  succeed silently and now returns `Error::Conflict`. The losing turn row is
  left exactly as it was; only the head write is refused. *Migration:* where you
  called `session.turn(...)` and ignored the possibility of a race, match
  `Err(Error::Conflict { .. })` and either re-read the head with
  `Store::session_head` and take the turn again, or keep the answer you have and
  branch from it with `Session::branch_from`. A single-process program never sees
  it. `Store::set_session_head` is unchanged and still writes unconditionally for
  a caller that wants the old behaviour outright.

### Added

- `Store::acquire_lease`, `Store::renew_lease`, `Store::release_lease` and
  `Store::run_lease`, with the `Lease` guard they hand back and the `LeaseRow`
  they read. An acquire is refused only when the lease belongs to another owner,
  has not lapsed, *and* that owner's process is still alive — checked against the
  pid in the owner id with `kill(pid, 0)` on unix and with
  `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` plus `GetExitCodeProcess` on
  Windows, neither of them a dependency this crate did not already have — so a
  `kill -9`'d owner's run is taken over at once rather than at the ttl on both. A
  process that is there but somebody else's counts as alive: `EPERM` on unix, a
  handle refused for lack of rights on Windows, where only
  `ERROR_INVALID_PARAMETER` means no such process. The check errs
  towards "alive", which is the direction that costs a wait rather than a second
  driver: an owner id with no readable pid, a platform that is neither unix nor
  Windows, and a Windows process that exited with code 259 — which is
  `STILL_ACTIVE` and so cannot be told from a running one — all report the owner
  as running, and the ttl governs exactly those cases and a recycled pid. A
  lease is released when its guard drops, so no run-loop exit path had to grow a
  release call. `Store::owner` is this handle's opaque owner id.
- `Store::set_session_head_if`, the compare-and-swap behind every in-crate head
  advance. `Store::set_session_head` is unchanged and still unconditional.
- `TaskContract::lease_ttl` and `TaskContract::with_lease_ttl`, defaulting to
  the new `DEFAULT_LEASE_TTL` — twice `DEFAULT_EXEC_TIMEOUT`, because the
  renewal rides each step commit and so what the ttl must outlast is one step
  rather than a whole run.
- One additive table, `run_leases`. No index: `run_id` is its `INTEGER PRIMARY
  KEY` and therefore a rowid alias, so every lookup the crate makes on it is
  already a primary-key search. `CHECKPOINT_FORMAT` stays 7, and a 0.61.0 binary
  opens a store this release wrote without ever naming the table.

### Changed

- `Store::checkpoint_step` verifies the caller's lease generation *inside* the
  transaction it already opened, when the calling handle holds a lease on that
  run. A driver whose run was taken over mid-completion therefore writes nothing
  at all — no `steps` row and no `checkpoint_events` row — rather than writing a
  step it was no longer entitled to. A handle holding no lease commits exactly as
  it did before.
- `Store::delete_session` and `Store::archive_session` now clear `run_leases`
  along with every other run-keyed table.

### Deprecated

### Removed

### Fixed

- **A session turn could be answered, billed, and then dropped without a word.**
  `set_session_head` was a bare `UPDATE`, so two processes taking a turn on one
  session both wrote their own turn id and the second won outright; the first
  process's turn stayed in `session_turns` with its parent intact but off the
  head path, invisible to every later turn. It is now a compare-and-swap and the
  loser is told.

### Security

## [0.61.0] - 2026-08-17

Every name the harness answers is now reserved, and a test derived from the
crate's own tool constants is what keeps the set complete. Eighteen built-in
names were dispatched and unreserved, so a registered tool taking one of them
validated cleanly and was then unreachable for the life of the process — with no
error, no event and no log line. 0.17.0 closed this once as a hand-patched list,
and every built-in added afterwards reopened it by one name.

### Breaking changes

- **BREAKING (behaviour)** — eighteen names the harness answers are now reserved,
  so a program registering a `Tool` called `forget`, `check`, `patch_file`,
  `git_branch`, `git_worktree`, `lsp_definition`, `lsp_references`,
  `lsp_symbols`, `lsp_hover`, `lsp_rename`, `browser_navigate`, `browser_read`,
  `browser_screenshot`, `browser_click`, `browser_type`, `browser_scroll`,
  `send_message` or `read_messages` fails at run start with `Error::Config`
  instead of validating. Sixteen of those were already broken and broken
  silently — the built-in answered every call and the registered tool never ran
  for the life of the process. `send_message` and `read_messages` are the two
  that did work, in a flat run, while being shadowed inside an agent tree.
  *Migration:* rename the registered tool. A name the harness does not answer is
  unaffected, prefixes are not reserved, and `browser_history` or `checker`
  remain yours to take.

### Added

### Changed

- The six `browser_*` tool-name constants exist in every build, not only one with
  the `browser` feature enabled. The tools behind them are still feature-gated
  and no catalogue changes; the names are now the harness's in all builds, for
  the reason the image and document names have been since 0.17.0 — enabling a
  feature can never take away a tool that was working.

### Deprecated

### Removed

### Fixed

- **A built-in added after 0.17.0 reopened the tool-name shadowing defect by one
  name, every time, and nothing said so.** The reserved set is now derived from
  the crate's own tool constants by a test that fails when a built-in name is
  missing from it, in either direction, rather than being a list kept current by
  hand.

### Security

## [0.60.3] - 2026-08-16

Every block a classifying turn is composed from now says something true of that
turn. Three did not. No public item is added, renamed or removed and
`docs/public-api.txt` is byte-identical to 0.60.2's, so a program on 0.60.2
recompiles unchanged — but this release does change composed prompt text, which
0.60.1 and 0.60.2 did not, and the changes are named below.

### Fixed

- **A plan-gated session turn was ordered to propose a plan before it was
  permitted to answer.** The classifying opening composed the planning directive
  — "Before you do anything else you must call `propose_plan` … and wait" —
  immediately above the crate's own ending, which says "if a plain answer is the
  whole of what is wanted, write that answer and call no tool". An operator who
  typed a greeting into a gated session got a plan proposed for the greeting and
  a human asked to approve it. The directive now binds the *work* reading on that
  one block: if any part of the turn needs the repository written to or a command
  run, the plan comes first. **The gate is not weakened.** The sentence naming
  what is refused is identical in both forms, and the policy layer that refuses a
  write during the phase is unchanged; a turn already decided to be work still
  reads the unconditional form.

- **A plan-gated turn was told a boundary that was not the one holding it.** The
  system block described the plan-narrowed policy while the classifying opening
  was handed the post-plan one, at both call sites — so a turn under the gate
  read that writes were allowed while the gate refused every one of them. Both
  blocks of a gated turn now describe the same narrowed policy, and the selection
  is made in the one function both loops share rather than at each call site.

- **A `SystemPrompt::Preset` discarded the framing the loop had chosen.**
  `Preset::Concise` and `Preset::Careful` each carried a whole agent description
  and were composed *instead of* the run's own, so an embedder who selected one
  had the conversational framing thrown away on a session turn — putting back the
  two claims 0.49.0 removed, "to meet a stated specification" and "checked against
  the success criterion", on every greeting — and had the tree framing thrown away
  on a contained turn, dropping the paragraph that says the agent may spawn. A
  preset is now a working style appended to whichever framing the loop chose,
  which makes `Preset`'s own documented promise — it shapes how the work is done
  and reported, never what the agent can reach — true as written.

### Changed

- **One composed prompt string moves, and only for a caller who selected a
  preset.** The old preset bodies omitted the sentence "You may edit several
  files." that `WORKSPACE_PROMPT` carries; a preset is now that framing plus the
  style clause, so the sentence is present. An embedder who snapshots composed
  system prompts and uses `SystemPrompt::Preset` will see this one difference. No
  other `SystemPrompt` variant's output moves: the `Builtin`, `Append` and
  `Replace` baselines are byte-identical to 0.60.2's.

- **`docs/CONTRACT.md` records the design the classification rule comes from** —
  the one-shot API is task-framed because its caller declared work in code, the
  session API is conversation-framed because its operator did not, and every block
  composed for such a turn is held to being true of it. That rule is the common
  cause of 0.48.0's, 0.49.0's and this release's fixes, and it was the one thing
  the contract never stated.

- **`docs/STYLE.md` is new**, documenting the register the crate's prose already
  follows. It changes no existing file and no test enforces it.

### Added

- Seven assertions covering the classifying turn's composition: four in
  `tests/prompt.rs`, end to end through `Session::turn_bounded` under a real plan
  gate, and three in `src/run.rs`'s own tests for the tree loop's framings.
  `tests/prompt.rs` composed only under a permissive, ungated policy before this
  release, which is why all three defects survived it.

## [0.60.2] - 2026-08-16

Documentation only. Nothing under `src/` changes except doc comments,
`docs/public-api.txt` is byte-identical to 0.60.1's, and a program on 0.60.1
recompiles unchanged.

### Fixed

- **`docs/CONTRACT.md` gave two opposite answers about what a command the agent
  runs is bounded by, and the wrong one was the reassuring one.** The
  command-execution block said a command runs "in the workspace root with the
  embedding program's privileges, outside the sandbox" — true up to 0.44.0 —
  while the same file stated 1,300 lines earlier that everything a run starts is
  contained (0.48.0), with nothing telling a reader which superseded which. The
  block now states today's boundary: `ExecMode::WorkspaceWrite` is the default,
  `TaskContract::with_full_access` is how the pre-0.45.0 grant is asked for by
  name, and what containment is worth is given per platform rather than
  flattened into one sentence. A reader who checked the contract before trusting
  the harness with sensitive work was worse off than one who never opened it.

- **`TaskContract::exec_sandbox`'s documentation carried a sentence 0.48.0
  retired.** It told a caller the `shell_start` / `shell_poll` / `shell_kill`
  handles "are not contained because a handle outlives the call that made it".
  They have taken the same containment every other spawn takes, per stage, since
  0.48.0. It now says so, along with the half that was true and had never been
  written here: a handle's restriction lives with its processes, and
  `SandboxLimits::max_wall_secs` deliberately does not reach one.

- **The reserved-tool-name claims disagreed with the source in three places, in
  both directions.** `docs/guide/tools-and-skills.md` said the feature-gated
  built-ins are *not* in the reserved set, which 0.17.0 made false, and its
  hand-typed list named `forget`, which is not reserved, while omitting
  `edit_file`, `shell_start`, `shell_poll`, `shell_kill`, `todo_write`,
  `ask_question` and `propose_plan`, which are. The page now refers to
  `RESERVED_TOOL_NAMES` instead of restating it. In the other direction, that
  set's own doc comment and the contract's 0.17.0 paragraph both claimed it names
  every built-in: eighteen dispatched names are absent from it — `forget`,
  `check`, `patch_file`, `git_branch`, `git_worktree`, the five `lsp_*` tools,
  the six `browser_*` tools, `send_message` and `read_messages` — because
  0.17.0's fix was a hand-maintained list rather than an invariant, so each
  built-in added since reopened the defect by one name. All three places now say
  that. Closing it changes what `Toolbox::validate` accepts, so it is 0.61.0's
  work and not a patch's.

- **A sweep of every version-numbered assertion in `docs/CONTRACT.md` corrected
  nineteen more claims a release had outlived**, in twenty-two places. Three are
  worth naming. On a stock Ubuntu 24.04 — the commonest Linux CI image — the file
  said a contained command takes the portable floor and gets **no filesystem
  confinement and no egress denial**, in three separate places; the Landlock rung
  needs no namespace and exists precisely because that host refuses one, so it
  gets confinement. "Everything a run starts is contained (0.48.0)" declared a
  class closed, and 0.51.0, 0.52.0 and 0.53.0 then each opened one — the
  post-edit checker, a language server, and the browser child on unix all spawn
  outside the backend `sandbox::select` chose; the claim is narrowed, the three
  are named with what each still passes through, and wrapping them is left to a
  later release. And "No seccomp filter is installed" has been false since the
  Landlock rung shipped one. The rest are of a kind: a deferral that shipped nine
  releases ago, a field "kept for one release" eleven minors ago, a rewind that
  exists, a third write tool that snapshots, four narrowing keys that are five,
  an as-of stamp twenty-five releases stale, and a restore that reports a
  different type than the one named.

### Added

- **Three documentation-drift tests**, so none of the three claims above can
  silently reopen: the contract's command-execution block may not carry the
  pre-0.45.0 sentence and must name the default mode; the `exec_sandbox` rustdoc
  may not carry the retired clause and must state what replaced it; and the tools
  guide may not hand-list a reserved name that `RESERVED_TOOL_NAMES` does not
  hold. The third resolves the set through the crate's own constants rather than
  comparing prose to prose, and the second is the first check in
  `tests/docs_drift.rs` to read a doc comment inside `src/` rather than a
  markdown page.

## [0.60.1] - 2026-08-16

### Added

- **A comparison table on the README.** io-harness set against the agent
  harnesses it is most often confused with (Claude Code, Codex CLI, opencode,
  Goose, pi) and against the Rust libraries in the same import slot (rig,
  swiftide, langchain-rust), on the four properties this crate is actually built
  around: a permission boundary that shows up in the trace, per-step resume that
  survives the process, an execution sandbox, and a spend ceiling shared across a
  tree of agents. Every cell carries its source and the date it was read
  (2026-08-16), and a cell reads "not documented" rather than "no" where a
  project simply does not state the property — the table records what each
  project says about itself, not what we assume it does.

- **Four documentation-drift tests**, one per gap this release found, so none of
  them can silently reopen: no release-version literal may appear in README
  prose, every guide page under `docs/` must have a row in the README's guide
  table, the README must link `docs/MEASUREMENTS.md`, and the release table in
  `docs/CAPABILITIES.md` must cover every released version.

### Changed

- **A documentation release.** Nothing under `src/` changed and
  `docs/public-api.txt` does not move; a program on 0.60.0 recompiles against
  0.60.1 unchanged.

- **The README is rewritten.** It had grown one release at a time to 618 lines,
  most of it a single flat section of thirty-odd paragraphs each written as the
  release that added it — "since 0.46.0", "0.47.0 closed", "through 0.49.0 a
  child came back as". That is a changelog wearing a landing page's clothes: a
  reader arriving today had to reconstruct the present state from a sequence of
  past ones. The page is now present tense, opens with a table of contents and a
  capability matrix above the prose, and carries no release-version archaeology
  at all — the fourth test above enforces that last part.

- **The measured numbers reach the landing page.** `docs/MEASUREMENTS.md` has
  held five measured benchmark sets for several releases and the README linked it
  zero times. The README now summarises them, names the machine each was measured
  on, and states plainly that none of them is a gate — they are recorded costs,
  not thresholds anything fails.

- **`docs/CAPABILITIES.md` covers the capabilities that shipped after it was last
  touched**, and gains a release table recording which release introduced what.
  That table is where the release-anchored facts removed from the README now
  live: the history is kept, it is just kept somewhere a first-time reader is not
  standing.

### Fixed

- **`docs/CONTRACT.md` no longer claims that nothing selects the Windows
  AppContainer.** That has been false since 0.59.0 shipped
  `SandboxConfig::with_access_confinement()`, which is exactly what selects it.

## [0.60.0] - 2026-08-16

### Added

- **Agents in a tree can talk to each other.** A tree could already nest, share
  one ledger, queue past its concurrency cap and hand a child's report up to its
  parent — every one of those a *vertical* edge. Two children investigating two
  subsystems had no way to tell each other what they found: the only channel
  between them was a file one wrote and the other happened to read, which is
  unaddressed, unordered, invisible to the trace and indistinguishable from
  ordinary workspace churn.

  Two tools, offered inside a tree and in no flat run. `send_message { to, body }`
  tells one named agent something. `read_messages { from, wait_secs }` returns
  what has been sent to you, oldest first, exactly once, optionally narrowed to
  one sender and optionally blocking until something arrives.

- **Every agent in a tree has an address, and it names one agent.** `spawn_agent`
  takes an optional `as`: the instance name this child answers to. Nothing in
  this crate could name one agent before — `AgentDef::name` is a *role*, and two
  children of one definition spawned in the same step are the ordinary shape of a
  fan-out. An address is unique within a tree, is letters, digits, `-` and `_` up
  to 64 characters, and may not be `root`, which is the agent at the top. Omitted,
  one is derived as `<role>#<run id>`, so every spawn written against 0.59.0 gets
  an addressable child rather than merely continuing to work. The address is
  durable in a new `spawns.as_name` column, so a resumed tree re-adopts its
  children under the names they already had.

- **A bounded wait, and it is never unbounded.** `wait_secs` blocks until a
  message arrives or the clock runs out. `[run] max_wait_secs` and
  `TaskContract::with_max_wait_secs` are the operator's ceiling — 30 seconds when
  neither is set — and an agent asking for longer is given the cap and told so on
  the same observation. It is a narrowing key: a project-scoped `io.toml` may
  lower it and may not raise it. There is deliberately no way to say "forever": an
  agent that blocks holds its concurrency slot, and the sibling that would answer
  it may be the one queued behind that slot.

- **A terminating agent posts one short line to its parent** — `[finished]` and
  its outcome, never its report. That is what makes "wait for a named child" and
  "wait for a message" one mechanism: a parent blocked on `from: "scout"`
  unblocks when the scout answers *or* when the scout finishes having answered
  nothing. The composed report still travels the path it has since 0.50.0, so
  nothing is delivered twice. And a wait on an agent that has already finished
  without sending returns immediately rather than at the clock.

- **`AgentMessage`, `Store::send_message`, `Store::read_messages` and
  `Store::messages_for`.** The read is the agent's own call and consumes what it
  returns; `messages_for` is the audit read and delivers nothing. Delivery is
  marked in a `read_at` column inside the same transaction as the select, which
  is what makes exactly-once survive a process boundary — a set of delivered ids
  in memory passes every in-process test and re-delivers everything the first
  time a tree is resumed. Sends and reads are also `agent_events` rows, so "who
  told whom, and when" is one query; the body is not in that table, because a
  second copy of the mailbox is one no retention call knows to delete.

- **`ROOT_ADDRESS`, `SEND_MESSAGE_TOOL`, `READ_MESSAGES_TOOL`, `DEFAULT_MAX_WAIT`**
  and a new guide page, [docs/guide/mailbox.md](docs/guide/mailbox.md).

### Fixed

- **`spawn_agent` no longer refuses `"wait": false` beside
  `"background_after_secs": 0`.** 0.50.0 wrote the rule that a zero-second wall
  clock and "do not wait" are the same request, and applied it to only one of the
  two spellings: `wait: true` with a zero clock detached, and `wait: false` with
  the same zero was refused as a contradiction. Filling every property of a tool
  schema with its zero value is ordinary model behaviour, not exotic — the live
  run that found this sent `"agent": ""`, `"deny_write": []`, `"deny_net": []`
  and `"background_after_secs": 0` on every call, and every other one of those
  was already read as "unset". Such a model could not spawn a detached child at
  all, and no fixture noticed because a fixture writes only the arguments it
  means. A contradiction is now a clock that is actually asked to elapse.

- **A blocked agent no longer starves the children it is waiting for.** A
  detached child is a future driven by its parent's own loop and by nothing else,
  so the first implementation of the wait — which slept — stopped the very
  siblings whose message it was waiting for. Every wait ran to its full clock and
  then succeeded on the step after, with a green suite throughout. The wait is now
  driven against the in-flight set the same way a provider call is. A two-child
  fan-out where one waits on the other went from 20 seconds to under one.

### Changed

- **`Store::record_spawn` takes the child's address as a ninth argument, and
  `SpawnRow` carries `as_name`.** Both are `pub` on `Store`, so a program calling
  them directly needs the extra argument; every caller inside the crate is
  updated. Rows written before 0.60.0 read back with an empty address, which is
  the honest answer for a spawn made by a release that had none.

- **A spawn's `agent_events` detail now leads with the child's address** —
  `scout as searcher: <goal>` where it was `as searcher: <goal>`. The role answers
  *what kind of agent this was*; only the address answers *which one*.

### Schema

- One additive table, `agent_messages`, and its index `agent_messages_to`. One
  additive column, `spawns.as_name`, `NOT NULL DEFAULT ''`. No
  `CHECKPOINT_FORMAT` move: no checkpoint layout changed, and a 0.59.0 binary
  opens a store this release wrote and never names either.

## [0.59.0] - 2026-08-16

### Added

- **The browser runs on Windows.** `browser::launch` had a Windows arm that
  refused outright, and its message said the work was tracked as its own release
  — a promise no roadmap entry ever backed. Every supported platform now drives a
  browser over the same pipe transport, in the same test suite, behind neither an
  `#[ignore]` nor a platform gate.

  The transport is **not** two inherited handles. Chromium turns the descriptors
  it is handed into handles with `_get_osfhandle`, so what the child needs is
  descriptors 3 and 4 open in the C runtime's own table, and the only structure
  that populates one is `lpReserved2` on the `STARTUPINFO`. Established against a
  real browser before any of it was written: the descriptor block alone speaks,
  the block plus a handle list also speaks, and the two ends as the child's
  standard handles fail with Chrome's own *"Remote debugging pipe file
  descriptors are not open"*. The pipes are anonymous, so unlike a named pipe
  there is no name another local process could open — the same argument that
  rejected a debugging port in 0.53.0, kept rather than traded away.

- **Windows can confine access, when the caller asks for it.**
  `SandboxConfig::with_access_confinement()` selects an AppContainer inside the
  Job Object, and `Backend::WindowsAppContainer` returns to the public enum with
  a production path that can produce it. A run under it has its writes confined
  to the paths the run resolved and holds no capability to reach the network
  unless its policy permits egress; both `Backend::confines_writes` and
  `Backend::denies_egress` answer true for it.

  **The Windows default has not moved.** Without that call a run still gets the
  Job Object, because the grant set is derived from the run's own facts and
  derived is not complete: a toolchain reading a machine-wide file outside it is
  refused, and a default boundary that cannot run an arbitrary payload is worse
  than one a caller reaches for deliberately. The default moves when the derived
  set has run a real cargo build, a real npm install and a real python payload on
  the CI image without a decline.

  **And it does not degrade.** Everywhere else in this crate an unavailable
  primitive falls back to a weaker rung and reports it. A boundary asked for by
  name that cannot be applied is an error naming the grant that failed, because a
  run that quietly took the Job Object instead would have had no boundary at all
  while every assertion about it still passed — which is how 0.47.0 twice read a
  green run as proof the container had run `cargo`.

- **`Backend::reaches_loopback_proxy`**, a third exhaustive predicate beside the
  two that already say what a backend delivers. Since 0.48.0 per-host egress is a
  loopback proxy the run owns, and that mechanism needs one thing from a backend
  that no backend had ever failed: that a contained command can make the
  connection at all. It is deliberately narrower than "does the proxy bind the
  payload" — on the portable floor the answer to that is no and the proxy is
  still reachable, and the agent's boundary line has to keep those two sentences
  apart.

### Changed

- **Under Windows access confinement, egress is a capability rather than a
  route.** A process inside an AppContainer cannot reach a loopback listener —
  measured on `windows-latest` with no capability, with `internetClient`, with
  `privateNetworkClientServer` and with both, four arms and one outcome, while
  the same request succeeds immediately outside the container and an outbound
  request to a real host succeeds inside it with `internetClient`. It cannot
  reach the host's own network address either. So a contained Windows command is
  given **no proxy at all** — pointing it at one it cannot reach would hang every
  request it makes instead of scoping it — the policy's per-host rules are not
  enforced there, and the agent's own boundary section says so rather than
  claiming the sentence it cannot back. The record is
  `US-IO-HARNESS-0.59.0-I03`.

- Both platform tables, the README, `docs/CONTRACT.md`, the sandbox, browser and
  command-execution guides and the crate's own module documentation now describe
  a Windows row that has two backends, and say which one a run gets.

### Deprecated

### Removed

### Fixed

- **The capability array never reached the child, so the network boundary this
  module documents in both directions was only ever half applied.**
  `Profile::create` built the array and `Spawned::start` passed
  `CapabilityCount: 0` on every spawn, so `internetClient` was registered against
  the profile name and absent from every token. The denying direction was right
  for a reason that had nothing to do with the capability set and the permitting
  direction had never once been applied. It was invisible because nothing
  selected the backend and because the only network test asserted a denial, which
  an ignored array and an empty one produce alike.

- **A capability SID must be aligned.** The buffer holding it was a bare
  `[u8; 68]`, which promises one-byte alignment; a `SID`'s sub-authority array is
  `u32`. `CreateProcessW` handed a misaligned one answers `ERROR_NOACCESS` —
  "Invalid access to memory location" — which reads like a bad pointer rather
  than a bad offset.

- **A grant to one container was read as a grant to another.** The memo that
  stops a path being granted twice in one process was keyed by path, access and
  reach but not by the container SID, so a second profile was told its grant was
  already done and read back carrying no ACE of its own. Same shape as the
  `GENERIC_ALL` defect 0.47.0 fixed: a grant that reports success and grants
  nothing.

- **A Windows host with Chrome installed answered "no browser was found".** The
  well-known install locations were cfg'd macOS-or-everything-else, so a Windows
  build carried `/usr/bin/chromium` — and nothing in `RESOLUTION_ORDER` is on
  `PATH` there, because a Windows browser is installed under Program Files and
  put on the Start menu. Machine-wide locations for Chrome, Chromium and Edge,
  and the per-user install under `%LOCALAPPDATA%` after them.

- **A deterministic container profile name is a shared object.** The profile is
  deleted on `Drop`, so two processes containing a command at once — two agents,
  or a runner that gives each test its own process — resolved the same name and
  whichever finished first deleted the container the others were still spawning
  into. Dormant while nothing selected this backend; a race the moment something
  did. The name stays deterministic — one name means one SID means every process
  writes the identical ACE, and per-process names instead make concurrent grants
  lose each other's entry on the DACLs they share — and it is no longer deleted
  on drop, at the cost of one profile left on a machine that has ever run a
  contained command. Two of them, keyed by the capability set, because a profile
  registers its capabilities at creation. And every decline path records its
  reason: three did not, so the one failure this release was least equipped to
  explain was the one that happened.

- **A test that could have passed against an open network.** The container's
  egress probe wrote its body to `NUL`, which a container cannot open, so `curl`
  exited 23 — "failed writing output" — whether or not the request succeeded,
  and the denial arm asserted only a non-zero exit. Both arms now assert the
  status the transfer actually produced.

### Security

- A run that asks for access confinement on Windows and cannot be given it is
  refused rather than run unconfined. The failure this replaces is the one this
  platform's history is made of: a decline was silent, the Job Object ran the
  command with no access boundary, and every assertion about that command still
  passed.

## [0.58.0] - 2026-08-15

### Added

- **A store can say what it is holding.** `Store::store_size` reports the file's
  own arithmetic — `page_size × page_count`, the bytes already free inside it, the
  counts of sessions and runs, and a per-table breakdown from `dbstat`, which is
  where a store that grew unexpectedly tells you which table grew.
  `Store::session_size` answers the same question for one session: its turns, the
  runs in its tree, the rows those runs hang off, and the summed `length()` of
  their text and blob columns. **That last figure is content bytes and not pages
  on disk**, deliberately: `dbstat` attributes a page to a table, and a page holds
  rows belonging to any number of sessions, so a per-session page count has no way
  to be right. An id the store does not hold is `None`, not a zero — asking the
  size of nothing has no answer.
- **`Store::delete_session` removes a session whole.** Its turns, the runs those
  turns drove, every run those runs spawned transitively, and every row across the
  schema keyed to that run set — in one transaction, reporting the sessions,
  turns, runs, rows, content bytes and restore points that went. The unit is a
  session and not a turn, because a turn's run may have spawned children and a
  half-removed tree is rows nothing can reach. **A `memory` entry is never taken**:
  a note outlives the run that wrote it, which is what 0.56.0's scope above the
  workspace made explicit.
- **`Store::sweep_sessions` applies that to a date.** Every session whose
  `created_at` is strictly before the cutoff, in one pass over the schema whatever
  the sweep's size. A session holding any run that is `Running` or `Paused` is
  **refused rather than deleted** — left byte-identical, with its id in
  `Pruned::refused` — because a date is a policy applied to sessions nobody looked
  at, and a crash-resumable tree that vanished for being old is the worst thing
  this release could ship. `delete_session` carries no such refusal: naming one id
  is somebody's decision about that session. The cutoff is a string because
  `sessions.created_at` is a `strftime` text column and a string comparison is what
  the storage performs; the guide shows how to build one.
- **`Store::archive_session` keeps every row and empties every word.** The counts,
  timings, tokens, cost, file paths, line counts, verdicts and statuses all
  survive; the prompts, replies, tool results, summaries, snapshot contents and
  edit hunks do not — so what a session cost and what it touched stay answerable
  after what was said in it is gone. It is **not** confined to the conversation
  table and must not be: `provider_calls` is the only pure accounting table in this
  schema, the user's own words are in `steps.prompt`, every tool result in
  `ledger_observations.text` and whole file contents in `snapshots.before`, and an
  archive that emptied only `session_turns` would report a removal it had not
  performed. Idempotent, and honest the second time.
- **`Store::compact` returns the space.** SQLite frees pages *into* the file
  rather than out of it, so a prune leaves the file the size it was and raises
  `StoreSize::free_bytes`. `compact` runs `VACUUM` and returns the bytes the file
  shrank by. It is a separate call because it rewrites the whole database, needs
  free disk of roughly the file's own size while it runs, and cannot run inside a
  transaction. `PRAGMA incremental_vacuum` is not an alternative — every store this
  crate has ever created was created without `auto_vacuum`, so it does nothing on
  any existing file.
- **A retention guide**, [docs/guide/retention.md](docs/guide/retention.md), and a
  retention section in [docs/CONTRACT.md](docs/CONTRACT.md).

### Changed

- **An archived restore point says so rather than restoring nothing.** A
  snapshot's content is words, so archiving empties it; the row stays and records
  that it was archived, and a restore reaching it reports the existing
  `Reverted::Stale` naming the archive. Restoring it naively would write an empty
  file over a real one, which is the one way this release could destroy something
  outside the database. No new variant and no changed signature.
- **Pruning a session shifts memory eviction, and this is stated rather than
  discovered.** No `memory` entry is removed, but `memory_recalls` rows for removed
  runs are, since they name a run that no longer exists — and 0.56.0's eviction
  ranks candidates by `COUNT(DISTINCT run_id)` over that table. A note that had
  earned its place mostly through runs you have now pruned therefore ranks lower,
  and a later write may evict it. Evidence from a run that no longer exists is not
  evidence; `Store::memory_pin` is what holds a note regardless.

### Migrations

- **One additive index.** `session_turns_session` on `session_turns (session_id)`,
  created on the first open by a 0.58.0 binary. `CHECKPOINT_FORMAT` does not move,
  no table or column is added, and a 0.57.0 binary reads the same database
  afterwards unchanged. An operator does nothing.
- **One new value in an existing column.** `snapshots.state` records an archived
  snapshot. A 0.57.0 binary reads it as an unknown state.
- **Nothing expires on its own.** Every call above must be made by name, and none
  is reachable by a model. A program upgraded to 0.58.0 and left alone behaves
  exactly as it did on 0.57.0. What cannot be rolled back is a deletion that
  already happened: this crate has no undo for one, and an operator whose recovery
  position matters copies the file first.

## [0.57.0] - 2026-08-15

### Added

- **A `remember` that restates a note you already hold says so.** Writing by key
  means the same fact learned twice under two names leaves two entries that
  disagree, both carried into the next turn, and the model acting on whichever it
  read last. A write whose text closely overlaps an entry already stored **in the
  same scope under a different key** now comes back naming that key and quoting
  what it holds, so the model resolves it in the same turn — replace one, or
  `forget` the other — instead of leaving the contradiction for a later run. The
  write still lands: this is a report, not a refusal. Rewriting the same key is
  not reported, because that is the replacement writing by key has always meant,
  and a workspace note restating a **global** one is not reported either, because
  that is the override the second scope exists for. The comparison is a
  normalised word overlap computed in-process — no embedding, no model call,
  nothing over a network.
- **`Store::memory_similar`.** The same answer for a caller: the entry in a
  workspace that a value most restates, under a different key, or `None`.

### Changed

- **Which notes a turn carries is decided by what the turn is about.** Until now
  the memory block kept the **newest** notes that fit its quarter-share of the
  turn, which is the right answer only while the whole store fits that share —
  and 0.56.0 is the release that let you raise the caps past it. When the store
  does not fit, the notes that survive are now ranked by how many words the entry
  shares with what this turn is about (the run's goal, and every path a tool has
  already named in this run), then by how many **separate runs** have carried the
  entry, then by the order the store returned. An entry with no signal and no
  evidence keeps exactly the position it had, so a turn about nothing the store
  knows behaves as it did before.
- **The block is still printed in the store's own order**, never in the order the
  selection chose, and this is deliberate. The memory block is a byte-prefix of
  the user turn, and the second prompt-cache breakpoint is withheld unless that
  prefix repeats byte-identically — so reordering the print would have turned
  cache reads into cache writes on every wire that takes the marker, for a
  reordering the model gains nothing from. A store that fits its share assembles
  a byte-identical prompt however the turn moves.
- **`Store::memory_list` breaks ties on the key rather than the row id.** Still
  oldest first; now a **total** order, which is what lets the block be printed
  back in the order the store returned it after selection has reordered it.
- **The block's elision line no longer calls the dropped notes "older".** It
  reads `(N note(s) elided to fit — Store::memory_list has all of them)`. They
  are dropped for being about something else, and the previous wording described
  a policy this release replaces.

## [0.56.0] - 2026-08-15

### Added

- **A `forget` tool.** A run can withdraw a note it learned was wrong. Writing a
  key again only replaces it, so an agent that learned the same wrong thing under
  two names previously had two disagreeing notes and no way to take either back.
  A note the operator pinned is refused, exactly as a write to one is; a key that
  was never there is reported as absent rather than as a removal; and nothing is
  withdrawn while a plan is unapproved, which is where `remember` is refused for
  the same reason. `Store::memory_forget` is the typed form and returns
  `MemoryForget::{Removed, Pinned, Absent}`.
- **A memory scope above the workspace.** `remember` and `forget` take
  `scope`, `"workspace"` (the default) or `"global"`. A global note is recalled
  by every run over every workspace — for a fact true wherever you run, such as
  the package manager an operator uses. `GLOBAL_MEMORY_WORKSPACE` is the key it
  is stored under, and it is an ordinary bucket: its own caps, its own pins, its
  own eviction. **A workspace's own note of the same key wins**, and the global
  one is not rendered beside it. The memory block renders the two scopes under
  separate headings, so a note kept for every workspace is never presented as
  something learned about this one.
- **`[memory]` in `io.toml`, and `TaskContract::with_memory_limits`.** The three
  caps — `max_entries` (64), `max_chars` (16,000) and `max_entry_chars` (2,000) —
  are now an operator's numbers rather than the crate's constants. The defaults
  are those constants, so a caller who sets nothing keeps today's behaviour. Each
  key applies on its own, and all three may only be **lowered** by a
  project-scoped file. `MemoryLimits` carries them; `Store::memory_write_with` is
  `memory_write` under caller-chosen caps.

### Changed

- **Eviction is ordered by evidence rather than by the write clock.** At a cap
  the store used to drop the oldest entry, which meant the build command learned
  on the first run and carried by every run since was the first thing to go while
  this morning's triviality survived. Candidates are now ordered by how many
  **distinct runs** carried the entry, then by how recently one did, then by the
  write clock. That last term is 0.10.0's order kept as the tie-break, so an entry
  with no recalls yet is treated exactly as it was before. The entry just written
  is still never a candidate and a pinned entry still never is.

  The count is of runs, not of recall rows: a row is written once per carried key
  per *step*, so rows measure steps elapsed since the write — one long run would
  outvote fifty short ones, and the order would be monotone in age again. A recall
  means the entry was carried into a prompt, which is the strongest signal this
  crate can observe; nothing claims the model read it.

  **What this changes for you:** a store already at its cap will, on its next
  write, drop a different key than 0.55.0 would have. Which keys are dropped is
  returned by the write and recorded in the trace, as it always was.
- **A rewind can undo a removal, not only an overwrite.** `Store::memory_restore`
  was an `UPDATE`, which put back an entry a run had edited and silently did
  nothing for one a run had removed. It is now an upsert, so `rewind_run` puts
  back what a `forget` took. Restoring an edited entry is byte-for-byte what it
  was.

### Fixed

- **A memory character cap larger than an `i64` no longer empties the
  workspace.** The cap was compared as `limits.max_chars as i64`, which is exact
  for this crate's own 16,000 and wraps *negative* for a large number — at which
  point the eviction loop's exit condition can never hold and a single write
  drops every entry but the one it just wrote. Reachable only once the caps
  became settable, which is this release, and found by running the measurement
  rather than by reading the code. The comparison is `u128` now.

### Migrations

- **One additive index.** `memory_recalls_entry` on `memory_recalls (workspace,
  key)`, created on the first open by a 0.56.0 binary. `CHECKPOINT_FORMAT` does
  not move, no table or column is added, and a 0.55.0 binary reads the same
  database afterwards unchanged. An operator does nothing.

## [0.55.0] - 2026-08-14

### Added

- **`read_file` has a type.** A UTF-8 file reads as it always did. A UTF-16 file
  with a byte-order mark is decoded and the encoding is named in the observation.
  An image is named as an image and routed to `view_image`; a known document is
  named and routed to its own tool; anything else is named as binary with its
  size and what its leading bytes look like. `Workspace::read_typed` returns that
  classification as `FileContent`, and `FileContent::refusal` is the sentence the
  model is shown.
- **`read_file` takes `offset` and `limit`, in lines.** `offset` is 1-based and
  the observation header states the range and the file's total line count, so a
  slice the model asked for reads as a slice rather than as a whole file. An
  `offset` past the end is an error naming the total, not an empty success.
- **`[run] max_read_chars`, and `TaskContract::with_max_read_chars`.** The
  largest file a single read may carry, in characters. Unset, the ceiling stays
  the one derived from the context budget, which is what every earlier version
  did — and which moves during a run, because it is a share of what is left. A
  read is refused when it exceeds either ceiling and the refusal says which: they
  call for different answers. A project-scoped `io.toml` may lower this key and
  may not raise it.
- **`Media::attach`, a wide door in front of a narrow wire.** The four types in
  `IMAGE_MEDIA_TYPES` pass through byte-identically; BMP, TIFF, ICO, TGA and PNM
  are decoded and re-encoded to PNG. `view_image` accepts everything the door
  accepts, and its observation says when bytes were converted. `Media::image` is
  unchanged: the four-type set is a fact about vendors, and it stays the wire.
- **`Media::source_type_for`**, the extension table for every image format the
  crate recognises — including the three it can only name — beside
  `Media::media_type_for`, which still answers the narrower "may this go on the
  wire".
- **`MAX_IMAGE_PIXELS`.** A decompression bomb is refused from its header before
  the decode it would otherwise pay for: a two-kilobyte TIFF header can declare a
  forty-thousand-square canvas.

### Changed

- **A read too large to fit is refused, not shortened.** It used to be cut to the
  per-observation cap with a marker in the text. What the model then held had the
  shape of a whole file, and nothing downstream could tell a whole file from the
  tail of one. It now returns **no content at all** — an error naming the path,
  the size, the ceiling, which ceiling, and both ways to proceed. **If your run
  depended on getting the tail of a large file, ask for a range.** Bounding is
  unchanged for every other kind: a command's output and a search's matches were
  never documents, and a prefix of one is not a lie.
- **A stored read is whole in the prompt or a stub, never a fragment.** A read
  that fitted when it happened and is later squeezed by a narrower budget share
  is replaced by a stub naming the file and the range to re-read. The same rule
  now covers the re-read of a file a write invalidated, which used to be bounded
  to its tail under a header saying the file had been re-read.
- **An image `view_image` cannot decode is refused by name.** The message used to
  be the vendors' four-type list, which is a true statement about three APIs and
  reads, at the doorstep, as this crate being unable to open a photograph. SVG,
  HEIC and AVIF are now named, with the reason and a one-line conversion.
- **The `media` feature compiles `image`.** It was already an optional dependency
  of this crate, reached through `barcode`; it now carries the extra formats and
  sits under `media`, which `browser` implies. Nothing is added to the default
  build and `cargo tree` on it is unchanged. A `--features media` build compiles
  more, all of it pure Rust — no C library and no `-sys` crate.

### Fixed

- **A binary file no longer reads as an empty file.** `Workspace::read_file` was
  `std::fs::read_to_string(path).unwrap_or_default()`, so an executable, an image
  or a UTF-16 log arrived as `Ok("")` — the same answer a file that does not
  exist gives, and a model told a file is empty writes over it. It is now an
  error naming the file, what it is and how big it is. **A caller that treated
  the empty string as "empty file" now sees the error it should have seen.** The
  one case where nothing really is the answer is untouched: a missing file still
  reads as empty, which is what lets an agent create one.

## [0.54.0] - 2026-08-14

### Added

- **A read starts before the model has finished speaking.** When a provider
  reports a finished tool call while its completion is still streaming, the
  harness starts the read-only ones then rather than waiting for the rest of the
  message. On a model that narrates its plan before it stops, that is most of the
  message. 0.41.0 already ran read-only calls concurrently with each other; this
  moves when the first of them begins, and moves nothing else — the same
  `ReadWork` runs the same way under the same `TaskContract::max_parallel_reads`.
- **`Provider::complete_streaming_calls`, a third defaulted method.** It takes the
  existing text sink plus one for finished tool calls, carrying a call's position
  in the completion and the call itself. **Its default reports no call at all**, so
  every implementation written before 0.54.0 compiles and behaves exactly as it
  did — including `Record` and `Replay`, which override neither streaming method,
  so a recorded or replayed run starts nothing early by construction rather than
  by anyone remembering to suppress it. The four built-in providers and `Fallback`
  override it.
- **`EventKind::Speculated { started, used, discarded }`**, once per step that
  started something early. 0.41.0 deliberately emitted nothing, because
  overlapping two reads costs nothing when it does not help; starting a read
  before the model has finished asking for it costs a whole read when the
  completion turns out not to want it. `discarded` is the number that makes that
  trade visible — a provider that streams its calls late, a model that revises its
  arguments, or a step whose completion had to be retried all show up here and
  nowhere else. A step that started nothing emits nothing.

### Changed

- **Nothing an embedder can observe, and that is the guarantee rather than an
  omission.** The same `EventKind::ToolCall` events in the same order, the same
  observations in the same ordinals, the same steps, the same `PolicyEvent` rows
  and the same ledger draws, whether a read started early or not. Every durable
  and observable act still happens serially, in the order the model asked, after
  the completion settled; only the work moved.
- `docs/CONTRACT.md` gains the rules that bound it, and its 0.20.0 statement that
  a streamed delta is not safe to act on now says what the harness itself does
  with one and why a consumer's rule is unchanged.

### Notes

Three rules bound what may start early, and each is a refusal to *speculate*
rather than a refusal to run — every call still runs, in order, exactly as it did
on 0.53.0:

- **Only the completion's leading run of read-only calls**, which is narrower than
  the maximal run 0.41.0 batches. A read started after a write that has not run
  yet would answer from before the write.
- **Only what the `Policy` allows outright.** A grey-tier call is never started
  early, so no approver is asked about a completion that may never settle, and a
  run with a tool hook configured starts nothing early at all.
- **A result is used only if the settled completion asks for that same call**, with
  the same name and byte-identical arguments, at that same position. A failed
  attempt, a retry and a `Fallback` fallover all reduce to that one rule: the
  settled completion is a different completion, so nothing speculated against the
  abandoned one matches it, and a discarded speculation leaves no observation, no
  row and no ledger draw behind.
- **A registered tool needing more containment than the run grants is never
  started early**, matching the refusal `dispatch` already makes before the arm
  that would run it. That refusal says nothing was started, and speculation must
  not make it a lie.

`with_max_parallel_reads(1)` turns starting early off along with the batching, so
there is one switch rather than two. `run_with` and the other one-shot entry
points do not stream and are unchanged, as is the tree loop that drives child
agents, which never took 0.41.0's batch path either.

## [0.53.0] - 2026-08-13

### Added

- **An agent can drive a real browser and see what it did.** Six tools —
  `browser_navigate`, `browser_read`, `browser_screenshot`, `browser_click`,
  `browser_type` and `browser_scroll` — behind the new `browser` cargo feature,
  which is not in `default`. Through 0.52.0 a run could read files and run
  commands; a page whose content is assembled by script was, to this crate, the
  handful of bytes the server sent before any of it ran. `browser_read` returns
  the text the page actually renders, and `browser_screenshot` puts a picture of
  it in front of the model — which is different evidence, not a nicer form of the
  same evidence: text says a heading exists, a screenshot says it is white on
  white, off-screen, or under a dialog.
- **Every document navigation is an `Act::Net` check against its `host:port`,
  decided at the paused request rather than at the URL a tool was handed.** That
  is what makes the boundary hold for a navigation the model never typed — a click
  on a link, a redirect, a script assigning `location` — and it is the difference
  between this and a browser wrapper. Each decision is one
  `EventKind::BrowserNavigated { host, permitted }`, so a trace records every place
  the browser went **and every place it was stopped from going**.
- **The browser is driven over a pipe on the child's own descriptors, and no
  debugging port is ever opened.** A remote debugging port is a TCP listener any
  other local process can connect to and drive with full control of the browser,
  including reading whatever the page can read. Avoiding it also costs nothing:
  NUL-framed JSON over two descriptors needs no websocket client, no TLS to
  localhost and no protocol crate, so **this release adds no dependency** and
  `cargo tree` does not move.
- **Console output and uncaught page errors ride the observation of the action
  that produced them**, and add no tool of their own. A run that clicks a button
  and gets a page that looks unchanged has learned nothing; the same run reading
  `Uncaught TypeError` from that click has learned the whole answer. An action that
  produced neither says so rather than omitting the section.
- **`BrowserConfig` and the `[browser]` table** — binary, extra arguments,
  headless, viewport and per-action timeout. Nothing is ever downloaded: the
  browser is one already installed, named outright or resolved from a documented
  ordered list of executable names, and its absence is a refusal naming what was
  looked for. A configured binary that is missing does **not** fall back to the
  list — falling back would drive a browser other than the one asked for. Refused
  in a project-scoped `io.toml` for the reason `[[hook]]` is: it names a program to
  execute, and that file arrives with a `git clone`.
- **`EventKind::BrowserStarted { binary, headless, ready_ms }`**, once per run,
  naming the binary that was actually resolved rather than the one that was asked
  for. The browser starts lazily, so a run that configures one and never browses
  starts no process at all.

### Changed

- A run that does not enable the `browser` feature, or enables it and configures
  no browser, is byte-identical to 0.52.0 in composed prompt and in trace. No tool
  schema, no process, no event.

### Security

- A page a run's policy does not permit is not reached. The check is enforced at
  the browser, on the paused request, rather than stated before one — so a
  navigation caused by page content is decided by the same rule as one caused by
  a tool call, and a refusal is recorded either way.
- The browser runs against a temporary profile the run owns and removes, so the
  operator's own cookies, extensions, history and logged-in sessions are not
  visible to it.
- No debugging port is opened, so the browser this crate drives cannot be driven
  by anything else on the machine.

### Fixed

- Nothing. This release adds a capability and changes no existing behaviour.

## [0.52.0] - 2026-08-13

### Added

- **The agent navigates a codebase the way an editor does.** Five tools —
  `lsp_definition`, `lsp_references`, `lsp_symbols`, `lsp_hover` and `lsp_rename`
  — answered by a language server named in `io.toml` or on the contract with
  `TaskContract::with_lsp`. Through 0.51.0 the only way to ask "where is `Ledger`
  defined" was to grep the spellings a definition might have, read the files that
  matched, and work out which hit was the definition; "who calls `measure`" meant
  grepping the identifier and reading every hit to discard the comment, the string
  literal and the identically-named method on another type. Each of those is a
  provider round trip carrying the whole system prefix, and the answer at the end
  is a text match that resembles a resolution. Measured over one such question:
  three provider calls and 6,052 prompt bytes against six and 11,901.
- **`LspServer` and the `[[lsp]]` table**, one entry per server: `id`, `command`,
  `args`, `env`, `extensions` and `timeout_secs`. Nothing is downloaded, guessed,
  or resolved from `PATH` by ecosystem — a server is named or there is no server,
  and a configured server that is not installed is a refusal naming it. Allowed in
  a project-scoped `io.toml` for the reason `[[mcp]]` is: the boundary is the
  `Act::Exec` check on the named binary, not the scope of the file that named it.
  Unlike `[[mcp]]`, a misspelled key inside an `[[lsp]]` table is rejected by name.
- **`lsp_symbols` is one tool with two behaviours** — no `query` is this file's
  symbols, a `query` searches the workspace — because two schemas for one question
  is prompt bytes on every request of every run.
- **`lsp_rename` writes nothing.** The server resolves the rename across the
  workspace and the tool answers with a **patch series** in `patch_file`'s own
  format, which you apply per file: one `Act::Write` check per path,
  all-or-nothing per file. A tool that wrote N files on a server's say-so would be
  the multi-file write 0.51.0 excluded, with the additional property that this
  crate did not compute the change.
- **A server's diagnostics are appended to the project's own checker**, in `check`
  and in the automatic post-edit note — never in place of it, because a language
  server's analysis omits borrow-check errors, monomorphisation errors and every
  lint, which are the errors a model writes. Pull only (`textDocument/diagnostic`,
  `workspace/diagnostic`): push diagnostics have no completion signal, so an empty
  result is indistinguishable from a slow one. A server that does not advertise the
  capability says so rather than reporting nothing.
- **`EventKind::LspStarted`**, once per configured server per run, carrying the
  root it was told to index and how long its handshake took.
- **`Error::Lsp`**, additive on an enum `#[non_exhaustive]` since 0.43.0.

### Changed

- **Every location a server returns passes the same `Act::Read` check `read_file`
  passes**, and a location the policy denies reading is dropped from the answer
  **with the omission counted in it**. A quietly shorter list is a wrong answer to
  "who calls this": the model reads two call sites where there are three and
  concludes it has seen them all. Only an outright `deny_read` omits — `Ask` does
  not, because naming a path is not reading its contents. Stated plainly: the
  server process itself has still indexed those files on disk. This crate can
  refuse to carry those bytes into the model's context; it cannot stop a server
  reading a directory it was pointed at.
- **An empty answer from a server that has not finished starting up is not
  believed.** The protocol has no readiness signal and a busy server answers `[]`
  rather than erroring, so an empty result from a server not yet observed warm is
  retried once its announced work settles, bounded by that server's own
  `timeout_secs`. A server that announces no work at all is asked once more and
  then believed.

### Deprecated

### Removed

### Fixed

### Security

- **A language server is spawned only after an `Act::Exec` check on its program**,
  through the same gate an `[[mcp]]` stdio server passes, and a refusal happens
  **before** any spawn is attempted. A denied server ends the run with
  `Error::Refused` rather than being skipped: silently navigating by text search
  while the operator believes a language server is answering is the worse failure,
  because the run looks successful.

## [0.51.0] - 2026-08-12

### Added

- **A change is kept, not just counted.** Every `write_file`, `edit_file` and
  `patch_file` now records the change as a unified diff of the whole file, in a
  new nullable `edits.hunk` column read back through `Edit::hunk`. Through 0.50.0
  a trace could say that step 7 added four lines to `src/parse.rs` and could not
  say which: the two texts were compared, the lines counted, and both texts thrown
  away. An operator reviewing an unattended run had to reconstruct the change from
  the restore point, which exists per file per *run* and so cannot answer "what did
  step 7 do" for a file the run wrote five times.
- **`Store::patch`** renders a run's whole change as a step-ordered patch series —
  one `--- a/path` / `+++ b/path` header pair per edit, in the order the run made
  them. A series and not one diff, deliberately: two edits to the same file take
  their line numbers from that file as it stood at each of them, so it applies as a
  sequence the way a multi-commit diff does. An edit with no stored hunk contributes
  a comment line saying so rather than being silently omitted.
- **`patch_file`**, a third write tool taking a unified diff for one file. A change
  touching four places in a file was four `edit_file` calls — four gate evaluations,
  four checker runs and four round trips, with the file's line numbers moving under
  the text the model read after the first of them. It is **all or nothing**: every
  hunk is matched against the file at its own position, against the original, before
  anything is written, so a patch whose third hunk does not fit leaves the file
  byte-identical and says which hunk and what it expected. One path per call, and it
  cannot create a file — that is still `write_file`.
- **`check`**, the project's own type-check as a tool the agent may call *before* it
  writes. The same ecosystem checker that has run automatically after every
  successful write, asked as a question instead of received as a note. It takes no
  arguments, so what runs is the detected command and not the model's guess. It is
  an `Act::Exec` check on that command — the program *and* the whole argv, exactly
  as `exec` is — because a model-callable path to the project's build command must
  be refusable by the policy that refuses `exec`; the automatic post-edit check stays
  ungated, being the crate's own reflex after a write the policy already allowed.
  When there is no checker for this ecosystem the tool says so, where the automatic
  path stays silent: an empty answer to a direct question reads as "your project is
  clean".
- **`rewind_step`** and **`rewind_step_observed`** undo one step by reverse-applying
  its stored hunks, returning one `Reverted` per path that step wrote. `rewind` puts
  a file back to before the run's *first* write to it and `rewind_run` does that for
  a whole run; neither could undo step eighteen of twenty, so a run that did nineteen
  right things and one wrong one had to be thrown away whole.
- **`Reverted`**, with three variants because "it did not happen" has two causes an
  operator must tell apart. `Stale` means the file has moved on — revert the later
  steps first and it applies. `NoHunk` means there is nothing to undo with, and
  never will be. Both leave the file untouched. **Reverse-application is
  order-sensitive: walk a run back newest step first.**
- **`rewinds.undid_step`** and **`RewindRecord::undid_step`**, so `Store::rewinds`
  can tell a step revert from a whole-run rewind instead of reporting both as
  "something was undone". `None` for a rewind.
- **`EventKind::Reverted`**, additive on an enum `#[non_exhaustive]` since 0.24.0.

### Changed

- **BREAKING: `Edit` gains `hunk: Option<String>` and `RewindRecord` gains
  `undid_step: Option<u32>`.** Both types are produced by the store and read by a
  caller, and both derive `Default`, so `..Default::default()` keeps working and
  only an exhaustive struct literal outside the crate breaks. *Migration:* add
  `..Default::default()`, or name the new field. No trait method changed, so every
  `impl Provider`, `impl Tool`, `impl Reviewer` and `impl Approver` compiles
  unchanged.
- **The line counts did not move, and that is the deliberate half.** An
  `edit_file`'s `lines_added` and `lines_removed` still measure the fragment it
  replaced rather than the file, which is what they have measured since 0.18.0. The
  two answers genuinely differ when a replacement does not begin and end on a line
  boundary — deleting a substring *inside* a line is nothing added and one line
  removed over the fragment, and one and one over the file. Computing both from the
  same texts would have been tidier and would have silently renumbered every trace
  ever recorded.
- **The tool catalogue grows by two schemas.** A run that calls neither new tool
  behaves identically; `write_file` and `edit_file` take the same arguments, produce
  the same observations and record the same counts.

### Deprecated

### Removed

### Fixed

### Security

- **The new `check` tool cannot be used to run a build command the policy
  refuses.** It is gated on the resolved argv before anything is spawned, and a
  refused call spawns no process at all.

## [0.50.0] - 2026-08-12

### Added

- **A parent chooses how a child comes back.** `spawn_agent` takes two optional
  arguments. `"wait": false` detaches the child: the parent takes its next step
  immediately and the child's report reaches it at a later one.
  `"background_after_secs"` waits, and stops waiting when the clock runs out. In
  both cases the child keeps running — it is not cancelled, its work still lands,
  and the tree does not return while it is going. Naming neither argument is the
  spawn every existing caller writes today, unchanged: the parent waits, results
  fold into the step that asked for them, and the trace is reproducible.
- **A child reports what it concluded.** Through 0.49.0 a finished child was
  composed into its parent's log as `[child 7 "goal" -> Success { steps: 4 }]` —
  an outcome discriminant and a step count, and nothing it found, because
  `RunOutcome::Success` carries no text. A parent that fanned out to investigate
  four subsystems learned that four runs succeeded and none of their findings, and
  the only way a finding could travel was a file the parent then read. A child's
  composed result now carries the text of its last completion beside its steps and
  its tokens, bounded by the same per-observation cap as everything else.
- **An agent's own words are durable.** `AgentEvent::said` records what an agent
  said on each step, in the table the tree already writes to. `steps.result` holds
  the observations a step produced, and a completion's prose reached the ledger
  only when it carried no tool call at all — so an agent that wrote a file and
  explained why left the explanation nowhere. This is what a parent reads back as
  its child's conclusion, including for a child a later process adopts.
- **`AgentEvent::spawn_args`**, which records a spawn's own arguments so a
  detached child can be resumed after a restart from the call that made it rather
  than rebuilt from the five of nine arguments the `spawns` row keeps.
- **`TaskContract::with_spawn_background_after`** applies a wall clock to any
  child spawned without one, and **`TaskContract::without_detached_spawns`**
  refuses to let a child outlive the step that spawned it at all. Both narrow and
  never widen: a spawn asking for a longer clock gets the contract's, and a
  refused detachment becomes an ordinary blocking spawn with a line in the
  parent's log saying so.
- **`EventKind::ChildDetached`** and **`EventKind::ChildCollected`**, so an
  observer can render a fan-out that is no longer synchronous. Both are additive
  on an enum that has been `#[non_exhaustive]` since 0.24.0.

### Changed

- **BREAKING (behaviour): a child's composed result carries its conclusion.** The
  line a parent reads changed from `[child 7 "goal" -> Success { steps: 4 }]` to
  the same opening plus the child's step and token counts and what it said. There
  is no switch: a knob that reported the old shape would be a way to ask for this
  release and quietly not get it. *Migration:* an embedder asserting on the exact
  text of that observation matches the prefix `[child <id> "<goal>" ->` instead of
  the whole line. Nothing about the outcome, the run id or the goal moved.
- **A detached or backgrounded spawn gives up step-for-step trace
  reproducibility** for the calls that use it: which step a report lands on depends
  on how long the child took. Reports still fold in the order the children were
  spawned rather than the order they finished, so two children racing leave the
  same ledger, and a run that detaches nothing is byte-identical to one on 0.49.0.
  `TaskContract::without_detached_spawns` refuses detachment outright.
- A resumed parent takes back every child it detached and did not live to see
  finish, before its first step, resuming each from its own checkpoint through the
  ordinary spawn path rather than as a second child.

### Deprecated

### Removed

### Fixed

### Security

## [0.49.0] - 2026-08-11

### Added

- **A request can carry a conversation.** `CompletionRequest` gains
  `messages: Vec<Message>`: an ordered transcript of user turns, assistant turns
  carrying the calls the model made, and batches of results answering them. Each
  built-in wire maps it onto that vendor's own block types one to one — `tool_use`
  and `tool_result` on Anthropic, `tool_calls` and `role: "tool"` messages on the
  OpenAI wire — so a run's own history reaches the model in the shape every model
  this crate targets was post-trained on. Through 0.48.0 the request held one
  `system` string and one `user` string, and a step's results were re-rendered as
  bracketed prose inside the next user message: the crate parsed the protocol off
  a response and then discarded it on the way back in, leaving the model to read a
  third-person account of its own past actions. The failure that produced was not
  an error and left nothing in a log — restating plans, narrating intent instead
  of acting, losing that a tool had already been called.
- **`CompletionRequest::cache_through`**, the transcript half of 0.44.0's second
  cache breakpoint: how many leading messages the caller states are byte-stable.
  It marks the same content the byte offset marked, expressed where a real
  transcript has boundaries.
- **`SystemPrompt::Preset`**, opt-in **by name**. `Preset::Concise` acts first and
  reports briefly; `Preset::Careful` verifies its own work before reporting it and
  says what it checked. `SystemPrompt::Builtin` is still the default and is
  byte-identical to 0.48.0's. 0.45.0 declined to ship a preset catalogue on the
  grounds that a library must not install opinions into someone else's product;
  a preset nobody can reach without naming it installs nothing, and the builtin is
  asserted unchanged after one exists.
- **`Message`, `ToolResult` and `Preset`** are new public types.
  `context::Assembled` gains `emitted`, the piece-by-piece view of the same
  emission its flat `text` is built from.

### Changed

- **BREAKING: `CompletionRequest` gains two fields and `context::Assembled` gains
  one.** An exhaustive struct literal of either stops compiling.
  *Migration:* use `..Default::default()`, which is what this type's own
  documentation has advised since 0.15.0 and what every construction site in this
  repository already used. The same break `media` (0.15.0), `model` (0.21.0),
  `web` (0.22.0), `effort` (0.31.0) and `cache_boundary` (0.44.0) each were.
- **BREAKING (behaviour): what the crate's own loop sends changed.** A request
  built by this crate now carries a role-tagged conversation where it carried one
  user message of prose. A model's answers will differ — that is the release, not
  a side effect — and an embedder asserting on exact model output will see it.
  *Migration:* none is available or wanted; the previous shape is what the release
  exists to remove. A caller building its own `CompletionRequest` and leaving
  `messages` empty gets 0.48.0's body from every built-in wire, byte for byte.
- **`CompletionRequest::user` is derived and retained for one release.** The loop
  fills it with exactly the string it filled before, so a `Provider` that reads it
  keeps working unchanged and is honestly non-conversational. A built-in wire
  ignores it whenever `messages` is non-empty. It will be removed in a later
  version; read `messages` in new code.
- **`CompletionRequest::cache_boundary` applies to the derived `user` path only.**
  A request carrying a transcript marks it with `cache_through` instead.
- **A classifying session turn is no longer framed as a task.** Its system block
  opened "You are an agent working across a repository to meet a stated
  specification" and promised the whole set is checked against the success
  criterion after every step — of a turn carrying `Verification::None`, where
  nothing is checked. An operator who typed a greeting was being told they had
  written a specification. The tools, the workspace and 0.37.0's sentence about
  how a turn may end are unchanged. This is the same mismatch 0.48.0 fixed in the
  user block, one block higher.
- **A session's earlier turns arrive as their own messages.** The seed narrated
  them — "the operator asked: …", "you answered: …" — inside the one user message
  a request could carry. The attribution moved to the message's role, and the
  ledger entries read `[operator] …` and `[agent] …`. A program parsing the old
  wording out of a prompt will not find it.
- **A `Replay` cassette key no longer includes the transcript.** It is a rendering
  of content the key already covers, and including it would make every recording
  miss the moment the loop started sending one — and would break replay after a
  resume, since a resumed run rebuilds its ledger from stored text and carries no
  transcript. No recording needs re-recording. The two cache markers stay in the
  key, as 0.44.0 decided.

### Deprecated

### Removed

### Fixed

### Security

## [0.48.0] - 2026-08-11

### Added

- **A `shell_start` handle runs inside the boundary.** A backgrounded handle was
  the one execution path left at full privilege: an agent that could not write
  outside the workspace with `shell` could start the same line with `shell_start`
  and write wherever it liked, so the boundary depended on which tool the model
  picked. A handle now takes the same containment the foreground line takes, per
  stage, and its create / exec / destroy rows name the backend that applied.
- **The six git built-ins run contained**, under the mode they declare, and they
  write the same sandbox rows every other spawn writes. The three readers gain
  `--no-optional-locks`, which is git's own way of not taking an index lock it did
  not have to.
- **Each spawning tool declares the mode it needs, resolved before the spawn.** A
  call runs under the narrower of what it declares and what
  `TaskContract::exec_sandbox` granted — least privilege per call rather than per
  run — and a need the grant cannot satisfy is refused with **no process started**,
  naming both modes so the model reads a reason instead of decoding an errno.
  `Tool::exec_mode` is a new defaulted trait method; a toolbox written against any
  earlier release compiles unchanged. For a registered tool the declaration is a
  refusal mechanism and not a confinement one, and the crate says so: it does not
  see that tool's own spawn and does not claim to govern it.
- **Per-host egress under containment.** `Policy` has carried per-host `Act::Net`
  rules since 0.8.0 and a contained command could never be held to them, because a
  backend takes one boolean. A run whose policy names any host now routes its
  contained commands through a loopback proxy the run owns: the sandbox permits
  that address and nothing else, and the proxy asks the run's own policy about
  every `host:port` before it connects, refusing with the rule and layer named.
  What that proves differs per backend and `docs/CONTRACT.md` carries the table —
  address-scoped on macOS, **port-scoped** under Landlock, and **advisory** on the
  portable floor and on Windows, in that word.
- **`EventKind::Dialed { host, port, allowed }`**, one per outbound connection,
  with `dialed` joining the names an operator may filter on in a `[[hook]]`.
- `ExecMode::narrower` and `ExecMode::satisfied_by`; `RunSpec::proxy` and
  `RunSpec::with_proxy`.

### Changed

- **A run whose policy names hosts reaches those hosts and no others.** Before
  this release such a run's contained commands could reach everything. A command
  that ignores `HTTP_PROXY` now reaches nothing rather than everything, which is
  stricter in the direction the policy already asked for. A run with no `Act::Net`
  rules, or one whose default permits the network, is unaffected and starts no
  proxy.
- **A backgrounded handle and a git built-in are now confined.** A program relying
  on either writing outside the workspace — and getting away with it because the
  boundary did not apply, not because it was permitted — sees that refused.
  *Migration:* `TaskContract::with_full_access()` restores an uncontained run at
  the construction site, and `SandboxConfig::floor_only()` keeps containment while
  taking the portable floor, which for the newly contained paths is 0.47.0's
  behaviour exactly.
- **A Linux run whose policy names hosts prefers a rung that can reach its proxy.**
  The namespace rungs put the child in an empty network namespace where the host's
  loopback is unreachable, so they cannot serve such a run at all; where no rung
  can, the run takes the boolean and reports the backend that applied.
- `sandbox_events` rows of kind `create` now carry the mode that call resolved to
  in `detail`, because the mode is a per-call fact once a reader declares less than
  the run grants. No schema change: `kind` and `detail` are text columns.

### Fixed

- **A classifying turn is asked one question instead of two that disagree.**
  0.37.0 gave a conversational turn its own system prompt — "if a plain answer is
  the whole of what is wanted, write that answer and call no tool" — and left the
  user block unconditional, so the same completion also carried "(nothing yet —
  start by grepping or finding)" and "Call a tool to make progress toward the
  success criterion." A model handed both resolved the contradiction in its reply.
  The user block is now chosen by the condition that already chooses the system
  half and carries the operator's words and the conversation and nothing else.
  Every later step of a promoted turn is byte-for-byte what it was.

## [0.47.0] - 2026-08-09

### Added

- **Linux containment is a chain, and its first rung needs no namespace.**
  `Backend` gains `LinuxLandlock` and `LinuxBubblewrap`, and a contained Linux run
  now takes the strongest rung the host can actually deliver: Landlock first, a
  `bwrap` helper where the host has a working one, the existing `unshare` backend
  after that, and the portable floor last. **A run that denies egress is never
  given a rung that cannot deny egress** — the one rule that can send a host below
  its strongest available primitive, and what makes the chain honest rather than
  merely ordered.
- **The Landlock rung, which is why this release exists.** A stock Ubuntu 24.04
  ships `kernel.apparmor_restrict_unprivileged_userns=1` and refuses the namespace
  the older backend needs — and `ubuntu-latest` is a stock Ubuntu 24.04 — so on the
  commonest Linux CI image in the world every contained run up to 0.46.0 took the
  portable floor, and the filesystem confinement this crate documents was applied
  nowhere. Landlock needs no namespace. It is also the only rung that wraps the
  payload in nothing: the restriction is installed in the child between fork and
  exec, so the argv spawned is the argv you asked for and `current_dir` means what
  it says. Egress is denied through Landlock's own network rules where the kernel
  has them (ABI 4, Linux 6.7); below that the chain hands the run to a rung with a
  network namespace instead.
- **A seccomp deny-list beside the Landlock rule set.** `mount`, `umount2`,
  `pivot_root`, `ptrace`, `process_vm_readv`, `process_vm_writev`, `init_module`,
  `finit_module`, `delete_module`, `kexec_load`, `kexec_file_load`, `bpf` and
  `perf_event_open`, refused with `EPERM` rather than a kill so the failure is
  diagnosable. It is a deny-list and not a jail, and it says so; it is written in
  the host architecture's syscall numbers, so a process under a foreign
  personality is allowed through rather than denied by coincidence.
- **A note about Windows, because a reader will look for it here.** This release
  was planned with a Windows half — the AppContainer selected, so a Windows run
  would enforce files and network rather than resource caps alone — and it is not
  in it. That work moved whole to **0.59.0** on 2026-08-10; the record is
  `US-IO-HARNESS-0.47.0-I01`. A Windows run gets exactly what 0.46.0 gave it: a
  Job Object, which contains resources and not access. `Sandbox::select` returns
  `WindowsJobObject` as it has since 0.24.0, and `docs/CONTRACT.md` says "native
  resource containment only" in the platform table because that is what is true.
  Nothing on Windows is refused today that was permitted yesterday.

### Changed

- **A contained Linux run on a host with Landlock now confines writes where it
  previously did not**, and on kernel 6.7 or later refuses outbound TCP for an
  egress-denying run. Same remedy if a program was relying on the absent
  boundary. *Migration:* `TaskContract::with_full_access()`, or a mode that says
  what the run actually needs.
- `Backend::as_str` gains `"linux-landlock"` and `"linux-bubblewrap"`.
  `EventKind::Contained` and the `SandboxEvent` rows
  carry them through their existing fields, so an observer written against 0.46.0
  reads the new backends with no change and a trace written by 0.46.0 stays
  readable.

### Security

- The host where "contained by default" was true of the API and not of the
  machine — a stock Ubuntu 24.04, which is what `ubuntu-latest` is — now enforces
  it. **Windows remains the host where it is not**, and that is stated rather than
  implied: `select().backend()` answers `WindowsJobObject` before a run,
  `EventKind::Contained` records what was applied, the agent is told its boundary
  in its own prompt, and the platform table says resource containment only. The
  access half is 0.59.0.

## [0.46.0] - 2026-08-09

### Added

- **A run's own commands are contained by default.** `ExecMode` names three
  grants and `TaskContract::exec_sandbox` carries one: `ReadOnly` (the system
  temporary directory and nothing else), `WorkspaceWrite` — **the default** — (the
  workspace root, the system temporary directory and the detected toolchain's own
  cache directories), and `FullAccess` (the embedding program's own privileges,
  which is what every release up to 0.45.0 did without being asked). Every command
  `exec` and the foreground `shell` start now goes through the backend
  `sandbox::select` chose, with the workspace root as its working directory, so an
  incremental build still survives from one command to the next.

- **`TaskContract::with_full_access()` — the escape hatch, spelled at the call
  site.** A run that genuinely needs the machine still gets it; what changed is
  that a reader of your source can see that it was chosen. `with_exec_mode` sets
  the third mode. Both leave the resource caps alone.

- **The detected toolchain's cache directories are writable roots.**
  `Toolchain::cache_dirs` derives them for the ecosystem `toolchain::detect`
  already found, honouring that ecosystem's own environment variable
  (`CARGO_HOME`, `GOMODCACHE`/`GOPATH`, `npm_config_cache`, `PIP_CACHE_DIR`,
  `POETRY_CACHE_DIR`, `UV_CACHE_DIR`, `GRADLE_USER_HOME`, `NUGET_PACKAGES`,
  `DENO_DIR` and the rest) before falling back to the conventional path. This is
  what removes 0.40.0's recorded limitation: a cold `cargo fetch` writing
  `~/.cargo/registry`, or `npm install` writing `~/.npm`, now succeeds under the
  default rather than failing. Only roots that exist on this host are granted —
  a bind of a path that is not there fails the Linux mount setup, and a failed
  setup would degrade the whole backend to the portable floor.

- **The verification gate takes the same writable roots**, so a gate running the
  project's own build command no longer fails for a reason that has nothing to do
  with the code it is judging.

- **`EventKind::Contained { mode, backend, roots }`, once per run.** The mode
  asked for, the backend that **actually applied** — `portable-floor` on a host
  that degraded — and how many writable roots were granted. A `FullAccess` run
  emits it too, with `backend: "none"`: "this run was not contained" is the first
  thing an audit asks and an absent event is not a statement.

- **`[sandbox] mode = "read-only"` in `io.toml`.** It obeys the standing trust
  rule: a project-scoped file may narrow and never widen, so `mode = "full-access"`
  is refused there exactly as `force_floor = false` and `allow_network = true`
  already are.

- 0.45.0's boundary section names the mode, so an agent under `ReadOnly` is told
  it may not write rather than finding out from a failed command — and a
  `FullAccess` run is told it is not contained.

### Changed

- **BREAKING (behaviour): a run built from `TaskContract::workspace` or
  `TaskContract::new` is now contained.** `exec`, each `shell` stage and the
  sandboxed verification gate may write inside the workspace root, the system
  temporary directory and the detected toolchain's cache directories, and nowhere
  else. A program relying on a command writing elsewhere — a sibling checkout, a
  home-directory dotfile, an absolute output path — will see those writes refused
  by the operating system, **with no compile error to warn it**.
  *Migration:* add `.with_full_access()` to the contract. That restores 0.45.0's
  execution behaviour exactly, in one call, at the construction site.

- **No resource cap is applied by the default.** The mode-derived default carries
  `SandboxLimits::none()`, so nothing that completed before is newly killed by a
  clock or a memory ceiling. Defaulting containment on is a claim about where a
  command may write; defaulting the 0.6.0 ceilings on would be a claim about how
  long your build may take.
  *Migration:* none needed. `with_contained_exec(SandboxConfig::new())` still asks
  for the standing 60s CPU / 120s wall / 2 GiB / 512 fd caps.

- **BREAKING (API): `TaskContract::exec_sandbox` is a `SandboxConfig`, not an
  `Option<SandboxConfig>`.** Three modes expressed as two variants plus a `None`
  is a model a reader has to hold in their head rather than read off the type.
  *Migration:* `contract.exec_sandbox.is_some()` becomes
  `contract.exec_sandbox.mode != ExecMode::FullAccess`;
  `contract.exec_sandbox.as_ref().unwrap().limits` becomes
  `contract.exec_sandbox.limits`. `with_contained_exec(config)` is unchanged.

- **BREAKING (API): `sandbox::RunSpec` is `#[non_exhaustive]` and is built through
  `RunSpec::new`.** It carries the mode and the writable roots to the backends.
  *Migration:* `RunSpec { argv, workdir, limits, allow_network }` becomes
  `RunSpec::new(argv, workdir, limits).with_network(allow_network)`, with
  `with_mode` and `with_writable_roots` for the rest. The attribute is why 0.47.0's
  and 0.48.0's additions to this type will cost nothing.

- **On a host whose backend cannot enforce a mode, the mode is routed, reported,
  and enforces nothing for the filesystem.** A Windows Job Object has no
  filesystem facility, and a Linux host that refuses an unprivileged user
  namespace — a stock Ubuntu 24.04, which is `ubuntu-latest` — takes the portable
  floor. This is 0.40.0's finding and it is unchanged here: `EventKind::Contained`
  names the backend that applied, the `SandboxEvent` rows name it per command, and
  the agent is told in its own prompt.

### Fixed

- **A missing program reads the same way contained and uncontained.** A contained
  spawn is of the backend's wrapper (`sandbox-exec`, `unshare`), which exists, so
  a missing payload came back as the wrapper's own failure rather than as
  `[exec unavailable]`. With containment now the default that would have turned
  every "no such program on PATH" into "your command failed" — the wrong diagnosis
  for the model and for whoever reads the trace.
- **A verification gate resolves its own writable cache roots when it was handed
  none.** 0.46.0 gave the gate the detected toolchain's caches, and only the run
  filled them in: a gate reached through `passes_in`, `passes_in_guarded` or an
  `ExecGuard` an embedder built by hand got an empty set, so a `cargo` criterion
  could not populate a registry cache. On the unix backends that is a refused
  write, which cargo mostly survives; under the Windows AppContainer it is a
  refused **read**, because that backend is default-deny for reads, and the gate
  fails outright with the compiler's own error nowhere in sight. Both call paths
  now derive the set from the same function.

## [0.45.0] - 2026-08-09

### Added

- **The agent is told the boundary it is working inside, instead of discovering
  it one refused call at a time.** A run under a `Policy` learns its edges by
  being refused — a completion billed, a tool call dispatched, a refusal written
  into the context every later completion carries — for facts the crate knew
  before it built the request. The system block now carries them: one line per act
  (read, write, execute, network) naming that tier's default and the patterns the
  layers rule on, grouped by what `Policy::explain` actually returns for each and
  attributed, on a refusal, to the layer that produced it — the same vocabulary a
  `Refused` event carries. `Effect::Ask` is rendered as itself, because neither
  "allowed" nor "refused" is true of it and both mislead. A permissive policy
  renders nothing at all, single-file mode never renders it, and at most 24
  patterns per act are named with the omitted count stated. Measured cost on the
  policy most runs carry: **732 bytes** of system prompt, which 0.38.0's cache
  breakpoint serves at the cache-read rate from the second call onward.

- **With containment on, the agent is told which backend it actually got.** One
  further line names what `sandbox::select` returned on this host — not what was
  asked for. Where that is the portable floor or a Windows Job Object it says the
  resource caps apply and filesystem and outbound-network confinement do not,
  which on a stock Ubuntu 24.04 is the truth an agent would otherwise have to find
  out by trying (0.40.0).

- **`SystemPrompt` and `TaskContract::prompt`: an embedder can give its agent its
  own voice.** `Builtin` is the default and is every release before this one;
  `Append(String)` puts the caller's text after the crate's description of the
  agent and its tools; `Replace(String)` supplies that description instead.
  Neither can reach the crate's own sections or its ending. There is no preset
  catalogue and there will not be one — a preset shipped by a library is an
  opinion about model behaviour the library cannot test and cannot withdraw.

- **`TaskContract::instructions` and `with_instruction`.** Where a repository's
  own guidance now lives.

- **`PromptFamily`, `PromptFamily::from_model` and a defaulted
  `Provider::prompt_family`.** The crate reaches four wire shapes and, through
  `Compatible`, some two dozen vendors. The family is read from the provider for
  the two built-in vendors, from the model slug for `OpenRouter` and `Compatible`,
  and is `Generic` for everything unrecognised. **It decides delimiters and
  nothing else**: every family is given the same sections, in the same order, with
  the same words and the same ending, asserted by stripping the delimiters and
  comparing the rest byte for byte. Nothing here claims one family's wording
  performs better than another's. Defaulted, so no `Provider` implementation
  changes.

- **`EventKind::PromptComposed { family, bytes, source, boundary, instructions }`.**
  Once per run, at composition time. It carries no prompt text — that can be a
  repository's whole `AGENTS.md` — and it is where "this run told its agent
  nothing about its boundary" has an answer.

### Changed

- **`[instructions]` is discovered by default.** `Config::discover` looks for
  `AGENTS.md` whether or not an `[instructions]` table is present, so a repository
  carrying the file every other agent reads and no `io.toml` at all is now read.
  *Migration:* `[instructions] files = []` is the opt-out, and it is distinct from
  an absent table. Nothing became implicit that was not already — the caller still
  chooses to read the configuration, once, before the run.

- **A discovered `AGENTS.md` lands in `TaskContract::instructions`, not in
  `TaskContract::constraints`, and rides in the system block rather than in the
  user turn on every step.** A constraint is a rule the goal is checked against;
  this is guidance the agent reads. *Migration:* a caller that read
  `contract.constraints` after `Config::apply_to` to find the repository's
  instructions reads `contract.instructions` now. It is untrusted text moving
  somewhere more authoritative, and what bounds it is structural: delimited,
  framed as the repository's own guidance that grants nothing, and emitted before
  both the boundary section and the crate's ending — so it cannot be the last
  word.

- **The crate's ending sentence is now the last thing in the system prompt.**
  Until 0.44.0 it sat inside the base description, which put the tool and skill
  catalogues after it. Every sentence a 0.44.0 prompt carried is still carried, in
  the same words; one of them is in a different place. *Migration:* nothing to do
  unless you assert on the exact system string, which no public API exposes. The
  move is what makes the guarantee real: nothing a caller or a repository supplies
  is emitted after the sentence that decides what a turn is, so an embedder's
  prompt cannot quietly turn every greeting into a run.

### Deprecated

### Removed

### Fixed

### Security

- **Repository-controlled text moved into the system block, deliberately and with
  its bounds stated.** A hostile or careless `AGENTS.md` now speaks where the
  crate's own rules speak. It is delimited and framed as the repository's
  guidance, it is emitted before the boundary section and the ending so it cannot
  be the last word, and it grants nothing: the `Policy` is enforced in the tool and
  verification layers before any call runs, and no prompt text widens it. What the
  crate asserts is the composition, not what a model does with a prompt.

## [0.44.0] - 2026-08-09

### Added

- **A long run stops paying full price for the part of the request that never
  changes.** 0.38.0 marked one cache breakpoint, at the end of the system block,
  and deliberately left the transcript unmarked: context assembly supersedes,
  invalidates, re-reads and re-fits earlier observations on every turn, so it was
  not the byte-identical prefix a cache needs and a marker there would have been
  billed as a cache *write* almost every turn. 0.43.0's summarising compaction
  removed that objection for the part of the prompt ahead of the fold, and this
  release marks it. Measured live against `anthropic/claude-haiku-4.5` through
  OpenRouter: the system breakpoint alone served 7,408 tokens of a 13,113-token
  prompt from cache; with the transcript breakpoint the same request served
  **13,093** — 5,685 tokens a step that were being paid for fresh.

- **`CompletionRequest::cache_boundary`.** A byte offset into `user`: the end of
  the prefix the caller states is byte-stable across requests. `None` — the
  default, and every caller before this release — sends the body 0.43.0 sent, byte
  for byte. A `Provider` that ignores it keeps working and is honestly
  non-caching, exactly as with `model`, `web` and `effort`. An offset past the end
  of `user`, one that is not on a UTF-8 character boundary, and `Some(0)` are all
  ignored rather than refused: a boundary is an optimisation, and one that turns a
  working run into an error costs more than it can save.

- **`EventKind::CacheMarked { through_step, prefix_bytes }`.** Emitted when the
  marked prefix *changes* — the step it is first offered, and again whenever a
  later fold moves it — and never once per step. The absence of it across a run is
  the signal that nothing was ever marked, which is what makes "why is this run
  getting no cache reads" answerable without a wire trace. Additive: `EventKind`
  has been `#[non_exhaustive]` since 0.24.0.

### Changed

- **The crate never asks a vendor to cache a prefix it has not already sent.**
  "Everything before a compaction boundary is immutable" is not true of the whole
  prefix: the memory block renders *ahead* of the summary and is re-read from the
  store every turn, so a note a run writes about its own work moves the prefix
  without touching the summary. The run loop therefore holds the previous step's
  candidate prefix and marks only on a byte-identical repeat. Two visible
  consequences, both deliberate — the step a fold happens on is never marked, so
  the marker is always one turn behind the boundary, and a note written mid-run
  withdraws it for exactly one step. The failure mode is lost saving, never lost
  money.

- **A marked request sends the user turn as two content blocks instead of one**, on
  the Anthropic and OpenRouter wires, with byte-identical content. Bodies for
  `OpenAi` and every `Compatible` endpoint are unchanged under all inputs, and so
  is any request that names no boundary.

- **A request carrying an image is marked on OpenRouter and not on Anthropic.**
  Anthropic puts image blocks before the text, so a marked text block would write a
  one-turn attachment into the cache entry and the next turn could never hit it;
  the OpenAI-shaped wire puts text first, so the marked span is still a real
  prefix. A property of the two orderings rather than a policy this crate chose.

- **`CompletionRequest` gains a field.** *Migration:* construct it with
  `..Default::default()`, which the type's own worked example has advised since
  0.15.0 and which every caller already using it needs no change for. Only an
  exhaustive struct literal outside this crate stops compiling — the same break
  `media` (0.15.0), `model` (0.21.0), `web` (0.22.0) and `effort` (0.31.0) each
  were. The type is deliberately **not** `#[non_exhaustive]`: that attribute
  forbids every struct expression outside the defining crate, functional update
  included, so marking it would forbid the very `..Default::default()` this note
  tells you to write.

### Deprecated

### Removed

### Fixed

### Security

## [0.43.0] - 2026-08-09

### Added

- **A long run no longer forgets its own beginning.** When the observation ledger
  crosses a share of the turn's context budget, everything but the newest few
  observations becomes one model-written paragraph — what was being attempted,
  which files were read or changed, what was decided, and what is still open — and
  the run continues from that. Before this the assembler could only *truncate*:
  the oldest observations became one-line stubs saying that a read happened and
  how big it was, and nothing in the crate had ever written a sentence about what
  the run learned from them.
- `Compaction { at_share, keep_recent }` and `TaskContract::with_compaction`
  decide when that happens. **It is on by default** (`at_share: 0.8`,
  `keep_recent: 8`), because the failure it replaces is silent: a run whose oldest
  work became a list of byte counts reports nothing.
- The paragraph is written by the run's **own** provider and model — no second
  provider to configure — and costs one ordinary `provider_calls` row for the step
  it happened in. Its tokens are inside `Store::spent_tokens` and inside the run's
  token budget, so a fold is spend you can see where you already look.
- `Summary`, `Store::summaries` and `Store::summary_for` make each fold durable, in
  a new `summaries` table. A resumed, branched or replayed run **replays** its
  stored folds rather than paying a model to write the same paragraph again.
- `EventKind::Compacted { through_step, before_tokens, after_tokens }` reports a
  fold as it happens, with what it cost and what it bought.
- **A context overflow no longer ends the run.** `ProviderErrorKind::ContextOverflow`
  tells an over-window rejection apart from any other 4xx, using the vendor's own
  wording via the new `ProviderErrorKind::from_response`. The loop answers it by
  compacting and asking once more with a smaller request; a second overflow
  escalates exactly as before. The classification is deliberately conservative — a
  wording it does not know costs exactly what it costs today, while a false
  positive would re-send a request the server had already refused.
- **A session's whole conversation can be exported.** `Session::transcript` returns
  a `Transcript` of `TranscriptTurn`s and `Transcript::to_markdown` renders it: every
  turn in order, what was asked, what was answered, where a summary stands in for
  the steps behind it, and which turns a branch left off the model's path. It is a
  read — no provider is called and no row is written.
- **A conversational turn can carry an image.** `Session::attach` stages images for
  the next turn, through the same path `TaskContract::with_images` has used since
  the `media` feature shipped. One method covers all six turn entry points, and the
  staging is cleared once the turn has been driven. `media` feature only.

### Changed

- `ProviderErrorKind` is now `#[non_exhaustive]`, and gained `ContextOverflow`.
  `is_retryable()` returns `false` for it, which is the correct answer rather than a
  compromise: re-sending bytes a server has said were too many cannot work. The
  recovery is a *different* request, and it belongs to the run loop.

  *Migration:* a `match` on `ProviderErrorKind` that was exhaustive now needs a
  wildcard arm. Prefer one that fails loudly over one that guesses — this crate's
  own test panics in that arm, so a kind added later still cannot slip past a
  decision about retrying it. Nothing else about the enum moved:
  `ProviderErrorKind::from_status` behaves exactly as it did on 0.42.0 over every
  status it maps.
- A run long enough to cross `Compaction::at_share` now makes a summarising
  provider call it did not make on 0.42.0 — billed, traced, and inside the token
  budget. A caller who wants 0.42.0's behaviour exactly writes
  `.with_compaction(Compaction { at_share: 1.0, ..Compaction::default() })`, which
  also leaves an over-window request terminal as it was.

### Deprecated

### Removed

### Fixed

### Security

## [0.42.0] - 2026-08-08

### Added

- **An unattended run no longer dies on a decision nobody is there to make.**
  `ModelApprover` installs a model as the `Approver`: it reads the pending act,
  what it targets, the bytes a write would land, **the rule and the policy layer
  that flagged it** and the run's goal, then approves, denies with a reason the
  agent reads and adapts to, or defers — persisting the action so the person who
  reads the trace tomorrow answers it with `resume_with_decision`. A verdict it
  cannot read is a defer, never an approval.
- What a model approver may decide is bounded by what reaches it, and that bound
  is unchanged: an action the `Policy` **denies** never reaches any approver. A
  `ModelApprover` also never rewrites the action and never remembers a rule, so it
  answers the call in front of it and cannot widen the run's boundary.
- A model is refused before the first request is billed when it would be answering
  for its own model — the approval mirror of the review refusal 0.34.0 added, with
  `ModelApprover::allow_self_approval(true)` as the visible way to say you meant
  it. The refusal costs zero calls to either provider, and it covers a tree as
  well as a flat run.
- `ApprovalContext`, carrying the goal, the rule and the layer, with the defaulted
  `Approver::decide_in_context` that receives it. The default forwards to
  `decide`, so **every approver written before this release compiles and behaves
  identically**. `Approver::model` and `Approver::self_approval_allowed` are
  defaulted too.
- **An operator can stop a tool call from `io.toml` instead of from Rust.** A
  `[[hook]]` table gains `at = "before_tool"` and an optional `tools` filter. The
  hook is spawned with the pending call on its stdin *before the call runs*; a
  non-zero exit refuses that one call, its first line of stdout becomes the reason
  the model reads, and the run adapts. `on_failure` gains `refuse`, which is a
  lifecycle hook's default — a check attached to a point that exists to stop
  something is not a notification — while `cancel` still ends the run and
  `continue` lets the call through.
- A lifecycle hook is refused in a project-scoped `io.toml` exactly as an event
  hook is, inside a `[profile]` too: a hook that can stop a tool is strictly more
  dangerous than one that appends a log line. A table this crate cannot honour —
  an unknown `at`, `on` and `at` together, `tools` without `at`, or a lifecycle
  hook whose only action is `append` — is refused when the file is read, because a
  check that loads and never fires looks exactly like one that approved
  everything.
- `TaskContract::with_tool_hooks` installs them. It takes the same `Hooks` value an
  application already installs as an `Observer`: as an observer it ignores the `at`
  tables, as a gate it ignores the `on` ones. Nothing is implicit — a configuration
  describing a `before_tool` hook does nothing until an application installs it.
- **A review criterion can read the change rather than the outcome.**
  `ChangeReview` and `FileChange` carry each written file as it was before the run
  first touched it and as it stands now, so a rubric about what a change *did* —
  nothing lost its doc comment, no public item was removed — is answerable at all.
  It is built from the restore points the store has kept since 0.28.0: no table,
  column or index is added.
- `Reviewer::review_change` is defaulted to forwarding the `ReviewRequest` a
  reviewer has always received, so **no existing `Reviewer` needs an edit**. The
  consequence is stated rather than hidden: a reviewer that does not override it
  sees the outcome and not the change. `ModelReviewer` overrides it.

### Changed

- A refusal from a `before_tool` hook is reported through the existing
  `EventKind::Refused`, with the hook's program where a rule's pattern would be and
  `io.toml hook` where a layer would be. No new event kind: a refusal that did not
  come from the policy is still a refusal, and an observer already routing on them
  sees it.
- A `[[hook]]` table written for 0.42.0 is **refused** by an older binary rather
  than silently ignored, because `at` and `tools` are new keys under
  `deny_unknown_fields`. That is the intended direction — a lifecycle check that
  loads and never fires is the failure mode this crate refuses to ship.

There is no migration note for this release: nothing is removed, renamed or
altered, both new trait methods are defaulted, `ReviewRequest` and `Request` keep
their exact shape, no table, column or index is added, `CHECKPOINT_FORMAT` is
unmoved, and a 0.41.0 store and a 0.42.0 store are the same store.

## [0.41.0] - 2026-08-08

### Added

- **A read-heavy step no longer costs one round trip per file.** When a completion
  carries several read-only tool calls, they now run at the same time instead of
  one after another. A model that spent a completion asking for eight files pays
  for the slowest read rather than the sum of all eight. Three built-ins are
  read-only — `grep`, `find` and `read_file` — and everything else runs exactly as
  it did, one at a time, in the order the model asked.
- `TaskContract::max_parallel_reads`, with `TaskContract::with_max_parallel_reads`,
  caps how many read-only calls from one completion may be in flight. It defaults
  to **10** and clamps to a floor of 1; `0` means serial rather than an error. It
  caps calls in flight, not calls attempted, so a completion carrying four reads
  runs four whatever the cap says.
- `ToolEffect`, and a defaulted `Tool::effect` returning it. A registered tool that
  returns `ToolEffect::ReadOnly` joins the concurrent group; the default is
  `ToolEffect::Mutating`, so **every `Tool` implementation written before this
  release compiles unchanged and keeps being called one at a time**. The
  declaration is a promise the tool makes about itself — the harness cannot check
  it, which is why concurrency is opted into rather than given.

### Changed

- The step loop partitions each completion's tool calls by whether they can change
  anything and dispatches each run of read-only ones together, bounded by a
  `tokio::task::JoinSet`. **This changes timing, not results.** Observations,
  decisions, recorded steps, `Edit` rows, budget draws and the events an `Observer`
  receives are folded back in the order the model asked for the calls, never in the
  order they finished, and a run's trace, ledger and replay are identical to the
  serial run of the same recorded case. If you want to rule the concurrency out
  while debugging something else, `with_max_parallel_reads(1)` puts the run back on
  0.40.0's execution path exactly — the batch is not entered at all.
- A batch collapses to serial at the first call whose policy decision is not an
  outright allow. A read-only call an approver defers stops the step as it always
  has, and the calls after it in that completion are not started — not merely not
  recorded.

There is no migration note for this release: nothing is removed, renamed or
altered, no table, column or index is added, `CHECKPOINT_FORMAT` is unmoved, and a
0.40.0 store and a 0.41.0 store are the same store.

## [0.40.0] - 2026-08-07

### Added

- **The project's own commands can run contained.**
  `TaskContract::with_contained_exec(SandboxConfig)` puts every command the `exec`
  tool and the foreground `shell` tool start inside the sandbox backend this host
  offers: the config's resource caps, filesystem writes confined to the workspace,
  and outbound network denied unless the run's `Policy` would permit `Act::Net`.
  The new field is `TaskContract::exec_sandbox`.
- A contained command keeps the **workspace root** as its working directory.
  Nothing is copied to a temporary directory and nothing is discarded, so an
  incremental build survives from one command to the next — which is what makes
  containment usable for a build rather than only for a verification gate.
- A command stopped by a resource cap is now reported as *that cap* — the model is
  told which resource ran out, and `sandbox_events` records a `cap_hit` row —
  rather than surfacing as an ordinary non-zero exit.
- A contained command writes the `sandbox_events` lifecycle rows the verification
  gate has written since 0.6.0, carrying the backend that **actually applied**, so
  a run that fell back to the portable floor is legible afterwards instead of
  looking identical to one contained as asked.

### Changed

- **Linux now confines filesystem writes, which it did not before.** The Linux
  backend unshared a mount namespace and remounted nothing into it, so the
  namespace existed while the filesystem view stayed the host's and a write
  outside the working directory landed; only the network namespace was doing real
  work. It now remounts the tree read-only inside its namespace and binds back the
  working directory and the system temporary directory. A host whose kernel
  refuses the remounts degrades to the portable floor and reports the floor. This
  affects the verification gate as well as contained commands, and it is a
  tightening: a gate command that wrote outside its working directory on Linux
  succeeded before and fails now.
- **Read this before relying on the Linux row: a stock Ubuntu 24.04 host does not
  get filesystem confinement.** The backend needs an unprivileged user namespace,
  and Ubuntu 24.04 ships `kernel.apparmor_restrict_unprivileged_userns=1`, which
  refuses one. There a contained command takes the portable floor: the resource
  caps still apply and the filesystem confinement and egress denial do not. It is
  reported rather than hidden — `select().backend()` says `PortableFloor` before
  the run and the `SandboxEvent` rows say it afterwards — but it is not what the
  per-platform table's Linux row reads like at a glance. Setting
  `kernel.apparmor_restrict_unprivileged_userns=0`, which most other
  distributions already ship, gives the real backend. `docs/CONTRACT.md` states
  it in full.

### Deprecated

### Removed

### Fixed

- `src/lib.rs` claimed Linux confined writes the way macOS does. It did not, until
  this release.

### Security

- Nothing about the default changes. With `exec_sandbox` unset — every contract
  written before this release — `exec` and `shell` run exactly where and as they
  did in 0.39.0. This release makes containment *available*; it does not apply it.
  The standing bound is unchanged for anyone who does not opt in: the policy
  decides what may start, not what a started process then does.
- Two limits worth reading before turning it on. Egress at the sandbox wall is one
  boolean, so a policy allowing a single host gives a contained command a route to
  every host; per-host checking of the crate's own tools is unchanged. And the
  `shell_start` / `shell_poll` / `shell_kill` handles are **not** contained,
  because a handle outlives the call that made it.

## [0.39.0] - 2026-08-06

A conversation can fan out.

An operator asks for something wide inside a session — migrate these forty
handlers, review these twelve files — and until now they got one agent working
through forty items in one context window, or they left the conversation. The
reason was structural rather than a missing feature: the `spawn_agent` tool is
registered inside the agent-tree loop, and no session turn reached that loop. A
run was a tree or a turn and not both.

**A turn can now be a tree.** `Session::turn_contained` and
`Session::turn_contained_observed` take a `Containment` and drive the turn through
the tree loop, so the agent answering it may decompose the work into contained
sub-agents. Every mechanism the fan-out uses already shipped — inherit-and-narrow
policy, one shared ledger, per-tier concurrency slots with a durable queue, spawn
accounting, tree-wide resume. What 0.39.0 adds is the caller.

**It is still one turn.** A turn that spawned forty agents is one row in
`session_turns`, one entry in `Session::history`, and one move of the session
head. The children are runs under the turn's own run, reconstructable through
`Store::agent_events` and `Store::children`.

**A child is given its goal, not the conversation.** The transcript reaches the
root agent and stops there — forty children each carrying it is the multiplied
version of the cost the context budget exists to bound, and a child that has read
the conversation is one that can act on an instruction the operator has since
withdrawn.

**Nothing else moved.** No public item is removed or altered, no type gains a
field, no table is created or migrated, `CHECKPOINT_FORMAT` is unchanged, and
`docs/public-api.txt` is byte-identical: the two additions are methods on an
existing type, and the crate root's surface is unchanged. The five turn entry
points that predate this still never offer the spawn tool, so a session that does
not pass a `Containment` behaves exactly as it did in 0.38.0. Upgrading changes no
code.

**The limits, stated rather than discovered.** The ledger is per turn and not per
session, so a conversation has no single ceiling. A contained turn that pauses is
continued with the `resume_tree_*` family on `TurnResult::run_id`, and its turn row
reports the pause rather than the continuation. Both are in `docs/CONTRACT.md`.

### Added

- `Session::turn_contained` — take a turn that may fan out, under a `Containment`
  that bounds the whole resulting tree.
- `Session::turn_contained_observed` — the same, reporting to an `Observer` as the
  fan-out happens, and streaming the root's text. `EventKind::Spawned`,
  `SpawnRefused`, `Fleet` and `SpendDraw` reach a session's observer for the first
  time.

### Changed

- `docs/guide/sessions.md` and `docs/guide/composition.md` document the fan-out
  from both sides; `docs/CONTRACT.md` states what a contained turn gives and what
  it does not.

### Deprecated

### Removed

### Fixed

### Security

## [0.38.0] - 2026-08-06

A long conversation stops paying full price for the part of itself that never
changes.

Every completion this crate makes carries the same block ahead of anything new:
the system instructions, the skill catalogue folded into them, and the JSON schema
of every tool on offer. It is built once per turn and handed to every step of the
loop, so a twenty-step run sent it twenty times and a fifty-turn session sent it
fifty times — and paid full price each time. The vendors have offered to serve a
repeated prefix from their own cache for years, this crate has read the counters
back since 0.18.0 and priced them since the same release, and it had never asked.
`Usage::cache_read_tokens` was structurally zero.

**It asks now.** The Anthropic request marks the end of its `system` block as a
cache breakpoint. That wire orders a request's cacheable prefix tools-then-system,
so the one marker covers the tool schemas *and* the instructions. The OpenRouter
request carries the same marker in the shape that wire spells it.

**Measured, not asserted.** Two consecutive calls over an identical system block
through OpenRouter against `anthropic/claude-haiku-4.5`: the second read **7,408
of its 7,421 prompt tokens** from the vendor's cache. The control — the same
endpoint and the same model reached through `Compatible`, which sends no marker —
read none on either call.

**Nothing else moved.** No public item is added, removed or altered, no table is
created or migrated, `CHECKPOINT_FORMAT` is unchanged, and `docs/public-api.txt`
is byte-identical to 0.37.0's. Upgrading changes no code.

**OpenAI and `Compatible` send nothing, deliberately.** OpenAI caches a repeated
prefix by itself with no request-side control, and that path also serves the 21
endpoints reached through `Compatible`, where an unknown body key would be a 400
this crate had caused. Their request bodies are byte-identical to the ones 0.37.0
sent.

**The honest cost, stated as loudly as the saving.** A cache write is billed above
a fresh read and a cache read far below one, so the block pays for itself from its
*second* use — a prefix used exactly once now costs about a quarter more than it
did. A prefix below the vendor's minimum cacheable length is silently not cached
and the marker does nothing. And OpenRouter reports no cache-write counter, so a
run cached through it under-reports what the writing call cost. All three are in
`docs/CONTRACT.md` rather than left to an invoice.

**What is deliberately not cached: the transcript.** The context assembler
supersedes, invalidates, re-reads and re-fits earlier observations on every turn,
so it is not a byte-stable prefix — and a breakpoint that misses is billed as a
write, which would cost money rather than save it. Making the assembler
prefix-stable is a larger change than this release.

### Added

### Changed

- Requests to `Anthropic` now send `system` as a one-element content-block array
  carrying `cache_control: {"type": "ephemeral"}`, rather than as a bare string.
  The instruction text itself is unchanged, and no marker is placed on the `tools`
  array — a changed tool list already invalidates everything after it in that
  vendor's ordering.
- Requests to `OpenRouter` now send the system message's `content` as a
  one-element parts array carrying the same `cache_control` object.
- The first call of a run against those two vendors is billed at the cache-write
  rate for that block rather than the input rate; from the second call it is
  billed at the cache-read rate. This is a change to what an operator is charged,
  not to any API.

### Deprecated

### Removed

### Fixed

### Security

## [0.37.0] - 2026-08-06

A conversation answers without opening a run.

`Session::turn` had one shape: build a contract, open a run, drive the loop. That
is the right machinery for "migrate the forty handlers" and it was the whole
machinery for "hi". An operator who said hello got a run in their trace, a plan
gate they might have to answer, a checkpoint on disk and a row in the ledger that
said work happened. Nothing did.

**The turn's own first completion now decides what the turn was.** The completion
is made the way it has always been made — the workspace tools offered, the
conversation seeded, the operator's text as the goal — and what comes back is read
rather than assumed. A completion that stops on text is an answer: the turn closes
as `TurnKind::Reply` with no step, no gate attempt, no checkpoint, no snapshot, no
plan gate and no call to the `Approver`. A completion carrying a tool call is work:
the turn is a `TurnKind::Run` and the loop continues **from that same completion**,
so the run's first step is the call that was already paid for.

**The classification is therefore free, and it is the model's.** A turn that
answers makes one provider call. A turn that promotes and takes two steps makes
two, not three. There is no list of greetings in this crate and none is needed in
the program embedding it — which is the point, because a list is a list in one
language, matches `hi` and not `namaste`, and answers `hi, the login page is
broken` correctly only by accident.

**A reply is billed, and says so.** The completion happened and cost money, so the
run row is written, `Store::run_summary` reports its tokens, the per-call
accounting row carries its model and its latency, and the token ceiling is applied
*before* the answer is served — a turn that cannot afford its own reply is refused
rather than served free. A reply that recorded nothing would satisfy every
assertion about what is absent and would make the crate's own cost reconstruction
wrong.

**A reply is part of the conversation.** It is in `history()`, the next turn reads
both the prompt and the answer, and `branch_from` takes it like any other turn.

**What is deliberately not classified.** A contract carrying a `Verification` is
always work: a caller who declared how the turn is judged has said it is work, and
handing back an answer instead of running the gate would be answering a different
question. `run_with` and `run_with_observed` are untouched — a one-shot contract is
work by declaration, and an entry point that sometimes answers instead of running
is a worse contract than one that always runs.

**Only the first completion of a turn can be a reply.** A run whose fifth step
stops on text is a run that finished, exactly as in 0.36.1.

**The honest cost.** The system prompt for a turn's first completion now permits
answering, which is a real behaviour change for every session turn rather than
only for greetings. A model that answers in prose where it should have acted costs
the operator one retype — the turn's reply is on screen — and that asymmetry is
accepted by choice: answering something meant as work costs a retype, while
running something meant as a greeting plans it, checkpoints it, gates it and bills
it.

### Added

- `TurnKind`, with `Reply` and `Run`, and `TurnResult::kind`. Branch on this
  rather than inferring from a step count — a run refused at its first step also
  has no steps, and the two are not the same thing.
- `EventKind::Answered { turn_id }`, with its `EVENT_NAMES` entry. Emitted once,
  before `Finished`, when a turn closes as a reply. Which run served it is the
  event envelope's own `run_id`; a second copy in the kind is a duplicate key once
  the kind is flattened onto the wire, which is the constraint `Rewound` already
  records.
- One additive column, `runs.turn_kind`, and one index, `provider_calls_run`.
  `CHECKPOINT_FORMAT` is unmoved at 7 and no existing table is otherwise altered.
- `Store::check_resumable` refuses a conversational turn that was still answering
  when it stopped: it committed no step, so there is nothing to continue, and
  asking again replaces the one completion at the same price. A reply that
  *finished* is a completed run like any other and a resume reports its outcome,
  unchanged.

### Changed

- **BREAKING (API)** `TurnResult` gains the `kind` field and becomes
  `#[non_exhaustive]`. Both are the same break for a struct literal or an
  exhaustive destructuring outside this crate, so they are paid together rather
  than twice — and the second one means the next field this type gains is free.
  *Migration:* `TurnResult` is constructed only inside this crate and every method
  returning one keeps its signature, so a caller that reads `turn.reply`,
  `turn.outcome`, `turn.run_id` or `turn.turn_id` needs no change at all. Add a
  `..` to any exhaustive `let TurnResult { .. } = turn` destructuring. No field was
  removed and no signature moved.
- **BREAKING (behaviour)** a session turn whose model stops on text without
  calling a tool now closes without running the loop. An embedder that inferred
  "a turn happened, therefore work was attempted" from the existence of a
  `TurnResult` was reading something that was never guaranteed and now differs.
  *Migration:* read `TurnResult::kind`. `TurnKind::Run` is every turn that would
  have opened a run in 0.36.1 and did work; `TurnKind::Reply` is the turn that
  answered. A caller that wants a turn always to be work gives its contract a
  `Verification`, which is never classified.
- `Store::spent_tokens` reads a reply's spend from its one `provider_calls` row,
  because a turn that answered has no `steps` row to sum. Every other run is
  summed from its steps exactly as before.
- `rmcp` moves from 3.0.0 to 3.1.0 and `zip` from 2 to 8. The second is a
  major-version jump, and it is confined to the optional `documents` feature —
  `zip` is how a `.docx`, `.xlsx` or `.pptx` is opened. Neither changes anything
  this crate exposes: no public item moves, `cargo tree` still reads 402 lines,
  and both feature polarities pass on all three operating systems.

### Deprecated

### Removed

### Fixed

### Security

## [0.36.1] - 2026-08-04

A verdict in minutes, and a front page that is true.

No public item is added, removed or changed. `docs/public-api.txt` is
byte-identical, `CHECKPOINT_FORMAT` stays 7, no table is added or altered, and no
dependency moves — `cargo tree` still reads 402 lines. A database written by
0.36.0 is a database 0.36.1 reads, and the reverse. Upgrading is one digit and a
recompile.

### Changed

- **CI answers in a fraction of the time, proving exactly what it proved before.**
  The workflow took 17m51s on the run that merged 0.36.0, and effectively all of
  it was one leg: `test (windows-latest)` 17m51s against 8m51s on macOS and 8m23s
  on Linux, with every other job finishing inside 90 seconds and then waiting.
  Inside that leg, on a warm cache, `cargo build --all-targets` was 317s, `cargo
  test` 295s and `cargo test --all-features --lib --tests` 400s — largely the
  same work three times, because the feature flip invalidated what the build had
  just produced and the doctests are their own compile and their own link.

  The matrix is now a matrix over operating system *and* feature polarity, each
  leg on its own cache key, so the two polarities are computed at the same time
  instead of one after the other. The doctests are their own per-OS job. The
  matrix builds the eight fixture examples a test actually spawns rather than
  linking all 35 six times a run; the rest are compile-checked once on Linux in
  both polarities. `cargo-nextest` runs the test binaries concurrently. And
  Windows links with `rust-lld`, the linker the toolchain already ships, which
  is the lever aimed at what that platform's cost actually is — roughly 95
  linked executables per polarity plus one link per doctest.

  **Measured, warm cache to warm cache: 17m51s to 6m58s.** Per lever, on the
  same leg with nothing else changed, `rust-lld` took the Windows doctest step
  from 338s to 200s and the Windows build step from 253s to 205s. The remaining
  critical path is the Windows `--all-features` leg at 6m58s, of which 267s is
  building the 59 test binaries.

  No acceptance criterion in this release asserts a duration. This repository
  has three separate flaky tests that are wall-clock assertions failing on
  loaded machines, and CI is the place with the least control over its own
  hardware. The criteria assert structure — which jobs exist, which commands run
  in them, which targets are built, which linker performed the link — and the
  durations above are recorded from the GitHub Actions API by run id
  (baseline 30834723995, after 30925715697) rather than gated on.

  **Nothing was traded away for it.** Both polarities still run on all three
  operating systems, all doctests still run on all three, and the MSRV floor,
  `fmt`, `clippy` in three feature shapes, the docs.rs nightly build, the `links`
  coexistence wall and the 0.29.0 cross-version pair are untouched. Every
  `(operating system, cargo invocation)` pair the old workflow ran is still run.

- **The release workflow's gates run in parallel**, behind one `checks` job that
  still refuses everything it refused before: a tag that does not match
  `Cargo.toml`, a commit that is not an ancestor of `main`, a tag or Release that
  already exists, and a version with no changelog section. No tag is pushed and no
  Release is cut until every gate has passed.

- **The install snippet says the version you are reading about.** `README.md` told
  a reader to write `io-harness = "0.25"` — eleven releases stale, and the one
  line in the file meant to be copied. A stale snippet raises no error, because an
  old version resolves; a reader simply gets an eleven-release-old library and
  concludes that is the crate. The dependency version now joins the MSRV, the
  feature list and every relative link as a fact checked against `Cargo.toml` by
  `tests/docs_drift.rs`, so it cannot drift again.

- **No badge on the README carries a value a human typed.** The MSRV badge had
  `1.95` written into its URL — the same defect as the stale install line, eleven
  lines below it in the same file. It now reads `rust-version` from the published
  manifest, so it cannot disagree with the crate even in a release that raises the
  floor. A downloads badge is added. None is added for coverage, benchmarks or
  unsafe-freedom, because this crate measures none of the three.

- **The product table is true.** io-cli is released, public and on crates.io; it
  was listed as "in development" and unlinked. io-studio is **not built**. The
  sentence claiming this was the only public repository is gone. The table states
  status rather than version numbers, so it stays true as the sibling products
  release.

- **The README leads with what the crate is, how to start, and who it is for**,
  ahead of the capability inventory — which now begins at line 122 rather than
  line 100 and no longer sits between a reader and the quickstart.

### Fixed

- **`AgentDef::worktree`'s documented path was missing a component.** It named
  `<root>/.worktrees/<agent>-<parent run>-<step>`; the path a spawn actually
  creates ends in a digest of the child's goal. That component is what stops two
  children of the same definition, spawned in the same step — the ordinary shape
  of a fan-out — being handed one worktree between them. Documentation only; the
  behaviour was already correct.

- **The agent-composition guide never mentioned per-child worktrees.** The
  capability 0.36.0 added to lift that page's central bound — one shared checkout,
  so concurrent children overwrite each other — was documented on three other
  pages and not on the one a reader asking "can two children work at once" opens.

- **The verification guide over-claimed the self-review refusal.** "A model may
  not review its own work" rests on `Provider::model_hint`, which is a defaulted
  trait method returning `None`. For a provider that does not override it the rule
  is a **no-op, not a failure**: the review runs, possibly on the model that wrote
  the change. The public contract already said so; the guide did not.

- **The capability index still listed the git built-ins as status, diff, log, add
  and commit.** Branch and worktree shipped in 0.36.0.

### Added

- **`tests/ci_workflow.rs`** — a test that fails when the CI matrix and the test
  suite disagree about which example binaries exist. `--lib --tests` does not
  build `examples/`, and this repository has rediscovered that four separate
  times, each as a confusing CI failure about a missing file. Both sets are
  derived — one from `tests/`, one from the workflow — so the fifth occurrence is
  a named test failure that says what to add.

## [0.36.0] - 2026-08-03

A run lands as a branch, and can be put back.

Git reached this crate as five fixed-argv built-ins — status, diff, log, add and
commit — with `checkout` on the forbidden list, so a run committed onto whatever
branch it found and every agent in a tree shared one working directory. Two
children editing the same file were one overwriting the other, which made the
concurrency 0.32.0 bought usable only for work that did not overlap.

**`git_branch` creates a branch at the current commit and moves onto it.** It
renders `git switch --create=<name>` — the one shape of a checkout that cannot
discard anything: the new ref starts at `HEAD`, an existing name is refused by
git, and the working tree is carried across rather than replaced. That is why
`switch` is reachable while `checkout` stays refused, and the test that holds
the forbidden list passes **unchanged** rather than being relaxed to fit.

**`git_worktree` makes a second working tree at its own new branch**, at a path
the policy allowed. The branch name is fused into one argv element where git
offers that form and validated by an ASCII allowlist where it does not, so a
name git could read as an option is refused before a spawn rather than escaped.

**An `AgentDef` can ask for its own checkout.** A child of a `worktree = true`
definition is rooted at `.worktrees/<agent>-<parent>-<step>-<digest>` on a
branch of that name, created before its first step. The path is *derived* from
the key a spawn is adopted by, so a resumed tree finds the worktree it already
made — with the files the child had written still in it — rather than
re-creating it. If one cannot be made, the spawn fails and says why: quietly
sharing the parent's tree is the collision the flag exists to prevent.

**`rewind_run` widens a rewind from a path to a run.** 0.28.0 kept the previous
contents of every path a run wrote and put one back; it did not put back what
the run *learned* or what it had *queued*. A run that wrote three files,
recorded two decisions in memory and queued four children left three of those
five effects standing after an operator had restored every file — and the two
that remained are the ones that change what the next run does. Memory is read
into context, so a wrong fact outlives the files it was learned from; a backlog
is adopted on resume, so work the operator undid is re-admitted. A partial undo
is worse than none, because it looks complete.

One call now restores every file the run wrote — with the four verdicts
`Rewind` already distinguishes — restores every memory entry to the value that
was there before the run's **first** write to that key, and clears the spawn
backlog it left queued.

**The trace keeps both branches.** Nothing in the steps, the event stream, the
spawn records or the ledger is deleted or altered by a rewind: the spend
happened, and an undo that erased the rows would make the ledger disagree with
the invoice. What the rewind took is written down before it goes, readable
through `Store::rewinds`.

**What a rewind does not undo, plainly rather than by implication:** a commit
the run made is still there, a push is not recalled, a migration is not
reversed, a provider call is not un-billed, and no worktree is ever removed by
this crate.

### Added

- `git_branch` and `git_worktree` built-ins, with `GIT_BRANCH_TOOL` and
  `GIT_WORKTREE_TOOL` in `io_harness::tools`.
- `AgentDef::worktree` and `AgentDef::with_worktree()`.
- `rewind_run` and `rewind_run_observed`, returning `Rewound` — the files with
  their per-path verdicts, the memory keys restored and removed, and the queued
  children cleared.
- `Store::rewinds`, returning `RewindRecord`: what one rewind put back, took
  away and cleared, and when.
- `EventKind::Rewound { files, memory, queued }`, with its `EVENT_NAMES` entry.
- Two additive tables, `memory_snapshots` and `rewinds`, and their indexes.
  `CHECKPOINT_FORMAT` is unmoved at 7 and no existing table is altered.

### Changed

- **BREAKING (API)** `AgentDef` gains the `worktree` field and becomes
  `#[non_exhaustive]`. Both are the same break for a struct literal or an
  exhaustive destructuring outside this crate, so they are paid together rather
  than twice — and the second one means the next field this type gains is free.
  *Migration:* build definitions with `AgentDef::new` and the `with_*` builders,
  which is what every documented caller already does and which is unchanged.
  Replace any `AgentDef { .. }` literal with `AgentDef::new(name)` plus
  builders, and add a `..` to any exhaustive `let AgentDef { .. } = def`
  destructuring. No field was removed, no signature moved, and a roster
  deserialized from `io.toml` or a `plugin.toml` that names no `worktree` is
  unchanged.
- `git_commit`'s tool description no longer claims there is "no branch
  switching". `git_branch` is the narrow form that now exists, and the
  description points at it.

### Deprecated

### Removed

### Fixed

### Security

## [0.35.0] - 2026-08-03

A directory is a capability bundle.

Six capabilities had a discovery path each and shared nothing: skills came from
one directory, templates from another, agents, MCP servers and hooks from arrays
in `io.toml`, and policy layers from a stack the application assembled. Handing a
coherent set of them to somebody else meant six manual steps, and once they were
in place nothing recorded that any of them came from anywhere but the operator.

**A `plugin.toml` declares what a directory contributes**, and a `[[plugin]]`
entry in any configuration scope names one by path. `Config::plugins()` loads
every declared bundle; `Plugins::apply_to`, `apply_to_policy`, `apply_to_hooks`
and `templates()` install what they brought. The manifest's contribution types are
the ones `io.toml` already deserializes, so it is the configuration file's
vocabulary rather than a second one.

**A bundle is a stranger's directory, and the 0.28.0 trust rule governs it.** A
plugin declared in the committed, cloned `io.toml` contributes skills, templates,
agents and deny rules, and may **not** contribute a `[[hook]]` or an `[[mcp]]` —
both name a program this machine would run. Declared in `io.local.toml` or the
user file it contributes all six. The refusal is whole: a project-scoped bundle
that declares one contributes none of its other kinds either. `${cmd:}` is refused
inside a manifest in every scope.

**Plugin-supplied policy may only narrow.** A `[policy]` block may carry layers of
`deny` rules and nothing else; an `allow` rule, an `ask` rule or a `defaults`
block drops the bundle. A bundle may take capability away and may never hand it
out.

**Every contribution carries its plugin.** Skills, templates, agents, policy
layers and MCP server ids are namespaced `<plugin>__<name>` as they load, so the
bundle is already inside the strings the trace has recorded since 0.4.0: a refusal
names `<plugin>__<layer>` in `PolicyEvent.layer`, a call names `<plugin>__<server>`
in `McpEvent.server` and offers `mcp__<plugin>__<server>__<tool>`, and a spawned
child's tokens are billed under `<plugin>__<agent>`. **No table, column or index
was added** — `CHECKPOINT_FORMAT` is unmoved at 7. It also makes a name collision
impossible rather than unlikely: a bundle cannot occupy a name the operator uses,
and ids are unique, bounded and may not contain `__`.

**A broken bundle costs exactly itself.** Loading has no error path. A directory
with no manifest, unparseable TOML, an unknown key, a malformed or duplicate id,
or a contribution its scope may not make is dropped — recorded on
`Plugins::dropped()` with its reason and reported as `EventKind::PluginDropped` —
while every bundle that did load is applied and the run proceeds. An application
that wants a broken bundle to be fatal writes one `if`.

### Added

- `io_harness::plugin`, with `Plugins`, `Plugin`, `Dropped`, `PLUGIN_FILE`,
  `NAMESPACE` and `MAX_ID`.
- `Config::plugins()`, and `[[plugin]]` in `io.toml`, `io.local.toml` and the
  user-scope file. The array **appends** across scopes, like `policy.layers` and
  `[[agent]]`.
- `Plugins::apply_to`, `Plugins::apply_to_policy`, `Plugins::apply_to_hooks` and
  `Plugins::templates`, plus `Plugins::get`/`iter`/`names`/`len`/`is_empty`/
  `dropped`/`none`.
- `TaskContract::plugins` and `TaskContract::with_plugins`.
- `EventKind::PluginLoaded { plugin, contributions }` and
  `EventKind::PluginDropped { plugin, why }`, with their `EVENT_NAMES` entries.
- [`docs/guide/plugins.md`](docs/guide/plugins.md).

### Changed

- **BREAKING (API)** `TaskContract` gains the `plugins` field and becomes
  `#[non_exhaustive]`. Both are the same break for a struct literal or an
  exhaustive destructuring outside this crate, so they are paid together rather
  than twice — and the second one means the next field this type gains is free.
  *Migration:* build contracts with `TaskContract::new` or
  `TaskContract::workspace` and the `with_*` builders, which is what every
  documented caller already does and which is unchanged. Replace any
  `TaskContract { .. }` literal with a constructor plus builders, and add a `..`
  to any exhaustive `let TaskContract { .. } = contract` destructuring. No field
  was removed, no signature moved, and reading any existing field still compiles.

### Deprecated

### Removed

### Fixed

### Security

## [0.34.0] - 2026-08-02

A second model checks the first, one failed gate is retried on its own, and which
model answers stops being fixed for the whole run.

Every criterion this crate has shipped is a command or a string: `Command` runs an
argv and reads its exit status, `WorkspaceFileContains` reads a file for a needle,
`EachCompilesRust` compiles. That is the right default and it cannot catch the
change that compiles, passes the suite, and is still the wrong change.

**`Verification::Review` is a criterion a model answers.** It carries a rubric the
caller wrote; a `Reviewer` — `ModelReviewer` over any provider, or a human, or a
stub — reads the goal, the rubric and the files the run wrote, and returns a
`Review { passed, reasons }`. The reasons reach the trace and the `Observer` as
`EventKind::Reviewed`, because a refusal a human cannot argue with is a gate
nobody trusts twice.

**A model may not review its own work.** With `allow_self_review: false` (the
default), a reviewer whose model is the model that produced the change is refused
with `Error::Config` **before a request is built** — not warned about, and not
after the answer arrives. Set the flag to `true` to say you meant it.

**Every gate evaluation is durable, and `Errored` is not `Failed`.** A new
`gate_attempts` table records what each gate decided: `Passed`, `Failed`, or —
the distinction the crate has never had — `Errored`, a criterion that could not be
evaluated at all. `Store::gate_attempts` and `Store::last_gate_attempt` read them
back; attempts are appended, never overwritten, so the history of what a gate said
survives a retry.

**`retry_gate` re-runs the criterion and nothing else.** A run that spent forty
model calls and then lost its review gate to a 529 no longer has to run the task
again to get a verdict: `retry_gate` evaluates the criterion against the workspace
as the run left it, from the run's own checkpoint. Its steps rows, its token
ledger and its files are unchanged by the retry except for the review it asked
for. It refuses a gate that `Failed` — that criterion ran and answered, and the
work has to change before the answer can.

**`Routing` changes which model the run asks, while it is running.** Three rules,
applied by the run itself and set on the request that is actually sent:
`escalate_after(n, model)` after n consecutive failed gates, `downshift_under(bytes,
model)` while the change is small, and `require_primary()`, which asks
`Provider::reachable()` before the first step and refuses to start rather than
running an unattended job on a fallback nobody chose. `EventKind::Routed` is
emitted once, at the transition.

### Added

- `Verification::Review { rubric, allow_self_review }` — a criterion whose check is
  a model reading the change against a rubric.
- `Reviewer`, `Reviewing`, `ReviewRequest`, `Review` and `ModelReviewer<P>` — who
  answers a review, and the model-backed implementation.
- `TaskContract::with_reviewer`, and `TaskContract::reviewer`.
- `GateOutcome` (`Passed`, `Failed`, `Errored`), `GateAttempt`,
  `Store::put_gate_attempt`, `Store::gate_attempts`, `Store::last_gate_attempt`,
  and the additive `gate_attempts` table with its `(run_id, id)` index.
- `retry_gate` and `retry_gate_observed` — re-run one run's criterion, from its own
  checkpoint, without re-running a step.
- `Routing`, `TaskContract::with_routing`, and `TaskContract::routing`.
- `Provider::reachable` and `Provider::model_hint`, both **defaulted**, so every
  existing implementation compiles and behaves exactly as it did in 0.33.0.
- `EventKind::Reviewed { passed, reasons }` and `EventKind::Routed { from, to, why }`,
  with their `EVENT_NAMES` entries.

### Changed

- **BREAKING (API)** `Verification` gains a variant and becomes
  `#[non_exhaustive]`. Both are the same break for an exhaustive `match` on it
  outside this crate, so they are paid together rather than twice.
  *Migration:* add a wildcard arm — `_ => { /* a criterion this code does not
  handle */ }` — to any `match` on `Verification`. Constructing every existing
  variant, and every `with_verification` call, is unchanged. Nothing was removed
  and no signature moved.

### Deprecated

### Removed

### Fixed

### Security

## [0.33.0] - 2026-08-02

Two processes, one run.

A run used to belong to the process that started it. `Observer` is an in-process
callback, so its events existed only where the run did; `resume_*` reattaches to a
run that has **stopped**. A run left unattended and parked on an approval was
therefore unreachable — the only way to make progress was to kill the process, at
which point `resume_with_decision` worked, on a run that was no longer live.

**`Broadcast` makes the stream durable.** It is an `Observer` that wraps another
one, writes each `RunEvent` to a new `run_events` table and passes it on. Wrap it
around whatever observer you already have and hand it to any of the fourteen
`*_observed` entry points; nothing else changes. It is a decorator on purpose: what
a second process reads back is the *same* value the in-process observer received,
not a reconstruction assembled from the trace's twenty tables — and a
reconstruction would drift the first time one of those tables gained a column the
event did not, with no test able to catch it. The test asserts `RunEvent` equality
against what the owning observer collected, and its control is a run with no
`Broadcast`, which must read back empty.

**`Attach` is what a second process opens.** A `&Store` on the same file, a run id
or a tree's root, and a cursor the caller chooses: from the beginning, `from_now()`,
or `from_cursor(n)` for a reader that recorded where it was. `poll()` returns what
is new and advances. `waiting()` reports what the run is parked on — a
`Waiting::Approval`, a `Waiting::Question` or a `Waiting::Plan`. All three, decided
explicitly: a pending plan is as much a stopped run as a pending approval is, and
leaving it out would have made "answer what it is holding" a two-thirds claim.

`answer_approval`, `answer_question` and `answer_plan` write into the same durable
row the run already writes, and the run — sitting in its `Approver`, `Responder` or
`PlanGate` — picks it up and carries on. The four gate call sites now write their
row **before** consulting the in-process gate (the ordering `put_plan` has had since
0.31.0) and then await the gate and the row together, taking whichever answers
first.

**First answer wins, and the loser is told.** Each answer returns `bool`: `true` if
this caller's answer is the one the run acted on. Two operators answering one
approval stops being hypothetical the moment a run is reachable from more than one
place, and a harness that lets both writes land and then acts on the second has a
defect nobody can see. It is a single conditional `UPDATE` in the store rather than
a read followed by a write — and the run reads the decision back **from the row**,
in both arms, so what an event and a `policy_events` row report is what the store
holds rather than what the racing caller proposed.

**An attaching process reads and decides; it does not take ownership.** `Attach` has
no method that starts, resumes or steps a run, which is the mechanism rather than
the advice — a source-reading test asserts it, with a control that splices such a
method in and must name it. The failure modes stay bounded and both are executed
with real kills: SIGKILL the attached process and the owner runs to completion with
a stream continuous across the kill; SIGKILL the owner and what is left is exactly
the unresolved row `resume_with_decision` has consumed since 0.7.0.

**No new dependency and no migration.** `cargo tree` still reads 402 lines: the
transport is the SQLite store both processes already open, the wait is
`tokio::time`, the race is `tokio::select!`. And `Store::open` has set
`journal_mode = WAL` and a five-second `BUSY_TIMEOUT` since 0.12.0, so two
connections to one file needs no on-disk change at all — `CHECKPOINT_FORMAT` stays
7 and a 0.32.0 binary reads a 0.33.0 store without ever naming the new table.

### Breaking changes

- **BREAKING** — `Store::resolve_pending`, `Store::answer_question` and
  `Store::decide_plan` return `Result<bool>` instead of `Result<()>`. The bool is
  whether *this* call is the one that landed, which is what makes the race
  decidable instead of silent. The last two also return `Ok(false)` where they used
  to return `Err(Error::Resume)` for an already-answered row: losing a race is a
  fact about the race, not an error. A row that does not exist is still an error.
  *Migration:* a caller writing `store.answer_question(id, a, by)?;` as a statement
  recompiles unchanged. A caller matching on the already-answered error moves to
  the bool:

  ```rust
  // was
  if store.answer_question(id, answer, "human").is_err() { /* already answered */ }
  // now
  if !store.answer_question(id, answer, "human")? { /* somebody else got there first */ }
  ```

  `resume_with_answer` and `resume_with_plan_decision` still refuse a second answer
  with `Error::Resume`, and `resume_with_decision` now refuses an
  already-decided request the same way. Driving a run twice from two answers is a
  different thing from writing one, and only the write path returns a bool.

### Added

- `Broadcast`, an `Observer` decorator that writes every event it forwards to the
  store. It takes a `Store` of its own — `Observer` is `Send + Sync` and
  `rusqlite::Connection` is `Send` and not `Sync` — which is the release's own
  premise rather than a workaround: two connections to one file is exactly what
  attaching is.
- `Attach` and `Waiting`, in a new `attach` module, plus `POLL_LIMIT`.
- `Store::put_event`, `Store::events_since`, `Store::tree_events_since`,
  `Store::event_cursor` and `Store::unresolved_approvals`.
- A `run_events` table and its `(run_id, id)` index. Additive, `CHECKPOINT_FORMAT`
  still 7. `id` is `AUTOINCREMENT` so the cursor is globally monotonic and one
  number orders a whole tree's interleaved stream; `kind` is denormalised out of
  the JSON so a reader can filter without deserialising, and is deliberately in no
  index — it is the control column the query-plan test needs.
- `run::ATTACH_POLL`, the interval at which a parked run checks whether a second
  process answered for it. 200 ms, and never reached at all when the in-process
  gate answers, so an unattended run pays nothing for a feature it is not using.
- `examples/attach_fixture`, a deterministic offline run that parks *live* in a
  gate that never returns — unlike `crash_fixture`, `plan_gate_fixture` and
  `fleet_fixture`, which park after the run has stopped.

### Changed

- The durable row for an approval and for a question is written before the
  in-process gate is consulted rather than after it. A row now exists for approvals
  that were answered instantly, so "a `pending_approvals` row exists" no longer
  means "the run is waiting" — read `Store::unresolved_approvals`, which
  `Attach::waiting` uses.
- `pending_questions.answered_by` and `plans.decided_by` carry `attached` when the
  answer came from a second process, against `responder` / `gate` for the
  in-process arm, so an audit can tell which.
- Reading a tree's event tail is `CROSS JOIN ... INDEXED BY`, not a plain join, for
  the reason 0.32.0's queue read is. Left to itself the planner drives from
  `run_events` by rowid — every tree's events from the cursor forward — and probes
  the recursive CTE through an automatic index it has to build, because a CTE is a
  co-routine it cannot seek into. Measured over 40 trees of 40 events: 0.093 ms
  seeking against 0.179 ms, and the gap grows with the number of trees in the file.

## [0.32.0] - 2026-08-02

A fleet holds the ceiling instead of hitting it.

`Containment` had one agent limit doing two jobs badly. `max_total_agents`
refused the spawn that crossed it, so a task that wanted a hundred agents failed
at its hundred-and-first child rather than running a hundred at a time until it
was done; and `max_concurrent` was never a tree-wide cap at all — it was the
width of one `buffered()` inside one parent's step, so two parents fanning out at
once got twice the concurrency they had asked for, and a child past the width was
not queued, it simply never started, invisibly.

0.32.0 separates the two. **`max_concurrent_agents` throttles.** A spawn past it
does not fail: the child takes a place in a FIFO queue for its tier and starts
when a slot frees. **`max_total_agents` still refuses**, alongside the spend and
duration ceilings, because some limits are meant to stop a run and some are meant
to shape it. A caller who wants five hundred children four at a time now writes
`Containment { max_total_agents: 501, max_concurrent_agents: 4, .. }` and gets
exactly that.

**The cap is per tier, not tree-global, and that is the deadlock argument rather
than an oversight.** Each nesting level has its own set of slots. A parent holds a
slot at its own tier while it waits for children at the tier below, so the wait
graph runs strictly downward and cannot contain a cycle; one global pool would
hang the first time the agent holding the last slot spawned a child, because only
that child could free it. The honest consequence, stated here because a reader
should not have to derive it: a tree of depth *d* can hold up to
`max_concurrent_agents * d` agents working at once.

**The fleet is visible while it drains.** `EventKind::Fleet { tier, working,
queued, done }` reaches an `Observer` on every enqueue, every admission and every
completion. Per tier, because one tree-wide number cannot tell an operator
whether the fan-out at depth two is stuck behind the one at depth one. The slot is
released on the way out of *every* path — finished, paused for a human, or an
error propagating — because a slot freed only on the happy path is a fleet that
stops draining the first time something goes wrong.

**The queue is durable, and a queued child is not charged.** Waiting is a row in a
new `agent_queue` table, written when a child starts waiting and deleted when it
is admitted, so a tree that drains leaves none and a tree that is killed leaves
exactly the backlog it had. A process that comes up afterwards reports that
backlog — at the tier it had — before it authorises a provider, let alone calls
one. And a child that only ever waited has no run row, no step rows and no tokens
against the tree's ceiling, because nothing about it was started.

That last claim is tested for the shape of its failure rather than its outcome. A
queue silently re-derived from the spawn calls the model repeats produces the same
children, the same outcome and the same final state, so the test asserts an
**absence**: one `agent_queue` row is deleted from a killed fixture's store, and
the depth the resumed process reports must be one smaller — and must arrive before
the resumed provider has been handed a single request. Either fact alone passes on
a re-derivation.

`cargo tree` still reads 402 lines. The queue is `tokio::sync::Semaphore`, which
the `sync` feature already provided, and one SQLite table.

### Breaking changes

- **BREAKING** — `Containment::max_concurrent` is renamed
  `max_concurrent_agents`, and its meaning changes with it: it was the width of a
  per-step `buffered()` inside one parent, it is now a tree-wide, per-tier
  throttle that queues rather than skipping. The rename is the smaller half of
  this break — read the meaning, not just the name. A caller who was relying on
  several parents each getting the full width at the same time will now see a
  lower effective concurrency, and should raise the number.
  *Migration:* rename the field in any struct literal. Stored configuration needs
  no change at all: the field carries `#[serde(alias = "max_concurrent")]`, so a
  file written for 0.31.0 still deserialises.

  ```rust
  let containment = Containment {
      max_total_agents: 501,
      max_concurrent_agents: 4, // was: max_concurrent
      max_depth: 2,
      max_total_tokens: 500_000,
      max_total_cost: None,
      max_total_duration: None,
  };
  ```

- **BREAKING** — `EventKind` gains one variant, `Fleet`. `EventKind` has been
  `#[non_exhaustive]` since 0.24.0, so an exhaustive `match` already carries a
  wildcard arm and this costs it nothing.
  *Migration:* none required. `EVENT_NAMES` gains `fleet`, so a `[[hook]]` may now
  filter on it.

### Added

- `FleetTally { working, queued, done }` and `Ledger::tally(tier)`, so an
  application holding the ledger can read a tier's shape without an observer.
- `Store::enqueue_agent`, `Store::dequeue_agent` and `Store::queued_agents`. The
  first two answer whether they changed a row, which is what lets a resumed tree
  tell a fresh wait from a replayed one without guessing; `queued_agents` reads a
  whole tree's backlog as `(tier, goal)` in FIFO order, and answers "what was
  still waiting when this died" long after the process is gone.
- An `agent_queue` table and its unique index on `(parent_run_id, step, goal)`.
  Additive, `CHECKPOINT_FORMAT` still 7 — a 0.31.0 binary never queries it. The
  unique key doubles as the dedupe: `INSERT OR IGNORE` is the whole of "re-queue
  this only if the store does not already hold it", which is what stops a resumed
  tree's replay from doubling the depth it just restored.
- `examples/fleet_fixture`, a deterministic offline tree that fills its queue and
  parks, so a test can `SIGKILL` a process that is unambiguously alive.
- A fleet section in `docs/CONTRACT.md` and in the composition and observability
  guides, including the per-tier ceiling stated as a limitation rather than
  implied.

### Changed

- The per-step spawn fan-out is now `buffered(spawn_calls.len())` — every spawn
  reaches the ledger, and the ledger decides how many run. The collection order is
  still the order the model asked for them, so 0.12.0's deterministic replay is
  unaffected.
- Reading a tree's backlog is `CROSS JOIN ... INDEXED BY`, not a plain join. A
  recursive CTE is a co-routine SQLite cannot seek into, so left to itself the
  planner scans every tree's queue and probes the CTE — right for a file holding
  one tree, wrong for a file holding a hundred, and the statistics cannot tell it
  which it has. Measured over 200 trees of 100 waiting children: 0.057 ms seeking
  against 0.593 ms scanning.

### Fixed

- `resume_tree_observed` never called `record_run_policy`, so a tree resumed in a
  second process left no record of the boundary it was actually resumed under and
  an audit could read only the boundary of the process that died. A standing
  defect since 0.5.0, listed in `docs/CONTRACT.md`, fixed here because this
  release was already in that function.

## [0.31.0] - 2026-08-01

The agent proposes before it acts, and a caller can say how hard it is allowed to
think. Two knobs on the same moment — the request, before the agent does
anything — which is why they ship together.

**The plan gate.** Hand a `TaskContract` a `PlanGate` and the run opens in a
planning phase: the agent may read the workspace and may change nothing in it, and
the only way out is a `Plan` — ordered steps, each optionally naming the sub-agent
that will own it — that a human approves, sends back with a correction, or
cancels. When nothing in the process will answer, the plan is persisted and the
run stops with `RunOutcome::AwaitingPlan`. That process may then exit;
`resume_with_plan_decision` continues the run under its original id, from
anywhere, later.

The enforcement is the part worth reading. The phase is held by a `plan-gate`
policy layer denying every `Act::Write` and `Act::Exec`, not by a list of tool
names — so `write_file`, `edit_file`, `exec`, the shell tools, `git`, every
registered `Tool` and every MCP tool are covered by the one deny-first resolution
they already share, and a tool added tomorrow is covered the day it lands. A
refusal during the phase appears in the trace attributed to the `plan-gate`
layer, legible to someone who has never heard of this feature. `remember` is the
single write the policy cannot see — it lands in this crate's own store — and is
refused explicitly, so "nothing is written" means nothing rather than nothing the
policy happens to see.

Whether the gate has been satisfied is asked of the **store**, at every loop
entry, never carried in memory. That one decision is the whole of the durability
claim: a run approved in one process and killed in the next does not plan again,
and one that was never approved does not start writing because the approval died
with the process that held it. Proven against a real `SIGKILL`, with the
resumed run asserted never to be offered `propose_plan` again — the only
observable that separates "read the approval from the store" from "planned again
and was approved again".

This is **not** the 0.21.0 `todo_write` tool. That one records a plan the agent is
already executing so an operator can watch it. This one executes nothing until an
answer arrives. Both exist and neither replaces the other.

**Reasoning effort per role.** `AgentDef` carried a role, a model and a narrowed
boundary and could not say how hard the model should think, so a `searcher` doing
lookups and a `critic` looking for what is missing paid the same reasoning bill.
`Effort` — `Low`, `Medium`, `High` — sits on `AgentDef` and on `TaskContract`, and
reaches the wire projected onto whatever each vendor calls it:
`reasoning.effort` on OpenRouter, `reasoning_effort` on OpenAI and `Compatible`,
and on Anthropic — which has no tiers at all — a `thinking` budget with
`max_tokens` raised to clear it, because Anthropic refuses a request whose budget
is not strictly below the cap.

Where a vendor cannot honour a tier the crate says so rather than pretending.
**OpenAI's Chat Completions returns no reasoning text**, so `Effort` changes how
the model behaves and leaves `CompletionResponse::reasoning` at `None`;
`Usage::reasoning_tokens` is the only visibility on that path. The whole per-vendor
table, including what each one does not do, is in `docs/CONTRACT.md`.

Where the thinking *is* returned it reaches an `Observer` as
`EventKind::Reasoning` and goes nowhere else. It is never appended to the
observation ledger and therefore never enters the prompt assembled for the next
turn: a vendor bills thinking once as output, and a harness that folded it into
the next request would be billed for it again as input, every turn, for the rest
of the run.

`cargo tree` still reads 402 lines. Nothing here needed a dependency.

### Breaking changes

- **BREAKING** — `CompletionRequest` gains `effort: Option<Effort>`, how hard the
  model should think. Every existing caller meant `None`, which is the body every
  release before 0.31.0 sent — asserted, not assumed: the vendor body tests
  compare a no-effort request against the whole 0.30.0 body rather than only
  against the absence of the key. This affects only code that builds a
  `CompletionRequest` with a struct literal listing every field.
  *Migration:* add the field, or spread the default.

  ```rust
  let request = CompletionRequest {
      system: system.into(),
      user: user.into(),
      tools,
      ..Default::default() // or: effort: None,
  };
  ```

  An out-of-tree `Provider` that ignores the field keeps compiling and is honestly
  non-thinking, which is the same bargain `model` (0.21.0) and `web` (0.22.0)
  offered.

- **BREAKING** — `CompletionResponse` gains `reasoning: Option<String>`, the
  thinking the provider returned. This affects only code that constructs a
  `CompletionResponse` with an exhaustive struct literal — which every out-of-tree
  `Provider` does, since `complete` must return one.
  *Migration:* spread the default, or set `reasoning: None`, which is what a
  provider that returns no thinking honestly means.

  ```rust
  Ok(CompletionResponse {
      text: Some(answer),
      tool_calls,
      ..Default::default() // or: reasoning: None,
  })
  ```

- **BREAKING** — `RunOutcome` gains two variants, `AwaitingPlan { plan_id, steps }`
  and `PlanRejected { steps }`, so an exhaustive `match` on it needs two more arms.
  *Migration:* handle `AwaitingPlan` by showing `Store::plan(plan_id)` to a human
  and calling `resume_with_plan_decision`; treat `PlanRejected` as terminal, the
  way `Denied` and `Cancelled` are. A caller that never registers a `PlanGate` can
  never receive either.

- **BREAKING** — `EventKind` gains three variants, `PlanProposed`, `PlanDecided`
  and `Reasoning`. `EventKind` has been `#[non_exhaustive]` since 0.24.0, so an
  exhaustive `match` already carries a wildcard arm and this costs it nothing.
  *Migration:* none required. `EVENT_NAMES` gains the three spellings, so a
  `[[hook]]` may now filter on `plan_proposed`, `plan_decided` or `reasoning`.

### Added

- `Plan`, `PlanStep`, `PlanVerdict`, the `PlanGate` trait and `PlanReview`, in
  `src/approve.rs` beside `Question` and `Responder`, whose shape they copy: the
  review returns `Option<PlanVerdict>` and `None` means "nobody in this process can
  answer", which is what makes the run persist and pause rather than guess.
- `PlanGateNone` (the honest unattended default — every plan pauses for a human),
  `AcceptPlan` (tests, and callers who want the shape without the wait) and
  `StdinPlanGate` (a CLI: `y` approves, `n` cancels, an empty line defers, anything
  else is taken as the correction).
- `TaskContract::with_plan_gate` and `TaskContract::with_effort`;
  `AgentDef::with_effort`, which also deserialises from an `io.toml` roster.
- `PROPOSE_PLAN_TOOL` (`propose_plan`), offered only while a gate is registered and
  unsatisfied, and withdrawn the moment a plan is approved.
- `resume_with_plan_decision`, `resume_tree_with_plan_decision` and their
  `_observed` twins, matching the four `resume_*_with_answer` entry points.
- `PendingPlan` and five `Store` accessors — `put_plan`, `plan`, `approved_plan`,
  `decide_plan`, `plans` — mirroring the question accessors. `decide_plan` refuses
  a second verdict with `Error::Resume`, the way `answer_question` refuses a second
  answer.
- `Effort`, with `Ord` so "at least Medium" needs no `match`, `FromStr` for a
  configuration file, and `thinking_budget()` for the one vendor that has no tiers.
- A `plans` table. Additive, `CHECKPOINT_FORMAT` still 7 — a 0.30.0 binary never
  queries it — indexed on `(run_id, verdict)`, which is the shape of the question
  the loop asks at every step. Measured at 26µs per lookup over 20,000 plan rows.
- The plan-gate and reasoning-effort sections of `docs/CONTRACT.md`, including the
  per-vendor table and what each vendor does not do, and new sections in the
  permissions and providers guides.

### Changed

- Anthropic's `max_tokens` is no longer unconditionally 8,192: it is raised to
  clear the thinking budget when, and only when, a tier was asked for. A request
  with no tier sends exactly the body 0.30.0 sent.

## [0.30.0] - 2026-08-01

The store can answer *why*, for three questions it could already answer the
*what* of. Which file decided a setting, when four scopes merge and a project
file may narrow what you set in your own. Whether a remembered fact is something
somebody decided — and whether a run may overwrite it, or is refused and told so.
And how often a run was verified first try, which gate phase fails most, and how
many runs a fallback, a replan or a resume rescued.

Nothing breaks. Two nullable columns, one new table and six indexes: a 0.29.0
database opens, resumes and replays unchanged, and a 0.29.0 binary still reads a
store this release wrote — both directions executed against a real 0.29.0 build
rather than argued.

### Added

- **`Config::origin(key)` and `Config::origins()`**, reporting the scope and the
  file that decided each key by dotted path — `run.max_steps`,
  `policy.defaults.exec`. `Config::sources()` answers which files were read and
  keeps answering exactly that; this answers which of them won, which is the half
  a reader needs when a value is not the one they set. An empty answer means no
  file named the key: that is the crate's default speaking, and naming a file for
  it would be an invention.
- **`config::Origin`**, the scope and path pair those return. Ordinary keys have
  exactly one; `policy.layers` and `agent` append across scopes and report every
  file that contributed, in order, because naming a single winner for a value
  three files built would be a lie.
- **`MemoryKind`** (`Fact` or `Decision`, `#[non_exhaustive]`) and
  **`MemoryEntry::pinned`**. A pinned entry is not overwritten by a run and is
  not evicted to hold the caps; pinning is a caller's act, never an agent's.
- **`Store::memory_write`** and **`MemoryWrite`**, the full form of `memory_put`:
  it takes the kind and it *reports the refusal*. A caller that cannot tell a
  write from a refusal will tell the model it corrected something it did not.
- **`Store::memory_pin`**, which is how a human makes a correction stick when the
  agent keeps re-learning something wrong.
- **`Store::memory_recalls` and `MemoryRecall`**, the per-run record of which
  entries a run actually drew on. `Store::memory_list` says what the agent knows
  about a workspace; this says what *this run* used, which is the half that tells
  a reader whether an entry was load-bearing. `Assembled::recalled_keys` carries
  the same list during the run.
- **`Store::runs_by_outcome`, `Store::runs_by_day` and
  `Store::gate_failures_by_phase`**, returning `Tally` rows. Grouped counts, in
  the shape 0.18.0's spend groupings established: rows out, the crate renders
  nothing.
- **`Store::first_try` and `FirstTry`** — runs, successes, and successes with no
  failed gate phase. Three counts and deliberately no rate: *of the ones that
  worked* and *of everything we tried* are both legitimate denominators, and
  returning one number would pick between them invisibly.
- **`Store::recovery` and `Recovery`** — fallbacks, replans and resumes. There is
  no escalation count, because nothing records an escalation as an event and an
  escalation is the opposite of a rescue: it is the run handing the problem back.

### Changed

- **A run that tries to overwrite a pinned memory entry is refused, told so, and
  the attempt is recorded** as a `memory_refused` row in the trace. The refusal
  also reaches the model as an observation, so an agent cannot proceed believing
  it corrected something it did not.
- **The memory table gains two nullable columns and the store gains a
  `memory_recalls` table and six indexes.** Additive only: `CHECKPOINT_FORMAT`
  stays 7, an entry written before this release reads back as an unpinned `Fact`
  — which is what it was — and a 0.29.0 binary, whose queries never name the new
  columns, still opens and reads a migrated database.
- **`Assembled` gains `recalled_keys`.** The type is constructed through
  `..Default::default()`, so this is additive for any caller building one.

## [0.29.0] - 2026-08-01

The model a run uses stops being a choice between three vendors. A provider is a
base URL, an auth style, a key and a model name, so every OpenAI-shaped endpoint
is reachable without writing one — thirteen hosted vendors and eight runtimes a
developer starts on their own machine, behind named constructors. A model on
your laptop is the half that had no equivalent before, and it is the half that
costs nothing to run.

A connected provider also says what it can run and what that costs, fetched from
the vendor at run time rather than compiled into this crate. Nothing here
breaks: `Provider` gains a defaulted method, `ProviderSpec` gains the variant
0.27.0's `#[non_exhaustive]` was added for, and `Price` is untouched.

### Added

- **`Compatible`, one provider for every OpenAI-shaped endpoint.** `openai.rs`
  and `openrouter.rs` were the same 160 lines apart from four strings over a
  wire module that was already shared, so a third vendor is a row in a table
  rather than a file. Twenty-one presets behind named constructors —
  `Compatible::groq(key, model)`, `Compatible::ollama(model)` — covering Groq,
  xAI, Mistral, DeepSeek, Together, Fireworks, Cerebras, Perplexity, Gemini
  through its compatibility endpoint, Moonshot, Zhipu, Qwen and MiniMax, plus
  Ollama, llama.cpp, vLLM, LM Studio, LocalAI, Jan, SGLang and KoboldCpp.
  `Compatible::new` reaches anything else that speaks the format.
- **`Auth`**, `Bearer` or `None`, `#[non_exhaustive]`. A local runtime sends no
  credential header at all rather than an empty bearer, which some servers
  reject.
- **`Provider::models()`**, returning the vendor's own catalogue: ids, and per
  model whatever the vendor stated — context length, maximum output, whether it
  takes images or tools, and the price. **Defaulted to an empty list**, so every
  out-of-tree `Provider` keeps compiling; a required method here would have
  broken the crate's one extension point.
- **`ModelInfo` and `PriceSource`.** `ModelInfo::price` is `Option<Price>` and
  `None` means the vendor did not say — never `Price::ZERO`, which would report
  real spend as free. A local runtime is the one place zero is true and it is
  recorded as a stated zero. `PriceSource` says whether a number came from the
  vendor or from a named reference, so an operator reading a cost can tell which
  they have.
- **`Reference`, an opt-in reference price catalogue** for the vendors that
  publish none, defaulting to a keyless public catalogue and configurable to a
  mirror. Off by default: it dials a host the caller did not name, so when it is
  on that host appears in `Provider::endpoints()` and a policy that denies it
  makes the run **refuse** rather than silently skip the lookup. Matching is an
  exact slug or one documented normalisation, and a miss stays `None` — never a
  nearest guess.
- **`pricing::PriceTier`, `PriceTable::with_tiers` and `PriceTable::tiers`.** A
  model does not necessarily have one price: many vendors charge more once a
  prompt passes a length threshold and the step is usually a doubling, which a
  long agentic run crosses routinely. The highest floor a prompt reaches prices
  the whole request, as the vendors bill it. A table with no tiers registered
  prices exactly as it did in 0.28.0.
- **`Compatible::price_table()`**, building a `PriceTable` dated by the moment it
  was read — so a derived cost stops depending on a table maintained by hand
  with an `as_of` somebody has to remember to update.
- **`[[provider]] kind = "compatible"`** in `io.toml`, taking exactly one of
  `preset` and `base_url` plus `model`, and optionally `api_key`, `auth`, `name`
  and `reference_prices`. Naming both or neither is refused by the entry's
  index; an unknown preset is refused listing the ones that exist.
- **`docs/guide/providers.md`**, and a section in `docs/CONTRACT.md` stating each
  vendor's divergence from the OpenAI wire plainly — including the one that
  fails silently: **vLLM and SGLang emit no tool calls at all** unless the
  server was started with a tool-call parser flag a client cannot set.

### Changed

- The README's provider paragraph named three vendors four lines under a tagline
  that says "any provider". It now names what the crate actually reaches.

### Security

- The reference price catalogue is the first thing in this crate that would dial
  a host the caller never configured. It is opt-in, and when enabled its host is
  authorised through the same `Act::Net` boundary as every other endpoint before
  the run's first step — refused rather than skipped.

## [0.28.0] - 2026-08-01

An operator can now shape a run without writing Rust. `[[hook]]` tables in
`io.toml` fire on the events the observer channel has emitted since 0.12.0, so an
audit log is a path, a notification and a formatter are an argv, and a local
policy check is that argv with one more key. Each reaches the run through the
`Observer` the crate already had — `Config::hooks()` returns one and the caller
installs it — so no run loop changed.

And a file the agent changed can be put back the way it was. Every write already
read the previous contents to measure the edit; this release keeps the first one
it sees per file, in the store, so `rewind` restores a path to what it was before
the run first touched it — including deleting a file the run created — and does so
after a crash and a resume.

### Added

- **`[[hook]]` and `Hooks`.** An array of tables in `io.toml`, each naming the
  events it wants and one thing to do with them. `on` names events by the wire tags
  `EventKind` serializes to and an absent `on` is every event; `append` writes one
  JSON line per matching event; `run` spawns a fixed argv with that JSON on its
  stdin, bounded by `timeout_ms` and killed past it; `on_failure = "cancel"` turns a
  failing hook into an ended run, which is the whole of a local policy check.
  Exactly one of `append` and `run` is required. `Config::hooks()` returns a `Hooks`,
  which implements `Observer`, and the caller passes it to `run_observed`,
  `resume_observed` or any of the tree forms. There is no shell anywhere: a hook's
  argv is a TOML array and reaches the process unsplit.
- **`[[hook]]` is refused in a project-scoped `io.toml`, whole.** Not its executing
  half. A hook that runs an argv is the `${cmd:...}` primitive 0.27.0 refused in that
  scope, arriving one release later; a hook that appends is a write to a path a
  cloned repository chose, which is the same hazard by a shorter route. Refused
  inside a `[profile.<name>]` too. `io.local.toml` and the user-scope file take them
  unchanged. This is 0.27.0's narrow-never-widen rule applied to a new key, and it
  still does not claim that cloning a repository is safe.
- **`rewind` and `Rewind`.** `rewind(&workspace, &store, run_id, path)` puts a file
  back the way it was before that run first wrote it. Four answers, because a caller
  must be able to tell them apart: `Restored` with the `Wrote` the workspace
  returned, `Removed` for a file the run created, `NotKept` with a reason for one
  whose previous contents were over the 1 MiB cap or were not UTF-8 — nothing is
  changed and the file is never truncated — and `NotRecorded` for a path this run
  never wrote. Restoring writes through `Workspace::write_file` and removing checks
  `Act::Write` first, so an undo obeys the policy the edit obeyed.
- **A durable restore point per file per run.** A new `snapshots` table records what
  was there before the run's *first* write to each path, so a rewind survives a
  crash and a resume rather than living in memory. A new table is additive:
  `CHECKPOINT_FORMAT` stays 7, and an 0.27.0 store opens, resumes and replays
  unchanged.
- **A new guide page, [Hooks](docs/guide/hooks.md)**, and a `### [[hook]]` section in
  the configuration guide.

### Fixed

- **The event census in `src/observe.rs` is now a census.** `every_kind()` matched on
  the items of its own vector, which proves the match arms exhaustive and never
  proves the vector complete — so `TodoWrote`, `QuestionAsked` and `QuestionAnswered`
  had been absent from it, and therefore round-trip-untested, since 0.21.0. The
  complete list of wire tags now lives beside the enum, is proven complete by a test
  that reads `pub enum EventKind` out of the source, and `every_kind()`'s tags are
  asserted **equal** to it rather than contained in it. It is load-bearing rather than
  tidy: a hook names an event by its tag, so a tag the crate forgot would be a hook an
  operator writes and that silently never fires.
- **The pre-write read no longer reports an unreadable file as an empty one.** Both
  write paths read the previous contents with `read_to_string(..).ok().unwrap_or_default()`,
  which is harmless for a line count and would be data loss for a rewind. Existing
  line counts are unchanged.

## [0.27.0] - 2026-08-01

One file is now the whole configuration. `io.toml` gains the piece it has been
missing since 0.19.0 — which provider and model to run, and what stands behind it
— so an embedder reads a spec instead of writing provider-selection code. Beside
it: a section the crate stores and never validates, so the programs built on this
one keep their settings in the same file; profiles that overlay a named set of
choices; a credential that can come from a command; and the project instructions a
repository already carries, discovered rather than pasted into a goal string.

And a project-scoped file stops being able to widen a boundary. `io.toml` is
committed and arrives with a `git clone`, and the keys whose only effect is to
remove containment are refused in that one scope — a project file may narrow, and
may never widen.

### Breaking changes

- **BREAKING (behaviour)** — a project-scoped `io.toml` may no longer set four keys
  to the value that *widens* a boundary. `policy.defaults.exec = "allow"`,
  `policy.defaults.net = "allow"`, `sandbox.allow_network = true` and
  `sandbox.force_floor = false` now fail the load naming the key, in the project
  scope only, including inside a `[profile.<name>]`. The narrowing value of each is
  still legal there. Also refused in that scope: `${cmd:...}`, anywhere in the file.
  `Config::from_toml` is the project scope and refuses them too.
  *Migration:* move the key out of `io.toml` and into `io.local.toml` (gitignored,
  yours) or your user-scope file, where all five are accepted unchanged and mean
  exactly what they meant before. Nothing else changes; a project file that only
  ever narrowed needs no edit.

### Added

- **`[[provider]]` and `ProviderSpec`.** The first table is the provider a run
  uses, each later one the next link in the fallback chain, in the order written.
  `Config::provider_spec()` and `Config::fallback_specs()` project them. It yields a
  *spec* rather than a provider because `Provider::complete` returns `impl Future`,
  so the trait is not dyn-compatible; the application constructs from the spec and
  nests `Fallback` itself. `ProviderSpec` is `#[non_exhaustive]` from this first
  release — match it with a `_ =>` arm, because a later release adds a variant.
  Unlike `[[policy.layers]]` and `[[agent]]`, a later scope replaces the chain whole
  rather than appending to it.
- **`[app]`, stored and never validated.** `Config::app::<T>(key)` deserializes
  `[app.<key>]` into the caller's own type. The crate learns nothing about the
  contents and rejects no key inside it — that is the feature, and it is the second
  and last exception to "an unknown key is an error", beside `[[mcp]]`. The accessor
  is generic, so no `toml` type enters this crate's public API.
- **`${cmd:...}`.** A credential from a command rather than a literal: the value is
  split on whitespace, the first word is the program, and there is **no shell**
  between the string and the process. Trimmed stdout on success; a missing program,
  a non-zero exit and empty output are each an error naming the key, as every other
  substitution already was. Refused in the project scope — see the break above.
- **`[profile.<name>]`.** A named overlay of the same file format, applied by
  `Config::with_profile(name)` through the same merge the scopes use: scopes merge
  first, the profile applies to the result. A name the file does not carry is an
  error naming it, and a typo *inside* a profile is rejected at load even when that
  profile is never selected. Profiles do not nest and do not compose.
- **`IO_CONFIG`.** Names the user-scope file outright, ahead of `IO_CONFIG_HOME` and
  every platform convention. It names a scope rather than bypassing the merge, so a
  project file still wins the keys it names and the scopes stay four.
- **`[instructions]`.** Discovers the files a repository already carries —
  `AGENTS.md` by default — inside `Config::discover`, and `Config::apply_to` lands
  each in `TaskContract::constraints`, naming the file it came from. No new public
  field, therefore no break. A named file that is absent is skipped: this is
  discovery, not substitution. What it finds is untrusted text from the repository,
  reaches the model verbatim, and grants nothing.

### Changed

- `docs/CONTRACT.md`'s "What configuration is, and is not" is rewritten rather than
  extended: three of its bullets stopped being true as written. The guide gains a
  section per new table, a trust-rule section, and five entries in its limits block.

### Deprecated

### Removed

### Fixed

### Security

- A committed `io.toml` in a repository you cloned can no longer re-enable outbound
  network inside the sandbox, switch the portable floor off, or default `exec` and
  `net` to `allow`, and can no longer cause a command to run when you read it. This
  narrows one specific hazard and does not make an unfamiliar repository safe:
  `[[mcp]]` still names a command, `[toolchain]` still names an argv, and the
  boundary against the agent is still the `Policy` the caller loaded.

## [0.26.0] - 2026-08-01

The release that closes the Windows tree kill. Killing a process handle now ends
every process it produced, including a grandchild whose own parent has already
exited — the shape a real dev server has, and the one no kill built on walking
the process table can reach. macOS and Linux got that guarantee in 0.25.0 through
process groups; Windows has no process group, so it gets a Job Object, and the
clause `US-IO-HARNESS-0.25.0-I01` withdrew from the previous release is met here.

Beside it, the crate gains the machinery for a Windows **access** boundary — an
AppContainer, which is the analogue of the macOS sandbox profile and the Linux
network namespace that the platform table has been missing. What shipped of it,
and what it does not yet default to, is stated exactly below rather than implied.

### Added

- **A process handle's processes are contained on Windows, so `shell_kill` takes
  the whole tree.** Each stage of a handle's line is created suspended, assigned
  to a Job Object belonging to that handle, and only then resumed; the kill closes
  the job. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` walks nothing, so membership
  cannot be escaped by a process whose parent died or outlived by one that was
  spawned later. The suspend is the correctness argument rather than caution: a
  process that runs even briefly outside the job can spawn a descendant that never
  joins it, and every call involved still returns success.

  The mechanism is the one the sandbox backend already had — `Job::create`,
  `Job::adopt` and its resume — reused rather than copied, so the crate still
  contains exactly one `AssignProcessToJobObject`. A handle's job carries the
  teardown guarantee and **no resource limits**, because a handle is a lifetime
  rather than a boundary and a twenty-minute build is exactly what the sandbox's
  caps exist to kill.

- **`io_harness::sandbox::appcontainer`**, the Windows AppContainer half: creating
  and deleting a container profile, deriving its SID, granting a path to it with an
  explicit ACE, and spawning a process into it through `CreateProcessW` with a
  `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` attribute list. The crate owns that
  spawn because it has no choice — the attribute list is the only documented route
  a container SID has to a child, and neither `tokio`'s `Command` nor `std`'s can
  carry one on stable Rust, where `raw_attribute` is still gated behind
  `windows_process_extensions_raw_attribute`.

  **The capability array is empty**, which is the network boundary: `internetClient`
  is the capability that buys a socket to the outside and it is not requested, so
  the denial is the token's own rather than a filter's — the same shape as an empty
  network namespace on Linux.

  Both claims are proven on the Windows CI runner against negative controls that
  must succeed outside the container, because a denial that would also have been a
  denial outside proves nothing. Loopback turns out to be refused as well, which is
  a stronger result than was asked for.

  **It is not what `Sandbox::select` chooses on Windows, and the platform table is
  therefore unchanged.** A run still gets the Job Object. The obstacle is the grant
  set rather than the mechanism: an AppContainer is default-deny for *reads*, so
  the workspace is the easy part and the executed binary, the toolchain, the
  redirected temporary directory and every language's install tree are the rest.
  Naming those for arbitrary ecosystems is a discovery problem this release did not
  close, and a default boundary that cannot run the payload would be worse than one
  a caller reaches for deliberately. Recorded, with the evidence, in
  `US-IO-HARNESS-0.26.0-I02`. A table that described the intent rather than the
  build is the defect 0.9.1 and 0.16.1 were both spent on.

### Changed

- Two more `windows-sys` features (`Win32_Security_Isolation`,
  `Win32_Security_Authorization`) and **no new crate on any platform**. `cargo tree`
  on the default feature set is unchanged on macOS and Linux, where this dependency
  is not resolved at all.

- `examples/orphan.rs` — the grandchild fixture — now runs on Windows as well as
  unix. The chain is identical and only the leaf's definition of "my parent is
  gone" differs, because the platforms do: unix reparents to init and the leaf
  watches `getppid`, while Windows never reparents at all — a process keeps its
  creator's pid forever, which is precisely the stale link that makes `taskkill /T`
  miss the leaf — so there the leaf waits for the middle to leave the process table.

### Deprecated

### Removed

### Fixed

- **`docs/CONTRACT.md` said tree kill on a handle was best-effort on every
  platform. It was not, and had not been since 0.25.0.** Process groups closed the
  unix half in that release, with a test and a named negative control for it, and
  this document was written from the plan rather than from the code.
  `docs/guide/command-execution.md` carried the same claim and then contradicted
  itself two sentences later by scoping the gap to Windows. Both now state one
  guarantee with a mechanism per platform.

  Under-claiming a shipped boundary is the same defect as over-claiming one —
  either way the file cannot be trusted — and 0.25.0's own notes record that exact
  sentence about a different paragraph, so this document has now done it in two
  consecutive releases. Recorded rather than quietly corrected.

- The `src/tools/handles.rs` module documentation had the opposite error: it
  described a handle's processes as "a process group" with no platform caveat, on a
  platform that has none.

### Security

- **A Windows handle no longer leaks a grandchild when it is killed.** Before this
  release `shell_kill` fell back to `taskkill /F /T`, which walks parent/child
  links in the process table; a descendant whose parent had already exited was
  reachable from nothing the handle recorded and survived. A process the operator
  did not ask for and cannot see is the failure that reaches their machine rather
  than their run, which is why this is here rather than under Fixed.

## [0.25.0] - 2026-07-31

The release that lets a run start something that does not finish. Every tool this
crate has ever had is a call that returns, so a dev server, a log tail, a watch
build and a twenty-minute compile were not slow here — they were unrepresentable,
and the only way to express one was to run it in the foreground and lose the loop
until a timeout decided for it. This release adds a handle: a command line the
run starts, reads from over many steps, and ends. Beside it, an edit now comes
back carrying what the project's own checker thinks of it, so the model stops
paying a step, a build and a provider call to discover the type error it just
wrote.

### Added

- **Three tools for a process that outlives the call that started it.**
  `shell_start` takes the same one command line `shell` takes and returns a
  handle instead of a result; `shell_poll` returns the output the process has
  produced since the previous poll, so a log tail polled ten times does not
  return its whole history ten times; `shell_kill` ends the process and
  everything it spawned.
- The line a handle runs is parsed and checked by **exactly** the machinery
  `shell` uses: the same allowlist lexer, the same refusal set, the same
  per-stage `Act::Exec` check and the same path-resolved redirect targets, all of
  it **before the first spawn**. There is no second parser, no second gate and no
  second refusal set, so a handle is a different lifetime for a command line
  rather than a second way to run one, and a line whose second stage is denied
  starts no first stage.
- **A handle does not survive the process that started it.** On resume it is
  reported orphaned, with a reason, and is never re-attached, polled or
  signalled. This is unconditional rather than a best-effort re-attach because a
  PID recorded before a crash may since have been reused by an unrelated program,
  and signalling it is the one way this crate could damage something outside its
  own workspace. An orphaned handle stays readable in the store rather than being
  silently dropped.
- A per-run cap of **8 live handles**, refused with a reason naming the cap
  rather than queued, so a model in a loop cannot fill the host with dev servers.
  Every handle still live when its run ends is killed rather than leaked.
- Store tables `process_handles` and `handle_output` — what was started, its
  parsed sub-commands, the whole of its output as it was read, how it ended, and
  whether a resume orphaned it — so "what did that dev server do" is answerable
  after the process that started it is gone, and a poll whose result was lost to
  a provider retry is recoverable by reading the run rather than by asking the
  process again. The change is additive: no existing table is created, altered or
  dropped, every 0.24.0 database opens unchanged, and **`CHECKPOINT_FORMAT` stays
  7**. A new table is not a format change — bumping the format would make an
  older store be refused over a table an older binary never queries.
- New `EventKind` variants for the handle lifecycle — started, polled, killed,
  orphaned — so a handle is visible through the observer channel an operator
  already subscribes to. These are additive and covered by the
  `#[non_exhaustive]` that shipped in 0.24.0: this is the first release in which
  adding a variant costs a consumer nothing, and the reason that rider shipped
  when it did.
- **Diagnostics attached to the edit that caused them.** After a successful edit
  in a project whose ecosystem this crate already detects, the project's own
  checker runs against the workspace and its findings come back attached to the
  edit — file, line, span, message, and the rendered error a human would see in a
  terminal. The pass is bounded by a timeout and by a cap on how much it may
  append, is skipped entirely when no ecosystem is detected, and **can never turn
  a successful edit into a failed one**: the edit has already happened when the
  checker runs, so a checker that is missing, times out or fails for its own
  reasons yields no diagnostics and is recorded as such.
- No new dependency. The three handle tools and the diagnostics pass are written
  in this crate, over what the tree already carries, and the default `cargo tree`
  is unchanged at 402 lines.

### Changed

- The three new tool names are reserved, as every built-in name is. A caller who
  has registered a custom tool called `shell_start`, `shell_poll` or `shell_kill`
  now fails validation at run start rather than having it silently shadowed by
  the built-in. This is the standing behaviour for built-in names rather than a
  new rule, but it newly applies to those three.
  *Migration:* rename the custom tool, and register it under the new name —
  there is no way to keep the old one, since the built-in answers every call to
  it. Nothing else changes: a tool with any other name is unaffected.

### Fixed

- Documentation that had gone stale a release ago. `docs/CONTRACT.md` and
  `docs/guide/sandbox.md` still said `SandboxLimits::max_processes` was "enforced
  by nothing, on any platform" and that the Windows Job Object was "not wired
  up", and the sandbox guide still carried a table row reading "no native backend
  yet" — all three made false by 0.24.0, which enforces `max_processes` on
  Windows through the Job Object. A contract document that under-claims a shipped
  boundary is the same class of defect as one that over-claims it: either way a
  consumer cannot trust the file.
- Documentation of a behaviour that had never been written down: the `shell`
  grammar treats `\` as an escape exactly as POSIX does, on every platform, so an
  unquoted absolute Windows path like `C:\repo\server.exe` loses its separators
  in the parse and the spawn then fails naming a program nobody typed.
  `docs/guide/command-execution.md` now states the rule, says to quote absolute
  paths on Windows, and shows what the unquoted form becomes. The behaviour is
  unchanged and has been true since `shell` shipped in 0.24.0.

## [0.24.0] - 2026-07-31

The release that gives the agent hands. A command line becomes something it can
write and the harness can check, and a Windows run becomes something the kernel
actually bounds.

### Breaking changes

- **BREAKING** `TaskContract::workspace` takes two arguments instead of three.
  The success criterion is no longer positional and defaults to
  `Verification::None`. A harness whose primary signature demands a checkable
  criterion reads as a verified-task runner whatever its prose says, and the
  `shell` tool in this release is precisely the capability that makes
  uncheckable work real.
  *Migration:* drop the third argument, and add `.with_verification(v)` where
  the project has a criterion.
  ```rust,ignore
  // before
  TaskContract::workspace("fix the build", "/repo", Verification::Command {
      argv: vec!["npm".into(), "test".into()], expect_exit: 0 })
  // after
  TaskContract::workspace("fix the build", "/repo")
      .with_verification(Verification::Command {
          argv: vec!["npm".into(), "test".into()], expect_exit: 0 })
  ```
- **BREAKING** `EventKind`, `Backend` and `Cap` are now `#[non_exhaustive]`.
  This release adds variants to all three, and 0.25.0's AppContainer backend
  will add another; marking them now means this is the last release in which
  adding one breaks a caller.
  *Migration:* add a wildcard arm to any exhaustive `match` on them —
  `_ => { /* a variant added after you compiled */ }`.
- **BREAKING** `Cap::Processes` is a new variant, reported when a Windows job
  stops a run for exceeding its active process limit.
  *Migration:* covered by the wildcard arm above. There is nothing else to
  write: no previous release could produce this value, so no existing branch is
  wrong, and code that does not care about Windows process limits needs no
  change.

### Added

- A `shell` tool that runs a command *line* — pipelines, sequences, conditionals
  and redirects — under the same boundary a fixed argv has always had. The line
  is parsed by this crate, not by a shell: every sub-command's program and argv
  is checked against `Act::Exec` and every redirect target against `Act::Write`
  or `Act::Read`, **all of it before the first spawn**, so a line whose second
  stage is denied does not run its first. Each stage is then spawned as argv the
  way `exec` spawns one, with this crate wiring the pipes. There is no `sh -c`
  and no `cmd /c` anywhere after the parse.
- Supported grammar: single and double quotes, backslash escapes, `|`, `;`,
  `&&`, `||`, the redirects `>` `>>` `<` `2>` `2>>` `2>&1`, and `cd`, which
  applies to the rest of the line. Everything else is refused **by name** with a
  reason the model can act on: command substitution, parameter and arithmetic
  expansion, process substitution, subshells, brace groups, here-documents,
  background `&`, the `if`/`for`/`while`/`case` keywords, and glob
  metacharacters outside quotes.
- `list_dir`, which lists one directory one level deep with each entry's kind
  and each file's size, checked as the read it is. `find` is unchanged: a
  whole-tree glob answers a different question.
- `TaskContract::with_verification`, the builder that replaces the constructor's
  third argument.
- **The Windows Job Object backend**, designed in 0.6.0 and implemented here.
  `Backend::WindowsJobObject` is now what the sandbox reports on Windows; memory,
  CPU and active process count are real bounds; closing the job handle kills the
  whole process tree; and Windows becomes the first backend on any platform to
  enforce `SandboxLimits::max_processes`.

### Changed

- The README and `docs/guide/permissions.md` lead with `Session`, the durable
  conversation, rather than `run_with`, the one-shot verified run. `run_with` is
  unchanged and still documented as the one-shot form.
- The platform table's Windows row now reads "native resource containment
  (memory, CPU, process count, tree kill); no filesystem or network boundary"
  rather than "portable floor only". It is deliberately not shortened to
  "Native": **a Job Object contains resources and nothing else**, so unlike the
  macOS and Linux rows it is not an access boundary. The access half is
  AppContainer and it is not written yet.
- Glob expansion is refused rather than performed or passed through. Expanding
  one would let the argv the policy checked differ from the argv that ran, since
  the filesystem can change in between; passing it through would hand the model
  a `*` that a real shell would have expanded. Use `find` or `list_dir` to
  choose paths, or quote the character to pass it literally.

### Security

- The shell parser's refusal set is enforced by an **allowlist at the lexer**
  rather than a blocklist of known-bad constructs: any character outside the
  permitted set for the current state is refused, so a construct nobody
  anticipated fails closed instead of being absorbed into a word. Two tests
  sweep the entire ASCII range against that claim, and eight sabotage runs
  demonstrate the tests are not vacuous.
- No new dependency reaches macOS or Linux. The shell parser is written in this
  crate, and `cargo tree` on those platforms is unchanged at 402 lines.
  `windows-sys` is declared under `[target.'cfg(windows)'.dependencies]`, as
  `libc` already is for unix.

## [0.23.0] - 2026-07-31

The dependency release. Nothing this crate does changes; who is allowed to call
it does.

`libsqlite3-sys` declares `links = "sqlite3"`, and cargo refuses to put two
versions of a `links` package into one dependency graph. Every release up to
0.22.0 pinned `rusqlite` 0.32, so a Rust program already using `rusqlite` 0.33
or later could not add io-harness **at all**. Not a warning, not a duplicate
symbol at link time, not a degraded build — a resolver error and no build, with
no workaround available to the consumer. They could not patch it, feature-gate
it, or vendor around it. They could only pick one of the two crates. This is
the exact error that retires with this release:

```text
error: failed to select a version for `libsqlite3-sys`.
    ... required by package `rusqlite v0.40.0`
    ... which satisfies dependency `rusqlite = "^0.40"` of package `links-consumer v0.0.0`
versions that meet the requirements `^0.38.0` are: 0.38.1, 0.38.0

package `libsqlite3-sys` links to the native library `sqlite3`, but it conflicts with a previous package which links to `sqlite3` as well:
package `libsqlite3-sys v0.30.0`
    ... which satisfies dependency `libsqlite3-sys = "^0.30.0"` of package `rusqlite v0.32.0`
    ... which satisfies dependency `rusqlite = "^0.32"` of package `io-harness v0.22.0`
    ... which satisfies dependency `io-harness = "=0.22.0"` of package `links-consumer v0.0.0`
Only one package in the dependency graph may specify the same links value. This helps ensure that only one copy of a native library is linked in the final binary. Try to adjust your dependencies so that only one package uses the `links = "sqlite3"` value. For more information, see https://doc.rust-lang.org/cargo/reference/resolver.html#links.

failed to select a version for `libsqlite3-sys` which could resolve this conflict
```

For a crate whose first line is "embeddable in your own process", that was the
one dependency conflict with no answer. Every release since 0.2.0 widened what
the harness could do while this floor quietly narrowed who could call it.

**Not one line of `src/` changed.** The move produced 28 compile errors and all
28 were one root cause, described under Breaking changes below. That is the
claim this release has to be reviewed for — that nothing behaves differently —
and it is why nothing else ships alongside it.

### Breaking changes

- **BREAKING** — `Error::State(#[from] rusqlite::Error)` now carries `rusqlite`
  0.40's error type, which is a nominally distinct type from 0.32's. This
  affects only code that matches the variant *and* uses its payload as its own
  `rusqlite::Error`; code that matches the variant and ignores the payload is
  unaffected. Moving to `rusqlite` 0.40 yourself is both the fix and the point
  of the release — it is now possible, where before it was not.
  *Migration:* take the same `rusqlite` version this crate does.

  ```toml
  # Cargo.toml — before, and the reason the two could not coexist
  rusqlite = "0.32"

  # after
  rusqlite = "0.40"
  ```

  ```rust,ignore
  // Unaffected — match the variant, ignore the payload.
  Err(Error::State(_)) => { /* the store failed */ }

  // Affected — the payload is now rusqlite 0.40's type, not 0.32's.
  Err(Error::State(e)) => inspect(e),
  ```

  `docs/CONTRACT.md` now records the intent to wrap this type so that a future
  `rusqlite` bump stops being a break here at all. That change is deliberately
  **not** in this release: a migration has to be reviewable for exactly one
  property, and an error-type redesign in the same diff destroys it.

- **BREAKING (MSRV)** — the minimum supported Rust version moves from **1.88 to
  1.95**. `libsqlite3-sys` 0.38.1's build script, and `rusqlite` 0.40's own
  source, call the std `cfg_select!` macro, stabilised in 1.95.0. Neither crate
  publishes a `rust-version`, so cargo cannot catch this at resolve time: an
  older toolchain fails *inside the dependency's build script* with
  `cannot find macro cfg_select in this scope`, which reads like a broken
  toolchain and is not one. Checked rather than assumed — 1.93 and 1.94 both
  fail, 1.95 is the first that builds.
  *Migration:* update your toolchain to 1.95 or later; there is no opt-out and
  no version of the dependency that avoids it. Every `rusqlite` at or above the
  0.40 floor carries the same requirement, and below that floor the `links`
  collision above comes back — so the choice was this floor or that wall.

  ```sh
  rustup update stable   # 1.95.0 or later
  ```

### Added

- `tests/cross_version.rs` and the committed fixtures under
  `tests/fixtures/store-0.22.0/`: three databases written by a real 0.22.0
  build, carrying `SQLite version 3046000` in their headers. A populated store,
  a three-run tree stopped mid-fan-out, and a run paused awaiting a human. The
  generator that wrote them, `tests/fixtures/gen-0.22.0/`, is pinned
  `io-harness =0.22.0` and its lockfile is committed as evidence of the
  dependency line that produced them.
- `tests/fixtures/links-consumer/`, a consumer crate depending on both
  `rusqlite` 0.40 and this one, and a CI job that builds and runs it — plus the
  negative control that matters more: the same fixture pointed at 0.22.0 must
  fail at resolution, and the job fails if that build succeeds. The property is
  kept as a fixture rather than a one-time check because a future dependency
  bump can silently break it again.
- `examples/store_throughput.rs`, and `tests/fixtures/throughput-0.22.0/`
  running the identical workload on the old line, so the engine bump's cost was
  measured rather than assumed.

### Changed

- `rusqlite` 0.32 → **0.40.1**, `libsqlite3-sys` 0.30.1 → **0.38.1**, and the
  bundled SQLite engine **3.46.0 → 3.53.2**. The `fallible_uint` feature is now
  enabled: `u64`'s `ToSql` and `FromSql` impls moved behind it in `rusqlite`
  0.38.0, and turning it on restores the same macro-generated impls, so the
  token counters and the budget ledger keep the checked conversions they always
  had — `ToSqlConversionFailure` above `i64::MAX` on write,
  `FromSqlError::OutOfRange` on a negative read — instead of a hand-written
  cast that would silently wrap. Those 28 errors were the whole migration.
- **No new direct dependency.** The default `cargo tree` goes from **407 lines
  to 402**: `ahash`, `hashbrown` 0.14, `zerocopy` and the `version_check` build
  dependency drop out, `foldhash` arrives, and `hashlink` follows `rusqlite`
  from 0.9.1 to 0.12.1. A dependency move that made the tree smaller.
- No store migration, and no schema change of any kind. `CHECKPOINT_FORMAT`
  stays **7**; no table is created, altered or dropped. A 0.22.0 database works
  under 0.23.0 and a 0.23.0 database works under 0.22.0, because the bytes are
  the same bytes. Asserted directly against the committed 0.22.0 fixtures, so a
  silent format bump cannot pass as a successful upgrade.
- Store write throughput is unchanged. `Store::checkpoint_step` — the hot
  durable path, two inserts and a WAL commit — was measured on both sides of
  the engine bump with the same workload. Order-balanced batches put the median
  difference at +0.06%, −0.99% and +1.65%; the sign flips, against a paired
  spread of 11–21%. The measurement rules out a regression above roughly 5% and
  does not claim to resolve anything smaller.
- Two engine-behaviour risks were closed by inspection rather than by hope.
  SQLite 3.53.0 changed default floating-point rendering from 15 to 17
  significant digits, which would matter to any float persisted as text: this
  crate has no `REAL` column, and the only floats it computes are converted in
  Rust before storage. And the double-quoted-string misfeature, whose default
  has been tightening across this engine range, is unreachable — no SQL
  statement in the crate contains a double-quoted identifier or literal.

### Security

- The bundled SQLite engine moves to 3.53.2, which includes the fix for the
  WAL-reset database corruption bug. A consumer who links this crate alongside
  their own SQLite now gets 3.53.2 in the process; that is stated rather than
  left to be noticed.

## [0.22.0] - 2026-07-30

The web release. The agent can look something up before it answers, and the run
records what it read.

Every provider this crate talks to will run a search for the model, inside the
completion, server-side. Until now there was no way to ask for one: a model
answered from what it was trained on, and a task that turned on a current fact —
the release that shipped yesterday, the API that changed last month — was a task
the harness could not do honestly.

The declaration is one type, `WebAccess`, and each provider translates it into its
own shape. What a provider cannot express is refused before anything is sent
rather than quietly dropped, because a boundary silently discarded is worse than
no boundary: the caller believes in it.

One line has to be said plainly, and it is said in `docs/CONTRACT.md` too. **The
provider dials the URL.** This crate opens no socket for a search or a fetch, so
`Act::Net` never sees one, no approver is consulted, and the domain lists you
declare are handed to the vendor's own filter and enforced there. A boundary that
must hold inside this process is a reason not to turn this on.

### Breaking changes

- **BREAKING** — `CompletionRequest` gains `web: Option<WebAccess>`, what the
  provider may look up on the model's behalf. Every existing caller meant `None`,
  which is the body every release before 0.22.0 sent. This affects only code that
  builds a `CompletionRequest` with a struct literal listing every field.
  *Migration:* add the field, or spread the default.

  ```rust
  let request = CompletionRequest {
      system: system.into(),
      user: user.into(),
      tools,
      ..Default::default() // or: web: None,
  };
  ```

  An out-of-tree `Provider` that ignores the field keeps compiling and keeps
  working, and is honestly non-searching rather than broken.

- **BREAKING** — `CompletionResponse` gains `citations: Vec<Citation>` and
  `server_tools: Vec<ServerToolCall>`, so a struct literal listing every field no
  longer compiles. Both are empty for a completion that searched nothing, which is
  every completion a pre-0.22.0 provider returns. *Migration:* spread the default,
  or set both to `Vec::new()`.

  ```rust
  // before — a literal naming every field
  CompletionResponse {
      text, tool_calls, usage, model, finish_reason, ttft_ms,
  }
  // after
  CompletionResponse {
      text, tool_calls, usage, model, finish_reason, ttft_ms,
      citations: Vec::new(),
      server_tools: Vec::new(),
  }
  // or, better, stop naming every field
  CompletionResponse { text, tool_calls, usage, ..Default::default() }
  ```

- **BREAKING** — `EventKind` gains `ServerToolUsed { provider, tool, ok }`, so an
  exhaustive `match` over it no longer compiles. A run that declares no web access
  never emits it. *Migration:* add an arm, or a `_` arm.

  ```rust
  match &event.kind {
      EventKind::Step { .. } => { /* … */ }
      EventKind::ServerToolUsed { tool, ok, .. } => render_search(tool, *ok),
      // …or ignore it, which is what a consumer that renders neither wants.
      _ => {}
  }
  ```

### Added

- `WebAccess`, and `TaskContract::with_web`: one declaration of what the provider
  may look up — `WebAccess::search()`, `.with_fetch()`, `.max_uses(n)`,
  `.allow(host)`, `.block(host)`, with `enabled()` and `vendor_filter()` reading it
  back. Nothing is on by default and a contract that declares nothing sends the
  body 0.21.0 sent.

  A vendor takes an allow-list *or* a block-list, never both, so `vendor_filter`
  sends the allow-list with anything also blocked removed from it — exactly the set
  the two lists together described.
- `WebAccess::from_policy`, which projects a `Policy`'s `Act::Net` rules onto the
  vendor's domain filter so the same hosts are not written twice. The port is
  dropped, and an allow-everything rule projects to **no** filter rather than an
  empty one: an empty allow-list reads to a vendor as "allow nothing", which fails
  closed in silence and leaves the model answering from memory believing it
  searched. An `ask_net` rule, and a pattern carrying a scheme or a path, are each
  an `Error::Config` naming the rule rather than a boundary dropped on the wire.
- Provider-executed search on all three vendors, and fetch on the one that has it.
  Anthropic gets dated `web_search_20250305` / `web_fetch_20250910` server-tool
  entries, with the fetch beta header sent only when fetch is asked for; OpenAI
  gets `web_search_options` with its allow-list; OpenRouter gets its `web` plugin.
  A declaration a vendor cannot carry — a fetch on OpenAI or OpenRouter, a
  block-list on OpenAI, any domain filter on OpenRouter — is an `Error::Config`
  before the request is built, naming what to write instead.
- `Citation` and `CompletionResponse::citations`, with the `citations` table and
  `Store::record_citations` / `Store::citations`: the sources an answer drew on,
  per run and step, readable after the process that ran it is gone. Recorded
  verbatim — the crate does not fetch the url or check the page, so a row says the
  provider cited a page and not that the page says what the model claimed.
- `ServerToolCall` and `CompletionResponse::server_tools`, with the
  `server_tool_calls` table and `Store::record_server_tool_calls` /
  `Store::server_tool_calls`: which vendor ran which tool, and the vendor's own
  error code when it failed. A broken search arrives inside an HTTP 200 as an error
  object, so without this a search that failed and a search that found nothing are
  the same empty result set.
- `EventKind::ServerToolUsed { provider, tool, ok }`, emitted as each
  provider-executed call is recorded.
- `[web]` in `io.toml` — `search`, `fetch`, `max_uses`, `allowed_domains`,
  `blocked_domains` — applied by `Config::apply_to`. The table is carried whenever
  it is present, so a file writing `search = false` states a decision instead of
  reading as nothing configured.
- [The web guide](docs/guide/web.md).

### Changed

- **A failed provider-executed call reaches the model as an observation.** The
  step's log gains `provider web tool: web_search failed (<vendor code>)`, so the
  model can retry or answer knowingly. A search that ran and found nothing says
  nothing, because nothing went wrong.
- **A `pause_turn` stop reason no longer ends an unverified run.** A provider
  running a long search hands back partial text with no tool call, which is exactly
  the shape of a finished answer to a `Verification::None` contract — so before
  this release such a run stopped mid-search and reported success. The loop now
  takes another step, and both turns are charged, because a paused turn is a
  completion like any other.

  The crate does not echo the vendor's partial assistant blocks back — the request
  has been one flattened user turn since 0.1.0 — so a paused turn resumes as a
  fresh request and the provider may repeat a search it already charged for.
  `max_uses` is the lever against that.
- `Usage::server_tool_requests`, added in 0.18.0, reads non-zero for the first
  time. These requests are billed per request rather than per token. It is
  populated where the vendor reports a counter: Anthropic does, and OpenAI and
  OpenRouter do not in the shape the crate reads, so the meter is zero on those two
  even when a `server_tool_calls` row says a search ran.
- Two new tables, `citations` and `server_tool_calls`, created on open like every
  addition since 0.13.0. `CHECKPOINT_FORMAT` stays 7, no existing table is altered,
  and a 0.21.0 database opens, resumes and is queried unchanged.
- No new dependency.

### Security

- Nothing in this release widens what an agent may do inside this process. A
  provider-executed search opens no socket here, executes no binary and touches no
  file; what it can reach is bounded by the vendor's own domain filter, which the
  declaration fills in and the vendor enforces. A run needing that boundary held
  locally should not declare web access at all.

## [0.21.0] - 2026-07-30

The agency release. The agent can plan where you can watch it, and it can ask you
what you actually meant instead of guessing.

Two things were missing and they are the same shape. An operator could see every
step a long run *took* and nothing about what it *intended*, so a run going the
wrong way was only recognisable once it ended. And the only channel back to a human
asked one question — *may I do this?* — so an ambiguous goal was resolved by the
model guessing, and a wrong guess spends a whole run.

A sub-agent also gets an identity. Before this release a spawned child was its
parent with a different goal string: same model, same prompt, so "search with the
cheap model, write with the strong one" — the largest cost lever the crate has —
could not be expressed at all.

### Breaking changes

- **BREAKING** — `EventKind` gains three variants, `TodoWrote`, `QuestionAsked`
  and `QuestionAnswered`, so an exhaustive `match` over it no longer compiles.
  Runtime behaviour is untouched for every existing entry point: a run whose agent
  never writes a plan and never asks a question emits none of them.
  *Migration:* add arms, or a `_` arm.

  ```rust
  match &event.kind {
      EventKind::Step { .. } => { /* … */ }
      EventKind::TodoWrote { items } => render_plan(items),
      // …or ignore them, which is what a consumer that renders neither wants.
      _ => {}
  }
  ```

- **BREAKING** — `RunOutcome` gains `AwaitingAnswer { question_id, steps }`, so an
  exhaustive `match` over it no longer compiles. A run that never asks a question
  never produces it. *Migration:* add an arm. It is a *pause*, not a failure —
  treat it as you treat `AwaitingApproval` and resume with the answer rather than
  retrying the run from scratch.

  ```rust
  match result.outcome {
      RunOutcome::AwaitingApproval { request_id, .. } => ask_permission(request_id),
      RunOutcome::AwaitingAnswer { question_id, .. } => ask_what_was_meant(question_id),
      _ => {}
  }
  ```

- **BREAKING** — `CompletionRequest` gains `model: Option<String>`, a per-request
  model override. Every existing caller meant `None`, which is "use the model the
  provider was constructed with". This only affects code that builds a
  `CompletionRequest` with a struct literal listing every field; the crate's own
  call sites already used `..Default::default()`.
  *Migration:* add the field, or spread the default.

  ```rust
  let request = CompletionRequest {
      system: system.into(),
      user: user.into(),
      tools,
      ..Default::default() // or: model: None,
  };
  ```

  An out-of-tree `Provider` that ignores the field keeps compiling and keeps
  working, and is honestly non-selecting.

### Added

- `io_harness::TODO_WRITE_TOOL` (`todo_write`) and the `todos` table: the agent
  writes down its plan and an operator reads it **while the run is still going**.
  The whole list is replaced on every call, so there is no item id for a model to
  mis-address, and the write is one transaction, so a reader on another connection
  sees the previous plan or the next one and never half of each. `Store::todos`
  reads it back, with `TodoItem`, `TodoState`, `TODO_MAX_ITEMS` and
  `TODO_TEXT_CAP`.

  A plan is the agent's stated intent and nothing more: nothing verifies it, no
  outcome depends on it, and it is not gated — it writes the harness's own store,
  not your workspace.
- `io_harness::ASK_QUESTION_TOOL` (`ask_question`), with `Question`, `Responder`,
  `ResponderNone`, `FixedResponder`, `StdinResponder` and `AnswerFuture`. The agent
  asks the operator about **intent**, which is a different question from the one
  the approval path asks about **permission**. Register who answers with
  `TaskContract::with_responder`.

  An answer is text the model reads and it authorizes nothing: every tool call that
  follows one is checked against the same `Policy` by the same code.
- `RunOutcome::AwaitingAnswer` and `resume_with_answer`,
  `resume_with_answer_observed`, `resume_tree_with_answer` and
  `resume_tree_with_answer_observed`: a question nobody can answer in this process
  is persisted to `pending_questions` and pauses the run, so a human can answer it
  after the process has exited. `PendingQuestion`, `Store::put_question`,
  `question`, `questions`, `answer_question` and `answered_question` are the read
  and write surface. `answered_by` says whether a machine or a person answered — a
  distinction worth keeping.

  A child's question pauses the whole tree, as a child's deferred approval does,
  and the tree resumes with every agent continuing from its own last committed step.
- `AgentDef` and `Agents`, plus `TaskContract::with_agents` and an optional `agent`
  argument on `spawn_agent`: a spawned child gets a role prepended to its system
  prompt, a model on the wire, a step cap, and a *narrower* boundary — declared in
  Rust or as `[[agent]]` tables in `io.toml`.

  A definition can only narrow. `deny_write` and `deny_net` compose through
  `Policy::contain`, unchanged since 0.5.0, and there is deliberately no
  `allow_write`: a roster in a config file that could grant would be a
  privilege-escalation path.
- `Template` and `Templates`: a directory of markdown prompt templates, discovered
  the way skills are, with `{{placeholder}}` substitution and `$ARGUMENTS` for the
  remainder. A placeholder with no argument is an error rather than an empty
  string — the rule 0.19.0 set for `${env:}`. Rendering returns a `String`, so a
  template can set no policy, budget, tool or model. `[run] templates` points at
  the directory.
- [The agency guide](docs/guide/agency.md), and an `agency_live` example that
  writes a plan, asks a question and spawns two named agents on two models,
  asserting each itself.

### Changed

- Stall detection has a second signal. The window added in 0.11.0 needs the
  workspace to have stayed still, and a spawned child that ran marks the parent's
  step as progress unconditionally — so a parent respawning the *same* child reset
  its own window every step and simply spent its budget. The same call `window`
  times in a row is now caught whether or not the workspace moved.

  No new setting: the threshold is the existing `StallPolicy::window`, because "the
  same call three times in a row" and "three unproductive steps" are one patience
  setting. `window = 0` still disables both. Three *different* calls that each get
  somewhere are untouched.
- The replan directive is reworded to cover both signals. It used to state flatly
  that the workspace had not changed, which is no longer true of every case that
  reaches it.
- Two new tables, `todos` and `pending_questions`, created on open like every
  addition since 0.13.0. `CHECKPOINT_FORMAT` stays 7, no existing table is altered,
  and a 0.20.0 database opens, resumes and is queried unchanged.
- No new dependency: the default `cargo tree` is unchanged at 407 lines.

### Fixed

- **A refused git built-in no longer ends the run.** A policy denying `Act::Exec`
  for `git` made `git_status`, `git_diff`, `git_log`, `git_add` and `git_commit`
  return `Error::Refused` out of the run loop, so one speculative `git status`
  escalated a whole run instead of costing a step. It is now an observation the
  model reads and adapts to, with a `policy_events` row attributed to the rule and
  layer — which is what every other refusal in the crate already was. Found while
  running the 0.20.0 live session, and fixed here because this is the release that
  touches the tool layer.

- **The test suite passes on a Windows clone.** A `.gitattributes` pins
  `tests/fixtures/**` to LF. Without it, a checkout with `core.autocrlf=true` —
  the default for Git for Windows and for the GitHub Windows runner — rewrote
  every fixture's line endings, and the template test that compares a rendered
  body byte for byte failed on Windows and nowhere else. Nothing in the crate
  changed: the frontmatter parser returns a body verbatim on purpose, so the
  fixture's bytes were the thing that had to be made the same everywhere.

### Security

- Nothing in this release widens what an agent may do. Both new tools are
  ungated because neither touches the workspace, the network or a binary; an
  answer to a question is an observation and not an authorization; and a named
  agent definition has no syntax for granting a permission, only for removing one.

## [0.20.0] - 2026-07-30

The session release. The crate stops being one-shot: an operator opens a durable
conversation over a workspace, sends a turn, watches the answer arrive token by
token while the model is still producing it, says something else mid-turn or
interrupts, and comes back later to branch from any earlier turn instead of
starting over.

A turn **is** a run — its own trace, its own budgets, its own policy boundary, its
own checkpoint — so a session is durable for exactly the reason a run already is,
and none of that machinery is rebuilt for conversations. `TaskContract` becomes
what it always was in practice: an optional bound on a turn, or a headless
one-shot for unattended work.

This is the release that makes a terminal or a desktop application possible.
Before it, SSE was consumed inside the provider and accumulated before anything
else saw it, events were step-granular, and there was nothing to render while the
model was typing.

### Breaking changes

- **BREAKING** — `EventKind` gains a `Token { text }` variant, so an exhaustive
  `match` over it no longer compiles. Nothing else changed: no item was removed or
  renamed, and no signature changed. Runtime behaviour is untouched for every
  existing entry point — `Token` is emitted only by a `Session` turn that was
  given an observer, so a one-shot run's event stream is byte-for-byte what it was.
  *Migration:* add an arm.

  ```rust
  match &event.kind {
      EventKind::Step { .. } => { /* … */ }
      // Either handle the new variant…
      EventKind::Token { text } => print!("{text}"),
      // …or ignore it, which is what a consumer that does not render text wants.
      _ => {}
  }
  ```

### Added

- `io_harness::Session` and the `session` module: `Session::open(store, root)`
  starts a conversation, `Session::reopen(store, id)` picks it up in any later
  process, and `id`, `root`, `head`, `history` and `branch_from` read and move
  within it. The conversation is a durable, append-only tree — branching is one
  write, and the branch you left stays readable.
- Five ways to take a turn: `Session::turn` (no criterion, quiet),
  `turn_observed` (streams to an `Observer`), `turn_steered` (streams and reads a
  `Steer`), `turn_bounded` (your own `TaskContract` for that one turn) and
  `turn_bounded_observed`. A bound applies to the turn that carries it, not to the
  session.
- `Provider::complete_streaming`, a defaulted trait method whose default delegates
  to `complete` and emits the finished text as one delta — so an out-of-tree
  provider keeps compiling and a UI still renders something. `OpenRouter`,
  `Anthropic`, `OpenAi` and `provider::Fallback` override it and emit each text
  delta as its SSE event arrives.
- `EventKind::Token { text }`: the model's text as it is produced. The deltas of
  one step concatenate to exactly that step's final assistant text, in order.
  Emitted only by an observed session turn.
- `Steer` and `SteerInbox`: `Steer::say` delivers an operator's message to the
  model at the next step boundary, and `Steer::interrupt` ends the turn there
  through the existing cancel path — whole steps only, recorded as `cancelled`,
  still resumable. A steer is text and not authorization: every tool call it leads
  to is checked against the same `Policy` by the same code.
- `Turn` and `TurnResult`, and the `Store` methods behind them —
  `create_session`, `session_root`, `session_head`, `set_session_head`,
  `record_turn`, `finish_turn`, `session_turn`, `session_turns`, `turn_for_run` —
  so a conversation is reconstructable from SQLite without the program that drove
  it.
- A `steered` context event, so a turn that changed course because a human said
  something reads that way in the trace afterwards.
- [The sessions guide](docs/guide/sessions.md), and a `session_live` example that
  streams, steers, branches and reopens across a process boundary, asserting each
  itself.

### Changed

- The `tokio` dependency gains its `sync` feature, for the steer channel. No new
  crate: the default `cargo tree` is unchanged at 407 lines.
- Two new tables, `sessions` and `session_turns`, created on open like every
  addition since 0.13.0. `CHECKPOINT_FORMAT` stays 7, no existing table is
  altered, and a 0.19.0 database opens, resumes and is queried unchanged.

## [0.19.0] - 2026-07-29

The configuration release. An operator configures the harness in a file instead
of in Rust: one `io.toml`, four scopes that merge in a fixed order, and every key
landing in a type this crate already had. It exists so that io-cli and io-studio
read the same file rather than inventing two formats that would have to be
reconciled later, and it is where the two tables the last two releases shipped as
types finally get something to fill them — `toolchain::Toolchain` was overridable
only from Rust, and `pricing::PriceTable` ships deliberately empty, so cost was
zero out of the box until someone typed a price in.

Nothing a caller wrote against 0.18.0 changes. No public item is removed,
renamed, or given a new argument, and a consumer who writes no config file gets
the behaviour they had.

### Added

- `io_harness::Config` and the `config` module: `Config::discover(root)` reads
  and merges the four scopes — the crate's defaults, `$IO_CONFIG_HOME/io.toml`
  (else `$XDG_CONFIG_HOME/io/io.toml`, `~/.config/io/io.toml`, or
  `%APPDATA%\io\io.toml`), the committed `io.toml` in the workspace root, and the
  gitignored `io.local.toml` beside it. Later wins, key by key. Discovery does
  not walk upward out of the root it was given.
- Projection onto the typed API: `Config::policy`, `Config::sandbox`,
  `Config::prices`, `Config::toolchain(detected)`, `Config::mcp_servers` and
  `Config::apply_to(contract)`. Every key reaches a typed field, asserted by a
  test over a fixture naming all of them.
- `${env:NAME}` and `${file:path}` substitution in any string value, so a
  credential reaches a config without being committed. `${file:...}` resolves
  against the directory of the file that wrote it and its contents are trimmed.
- `Serialize`/`Deserialize` on `RetryPolicy`, `StallPolicy`, `ContextBudget` and
  `Identity`. `RetryPolicy` crosses the wire as `base_ms` / `max_ms`, since
  serde's own form for a `Duration` is `{secs, nanos}`.
- [docs/guide/configuration.md](docs/guide/configuration.md) — every key, its
  typed destination, the merge rules, and the limits stated plainly.

### Changed

- One new dependency in the default build, `toml` 0.8 with default features off
  and only `parse` enabled — the first added to the default build since 0.1.0. Five
  crates are new to the tree: `toml`, `toml_edit`, `toml_datetime`,
  `serde_spanned` and `winnow`. `indexmap`, `equivalent` and `hashbrown` are
  *not* new — they were already there through `process-wrap`, and `cargo tree`
  simply lists them a second time. The default `cargo tree` goes from **401 lines
  to 413**; the standing constraint moved to accommodate it rather than the other
  way round, and no other feature's tree changes.
- An unknown key anywhere in the file is an error naming the key and the file,
  rather than being ignored. A typo in a permission rule that is silently
  discarded leaves an operator believing in a boundary that is not there. The one
  exception is an unknown key inside a `[[mcp]]` table: `McpServer` is
  `#[serde(flatten)]`-based and serde refuses `flatten` beside
  `deny_unknown_fields`.
- A failed substitution is an error, never an empty string — an unset variable,
  an unreadable file, and a value that resolves to nothing each fail the load.
- `Price` gains `#[serde(default)]`, so a config that prices input and output
  need not write three zeros for the dimensions the vendor does not charge for.
- `.gitignore` carries `io.local.toml`.

## [0.18.0] - 2026-07-29

The accounting release. Cost is the first question anyone asks about an agent run
and this crate could not answer it at any price: the model identifier was
recorded nowhere, cache tokens were parsed by neither provider client, no
provider call was ever timed, and `steps.tokens` collapsed a step that retried
twice and fell over to a second vendor into one integer attributed to nothing.
Every provider call is now its own row — the model that served it, the tokens it
reported, its latency and time to first token, and why the model stopped — and
money is derived from a price table you own, so correcting one price repairs the
whole history.

### Breaking changes

- **BREAKING** — `Usage` gains `cache_read_tokens`, `cache_write_tokens`,
  `reasoning_tokens` and `server_tool_requests`, so a struct literal naming the
  three existing fields no longer compiles.

  *Migration:* construct with `..Default::default()`, which this type has
  documented as the forward-compatible form since 0.2.0. Reading `total_tokens`
  is unaffected.

  ```rust
  // Before:
  let usage = Usage { prompt_tokens: 1_200, completion_tokens: 80, total_tokens: 1_280 };
  // After:
  let usage = Usage {
      prompt_tokens: 1_200,
      completion_tokens: 80,
      total_tokens: 1_280,
      ..Default::default()
  };
  ```

- **BREAKING** — `CompletionResponse` gains `model`, `finish_reason` and
  `ttft_ms`, with the same construction break. A custom `Provider` that already
  built its response with `..Default::default()` needs no change and reports no
  model, which is recorded as unknown rather than as absent.

  *Migration:* add `..Default::default()`, and fill `model` in if your provider
  knows which model answered — it is what makes a fallback auditable.

- **BREAKING** — `Verification::CompilesRust`, `RustTestPasses` and
  `WorkspaceTestPasses` are **removed**. They were deprecated in 0.17.0 naming
  this release, which is the shortest cycle this crate's contract allows.
  **A consumer upgrading straight from 0.16.2 meets the removal having never seen
  the deprecation warning**, and that is the cost of the short cycle: a reader who
  upgraded one minor at a time is not the only reader.

  *Migration:* each becomes a `Command` criterion, with the test living in the
  project's own suite where its own tooling runs it. `test_src` has no
  replacement and needs none — write that test into the repository.

  ```rust
  // Before:                              After:
  // Verification::CompilesRust           Verification::Command {
  //                                          argv: vec!["cargo".into(), "build".into()],
  //                                          expect_exit: 0 }
  // Verification::RustTestPasses { .. }  Verification::Command {
  // Verification::WorkspaceTestPasses {}     argv: vec!["cargo".into(), "test".into()],
  //                                          expect_exit: 0 }
  ```

- **BREAKING (behaviour)** — Anthropic's `prompt_tokens` and `total_tokens` now
  include cached input tokens. The vendor reports `input_tokens` *excluding* the
  cached counts and bills all three, so 0.17.0 under-reported a cache-heavy
  prompt; the OpenAI wire already included them. Both are reconciled at the wire
  boundary, so a row does not mean two things depending on which vendor wrote it.

  *Migration:* there is no opt-out, and none should be wanted — the previous
  figure was lower than the invoice. A token budget calibrated against 0.17.0
  numbers on a cache-heavy workload will be reached sooner, so raise it if a run
  now stops early.

- **BREAKING (behaviour)** — `TEST_BINARY` still exists so that policies written
  against it compile, but **nothing spawns it**: it named the test binary the
  removed variants built, and no criterion builds one now. `SandboxEvent`'s
  `criterion-compile` and `test-run` gate phases are likewise never emitted.

  *Migration:* there is nothing to write instead for the constant — a
  `deny_exec(TEST_BINARY)` rule is now inert and can be dropped. For compile-only
  verification use `EachCompilesRust`, or a `Command` naming the compiler rather
  than the test runner.

### Added

- A `provider_calls` table and `Store::provider_calls`: one row per
  `Provider::complete` call, with the attempt number, the provider, the model
  that served it, the full token breakdown, latency, time to first token, the
  finish reason, and the failure where there was one. A retried step is several
  rows; the attempts that failed are kept, because a model that produced tokens
  before the connection broke was still billed for them.
- An `edits` table, `Edit` and `Store::edits`: the lines each file change added
  and removed, for `write_file` and `edit_file` alike.
- `io_harness::pricing` — `Price`, `PriceTable` and `Spend`. Cost is derived at
  query time in integer micro-units and is never stored, so correcting a price
  repairs every past run. The crate ships **no prices**: a price table requires
  an as-of date at construction, and an unpriced model is counted in
  `Spend::unpriced_calls` rather than costed at zero.
- `Store::spend_by_model`, `spend_by_day` and `spend_by_run` — grouped raw rows
  with the derived cost beside them. Renderings are the consuming app's business.
- Both provider clients now parse what they already received and discarded:
  cache-read and cache-write tokens, reasoning tokens, the model id, the finish
  reason, and provider-executed tool request counts. Time to first token is
  measured where the stream is consumed.
- `Verification::Command` works in single-file mode, running in the edited file's
  own directory. Without it the 0.17.0 migration note would have been false for a
  single-file caller.
- [Accounting](docs/guide/accounting.md) — a guide page stating what each
  recorded number is and, more importantly, what it is not.

### Changed

- `docs/CONTRACT.md` records the provenance of every recorded figure: a token
  count is the provider's report and not the crate's measurement, a latency is
  the harness's own wall clock and includes its request building, a TTFT is
  `None` rather than zero where nothing measured it, and a cost is only as right
  as the operator's own price table.
- The 0.8.1 gate-hardening is now structural rather than defensive. It guarded
  against a subject shadowing a macro in a criterion the harness compiled into
  the subject's crate; no criterion is compiled that way any more, so the class
  is gone. Where the criterion lives — somewhere the agent's policy does not let
  it write — is what matters now.

## [0.17.0] - 2026-07-29

The release that makes the repositioning true in code. Before it the type system
required every run to name a `Verification`, and four of the five variants
compiled or ran Rust — so "debug a production issue", "fix a deployment" or
"build an application" could not be expressed, let alone executed, because there
was no way to run a command and no way to say the work was done without a Rust
gate. Point the harness at a project in any language and the agent can now run
that project's own toolchain, change a file by replacing an exact string, and
have the run's definition of done be that project's own test command — or nothing
at all.

### Breaking changes

- **BREAKING** — `Verification` gains two variants, `Command { argv, expect_exit }`
  and `None`, so an exhaustive `match` on it no longer compiles.

  *Migration:* add an arm for each rather than a wildcard, so the next variant is
  caught by the compiler instead of being silently absorbed.

  ```rust
  match verify {
      Verification::Command { argv, expect_exit } => { /* a command criterion */ }
      Verification::None => { /* no criterion at all */ }
      // ... the existing arms, unchanged
  }
  ```

- **BREAKING** — `RunOutcome` gains `Finished { steps }`, the terminal outcome of
  a `Verification::None` run, so an exhaustive `match` on it no longer compiles.

  *Migration:* add the arm. It means the agent stopped because it was done, not
  because a ceiling stopped it — distinct from `StepCapReached`, `Stalled` and
  the budget outcomes on purpose. It is **not** a claim the work is correct;
  nothing checked it.

  ```rust
  match result.outcome {
      RunOutcome::Finished { steps } => println!("agent finished at step {steps}; nothing verified it"),
      // ... the existing arms, unchanged
  }
  ```

- **BREAKING** — `TaskContract` gains a public `exec_timeout: Duration` field, so
  a struct literal that names every field no longer compiles.

  *Migration:* build the contract with `TaskContract::new` or
  `TaskContract::workspace` and the `with_*` builders, which is the supported
  path and was always the intended one. If you must construct one by literal, add
  `exec_timeout: io_harness::DEFAULT_EXEC_TIMEOUT`.

  ```rust
  let contract = TaskContract::workspace(goal, root, verify)
      .with_exec_timeout(std::time::Duration::from_secs(1800));
  ```

- **BREAKING (behaviour)** — a registered `Tool` named after **any** built-in now
  fails validation at run start. The reserved set previously named only seven
  built-ins while dispatch tested twenty-six, so a tool called `git_status`,
  `xlsx_read` or `view_image` passed validation and was then permanently
  unreachable — the built-in answered every call.

  *Migration:* rename the tool. The change is not that it stopped working; it
  never worked. The change is that the failure is now visible instead of silent.
  The names of feature-gated built-ins are reserved in every build, including
  builds that do not contain them, so enabling a feature can never take away a
  tool that was working.

### Added

- **An `exec` tool.** The agent can run a command in the workspace root — the
  project's own build, tests, linter, formatter or package manager. It takes a
  fixed argv array and never a shell string, so `;`, `&&`, `$( )` and a backtick
  are ordinary bytes inside one argument rather than syntax. Every call is an
  `Act::Exec` check against the policy on the program **and** on the whole argv,
  so `allow_exec("cargo test*")` beside `deny_exec("cargo publish*")` means what
  it reads, and both decisions land in `policy_events` with the rule and layer
  that produced them.
- **Bounds on what a command may do to the run.** A wall-clock timeout —
  `DEFAULT_EXEC_TIMEOUT`, 15 minutes, overridable per contract with
  `TaskContract::with_exec_timeout` — so a wedged command dies naming itself
  rather than consuming the contract's whole time budget and being reported as a
  budget stop. Oversized output keeps its head and its tail and elides the
  middle, stating in the result the model reads how much went, because a build
  log's useful content is at both ends.
- **An `edit_file` tool** performing exact string replacement. A search string
  matching zero times or more than once is a typed error naming the file and the
  count, and the file is not touched — a replacement that guessed which of three
  occurrences was meant is a corrupting write, not a cheap one. Gated by the same
  `Act::Write` check on the same path as `write_file`, because it is the same act.
- **`Verification::Command { argv, expect_exit }`** — a criterion that runs a
  caller-supplied command inside the existing execution sandbox and asserts its
  exit status. One variant covering every language the machine has a toolchain
  for. A command killed by a signal or a sandbox cap reports as no exit at all,
  so no `expect_exit` can match it.
- **`Verification::None`** — a run with no gate, ended by an assistant turn that
  calls no tool. No `done` tool is added, so an unverified run gains no tool
  surface over a verified one.
- **A project and toolchain table**, shipped as data in the crate
  (`io_harness::toolchain`). One marker file in the workspace root maps to an
  ecosystem and its conventional install, build, test, lint, format and run
  argvs, across Cargo, npm/pnpm/yarn/bun (resolved by lockfile), Deno, Go, Python
  (uv/poetry/pip), Maven, Gradle, .NET, Ruby, PHP, Elixir, Swift, CMake and Make.
  The detection is put in front of the model every turn, so it stops spending
  turns guessing the package manager. `Toolchain` is `Serialize`/`Deserialize` so
  0.19.0's configuration file can deserialize operator overrides into this same
  type. A directory with no marker reports no detection rather than guessing.
- **A `gate_output` sandbox event.** When a `Verification::Command` criterion
  fails, what the command printed is recorded in the trace — without it, "the
  agent's change is wrong" and "the test runner is not installed" are the same
  discriminant and need opposite responses.
- **A nightly docs.rs job in CI**, building the way docs.rs builds (nightly,
  `--cfg docsrs`, all features) and checking the **rendered HTML** for the feature
  labels rather than trusting exit zero. It is the only thing that would have
  caught the defect 0.16.2 had to fix, and its absence was recorded as a known
  limitation on that release.
- Guide pages for [command execution](docs/guide/command-execution.md) and
  [language support](docs/guide/language-support.md).

### Changed

- The crate's own description now states what it is: an embeddable agent runtime
  for Rust — any task, any provider, in your process, with a permission boundary,
  a sandbox and a durable trace you own. Verification is kept and demoted: it is
  an optional, language-agnostic gate rather than an entry requirement, and that
  demotion is what makes an open-ended task expressible at all.
- The tool-name constants in `io_harness::tools` are no longer `#[cfg]`-gated on
  their feature. The names exist in every build; only the tools behind them are
  optional. See the behaviour break above for why.

### Deprecated

- **`Verification::CompilesRust`, `Verification::RustTestPasses` and
  `Verification::WorkspaceTestPasses`.** They still work and behave exactly as
  they did; each now emits a deprecation warning naming its replacement. **They
  are removed in 0.18.0** — the minimum cycle this crate's contract allows, named
  by version so a caller reading the warning knows exactly how long they have.

  *Migration:* each becomes a `Command` criterion running the project's own
  tooling.

  ```rust
  // Verification::CompilesRust
  Verification::Command { argv: vec!["cargo".into(), "build".into()], expect_exit: 0 }

  // Verification::RustTestPasses { test_src }
  // Verification::WorkspaceTestPasses { files, test_src }
  Verification::Command { argv: vec!["cargo".into(), "test".into()], expect_exit: 0 }
  ```

  The behaviours are not identical and the difference is worth knowing. The old
  variants compile the named files in a throwaway directory with your `test_src`
  appended as a module, so they reach *private* items and need no cargo project.
  `cargo test` runs the repository's real suite, which is stronger in every way
  that matters and cannot check a criterion the repository does not contain — so
  write that test into the repository, which is where the warning should send
  you. `EachCompilesRust` is **not** deprecated: it has no `Command` equivalent.

  A consumer upgrading 0.16.2 straight to 0.18.0 meets the removal with no
  intervening warning. That is the known cost of the minimum cycle.

### Fixed

- **A registered tool can no longer silently shadow a built-in.**
  `RESERVED_TOOL_NAMES` named seven built-ins while dispatch had grown to
  twenty-six, so a tool called `git_status` or `xlsx_read` validated and was then
  unreachable. It now names every one, in every build. See the behaviour break
  above.

### Unchanged, deliberately

- No store schema change, no new table or column, and `CHECKPOINT_FORMAT` stays
  7 — a 0.16.2 database opens and resumes against 0.17.0 unchanged.
- The default `cargo tree` stays at 401 lines. The toolchain table is Rust data,
  not a parsed configuration file; the dependency that would need lands in
  0.19.0.
- No new system package on any CI runner.

## [0.16.2] - 2026-07-28

A build fix for docs.rs. **There are no breaking changes and no behaviour
change in this release** — no public item is added, renamed, or removed, and a
0.16.1 contract compiles and behaves identically. The one changed line is a
nightly feature gate that is only ever reached under the `docsrs` cfg, which
docs.rs sets and nothing else does.

### Fixed

- **The docs.rs documentation build now succeeds.** It failed for both 0.16.0
  and 0.16.1, so neither version has a rendered page: the crate root enabled
  the nightly `doc_auto_cfg` feature, and that feature was removed in Rust
  1.92.0 and merged into `doc_cfg`
  ([rust-lang/rust#138907](https://github.com/rust-lang/rust/pull/138907)).
  docs.rs builds on nightly, so the crate root failed to compile there with
  `` feature has been removed ``, while every stable `cargo doc` was unaffected
  and gave no warning.

  The gate is now `#![cfg_attr(docsrs, feature(doc_cfg))]`. `doc_cfg` does the
  automatic feature labelling itself since the merge, so the rendered docs say
  the same thing they were always meant to: every item behind `documents`,
  `media`, or a per-format feature is labelled with the feature it needs.

  0.16.0 and 0.16.1 remain unbuildable on docs.rs — a published version's
  sources cannot be changed — so 0.16.2 is the first version of the 0.16 line
  with a page on docs.rs.

## [0.16.1] - 2026-07-28

A documentation correction. **There are no breaking changes and no code
change in this release** — no public item is added, renamed, or removed, no
behaviour differs, and a 0.16.0 contract compiles and behaves identically.

### Changed

- **The "Part of initorigin" table now says which products exist and which you
  can go and look at.** It listed five repositories as if a reader could open
  any of them. Four are private, and two of those — `io-eval` and `website` —
  have not been built at all, so the table was describing an intention in the
  voice of a fact, on the last screen of a published crate's landing page.

  It now lists the three products this crate is for — io-harness, io-cli,
  io-studio — with a status column, and it links only io-harness, because that
  is the only repository a reader can open. io-cli and io-studio are named
  without links and marked as in development, which is both true and the reason
  there is no link: a link to a private repository is a 404 dressed as a
  promise. `io-eval` and `website` are removed rather than listed as unbuilt;
  they remain on the product's own roadmap, which is where an intention belongs.

  This is the one section 0.16.0 carried forward unchanged from the release-
  history README it replaced, and the one claim on the page that none of that
  release's five checkers looks at.

## [0.16.0] - 2026-07-28

The documented public contract. A developer arriving cold can tell what the
crate does and start using it; a developer already depending on it knows what
may change.

**There are no breaking changes in this release.** Upgrading from 0.15.0 is a
version bump and a rebuild — nothing you wrote stops compiling. That is worth
stating plainly in the release that introduces migration notes, because it is
the first entry the new format applies to.

### The two front doors

The README was 1072 lines organised as a release history: sections named after
the version that introduced each capability, ordered by nothing in particular,
with 0.15 filed before 0.14 and 0.12 and 0.13 absent from the file entirely. Its
first code fence was on line 107, and its status paragraph described a release
two versions stale. The crate root — the page docs.rs actually opens on — was
worse: 230 lines of "v0.2 adds", "v0.4 adds", with v0.3 arriving after v0.14 and
v0.10 through v0.13 never mentioned at all.

Both answered "what did each version add", which is a question only a maintainer
asks. Both are now landing pages: what the crate is, a quickstart above the fold,
badges, the MSRV, and what the harness does today, described as capabilities.

### Guides

The depth that made those 1072 lines worth reading is not deleted. It moved into
twelve pages under `docs/guide/` — permissions, verification, composition,
sandbox, durable runs, MCP and network egress, tools and skills, context and
memory, resilience, observability, documents, images and git — and every "the
limits, stated plainly" block travelled with its capability, because those are
the paragraphs that make the rest credible.

Two of the twelve are new writing rather than moved prose: observability and
replay had never been documented for a reader at all.

### Added

- A worked example on **every** one of the 106 public items re-exported from the
  crate root. Previously one item had one.
- `resume_tree_from_stored_policy` and `resume_tree_from_stored_policy_observed`.
  The tree loop was the only one of the three that could not be resumed under the
  policy it was started with, so the three resume paths disagreed about whether
  the permission boundary survives a restart. A tree with no stored policy is a
  typed `Error::Resume`, never a silent fall back to `Policy::permissive`.
- `docs/CONTRACT.md` as an actual contract: what is public and what is not, what
  pre-1.0 means here, the deprecation cycle, the MSRV policy, the feature table
  with what each feature costs, the platform matrix, and eleven limits that hold
  today.
- `docs/public-api.txt`, the enumerated public surface, compared against the live
  crate by a test.
- `[package.metadata.docs.rs]` with all features enabled and `doc_auto_cfg`, so
  the rendered page shows the documents and media surface with the feature each
  item needs labelled on it.

### The promises, and the checks that keep them

Three claims in this release decay into fiction unless something checks them, so
the checks ship rather than the promises. Each carries a negative control,
because a checker that silently matches nothing passes every input and reports a
green light wired to nothing.

- **Every public item has an example** — `tests/public_api.rs` enumerates the
  surface and fails naming any item without a fenced block. `cargo test --doc`
  compiles all of them, so an example cannot rot into a lie.
- **A rename or removal is announced** — the same file compares the surface
  against `docs/public-api.txt`. Removing or renaming an item fails the build
  until that file is edited by hand, which is the moment the `#[deprecated]`
  attribute and the migration note get written. There is deliberately no flag
  that regenerates it.
- **Every break has a migration note** — `tests/changelog.rs` finds every entry
  marked breaking and fails on any without one.
- **The prose matches the build** — `tests/docs_drift.rs` asserts the documented
  MSRV equals `rust-version` and the documented feature list equals the
  `[features]` keys in both directions, and that every relative link in the
  README and under `docs/` resolves.
- **The split lost nothing** — `tests/guide_pages.rs` names each capability and
  the limits block that had to travel with it.

### Fixed

Auditing the old README against the source, for the first time in fifteen
releases, found documentation that was not merely stale but wrong. All of these
are corrections to doc comments; none changes behaviour.

- `Verification`'s type documentation claimed the subject is compiled as its own
  crate with the criterion compiled against it. It is not: subject and criterion
  are one crate, the criterion in a child module. The separate-crate approach was
  tried during 0.8.1 and abandoned, because privacy is a wall between crates and
  a passing implementation is allowed to be private. The claim was wrong in the
  direction that made the 0.8.1 boundary sound stronger than it is.
- `TaskContract::with_tools` had no documentation. Its doc block had lost its
  closing line and merged into `with_images`, so the paragraphs explaining that
  registration grants availability and not authorization rendered under a
  `media`-gated method — that is, nowhere, on a default build.
- The sandbox module claimed a seccomp filter in three places. None is
  installed; the Linux backend is namespaces and rlimits, and any syscall
  restriction is the kernel's own default under an unprivileged user namespace.
- The sandbox module claimed `setrlimit` caps "CPU/procs/fds". It sets
  `RLIMIT_CPU` and `RLIMIT_NOFILE` only, and `SandboxLimits::max_processes` is
  enforced by nothing on any platform.
- A comment in `skills.rs` claimed a `README` is not discovered as a skill. The
  test is the `.md` extension and nothing else, so it is.
- Five unresolved intra-doc links in the documents module, which resolved under
  `--all-features` and dangled in the default build, plus six more across the
  provider modules and the context module. `cargo doc` is now warning-free on
  both builds.

### Changed

- The crate description was a 944-character comma-separated inventory of fifteen
  releases — the same field that reached 1041 characters at 0.11.0 and was
  refused by the registry's 1000-character cap. It is one sentence now.
- `docs/CAPABILITIES.md` is the guide index. It had also gone stale: it described
  0.15.0 as planned and listed image **and video** passthrough as a capability,
  though video was cut from the roadmap on 2026-07-27 and appears in no release.
- Historical entries in this file: 49 breaking changes across 16 versions are now
  marked and carry a migration note. The audit ran against the git tags rather
  than the prose, which is how it found that three releases claim in prose to
  break nothing and do.

### Known limitations

Documented in `docs/CONTRACT.md` rather than fixed here, because each is a
behaviour change and this release makes none. The two most likely to bite: a
registered tool named after one of the git, image or document built-ins passes
validation and is then permanently unreachable, since the reserved-name set
still lists only the original seven; and `resume_tree` does not record the policy
it executes under, so a tree resumed under a widened policy leaves an audit that
understates what was permitted.

## [0.15.0] - 2026-07-27

Images the model can see, and work the agent hands back as commits.

### The image half

A caller attaches images to the task with `TaskContract::with_images`, and they
ride every request — the task is about them, so they are never quietly dropped
partway through a run. The agent has its own route: `view_image`, a built-in
that takes a workspace path, gated on `Act::Read` against the path the model
named. That gate is the point. This is the model choosing which of the user's
files gets sent to a third party, so it is authorised per call on the real path
rather than once by tool name.

`CompletionRequest` gains a `media` field and `Provider` gains
`accepts_images`, which **defaults to `false`**. A provider implementation
written before this release keeps compiling and inherits a refusal rather than a
silent drop — a run that paid for a confident answer about an image the model
never received is invisible from the outside, because the response looks exactly
like success. The refusal happens before the request body is built, so nothing is
spent.

The wire shape is per vendor: an Anthropic base64 image content block, and an
`image_url` data URL for the OpenAI-shaped body OpenAI and OpenRouter share. A
request with no images still sends a bare content string, so every text-only body
is byte-identical to 0.14.0's.

MCP tool results carrying images are passed through, retiring a comment that had
promised this since 0.8.0. The trace records a digest, a size and a media type —
never the bytes.

All of it is behind an opt-in `media` feature whose only new dependency is
`base64`, already compiled in every build through `reqwest`. The default
dependency tree is unchanged.

### The git half

Five built-ins — `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit` —
so a run that edited four files ends as one reviewable commit instead of a
working tree someone has to reconstruct.

The shape of them is the release's real subject. `ExecGuard::check` enforces the
program name and records argv without checking it, so `Act::Exec("git")` is not
a boundary: it cannot tell `git log` from `git push --force`. Shipping git as an
exec allow-list entry would hand the model the whole binary under a rule that
reads like a restriction. Instead each built-in constructs its own complete argv,
and the model supplies paths, a message and a count — never a subcommand, a flag
or an argv. `push`, `fetch`, `clone`, `reset`, `checkout` and `rebase` are absent
by construction, and two tests keep them absent: one fails if a command variant
is added without being added to the covered set, and one asserts the subcommand
is always one of the five.

Two consequences are load-bearing rather than incidental. A model-named path can
never become a flag — every path goes after `--` and a leading `-` is refused
rather than escaped, because git parses its own argv and "escaped option" is not
a state that exists. And repository hooks do not run: `.git/hooks/*` is arbitrary
code carried by the repository the agent was pointed at, and nothing in this
crate's permission model covers it.

The path policy governs git on the paths git touches. Staging copies a file's
bytes into the object store, so `git_add` needs `Act::Read` on every path it
stages — which is what stops a policy-denied file from reaching a commit — and
`git_commit` needs `Act::Write` on `.git`. **A run under a narrow write policy
must allow `.git`, or commits are refused.**

Commit identity is supplied by the harness, defaulting to an agent name at an
RFC 2606 `.invalid` domain and overridable with
`TaskContract::with_commit_identity`. `git commit` fails outright with no
`user.email` configured, so this could not be left to the machine; inheriting the
repository's identity would attribute the agent's commit to whichever human
configured that checkout.

Git is a **runtime** capability, never a build dependency: no `git` on the
machine is an observation the model adapts to and the run carries on. The git
half adds no crate at all.

### Video is cut from the roadmap

Not deferred — cut, and it appears in no planned release. The roadmap promised
"image and video passthrough to any provider whose model accepts them". On the
evidence that resolves to one provider out of three and an unanswerable question:
the Anthropic Messages API and the OpenAI Chat Completions API accept images and
no video at all, and OpenRouter, the only one of the three carrying a `video_url`
content part, states that support varies by model and offers no way to ask which.
If video returns it will be a new roadmap entry argued on its own merits. Audio
is likewise absent, and OCR remains off the roadmap — named again here because
this is the image release and that is exactly the argument for folding it in.

### Breaking changes

- **BREAKING** — `CompletionRequest` gained a `media` field, so an exhaustive
  struct literal no longer compiles **when the `media` feature is on** (the field
  is `#[cfg(feature = "media")]`, so a default build is unaffected). The type now
  derives `Default`. *Migration:* construct it with the fields you set plus
  `..Default::default()`, which is the style the type documents and which survives
  the next field too:

  ```rust
  // 0.14.0
  let req = CompletionRequest { system, user, tools };
  // 0.15.0
  let req = CompletionRequest { system, user, tools, ..Default::default() };
  ```

- **BREAKING** — `TaskContract` gained the `images` and `commit_identity` public
  fields, so a struct literal over it no longer compiles. *Migration:* build the
  contract through `TaskContract::new` / `TaskContract::workspace` and the
  `with_*` builders — `.with_images(..)`, `.with_commit_identity(..)` — which is
  the documented path and does not change when a field is added.

- **BREAKING (behaviour)** — an out-of-tree `Provider` inherits
  `accepts_images() == false`, so a request carrying images is refused with
  `Error::Config` before the body is built. The method is defaulted, so the impl
  still compiles; a provider that genuinely accepts images will silently start
  refusing them. *Migration:* override it —
  `fn accepts_images(&self) -> bool { true }` — on any `Provider` impl whose
  vendor takes image content.

- **BREAKING (behaviour)** — `fill_form` writes a real `/AP` normal appearance
  stream per filled text field instead of setting `/NeedAppearances`. The output
  bytes of a filled PDF differ from 0.14.0's for the same input, and an empty
  value now removes the appearance rather than leaving a stale one. *Migration:*
  nothing to write; re-generate any checked-in expected PDF or byte-for-byte
  fixture rather than comparing against a 0.14.0 one.

### Added

- `Media`, `IMAGE_MEDIA_TYPES`, `MAX_IMAGE_BYTES`, `MAX_REQUEST_IMAGE_BYTES`, and
  `CompletionRequest::media`, behind the new `media` feature.
- `Provider::accepts_images`, defaulting to `false`.
- `TaskContract::with_images` and `TaskContract::with_commit_identity`; the
  `images` and `commit_identity` fields.
- The `view_image` built-in.
- The `git_status`, `git_diff`, `git_log`, `git_add` and `git_commit` built-ins,
  and `Identity`.
- MCP image content passthrough.

### Changed

- **`CompletionRequest` now derives `Default` and has a new field.** A caller
  constructing it with an exhaustive struct literal will not compile; add
  `..Default::default()`, which is the one-line fix and the construction style
  the type now documents. This is a source-breaking change in a minor release,
  which is what 0.x permits and what this crate has done before.
- Provider recordings taken against 0.14.x are refused rather than replayed
  against the new request shape. That is the existing series gate behaving as
  designed; re-record them.
- `fill_form` now generates a real `/AP` normal appearance stream per filled text
  field instead of setting `/NeedAppearances` and relying on the viewer to
  redraw. An empty value removes the appearance rather than leaving a stale one.
  Checkboxes and radios keep their existing state-selection behaviour; choice
  fields are drawn as plain text, left-aligned and single-line.

### Fixed

- `Media::byte_len` saturates rather than underflowing on a malformed base64
  payload, which is reachable from an MCP server — a trust boundary.

## [0.14.0] - 2026-07-27

Documents, governed by the rules that already govern source. An agent reads a
real `.xlsx`, changes one cell and writes the workbook back with the sheets it
did not touch intact; reads a `.docx` and generates one; generates a PDF,
extracts the text of an existing one, stamps a watermark across its pages and
fills its AcroForm fields; decodes a barcode or QR code out of an image; reads
the text of a slide deck.

That every one of those passes the 0.4.0 path policy on the path the model
actually named is the release, not a qualifier. The obvious way to ship a
document capability is a registered `Tool`, and the crate is explicit that a
registered tool is authorised once, by name, and that the policy "governs
whether it is *called*; it does not govern what it does once running — no
sandbox, no path scoping, and no egress control applies inside it." A
`docx_write` tool taking a path from the model would therefore write wherever
the model said, and `deny_write("secrets/*")` would not stop it. So these are
built-ins, dispatched in the same `match` as `read_file` and `write_file` and
gated per call on `Act::Read` or `Act::Write` against the path they name, over
new byte-level workspace IO that no document module bypasses. A refusal names
the file rather than the tool, and a sub-agent's narrowed policy applies to
documents exactly as it applies to source.

**What was cut, and why.** A reader who remembers the roadmap promising more
than this should find the reason here.

- **OCR is cut — off the roadmap, not deferred to a later release.** The owner's
  decision on 2026-07-27, and the evidence supported it independently. Every
  viable Rust path breaks a standing constraint that no CI runner installs a
  system package: the Tesseract binding needs the system library on all three
  runners, worst on Windows where it means vcpkg and a from-source C++ build
  plus libclang for bindgen, with language data distributed to every user. The
  pure-Rust alternative needs MSRV 1.89 against this crate's 1.88 floor, fetches
  its models over the network on first use, and recognises Latin script only. A
  capability that only works on machines that were prepared for it is not a
  capability this crate can claim. It appears in no planned release.
- **PowerPoint authoring is cut — off the roadmap, not deferred.** Same
  decision, same day. Generating a deck means writing slide layouts, masters,
  theme parts and the relationship graph that ties them together; hand-rolled on
  top of a zip writer, that produces a file PowerPoint may or may not open, and
  "may or may not" is not a capability. The one credible Rust crate is a
  46-star, single-maintainer, pre-0.3 project. Reading a deck is a projection
  and cannot corrupt anything, so the read half stays: `.pptx` is read-only
  here, and no write path for it exists in the public surface.
- **Editing a Word document in place is not claimed.** `docx-rs`'s reader models
  the OOXML it knows and drops what it does not, so read-then-write is a lossy
  rewrite rather than an edit. On a document this harness generated that costs
  nothing; on a user's real one — a comment thread, a content control, a field,
  a shape, a vendor extension — it silently deletes the parts the reader could
  not name, which is data loss presented as an edit. `xlsx_set_cell` exists
  because `umya-spreadsheet` genuinely round-trips a workbook it did not create;
  nothing in the Word ecosystem earns the same call yet.
- **Barcode and QR *generation* is not here.** The README's capability line
  promises read and generate; this release decodes and does not encode. The
  roadmap entry for 0.14.0 scoped barcodes to decoding off a page, and that is
  what shipped.
- **Deleting a document is not here either.** The same capability line promises
  create/edit/delete, but the harness has no delete for any file — there is no
  `delete_file` built-in for source text — so deletion is an absent file
  operation rather than a document gap, and this release does not introduce one.
- **High-fidelity PDF rendering and rasterisation is out of scope.** It means
  binding Pdfium, a per-OS binary the crate would have to tell users to install,
  which is the same constraint that removed OCR plus a redistribution question.

### Breaking changes

- **BREAKING** — `Verification` gained the `DocumentContains { file, needle }`
  variant, so an exhaustive `match` over `Verification` no longer compiles. This
  release's closing note says everything in it is additive; that is true of
  behaviour and of every existing item's name and shape, and it is not true of an
  exhaustive match. *Migration:* add an arm, or a `_ =>` arm:

  ```rust
  match verification {
      Verification::FileContains(s) => { /* ... */ }
      // ...
      Verification::DocumentContains { file, needle } => { /* ... */ }
      _ => { /* forward-compatible catch-all */ }
  }
  ```

### Added

- **The crate's first `[features]` section, with `default = []`.** The default
  build is unchanged: every document dependency is optional, so a consumer who
  does not want documents pays nothing for them. `documents` is an umbrella over
  the per-format features `xlsx`, `docx`, `pptx`, `pdf` and `barcode`, so a
  caller who wants spreadsheets does not compile a PDF stack:

  ```toml
  io-harness = { version = "0.14.0", features = ["documents"] }
  ```

  The MSRV floor stays at 1.88 with the features enabled; nothing here moves it.
- **Spreadsheets (`xlsx`)** — `sheet_names`, `read_sheet`, `write_new`, and
  `set_cell`, which changes one cell of an existing workbook and keeps the rest
  of it. Three crates, because the three jobs are separate: `calamine` reads and
  cannot write, `rust_xlsxwriter` writes new files and explicitly cannot modify
  an existing one, and `umya-spreadsheet` is the only one that round-trips a
  workbook it did not create. Preserving-edit fidelity is not promised for chart
  and drawing heavy workbooks, and the doc comment says so where a caller meets
  it rather than here.
- **Word (`docx`)** — `read_text` and `write_new`. Generate and read; there is
  deliberately no third function that edits an existing document, for the reason
  above.
- **PowerPoint (`pptx`)** — `read_text`, read-only. No presentation crate: a
  deck is a zip of XML and the slide text is the `<a:t>` content of
  `ppt/slides/slideN.xml`, which is a short walk over `zip` and `quick-xml`,
  two dependencies this tree can already justify. Layouts, masters and speaker
  notes are deliberately not read — boilerplate placeholder text and private
  notes are not what "the text of this deck" means.
- **PDF (`pdf`)** — `write_new`, `read_text`, `watermark` and `fill_form`. Text
  extraction is best-effort on reading order and says so: a PDF stores placed
  glyphs, not a document, so columns can interleave, tables lose their shape,
  and a scanned page contains no text at all and returns an empty string. Form
  filling sets `/NeedAppearances` as well as the field value, without which many
  viewers render a filled field blank — but that flag is a request to the viewer
  and not a rendered result, so a viewer that ignores it will still show the
  field empty.
- **Barcodes (`barcode`)** — `decode` returns every 1D and 2D symbol `rxing`
  supports (QR, Data Matrix, Aztec, PDF417, Code 128, Code 39/93, EAN-8/13,
  UPC-A/E, ITF, Codabar) without the caller naming the symbology first. An image
  with no code in it returns an empty result rather than an error, because "I
  looked and there was nothing there" is something the model can act on.
- **Twelve built-in tools the model can call**: `xlsx_read`, `xlsx_sheets`,
  `xlsx_write`, `xlsx_set_cell`, `docx_read`, `docx_write`, `pptx_read`,
  `pdf_read`, `pdf_write`, `pdf_watermark`, `pdf_fill_form` and
  `barcode_decode`, each present only when its feature is. A failure — a corrupt
  file, a form field that does not exist, a bad cell reference — comes back as an
  observation the model can read and adapt to, the same treatment a malformed
  regex gets from `grep`, and the run continues.
- **`Workspace::read_bytes` and `Workspace::write_bytes`** — byte-level IO
  through the same `check_path` gate every text read and write goes through,
  returning the same `Wrote` outcome. Every document byte in this release moves
  through them and no module opens a path itself, which is what makes the
  capability governable; they are useful on their own to any caller handling
  binary files. One deliberate difference from `read_file`: a missing file is an
  error rather than an empty buffer, because handing a parser zero bytes turns
  "there is no such file" into "this file is corrupt".
- **`Verification::DocumentContains { file, needle }`** — a stop condition that
  gates on a document's *extracted text*. It exists because
  `Verification::WorkspaceFileContains` reads with
  `read_to_string(..).unwrap_or_default()`, and every format here is a binary
  container, so on a document it reads the empty string and reports "does not
  contain" for every needle. That is a silent, permanent false FAIL rather than
  a false pass — a document task using the existing variant could not succeed at
  all. The variant is present in every build, feature or no feature: without the
  format's feature it returns a typed `Error::Config` naming the feature that
  was not enabled, rather than disappearing from the enum and breaking a
  consumer's exhaustive `match`.

Everything in this release is additive. No public item was renamed or removed,
no existing behaviour changed, no schema reshaped, and `CHECKPOINT_FORMAT` stays
at 7 — a consumer upgrades by changing the version number.

## [0.13.0] - 2026-07-27

A resumed run is the run it was. Resume restored the durable half of a run —
the step it reached, what it spent, how long it had been alive — and silently
substituted the rest: the permission policy it was executing under, and the
context it had assembled.

### Breaking changes

- **BREAKING (behaviour)** — `resume` and `resume_observed` refuse a run that was
  started under a non-permissive policy, returning `Error::Resume` where 0.12.0
  ran the agent permissively. *Migration:* call `resume_with` and hand it the
  policy the run was executing under —
  `resume(&contract, &provider, &store, run_id)` becomes
  `resume_with(&contract, &provider, &store, run_id, &policy, &approver)`. If a
  permissive resume is genuinely what you want, pass `Policy::permissive()` to
  `resume_with`. The worked before/after is under **Migrating from 0.12.0** below.
  A run started permissively, and any run recorded before 0.13.0, resumes exactly
  as it did.

- **BREAKING (behaviour)** — a resumed run restores its stored observation ledger
  instead of re-deriving context from the workspace, so the prompt a resumed step
  sends is the one the interrupted process would have sent rather than a freshly
  assembled one. *Migration:* nothing to write. If you compare a resumed run's
  `steps.prompt` against a 0.12.0 capture, re-capture it — the old bytes were the
  defect this release fixes.

### Added

- **`resume_with` and `resume_with_observed`** — resume an interrupted run under
  a permission policy, taking `policy` and `approver` in the same positions
  `run_with` uses. Until now there was no policy-preserving general resume at
  all: `resume` took no policy, and `resume_with_decision` took one but required
  a pending approval, so it served a run that paused for a human and not a run
  that crashed. The policy supplied is recorded against the run, so the store
  answers what rules the run actually executed under rather than only what it
  started under. The provider is re-authorized on resume rather than trusted from
  the interrupted run, matching what `resume_tree` already did: a host allowed
  before a crash may not be allowed after.
- **`Store::run_policy` and `Store::record_run_policy`** — the policy a run was
  started under, kept in a new `run_policies` table. `policy_events` recorded the
  decisions a policy produced, which is the opposite direction: a run that was
  never asked to do anything forbidden leaves no events, and a permissive run
  leaves none either, so the two were indistinguishable after the fact.
  `run_policy` returns `None` for a run that recorded nothing, which is
  deliberately not the same answer as a recorded permissive policy.
- **`Store::observations` and `Store::record_observations`** — the context
  observation ledger, made durable in a new `ledger_observations` table, one row
  per observation.
- **`Layer` and `Defaults` are re-exported** from the crate root, so a caller who
  reads a policy back can name the types inside it.
- **`Serialize`/`Deserialize` on `context::Ledger`, `Observation` and
  `ObsKind`**, with `ObsKind` rendering as ten stable snake_case strings.

### Changed

- **`resume` and `resume_observed` now refuse a run that was started under a
  permission policy**, returning `Error::Resume` naming the run and pointing at
  `resume_with`. See the migration note below. A run started permissively, and a
  run recorded before 0.13.0, resume exactly as they did.
- **A run's context is restored on resume rather than re-derived.** The
  observation ledger the 0.10.0 context assembler builds was in memory only, and
  was constructed empty at the top of both the workspace and sub-agent loops
  after the resume step was already known. A resumed run therefore re-assembled
  its context from the workspace and asked the model a different question than
  the process before it would have. It usually recovered, which is why this
  survived five releases; what it cost is that a resumed run was not comparable
  to an uninterrupted one.
- **A recorded case now replays identically across an interruption in every
  loop**, not only the single-file one. 0.12.0 shipped the honest boundary here —
  a replay that could not reproduce a request refused loudly rather than
  answering from the wrong recording — and recorded the cause in
  `iterations/US-IO-HARNESS-0.12.0-I01`. The refusal remains for a request that
  genuinely was never recorded; what is gone is the reason it was being reached
  falsely.

**Migrating from 0.12.0.**

`resume` on a run that was started under a non-permissive policy now returns
`Error::Resume` where it previously ran the agent permissively. Change:

```rust
// Before — the boundary was silently dropped.
io_harness::resume(&contract, &provider, &store, run_id).await?;

// After — supply the policy the run was executing under.
io_harness::resume_with(&contract, &provider, &store, run_id, &policy, &approver).await?;
```

An error is the default rather than a silent downgrade because the two failure
modes are not symmetric: a refused resume is visible and fixed in one line,
while a silently permissive one is a boundary the caller believes is enforced
and is not. If a permissive resume is genuinely what you want, pass
`Policy::permissive()` to `resume_with` and the decision is recorded as yours.

Every other resume entry point is unchanged. `resume` on a run started
permissively, on a run with no recorded policy, or on a run that has already
finished behaves exactly as it did in 0.12.0 — a finished run reports its
outcome regardless of policy, because it drives nothing.

Two new tables are created additively in `Store::from_conn`;
`CHECKPOINT_FORMAT` stays at 7, so a 0.12.0 store opens and a 0.12.0 checkpoint
resumes. A run checkpointed before 0.13.0 has no recorded policy and no durable
ledger, and resumes with 0.12.0's behaviour rather than being refused or having
a boundary invented for it.


### Fixed

- **A resumed workspace run no longer loses its permission boundary.** Through
  0.12.0, `resume` and `resume_observed` substituted `Policy::permissive()` and
  `ApproveAll` for every workspace run they resumed. A caller who ran under a
  deny-by-default path policy through `run_with`, whose process then died, got an
  agent with no boundary at all under the same run id — and nothing said so,
  because the trace showed no refusals for the simple reason that nothing
  refused. The crate stated the opposite principle sixty lines earlier, where
  single-file mode refuses a policy loudly so that a caller never believes a
  boundary is enforced when nothing is checking.

### Security

- The fix above is the security-relevant one. If you resume workspace runs and
  pass a policy to `run_with`, treat 0.12.0 and earlier as not enforcing that
  policy across a resume, and upgrade. Nothing needs to be re-issued or revoked:
  the boundary was dropped on resume, not leaked.

## [0.12.0] - 2026-07-27

The twelfth and last of the twelve pillars: observability and evaluation. The
harness is now what it was defined to be.

### Breaking changes

- **BREAKING** — `RunOutcome` gained the `Refused` and `Cancelled` variants, so an
  exhaustive `match` over it no longer compiles. *Migration:* add
  `RunOutcome::Refused` and `RunOutcome::Cancelled` arms, or a `_ =>` arm. Both
  are terminal: treat them as "the run is over and did not succeed".

- **BREAKING (behaviour)** — `Store::open` sets `journal_mode = WAL`, which is a
  persistent property of the database file rather than of the connection that set
  it. A store opened once by 0.12.0 stays in WAL, and rolling back to 0.11.0 does
  not undo it. *Migration:* nothing to write — 0.11.0 reads a WAL database
  happily. If a tool of yours copies the database file, copy the `-wal` and `-shm`
  sidecars with it, or run `PRAGMA wal_checkpoint(TRUNCATE)` first.

- **BREAKING (behaviour)** — `Containment::max_total_duration` is enforced.
  Declared in 0.5.0 and never read, so a tree that ran past it previously carried
  on; it now halts, measured from the root run's `started_at` and therefore
  counting time the process was down. *Migration:* raise the value to the tree's
  real wall-clock horizon, or set `max_total_duration: None` to keep 0.11.0's
  behaviour of no limit.

- **BREAKING (trace)** — `context_events` records a `replan` kind distinctly from
  `stalled`. A consumer matching `kind == "stalled"` to mean "was nudged and
  carried on" now misses every replan. *Migration:* match both —
  `kind == "stalled" || kind == "replan"` for the old union, and `kind ==
  "replan"` alone for "was nudged", `kind == "stalled"` alone for "gave up".

- **BREAKING (trace)** — the `"net_deny"` `sandbox_events` kind is removed. It was
  documented from 0.6.0 and never constructed, so no store has ever contained a
  row with it. *Migration:* delete the arm; network decisions are in
  `policy_events` with `act = "net"`.

- **BREAKING (trace)** — a durable memory note no longer renders the writing run's
  id into the prompt, and a tree composes its children's results in spawn order
  rather than completion order. Both change `steps.prompt` and `steps.result` for
  the same task run twice, which is the point: they were the two sources of
  run-to-run drift. *Migration:* nothing to write; re-capture any expected trace
  text taken from a 0.11.0 run.

### Added

- **Watch a run while it happens.** `Observer` is called as the run proceeds —
  steps, tool calls, refusals, approvals, spend draws, retries, fallbacks,
  stalls, spawns, memory writes, sandbox events, MCP calls and the outcome.
  Until now the crate exported no observer, callback or channel of any kind, so
  an application showing progress had to open the SQLite file with a second
  connection and poll a schema the crate never promised. Every `RunEvent`
  serialises to a flat, tagged JSON object, so a host process can forward events
  to a user interface written in another language without hand-writing a mapping.
  The events report the same facts the durable trace records, and that agreement
  is asserted rather than assumed — if the two ever disagree, the trace is right.
- **Stop a run from outside it.** `Flow::Cancel` returned from an observer ends
  the run at its next step boundary, records the outcome, and leaves it
  resumable. Previously the only way to stop a run in flight was to drop its
  future, which abandoned it mid-step and left `runs.status` as `running`
  forever — indistinguishable from a crashed process.
- **A per-run outcome record.** `Store::run_summary`, or `RunResult::summary` if
  you are holding a result, returns whether the run succeeded, its step count,
  its token spend and its duration, from one read.

  It is a method rather than a new `RunResult` field on purpose. A field would
  have to be filled at every entry point's return site — including the ones that
  return `Err` and never build a `RunResult` — so the caller's copy and the
  store's row could drift. Reading it from the store makes them the same row by
  construction, and as a side effect **this release breaks nothing**: no field is
  added, so an exhaustive struct pattern over `RunResult` still compiles.
  Three of those were derivable only by knowing which of eleven free-text
  outcome strings means success, that steps is `MAX(step)` rather than
  `COUNT(*)` because retry rows share a step number, and that spend is
  `SUM(steps.tokens)`. The fourth was not available at all: nothing recorded
  when a run *ended*, and `elapsed_secs` measures against `now`, so it keeps
  growing after the run is over. Written by `finish_run`, so a run that escalates
  or is refused — and therefore returns `Err` without ever producing a
  `RunResult` — still gets one.
- **A public request deadline.** `OpenRouter`, `Anthropic` and `OpenAi` each take
  `with_timeout(Duration)`, and `REQUEST_TIMEOUT` is public. This is the
  correction to 0.11.0's claim that the deadline was overridable; see the
  annotation on that entry.
- `AgentEvent` and `SpawnRow` are exported. Both were `pub` inside a private
  module and never re-exported, so `Store::agent_events` returned a `Vec` of a
  type no external caller could name — leaving `agent_events`, the only audit of
  per-step budget draws, unreadable through the public API.
- Provider types (`CompletionRequest`, `CompletionResponse`, `ToolCall`,
  `ToolSpec`, `Usage`) derive `Serialize`, `Deserialize` and `PartialEq`.
- `BUSY_TIMEOUT` and `SUCCESS_OUTCOME` are public constants.

### Changed

- `Store::open` sets `journal_mode = WAL` and a busy timeout. A store the crate
  hands out is now safe for a second reader without that reader configuring the
  file behind the API's back. **WAL is a persistent property of the database
  file**, not of the connection that set it: a store opened once by 0.12.0 stays
  in WAL afterwards, and rolling back to 0.11.0 does not undo it (0.11.0 reads
  WAL happily; it simply never set it).
- A tree composes its children in **spawn order** rather than completion order.
  The fan-out used `buffer_unordered`, so the composed observations and the
  decisions list — which become `steps.result` and `steps.decision` — came back
  in whatever order children happened to finish, making the same task over the
  same workspace produce a different trace run to run. Concurrency is unchanged
  and still bounded by `Containment::max_concurrent`; only when a finished
  child's result is *read* changed.
- Durable memory notes no longer render the writing run's id into the prompt.
  `run_id` is an `AUTOINCREMENT`, so the same case run twice sent the model
  different prompt bytes — and that string was persisted into `steps.prompt`.
  The note's `step` is kept: it is a stored column and stable across replays.
- `context_events` records a **`replan`** kind distinctly from **`stalled`**.
  Both were the one `"stalled"` kind, told apart only by an English sentence in
  `detail`, so nothing scoring a run could distinguish "was nudged and carried
  on" from "gave up" without matching prose the crate never promised.
- `Containment::max_total_duration` is **enforced**. Declared in 0.5.0 and never
  read, so a caller could bound a 24-hour tree's wall-clock and have it silently
  ignored. Measured from the root run's `started_at`, so it counts the whole
  tree's life including time the process was down.
- The step boundary is emitted from one place for all three loops, which
  previously each had their own copy of the commit and their own differently
  named log line.

### Deprecated

### Removed

- The `"net_deny"` `sandbox_events` kind, which was documented from 0.6.0 and
  never constructed or emitted. Removed rather than implemented: a sandbox denies
  egress *structurally* — the backend gives the child no route out — so there is
  no attempt to observe. Network decisions the harness does make are in
  `policy_events` with `act = "net"`.

### Fixed

- **A refused run is terminal.** `RunOutcome::Refused` is added and mapped, so
  resuming a run a human denied network access for reports the refusal. It had no
  variant and no `terminal_outcome` arm since 0.8.0 — the same defect 0.11.0
  fixed for `"escalated"`. The consequence was worse than a repeated question:
  because `resume` substitutes `Policy::permissive()` and `ApproveAll`, the
  resumed run dialled the socket, wrote the file and reported `Success`.
- `Containment::max_total_cost` is documented as reserved and not enforced,
  rather than left looking functional. It cannot be enforced: a provider reports
  tokens and never a price, so any figure compared against would be invented.
  Bound spend with `max_total_tokens`.

### Security

- Resuming a network-refused run no longer performs the refused access. See the
  `RunOutcome::Refused` entry above.

  The **broader issue is not fixed in this release, and you should know about
  it**: `resume` substitutes `Policy::permissive()` and `ApproveAll` for *any*
  resumed workspace run, so a caller who ran under a restrictive policy via
  `run_with` and then resumed via `resume` silently loses that boundary. The
  crate states the opposite principle elsewhere — it refuses a policy it cannot
  enforce rather than ignoring one — so this is a defect, not a design.

  There is currently **no policy-preserving general resume**.
  `resume_with_decision` does take a policy, but it exists only to deliver a
  pending approval: it requires a `request_id` and a `Decision`, so it cannot
  resume a run that merely crashed. Until this is fixed, a policy-governed run
  that dies mid-flight should be re-run rather than resumed if the boundary
  matters. Fixing it changes a public signature and is tracked for its own
  release.

## [0.11.0] - 2026-07-26

The release that lets a long unattended run survive a bad afternoon at a
provider. 0.7.0 made a run survive a crash; it did nothing for a rate limit, a
rolling deploy, a 503, or a hung socket — and the audit for this release found
three of those were worse than assumed rather than merely unhandled.

Recovery and retry is the eleventh of the twelve pillars to close. Only
observability and evaluation (0.12.0) remains.

### Breaking changes

- **BREAKING** — `Error::Provider(String)` is now a struct variant,
  `Error::Provider { kind, status, retry_after, message }`. Every `match` on it
  breaks, and so does every construction. *Migration:*

  ```rust
  // 0.10.0
  Err(Error::Provider(format!("openrouter: {e}")))
  // 0.11.0 — pick the constructor that names what happened
  Err(Error::provider_transport(format!("openrouter: {e}")))
  // also: Error::provider_status(status, retry_after, msg),
  //       Error::provider_malformed(msg), Error::provider(kind, msg)

  // 0.10.0
  Err(Error::Provider(msg)) => eprintln!("{msg}"),
  // 0.11.0
  Err(Error::Provider { kind, message, .. }) => eprintln!("{kind:?}: {message}"),
  ```

- **BREAKING** — `Workspace::write_file` and `FsTool::write` return
  `Result<tools::workspace::Wrote>` instead of `Result<()>`. `write_file(..)?;`,
  `let _ = ..`, `.is_ok()` and `.unwrap()` are unaffected. *Migration:* only an
  explicit unit pattern or annotation changes — `let () = ws.write_file(p, c)?;`
  becomes `let _wrote = ws.write_file(p, c)?;`, and a `-> Result<()>` wrapper
  becomes `-> Result<Wrote>` or ends in `Ok(())` after discarding the value.

- **BREAKING** — `RunOutcome` gained the `Escalated { steps, retryable }` and
  `Stalled` variants, so an exhaustive `match` over it no longer compiles.
  *Migration:* add both arms, or a `_ =>` arm; `Escalated.retryable` tells you
  whether re-running is worth it.

- **BREAKING** — `TaskContract` gained the `retry: RetryPolicy` and
  `stall: StallPolicy` public fields, so a struct literal over it no longer
  compiles. *Migration:* build through `TaskContract::new` / `workspace` plus
  `.with_retry_policy(..)` and `.with_stall_policy(..)`.

- **BREAKING (behaviour)** — stall detection is on by default
  (`StallPolicy { window: 3, max_replans: 1 }`). A run that repeats a tool call
  without changing the workspace is nudged once and then ends as
  `RunOutcome::Stalled` instead of spending its way to the step cap.
  *Migration:* to keep 0.10.0's behaviour exactly, disable it —
  `contract.with_stall_policy(StallPolicy { window: 0, ..Default::default() })`.

- **BREAKING (behaviour)** — a provider response that parses to no text, no tool
  call and no usage is `Error::Provider { kind: ProviderErrorKind::Malformed, .. }`
  where it used to return `Ok` with an empty response that the loops read as "the
  model chose not to call a tool". A response that parses and legitimately
  contains no tool call is unchanged. *Migration:* handle the error; if your code
  relied on the empty-`Ok` shrug to end a run, match `Malformed` and decide there.

- **BREAKING (behaviour)** — an `Auth` failure escalates on its first occurrence
  rather than consuming `max_retries` first, and a retry that does happen now
  *waits* (doubling per attempt, honouring `Retry-After`) where 0.10.0 retried
  immediately. A run's wall clock therefore changes. *Migration:* nothing to
  write; bound the waiting with `with_retry_policy(RetryPolicy { max, .. })` — no
  retry is allowed to sleep past the run's time budget.

### Added

- **Provider failures carry a kind.** `ProviderErrorKind` — `Transport`,
  `Timeout`, `RateLimited`, `Server`, `Auth`, `Request`, `Malformed` — with
  `is_retryable()`, the HTTP status preserved rather than formatted into prose,
  and the server's `Retry-After` kept when it sent one (delta-seconds or an
  HTTP-date; an unparseable value degrades to "no hint", never to a wrong wait).
- **Retry waits, and only for what is worth waiting on.** `RetryPolicy` doubles
  the delay per attempt to a ceiling, honours `Retry-After` above that ceiling
  because the server knows its own limit better than a default does, and refuses
  to sleep past the run's time budget — waiting is not a way to escape a limit.
  An `Auth` failure now escalates on its first occurrence.
- **A request deadline.** _(Correction, 0.12.0: this entry named
  `net::http_client()` and `http_client_with_timeout` as though a caller could
  reach them. They are `pub(crate)` in a private module, so the deadline shipped
  as documentation of a capability nobody had. 0.12.0 adds the public
  `with_timeout` that makes the sentence below true. The entry is annotated
  rather than rewritten — it is what 0.11.0 claimed.)_
  `net::http_client()` sets a request timeout, and
  `http_client_with_timeout` overrides it. The default is 600s: chosen from the
  slow end of the legitimate side, since a full 8192-token stream at a sluggish
  15 tokens/second is about nine minutes, and killing a real completion is worse
  than one hung socket costing ten minutes once.
- **Provider fallback.** `Fallback::new(primary, secondary)` is itself a
  `Provider`, so every existing entry point takes it unchanged, and it nests for
  three. It falls through only on a failure another provider might not have — a
  wrong key is not more valid at a different vendor. Which provider actually
  served a step is recorded per step, since one label for a whole run stops being
  true the moment a run can use two.
- **`Provider::endpoints()`**, defaulted to whatever `endpoint()` reports. This
  is what authorization uses, and it exists because a combinator reporting only
  its primary's host would let a fallback dial the secondary's host without the
  deny-by-default egress policy ever seeing it.
- **Stall detection and one bounded replan.** `StallPolicy` decides when an agent
  has stopped getting anywhere: `window` consecutive steps that change nothing in
  the workspace AND repeat a tool call the window already saw. On the first stall
  the agent is told, in its context, what it already tried; if it stalls again the
  run ends as `RunOutcome::Stalled` rather than spending the rest of its budget.
  `StallPolicy { window: 0, .. }` disables detection entirely.
- **`Wrote`**, returned by `Workspace::write_file` and `FsTool::write`:
  `Created`, `Changed`, or `Unchanged`. Content is compared, never metadata, so a
  same-length different-content write is `Changed`.
- **`RunOutcome::Escalated { steps, retryable }`**, so a caller learns whether
  what ended the run was survivable.
- **Trace rows** for every retry (naming the kind and the delay), every stall and
  replan, and the provider that served a step.

### Changed

- **Breaking (permitted pre-1.0): `Error::Provider(String)` is now a struct
  variant** — `{ kind, status, retry_after, message }`. Every `match` on it
  breaks. Constructing one: `Error::provider_transport(msg)`,
  `Error::provider_status(status, retry_after, msg)`,
  `Error::provider_malformed(msg)`, or `Error::provider(kind, msg)`. Matching
  one: bind the fields you need and branch on `kind`. The rendered `Display` text
  now names the kind and the status.
- **Breaking (permitted pre-1.0): `Workspace::write_file` and `FsTool::write`
  return `Result<Wrote>`** instead of `Result<()>`. `write_file(..)?;`,
  `let _ = ..`, `.is_ok()` and `.unwrap()` are all unaffected; only an explicit
  `Ok(())` pattern or a `Result<()>` annotation needs changing.
- **A malformed response is an error rather than a shrug.** A stream that
  produced no text, no tool call and no usage is `ProviderErrorKind::Malformed`.
  It used to return `Ok` with an empty response, which the loops read as "the
  model chose not to call a tool" — so a garbage response spent a step, was never
  retried, and was invisible in the trace. A response that parses and legitimately
  contains no tool call keeps its previous meaning exactly.
- **A retry trace row names what it retried and how long it waited**, where it
  used to say only `retry N after error`.

### Fixed

- **An escalated run is no longer silently restarted by the next resume.**
  `"escalated"` was absent from `terminal_outcome` and `finish_run` filed it as a
  plain completion, so `resume` found no terminal outcome and fell straight back
  into the loop — an unattended run that escalated at 3am was re-run by whatever
  resumed it. It now reports `RunOutcome::Escalated`, and the retryable and
  terminal cases are recorded distinctly so a resume, a trace reader and the
  caller all reach the same conclusion.
- **A hung provider no longer hangs a run forever.** With no request timeout, a
  server that accepted a connection and then stopped writing produced no step, so
  0.7.0's checkpointing never fired, the time budget — checked only at the top of
  a step — was never reached, and the 0.5.0 ledger saw no draw. The run was simply
  gone, with no remedy but killing the process.

### Security

- **A fallback cannot reach an unauthorized host.** Provider authorization now
  checks every endpoint in the chain before the run's first step, not just the
  first one, so the 0.8.0 deny-by-default egress policy governs a secondary
  provider exactly as it governs a primary.
- **Fallback does not promise equivalence.** Falling over swaps the model
  mid-run, and the harness cannot see whether the replacement is as capable or
  even the same size. An operator configuring two providers should expect a run
  that fell over may behave differently from one that did not, which is why the
  provider that answered is recorded per step rather than inferred from
  configuration.

## [0.10.0] - 2026-07-26

The release that stops the prompt from growing. Through 0.9.1 the workspace loop
kept a single string, appended every tool result to it, and re-sent the whole
thing verbatim on every turn — so a twenty-step run spent most of each request
re-sending text the agent had already acted on, and a file read at step 3 was
still presented as current after being rewritten at step 7. The token budget was
enforced; it was being spent on repetition.

0.10.0 assembles what the model sees instead of accumulating it, and gives the
agent memory that survives the run.

Two of the twelve pillars close here: **context construction** and **state and
memory**. Recovery and retry (0.11.0) and observability and evaluation (0.12.0)
are the two that remain.

### Breaking changes

- **BREAKING** — `TaskContract` gained the `context: ContextBudget` public field,
  so a struct literal over it no longer compiles. Code that builds the contract
  through `TaskContract::new` or `TaskContract::workspace` — every documented path
  — is unaffected. *Migration:*

  ```rust
  // 0.9.1
  let contract = TaskContract { goal, target, verify, /* ... */ };
  // 0.10.0 — either add the field
  let contract = TaskContract { goal, target, verify, context: ContextBudget::default(), /* ... */ };
  // or move to the constructor, which does not change when a field is added
  let contract = TaskContract::new(goal, target, verify).with_context_budget(ContextBudget::default());
  ```

- **BREAKING (behaviour)** — the workspace loop assembles the request per turn
  under a context budget instead of appending every tool result to one string and
  re-sending it. The model no longer sees the full verbatim history: superseded
  reads and greps become one-line stubs, a read a later write invalidated is
  re-read, and every observation kind is capped. A prompt captured from a 0.9.1
  run will not reproduce. *Migration:* nothing to write — `steps.result` still
  holds the full unelided text, so an audit is unaffected. Raise the ceiling with
  `with_context_budget(ContextBudget { max_tokens, share })` if a task genuinely
  needs more history in the request.

### Added

- **A context budget, and per-turn assembly under it.** The new `context` module
  holds a `Ledger` of typed `Observation`s — history, never trimmed — and an
  assembly step that decides per turn what of that history the request carries.
  `TaskContract::with_context_budget(ContextBudget { max_tokens, share })` sets
  the ceiling: an absolute per-request cap and a share of the token budget the
  run has left, whichever is lower. One budget derives both that ceiling and the
  per-observation cap, so they cannot drift apart.
- **Compaction of superseded observations.** Two reads of one path, or two greps
  of one pattern, are one answer: the later is carried whole, the earlier becomes
  a one-line stub naming the step that replaced it. Supersession applies only
  where the target *is* the subject of the answer — a registered or MCP tool
  called twice with different arguments keeps both results, because the target
  there is the tool's name.
- **Re-read of an observation a later write invalidated.** A read whose path is
  written later in the run is refreshed at assembly time, through the same policy
  and the same workspace containment as any other read. A refusal or a missing
  path becomes a stub naming the write that invalidated it and why the refresh
  failed, and both outcomes are trace rows.
- **Durable, workspace-keyed memory.** A built-in `remember` tool records a fact
  or decision; a later run over the same workspace gets it back in its assembled
  context, rendered as the agent's own notes rather than as instructions. On the
  operator's side, `Store::memory_list`, `memory_get`, `memory_put`,
  `memory_delete` and `memory_clear` list, read, write, remove and clear entries.
  Every entry is attributed to the run and step that wrote it, and the set is
  bounded by a count cap and a total-size cap with oldest-first eviction.
- **Assembly is in the trace.** A new `context_events` table records, per turn,
  how many observations were carried, stubbed and re-read, the estimated tokens
  the assembler enforced against, and the provider's own reported usage for that
  same request — so the estimator's drift is a recorded number rather than an
  assumption. Memory writes, evictions and recalls are rows there too.

### Changed

- **Breaking (permitted pre-1.0): `TaskContract` gained a public field,
  `context: ContextBudget`.** Code that builds the contract through
  `TaskContract::new` or `TaskContract::workspace` — every documented path — is
  unaffected. Code that built it with a struct literal must add
  `context: ContextBudget::default()`, or move to the constructors. Nothing else
  a 0.9.1 caller wrote changes: no public item was renamed or removed, a 0.9.1
  store opens under 0.10.0, and a 0.9.1 checkpoint resumes — `CHECKPOINT_FORMAT`
  is unchanged and both new tables are additive.
- **`find` and `write_file` observations are bounded.** They had no cap at all;
  one `find` over a large repository could exhaust a request on its own. Every
  observation kind now enters the context under the same budget-derived cap, with
  the elision visible to the model.
- **`TOOL_RESULT_CAP`, `OBS_READ_CAP` and `OBS_GREP_CAP` are no longer
  independent constants.** The size ceilings are derived from the context budget;
  `OBS_GREP_CAP`'s 50-hit ceiling remains as what it always was — a relevance
  choice, not a size one.
- **The trace still records the whole log.** `steps.result` holds the full,
  unelided observation text. Bounding what the model sees does not bound what an
  operator can audit, and the two bounds are separate on purpose.
- **The sub-agent loop and the single-file loop are bounded too.** 0.5.0's
  concurrent children each assembled their own unbounded log, and the single-file
  loop re-sent the entire current file every turn.

### Security

- **Memory is attributed, bounded, inspectable and clearable.** A fact one run
  records is read by later runs over that workspace, which is the point and also
  the risk: a wrong or planted note persists until it is removed. Entries carry
  the run and step that wrote them, are rendered to the model as its own notes
  rather than as directives, are capped in count and total size, and can be
  listed and deleted through `Store`. The README states this limit plainly.
- **Freshening a stale read is as contained as the read it replaces.** The
  assembly-time re-read goes through the workspace's own path resolution and the
  same `Act::Read` policy check, so it cannot reach a path the run itself may not
  read, and `Ask` is treated as a refusal because assembly has no approver.

## [0.9.1] - 2026-07-26

The first release verified on more than one operating system. 0.9.0 added the
repository's first CI and its Linux and Windows legs went red immediately —
not from anything 0.9.0 introduced, but from defects that had been shipping
since 0.3.0, 0.4.0 and 0.6.0 and that no amount of local testing could see.
This release makes the three-OS matrix green.

Nothing here changes the public API. Every fix is behaviour that was already
promised and not delivered on a platform other than macOS.

### Breaking changes

This release states that nothing here changes the public API, and that is true of
every signature. Three of its fixes change behaviour a caller could have been
depending on, and are marked here for a reader upgrading across it.

- **BREAKING (behaviour, Windows)** — a path deny rule that silently failed open
  now fires. Any `deny_read` / `deny_write` whose pattern or target used `\`, or
  was derived from a `std::fs::canonicalize` result (`\\?\C:\...`), never
  matched and the access was **allowed**. A Windows run that succeeded under such
  a policy may now be refused — correctly. *Migration:* nothing to write; the
  refusals you now see are the ones the policy always described. Read
  `Policy::explain` for the rule and layer that decided.

- **BREAKING (behaviour, unix)** — path matching on unix is literal on both sides.
  Through 0.9.0 the *target* was folded (`\` to `/`) while the pattern was not,
  so a pattern like `logs/*` could match a unix file literally named `logs\a.txt`.
  It no longer does — on unix `\` is a legal filename character and folding it
  merges two distinct paths. *Migration:* if you have a rule that relied on the
  fold, write the pattern with the separator the path actually uses:
  `deny_read("logs\\*")` for a literal backslash, `deny_read("logs/*")` for a
  directory.

- **BREAKING (behaviour)** — a sandbox wrapper that fails to start is
  `Error::Sandbox` instead of an indistinguishable "verification failed", and a
  resource cap that cannot be applied fails the spawn instead of running the
  payload uncapped. *Migration:* handle `Error::Sandbox` on the paths that call
  verification — code that treated every non-pass as "the model's code is wrong"
  now receives an `Err` for "the sandbox never ran it", which is the distinction
  the change exists to make.

### Added

### Changed

### Deprecated

### Removed

### Fixed

- **A path deny rule no longer fails open on Windows.** `Act::Read` and
  `Act::Write` patterns were matched against targets literally, so a rule and a
  target that named the same file with different separators did not match — and
  a rule built from a `Path`, as `std::fs::canonicalize` returns one
  (`\\?\C:\...`, backslashes), never matched the target it was written to cover.
  The deny simply did not fire and the access was **allowed**. Any consumer
  writing `deny_read("C:/secrets/*")`, or deriving a pattern from a path, had no
  protection there. Latent since 0.4.0 and unseen because the suite had never
  run on Windows. Both the pattern and the target now go through one
  normalisation — verbatim `\\?\` and `\\?\UNC\` prefixes stripped, `\` folded
  to `/` — so the two sides agree on what "the same path" is. Scoped to path
  acts: `Act::Exec` targets a binary name and `Act::Net` a host, where `\` is
  not a separator, and both keep matching literally. Scoped to Windows: on unix
  `\` is a legal filename character, and folding it would merge two distinct
  paths — the same fail-open bug reversed — so unix matching is now literal
  there too, where the target (but not the pattern) had previously been folded.
- **The Linux sandbox no longer fails every verification on a kernel that
  restricts unprivileged user namespaces.** The `unshare` wrapper was never
  probed and was selected unconditionally on Linux, so on hosts such as Ubuntu
  24.04 (`kernel.apparmor_restrict_unprivileged_userns=1`) every sandboxed
  `rustc` spawn failed and the caller was told its code had failed to compile.
  The wrapper is now probed once per process; when it does not work the run
  degrades to the portable floor and *reports* `Backend::PortableFloor`, which
  the trace records, so a degraded run is auditable rather than silent.
- **A sandbox wrapper failure is no longer reported as a failed verification.**
  When the `unshare` wrapper itself fails, the command never ran, and that is now
  `Error::Sandbox` instead of an indistinguishable "verification failed". A
  failing command's own stderr is also no longer discarded — both the sandboxed
  and the direct-host exec paths log it, so a diagnosis reads the compiler's
  error instead of inferring it.
- **The CPU cap now names itself on Linux.** Caps were applied with the soft and
  hard `rlimit` equal; Linux tests the hard limit first and `SIGKILL`s there, so
  `SIGXCPU` was never sent and a CPU-exhausted run was reported with no
  `cap_hit`. The hard limit is now kept one above the soft one, clamped to what
  `getrlimit` reports so it is never raised. A cap that cannot be applied now
  fails the spawn instead of silently running the payload uncapped.
- **The memory cap now measures the process tree, not just the process it
  spawned.** A payload that forks — which is what Linux `/bin/sh` does — grew in
  a child the monitor never watched, so the cap never fired and the run finished
  cleanly. The monitor now sums RSS across the process and its descendants and
  kills descendants first. It also no longer treats an unreadable process table
  as "the process is gone", which previously switched the cap off for the rest of
  the run after a single hiccup.
- **The wall-clock cap now actually kills on Windows, and its kill reaches the
  whole process tree everywhere.** The timeout *owned* the child, so expiry
  dropped it and the only kill left was `kill_on_drop`, which terminates the one
  process the harness spawned and not the descendants a shell puts the real work
  in. On unix those reparented; on Windows they also kept the pipes open, which
  stranded the blocking reads tokio uses there and hung the caller's runtime long
  after the cap had "fired" — a run that could never be stopped. The wait is now
  its own task, so the child is still alive when the clock expires and is killed
  by pid: `SIGKILL` descendants-first on unix, `taskkill /T` on Windows (a system
  utility, not a new dependency).
- **The sandbox no longer implies caps it cannot apply on Windows.** The CPU cap
  (`RLIMIT_CPU`) and the memory cap (an RSS monitor over `ps`) are unix
  mechanisms; on Windows neither is applied, so the floor there enforces the
  **wall clock only** — no CPU, memory, or process cap, and no kernel network
  boundary. A `Cap::Cpu` or `Cap::Memory` hit is never reported on a platform
  that applied no such cap, a run configured with one warns once that it is not
  in force, and the docs and README now say so, the same honesty 0.9.0 applied to
  the backend label.

### Security

## [0.9.0] - 2026-07-26

The tool layer, closed. 0.8.0 made the crate extensible *out of process*, which
is the right boundary for a capability that already lives elsewhere and the
wrong one for a capability already linked into the same binary — a second
process, a transport, and a serialization hop to call a function that is one
`await` away. 0.9.0 adds the in-process half: implement the public `Tool` trait
for something the embedding program already knows how to do, register it on the
task contract, and the model is offered it beside `grep`, `find`, `read_file`,
and `write_file`.

Registration makes a tool *available*; it does not authorize it. Every call is
an `Act::Exec` check on the tool's name under the same deny-first 0.4.0 policy
stack that decides paths, binaries, and hosts, so an operator can hand an agent
a toolbox and still refuse one tool in it.

Alongside it, skills: a directory of markdown instruction files that shapes
*how* the agent approaches a class of task, without touching Rust. Names and
one-line descriptions go into the system prompt; a body is loaded on demand
through a built-in `read_skill` tool, as an ordinary policy-checked file read.
Nothing in a skill executes.

Upgrading from 0.8.1 is a version bump and nothing else. Every existing public
item keeps its name and shape, a contract that registers no tools and no skills
behaves exactly as it did, the release adds no runtime dependency, MSRV stays
1.87, and there is no schema change — a 0.8.1 store opens and resumes unchanged.

### Breaking changes

This release states that upgrading from 0.8.1 is a version bump and nothing else.
That holds for every existing item's name and shape; these three are the
exceptions a reader upgrading across it needs.

- **BREAKING (MSRV)** — the declared minimum supported Rust version moves 1.87 →
  1.88. The code and the dependencies are unchanged: 1.88 is what the crate has
  actually required since 0.8.0 through rmcp's use of let-chains, and 0.8.x
  declared 1.87 while failing to build on it. *Migration:* build with Rust 1.88 or
  newer. A 1.87 toolchain now refuses at resolve time with a clear message instead
  of failing inside a dependency.

- **BREAKING** — `TaskContract` gained the `tools: Toolbox` and
  `skills: Option<PathBuf>` public fields, so a struct literal over it no longer
  compiles. *Migration:* build through `TaskContract::new` / `workspace` plus
  `.with_tools(..)` and `.with_skills(..)`.

- **BREAKING (behaviour)** — a Windows run reports `Backend::PortableFloor` where
  0.8.1 reported `Backend::WindowsJobObject`. No job object was ever created, so
  the old label named containment that did not exist; the variant is kept as
  public and reserved but is never reported. *Migration:* a trace reader or test
  matching `Backend::WindowsJobObject` on Windows must match
  `Backend::PortableFloor` instead — and should read it as the weaker guarantee it
  is: wall clock only, no CPU, memory, process or network boundary.

### Added

- **The `Tool` trait, and `Toolbox`.** An object-safe trait with two methods:
  `spec()` returns the same vendor-neutral `ToolSpec` the built-ins and MCP tools
  are already described by, and `invoke()` takes the parsed
  `serde_json::Value` arguments the model sent and returns a `String` result
  through a boxed future. `Toolbox::new().with(tool)` collects them (`with_arc`
  for a tool the caller also holds a handle to, plus a `FromIterator` impl), and
  a toolbox is cheap to clone because each tool sits behind an `Arc`.
- **`TaskContract::with_tools`.** Registration mirrors `with_mcp`, so in-process
  and out-of-process extension are configured the same way and no existing
  public function signature changes.
- **Name arbitration, before the provider is called once.** A registered tool
  may not take the name of a built-in (`write_file`, `grep`, `find`,
  `read_file`, `spawn_agent`, `read_skill`), may not use the `mcp__` prefix
  reserved for server tools, may not be nameless, and two registered tools may
  not share a name. Each is an `Error::Config` naming the offending tool, raised
  before the first completion — the difference between "your config is wrong"
  and "your agent silently stopped being able to write files".
- **Policy governance identical to an MCP tool.** Calling a registered tool is
  an `Act::Exec` check on its name; a refusal is recorded as a `PolicyEvent`
  with the rule and layer that decided it and surfaced to the model as an
  observation it can adapt to, and an `Ask` verdict routes through the
  `Approver` with the durable 0.7.0 defer path intact. A denied tool's
  `invoke()` is never entered.
- **Skills — `TaskContract::with_skills(dir)`.** Discovers `<dir>/<name>.md` and
  `<dir>/<name>/SKILL.md`, so a directory written for another agent tool usually
  works unchanged. Optional YAML frontmatter supplies `name` and `description`;
  without it the name comes from the file stem (or the containing directory, for
  a `SKILL.md`) and the description from the first prose line. Discovery is
  sorted, so the same directory produces a byte-identical prompt across runs.
  A missing path, a path that is not a directory, more than `MAX_SKILLS` (64)
  skills, or two skills with the same name is an `Error::Config` at run start —
  a rejected set rather than a silently truncated or arbitrarily resolved one.
- **The built-in `read_skill` tool.** Loads one skill's body into the
  observations on demand, as an ordinary policy-checked read of that file's
  path — so a policy denying `Act::Read` over the skills directory keeps the
  catalogue in the prompt and the bodies out of the context. A call naming an
  unknown skill returns an observation listing the skills that do exist rather
  than failing the run. The tool is only offered when a contract configures
  skills, the same way MCP tools appear only when servers are configured.
- **Bounds where text enters the context.** A registered tool's result is capped
  and truncated with a visible marker before it reaches the observations, and
  the truncated form is what the trace records, so one tool cannot flood a run's
  context. A skill description is capped for the prompt catalogue and a skill
  body is capped on read.
- **`examples/custom_tool.rs` and `examples/skills_run.rs`.** Both extension
  points run live against a real provider from an API key in the environment, in
  the style of the existing examples.

### Changed

- **A failing tool is an observation, not a failed run.** An `invoke()` that
  returns `Err` puts its message in the observations, commits the step, and the
  run continues — the same treatment `grep` already gives a malformed regex.
  Only the model can decide whether a failed lookup means "try another id" or
  "give up on this approach".
- **A 0.5.0 child inherits the parent's toolbox and skills.** The whole tree
  shares one toolbox and one catalogue, and every call a child makes is decided
  by the child's own narrowed policy: inheritance grants the tool, `contain`
  still decides the call. Per-child tool subsets are deliberately not added — a
  second narrowing mechanism competing with `Policy::contain` needs evidence,
  not a guess.
- **The system prompt carries a skills catalogue when a contract has skills.**
  Names and descriptions only, never bodies. Which skill is relevant stays the
  model's judgement; the harness does not rank, match, or auto-inject one.
  Automatic relevance selection is a context-construction question and belongs
  to 0.10.0.
- **Trace parity between extension and core.** A registered tool call records
  the same decision, arguments, and observation rows a built-in call does, and a
  skill load records which skill was read, so an audit can say which skills
  shaped a run and which caller-supplied tools it depended on without the
  caller's source — and does not have to distinguish extension from core to do
  it. No new table and no new channel.

### Fixed

- **The declared MSRV was wrong from 0.8.0 onward; 0.9.0 corrects the
  declaration, not the dependency.** `rust-version` said 1.87, derived from
  process-wrap — the highest floor that had been counted. It missed rmcp. rmcp
  2.2.0 uses let-chains, stabilised in 1.88, and publishes no `rust-version` of
  its own, so cargo had nothing to check when resolving and the mistake could
  only surface as a compile error inside the dependency, well after the point
  where a clear "requires rustc 1.88" would have been useful. 0.8.0 and 0.8.1
  are published declaring 1.87 while depending on an rmcp that needs 1.88: on a
  1.87 toolchain both resolve happily and then fail to build, inside a crate the
  user did not write. `rust-version` is now 1.88, which is what the crate has
  actually required since 0.8.0. The dependency is unchanged and the code is
  unchanged — only the claim about the floor is, so a toolchain that built 0.8.x
  still builds 0.9.0, and one that could not now says so before compiling.

- **The Windows sandbox backend no longer claims a Job Object it never
  creates.** `WindowsSandbox` documented "a Job Object with kill-on-close plus
  memory / active-process / CPU limits, and a restricted token", and reported
  `Backend::WindowsJobObject` into the trace. It calls no Win32 API: no job
  object is created, there is no restricted token, and none of those caps are
  enforced. What a Windows run actually gets is the portable floor, so that is
  what it now reports — a run that creates no job object must not name one in an
  audit trail. `Backend::WindowsJobObject` is kept as a public variant, reserved
  and never reported, and `JobLimits` is kept as the tested mapping the real
  implementation will use. Nothing is removed, and nothing on macOS or Linux
  changes. The same correction is applied to the Linux backend's documentation,
  which claimed a degrade-to-floor fallback at `select` time that `select` has
  never performed — it picks the backend for the target at compile time and does
  not probe the host.

- **First CI across three platforms, and what it found.** Until this release the
  crate had no continuous integration: every green suite ever reported was one
  developer's macOS machine. CI now runs on macOS, Linux, and Windows. macOS
  passes — 225 tests, clippy and rustfmt clean. The other two do not, and the
  failures are pre-existing defects from 0.6.0, not regressions in 0.9.0:

  - **Linux** — 9 failing lib tests. Seven in `verify::tests`, where
    `passes_guarded` returns false. Two in `sandbox::tests`, where the CPU and
    memory caps do not fire: `cpu_cap_kills_a_busy_loop` observes `backend:
    PortableFloor, exit_code: None, cap_hit: None` where it requires
    `Cap::Cpu`.
  - **Windows** — the suite hangs in `cargo test` until cancelled, consistent
    with a cap that never fires; the build itself is clean.

  These went unnoticed for three releases because no test could catch them —
  there was nowhere they ran. They are being fixed in a dedicated release rather
  than folded into this one, so 0.9.0's tool layer ships on the platform it was
  verified on and the cross-platform work gets its own contract and its own
  evidence.

### Security

- **Registration is availability, not authority.** A developer who registers a
  tool may reasonably read that as the grant; it is not. The tool reaches the
  model, and the policy still decides every call, refusals included. The
  documentation states this in the same paragraph that introduces the feature
  rather than in a note further down.
- **Known limit, stated plainly.** A registered tool runs **in the harness's own
  process, with the embedding program's privileges**. The policy governs whether
  it is *called*; it does not govern what it does once running — no sandbox, no
  path scoping, and no egress control applies inside it. This is exactly the
  bound 0.8.0 states for a stdio MCP server, for the same reason: the harness
  decides what starts, not what a started thing then does.
- **A skill is instructions with no execution of its own.** A skill that says
  "run `rm -rf /`" is a sentence the model reads, and any action it then takes
  passes the same policy every other action does. Executable skills are excluded
  by design; anything that should actually *do* something is a `Tool`, where the
  permission layer can see it.
- **Containment on Windows is the portable floor, and only that.** No Job Object
  is created, so there is no process-tree kill-on-close, no active-process limit,
  and no restricted token; the CPU and memory caps are unix-only mechanisms
  (`RLIMIT_CPU` and an RSS monitor) and do not apply either. A Windows run gets a
  fresh subprocess in an ephemeral workdir, the wall-clock timeout, and the
  best-effort proxy-env strip — filesystem-scoped, not a jail. On Linux the CPU
  and memory caps do not fire under CI, which means a runaway process there is
  bounded by the wall clock alone. Both are 0.6.0 defects, both are now stated
  wherever the backend is documented rather than left implied by a backend name,
  and both are scheduled for a dedicated release. Treat sandboxed execution of
  untrusted code as verified on macOS only until then.

## [0.8.1] - 2026-07-25

A correctness fix: the execution gate could be defeated by the file it was
verifying.

`RustTestPasses` and `WorkspaceTestPasses` compiled the file under verification
and the caller's criterion into one crate. The file was therefore in scope to
change how the criterion resolved — or to remove it. A file defining
`#[macro_export] macro_rules! assert` made `assert!(false, "this gate can never
pass")`, which no correct implementation can satisfy, report `test result: ok`. A
file opening with `#![cfg(any())]` deleted the whole crate including the
criterion, and a test binary with zero tests exits 0, which the gate read as a
pass. The first was found in the wild, not by inspection: an agent discovered it
unprompted during io-cli 0.1.0's live runs — an honest implementation failed the
gate at step 1, and the shadowing macro passed it at step 2.

The criterion now sits in a module of the subject's crate that re-imports the
prelude macros explicitly. A subject defining its own `assert` makes the name
ambiguous rather than authoritative, so the gate fails to compile instead of
passing an impossible criterion; a macro the subject exports under any other name
still reaches the criterion. Deletion is caught separately, by a probe item
compiled alongside the subject that a self-stripping crate strips too.

The two are deliberately still one crate. An intermediate build of this release
made the subject a separate crate the criterion compiled against, which stops
both attacks more directly — and broke honest code, because privacy is a wall
between crates: a subject written as `fn hello() -> u32 { 42 }`, without `pub`,
became invisible to its own criterion. The live run caught it. An agent wrote
that exact implementation, was told it failed, and rewrote it until it hit the
step cap. Correctly rejecting dishonest code is worth nothing if honest code is
rejected with it, so that structure was reverted.

The compile-only gates were defeatable the same way, by a mechanism that needs no
criterion at all: `#![cfg(any())]` followed by `pub fn hello() -> u32 { "not a
u32" }` compiled clean, because the attribute strips the item before rustc
type-checks it. `CompilesRust` and `EachCompilesRust` now verify that the file's
items actually survived to be checked. Legitimate crate-level attributes —
`#![allow(dead_code)]`, `#![no_std]` — keep working.

**This is an intended behaviour change: a gate that passed dishonestly on 0.8.0
fails on 0.8.1.** That is the point of the release. If a run stopped passing
after upgrading, the gate was being defeated.

No API change and no migration. `test_src` keeps the exact shape it had on 0.8.0
— it still calls the subject's items unqualified, and still reaches the subject's
private items — and a macro the subject legitimately exports still reaches it.
MSRV stays 1.87 and no dependency moved.

### Breaking changes

- **BREAKING (behaviour, in a patch release)** — an execution gate that passed on
  0.8.0 by defeating its own criterion fails on 0.8.1. A subject file that
  shadowed a prelude macro (`#[macro_export] macro_rules! assert`) or stripped the
  crate (`#![cfg(any())]`) reported a pass; it now fails to compile, or is caught
  by the probe item. *Migration:* **there is nothing to write on the caller's
  side, and no opt-out.** If a run stopped passing after this upgrade, the gate
  was being defeated and the earlier pass was false — treat the newly failing run
  as the correct verdict and fix the subject. `test_src` keeps the exact shape it
  had on 0.8.0: it still calls the subject's items unqualified, still reaches the
  subject's private items, and a macro the subject legitimately exports still
  reaches it. Recorded here as breaking because it is a behaviour change shipped
  in a patch version, which SemVer does not lead a consumer to expect.

### Fixed

- `Verification::RustTestPasses` and `Verification::WorkspaceTestPasses` can no
  longer be defeated by the file under verification shadowing a prelude macro the
  criterion invokes (`assert!`, `assert_eq!`, and the rest of the class — the fix
  is structural, not a blocklist).
- The same gates can no longer be defeated by a crate-level attribute in the
  subject, such as `#![cfg(any())]`, deleting the criterion and passing on an
  empty test binary. This vector was found while reproducing the first.
- `Verification::CompilesRust` and `Verification::EachCompilesRust` no longer
  pass a file whose items a crate-level attribute stripped before type-checking,
  so a body that does not compile can no longer pass a compile gate.

### Added

- `SandboxEvent::gate_phase_failed` records which phase of an execution gate
  failed — `subject-compile`, `criterion-compile`, `test-run`, or
  `subject-emptied` — so a run that stopped passing after upgrading is
  attributable from the store. A new `kind` value on the existing table: no
  schema change, and a 0.8.0 store needs no migration.

### Changed

- `Verification`'s documentation now states what a passing execution gate proves
  (the stated criterion was satisfied) and what it does not (that the artifact is
  correct). Unchanged behaviour, corrected claim.
- Every execution gate spawns `rustc` more than it did. The compile-only gates go
  from one spawn to two (subject crate, then the probe reference); the test gates
  go from two to four (subject crate, probe reference, combined build, run). On
  the reference machine, 20 runs each in one session: compile-only ~29 ms to
  ~59 ms, test gates ~290 ms to ~381 ms. Wall-clock on one machine under load, so
  read the ratios rather than the milliseconds — a separate session measured the
  same test-gate baseline at 120 ms. `EachCompilesRust` pays its share per listed
  file.

## [0.8.0] - 2026-07-25

MCP, and the network boundary it made necessary. The harness is now an MCP client:
point it at servers and their tools reach the agent beside the built-ins, so a
capability the crate lacks is added by configuration rather than by a fork. Because
those servers are the first thing in the crate that can dial an arbitrary host, the
0.4.0 permission model gains a fourth act — outbound connections are now governed
by the same layered, deny-by-default policy that already governs reads, writes, and
executions.

### Breaking changes

- **BREAKING** — `Act` has a fourth variant, `Act::Net(String)`, and `Defaults` a
  fourth field, `net: Effect`. An exhaustive `match` on `Act` and a `Defaults`
  struct literal both stop compiling. *Migration:* add an `Act::Net(target)` arm
  (or a `_` arm) and a `net: Effect::Deny` field:

  ```rust
  // 0.7.0
  let d = Defaults { read: Effect::Allow, write: Effect::Ask, exec: Effect::Deny };
  // 0.8.0
  let d = Defaults { read: Effect::Allow, write: Effect::Ask, exec: Effect::Deny, net: Effect::Deny };
  ```

- **BREAKING** — `Error` gained the `Mcp` variant and `TaskContract` gained the
  `mcp` public field. *Migration:* add an `Error::Mcp` arm (or a `_` arm) to an
  exhaustive match, and build the contract through `TaskContract::new` /
  `workspace` plus `.with_mcp(..)` rather than a struct literal.

- **BREAKING (behaviour)** — a policy serialized before 0.8.0 deserializes with
  `net: Deny`, because `Defaults.net` is `#[serde(default)]`. An old config parses
  rather than failing, and then makes no outbound calls. *Migration:* add the
  hosts the run uses — `policy.layer("app").allow_net("api.example.com")` — or
  `net: Allow` in the serialized defaults. Your provider's own host needs nothing:
  it is covered by the named `provider` layer.

- **BREAKING (MSRV)** — the minimum supported Rust version moves 1.75 → 1.87, and
  `reqwest` 0.12 → 0.13. rmcp 2.2.0 requires reqwest 0.13 and its child-process
  transport requires Rust 1.87. *Migration:* build with Rust 1.87 or newer; if
  your own tree pins reqwest 0.12, move it to 0.13 rather than carrying two TLS
  stacks.

- **BREAKING (behaviour)** — redirects are off on every built-in provider's HTTP
  client. A 3xx used to be followed and now surfaces as a non-success status,
  because a host change after the policy has decided would be a hole in the
  egress boundary. *Migration:* point the provider at the URL that answers
  directly rather than at one that redirects; there is deliberately no
  follow-redirects switch.

### Added

- **MCP client over [rmcp](https://crates.io/crates/rmcp), two transports.**
  `McpServer::stdio` spawns a server as a child process; `McpServer::http` dials a
  streamable-HTTP endpoint. Configure them with `TaskContract::with_mcp`, and their
  tools are offered to the model under `mcp__<server>__<tool>` — namespaced, so a
  server advertising `write_file` cannot shadow the built-in. Per-call timeouts,
  result-size capping, and one session shared by a whole 0.5.0 agent tree.
- **`Act::Net` — the network act.** An outbound connection is now a policy decision
  with a target (`host` or `host:port`), matched by the same glob matcher that
  matches paths and binaries, and decided by the same deny-first stack:
  `allow_net`, `deny_net`, `ask_net`. `Ask` routes to the `Approver` and, when
  deferred, persists across a full process restart like any other 0.4.0 approval. A
  0.5.0 child inherits its parent's network rules and can only narrow them — the
  spawn tool gained a `deny_net` argument to do so.
- **The named `provider` layer.** The harness contributes its configured provider's
  host as one visible policy layer, so a run under a deny-all-network base still
  reaches its model without the caller listing hosts, and `Policy::explain`
  attributes the allowance to that layer instead of it being a hidden exemption. An
  explicit `deny_net` of your own provider still wins, and fails fast as a refusal
  rather than hanging.
- **`Error::Mcp`.** A configured server that will not start or complete its
  handshake fails the run with a typed error, rather than the run proceeding
  quietly without a capability it was told it had.
- **MCP and network tracing.** A new additive `mcp_events` table records every
  connect (with transport), tool discovered, tool call (with latency and outcome),
  and disconnect. Network verdicts — allows, asks, and refusals alike, each with
  the layer that decided — go to `policy_events` beside every other permission
  decision, so one query answers "what was this run allowed to do". A 0.7.0
  database migrates in place.

### Changed

- **BREAKING — `Act` has a fourth variant and `Defaults` a fourth field.** A
  downstream `match` on `Act` that was exhaustive no longer is, and a `Defaults`
  struct literal now misses a field. Migration: add an `Act::Net` arm (or a `_`
  arm) and a `net:` field. Taken deliberately in a 0.x minor, which Cargo already
  treats as incompatible.
- **BREAKING (behaviour) — a policy serialized before 0.8.0 deserializes with
  `net: Deny`.** `Defaults.net` is `#[serde(default)]`, so an older config parses
  rather than failing, but it parses as deny-by-default. An existing config whose
  run makes outbound calls needs an `allow_net` for the hosts it uses; the
  provider's own host is covered by the `provider` layer and needs nothing. The
  alternative — defaulting to allow — would have left egress ungoverned for exactly
  the callers who upgraded to govern it. *Migration:*
  `policy.layer("app").allow_net("api.example.com")` for each host the run dials,
  or `net: Allow` in the serialized defaults to restore 0.7.0's behaviour.
- **The system prompt now names the tools it does not enumerate.** The workspace
  and tree prompts described a world of exactly four (or five) built-in tools while
  the request carried more, so a model trusting the prose over the schema could
  ignore an MCP tool — or call one repeatedly without noticing it had already
  answered. Extra tools are now listed, with a line saying results appear in the
  observations. Found by a live run that looped on one tool for its whole step
  budget.
- **Agents inside a `run_tree` are offered the session's MCP tools.** The tree
  shares one MCP session; it connected but never put those tools in the request, so
  no agent in a tree could call one.
- **Redirects are off on every built-in provider's HTTP client.** A 3xx is a host
  change, and a host change after the policy has decided would be a hole in the
  boundary. A redirect now surfaces as a non-success status instead.
- **`reqwest` 0.12 → 0.13, and the minimum supported Rust version 1.75 → 1.87.**
  rmcp 2.2.0 requires reqwest 0.13, and its child-process transport requires Rust
  1.87. Carrying two reqwest versions would have meant two TLS stacks and would
  have stopped the streamable-HTTP transport from accepting our own (no-redirect)
  client.

### Security

- **Egress is governed for the first time.** Every outbound connection the harness
  opens — the provider, an HTTP MCP server, any harness-initiated fetch — passes one
  checked entry point before a socket exists, and a denial is refused rather than
  performed. Spawning a stdio MCP server is an exec check on its binary; invoking
  one of its tools is an exec check on the namespaced tool name, so a policy can
  allow a server generally and still deny one of its tools.
- **Known limit, stated plainly.** The harness governs the connections *it* opens.
  A stdio MCP server is a separate process, and once running it dials whatever it
  likes; the harness decides only whether it may start and which of its tools may
  be called. Isolating a server's own egress would need OS-level containment, which
  0.8.0 does not build.

## [0.7.0] - 2026-07-25

Durable, unattended runs. A run can be left alone for a long horizon and survive
a crash or a full process restart: after every completed step the harness commits
that step and a checkpoint marker in one rusqlite transaction, and on restart it
resumes every agent — a single run or a whole 0.5.0 tree — from its own last
committed step, without re-running finished work, double-charging the budget, or
re-applying an edit already made.

### Breaking changes

- **BREAKING** — `Error` gained the `Resume` variant, so an exhaustive `match` on
  `Error` no longer compiles. *Migration:* add an `Error::Resume(_)` arm, or a
  `_ =>` arm. It is returned for a resume against a missing or newer-format
  checkpoint — handle it rather than expecting a panic or a silent half-resume.

- **BREAKING (behaviour)** — checkpointing is on by default, so `resume` continues
  from the last committed step instead of re-running the work the interrupted
  process had already done. A completed step is skipped and recorded as a
  `skipped` event, an irreversible edit is re-observed rather than repeated, and
  the budget is not double-charged. *Migration:* **there is no opt-out.** If you
  want a run to start from step 0, start a new run rather than resuming an old id.
  The store also gains a `PRAGMA user_version` format stamp: a 0.6.0 database
  migrates in place on open, and a store written by 0.7.0 carries a stamp that
  0.6.0 does not read.

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

### Breaking changes

- **BREAKING** — `Error` gained the `Sandbox` variant, so an exhaustive `match` on
  `Error` no longer compiles. *Migration:* add an `Error::Sandbox(_)` arm, or a
  `_ =>` arm. It is returned when a sandbox fails to start — one failed child
  never takes down its siblings.

- **BREAKING (behaviour)** — sandboxed execution is the default for the
  verification gate. Every `rustc` compile and test-binary run the gate performs
  now happens in an ephemeral per-run sandbox with outbound network denied,
  resource caps applied, and a workdir removed on every exit path. The same code
  passes or fails as before, but a gate that reached the network, wrote outside
  the workdir, or ran longer than the caps allow now stops.
  *Migration:* to get 0.5.0's exact direct-host execution back, opt out on the
  guard — `ExecGuard::default().no_sandbox()` — which is why the change is
  additive and reversible.

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

### Breaking changes

This release's security note says a 0.4.0 caller that constructs no `Containment`
gets the exact 0.4.0 surface and behaviour. That holds at run time; two enums grew
a variant, so an exhaustive `match` still has to change.

- **BREAKING** — `RunOutcome` gained the `BudgetCeilingReached` variant and
  `Verification` gained `WorkspaceFileContains`. An exhaustive `match` over either
  no longer compiles. *Migration:* add the arms, or a `_ =>` arm.
  `BudgetCeilingReached` means the tree-wide aggregate budget was exhausted, which
  is a different stop from the per-contract `CostBudgetExceeded`; treat both as
  terminal and non-successful.

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

### Breaking changes

- **BREAKING** — `RunOutcome` gained the `AwaitingApproval { request_id, steps }`
  and `Denied` variants, and `Error` gained `Refused`. An exhaustive `match` over
  either no longer compiles. *Migration:* add the arms, or a `_ =>` arm.
  `AwaitingApproval` is not a failure — it is a pause; carry the `request_id` to
  `resume_with_decision` when the human answers.

- **BREAKING** — `RunResult` gained the `remembered` public field, so a struct
  literal or an exhaustive struct pattern over it no longer compiles.
  *Migration:* add `remembered` — `let RunResult { outcome, steps, remembered, .. }
  = result;` — or add `..` to the pattern so the next field does not break it
  again. `remembered` carries the rules an approve-and-remember decision produced,
  for the caller to persist.

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

### Breaking changes

- **BREAKING** — `Verification` gained the `EachCompilesRust(files)` and
  `WorkspaceTestPasses { files, test_src }` variants, so an exhaustive `match`
  over it no longer compiles. *Migration:* add the arms, or a `_ =>` arm.

- **BREAKING** — `TaskContract` gained the `root` public field, so a struct
  literal over it no longer compiles. *Migration:* build through
  `TaskContract::new(goal, target, verify)` for a single-file task — which leaves
  `root` unset — or `TaskContract::workspace(goal, root, verify)` for a repository
  task.

`Provider::name()` is *not* a breaking addition: it is defaulted, so an existing
implementer keeps compiling and inherits the default label.

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

### Breaking changes

- **BREAKING** — `Store::record_step` is removed and replaced by `Store::record`,
  which takes a `StepRecord` so the new prompt / tool-call / token columns have
  somewhere to come from. *Migration:*

  ```rust
  // 0.1.0
  store.record_step(run_id, step, "wrote src/hello.rs", "ok")?;
  // 0.2.0 — StepRecord::new leaves the audit fields empty
  store.record(run_id, &StepRecord::new(step, "wrote src/hello.rs", "ok"))?;
  ```

- **BREAKING** — `Verification::check` is removed and replaced by
  `Verification::passes`, which is async, takes the path as well as the contents,
  and returns `Result<bool>` rather than `bool` — an execution-based gate runs a
  compiler, which can fail for reasons that are not "the criterion was not met".
  *Migration:*

  ```rust
  // 0.1.0
  if verification.check(&contents) { /* ... */ }
  // 0.2.0
  if verification.passes(&path, &contents).await? { /* ... */ }
  ```

- **BREAKING** — `Verification` gained the `CompilesRust` and
  `RustTestPasses { test_src }` variants, and `RunOutcome` gained
  `TimeBudgetExceeded` and `CostBudgetExceeded`. An exhaustive `match` over either
  no longer compiles. *Migration:* add the arms, or a `_ =>` arm.

- **BREAKING** — `CompletionResponse` gained the `usage` field, `StepRecord`
  gained `prompt`, `tool_call` and `tokens`, and `TaskContract` gained
  `max_duration`, `max_retries` and `max_tokens`. A struct literal over any of the
  three no longer compiles. *Migration:* `CompletionResponse` derives `Default`, so
  add `..Default::default()` to its literal — the construction style it documents.
  For `StepRecord` use `StepRecord::new(step, decision, result)`; for
  `TaskContract` use `TaskContract::new(..)` plus `.with_time_budget(..)`,
  `.with_token_budget(..)` and `.with_max_retries(..)`.

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
