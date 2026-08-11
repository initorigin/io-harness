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
