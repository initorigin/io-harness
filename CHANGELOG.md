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

## [0.11.0] - 2026-07-26

The release that lets a long unattended run survive a bad afternoon at a
provider. 0.7.0 made a run survive a crash; it did nothing for a rate limit, a
rolling deploy, a 503, or a hung socket — and the audit for this release found
three of those were worse than assumed rather than merely unhandled.

Recovery and retry is the eleventh of the twelve pillars to close. Only
observability and evaluation (0.12.0) remains.

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
- **A request deadline.** `net::http_client()` sets a request timeout, and
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
  the callers who upgraded to govern it.
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
