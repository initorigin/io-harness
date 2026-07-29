# Public contract — IO Harness

What you may depend on, what may change, and what does not work today.

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. This
page exists because "pre-1.0" is usually where a library stops explaining
itself, and that is precisely when a dependent needs the explanation most.

## What is public

The public surface is everything re-exported from the crate root plus the items
reachable through the public modules it names.

The re-exported half — the 110 items a caller reaches as `io_harness::Thing` —
is enumerated in [public-api.txt](public-api.txt), which a test compares against
the live crate on every run. That is the surface the deprecation cycle below
covers and the surface every item of which carries a worked example.

The module-path half is narrower in practice and wider on paper: items such as
`io_harness::context::assemble` or `io_harness::tools::Workspace` are `pub` and
do compile, but they are not individually snapshotted. Treat them as public and
stable in the same way, and expect the snapshot to grow to cover them rather than
the items to be withdrawn.

Not public, and free to change without any notice:

- Anything not reachable from the crate root — private modules, `pub(crate)`
  items, and the internals of a public type.
- The exact wording of an error message, a log line, or a trace record's prose.
  The `Error` **variants** are public; the sentences they render are not.
- The SQLite schema. The store is an implementation detail reached through
  `Store`'s methods; the tables are not a contract, and `CHECKPOINT_FORMAT`
  exists precisely so a checkpoint written by an older version is refused with a
  typed error rather than half-read.
- The system prompt, the tool descriptions handed to a model, and the shape of a
  provider request body.

## What a version number means here

- **PATCH** — backward-compatible fixes.
- **MINOR** — new functionality, and, below 1.0, occasionally a break.
- **MAJOR** — reserved. There is no 1.0 planned.

SemVer permits a 0.x minor to break, and this project uses that permission
rather than pretending otherwise. A break arrives as a minor bump, never as a
patch.

**What you can rely on is not that a break will not happen. It is this:**

1. Every break is marked in the [CHANGELOG](../CHANGELOG.md) against the version
   that made it.
2. Every marked break carries a migration note saying what to write instead —
   the old call on one side, the new call on the other. A test fails the build
   if a marked entry has no note.
3. A renamed or removed item goes through a deprecation cycle. It is marked
   `#[deprecated]` with the replacement named in the attribute for at least one
   minor release before it is removed, so an upgrade warns before it breaks.

The mechanism behind (3) is [public-api.txt](public-api.txt). Removing or
renaming a public item fails the test until that file is edited by hand — which
is the moment the deprecation attribute and the migration note get written,
rather than a moment nobody notices. There is deliberately no flag that
regenerates the snapshot automatically; a one-command regenerate would defeat
the mechanism the first time someone was in a hurry.

## Minimum supported Rust version

The MSRV is **1.88**, declared as `rust-version` in `Cargo.toml` and asserted
against this page by a test.

The floor comes from `rmcp`, which uses let-chains and publishes no
`rust-version` of its own, so cargo cannot catch it at resolve time — an older
toolchain fails inside that dependency rather than here. The other floors are
lower: `process-wrap` needs 1.87, `reqwest` 0.13 needs 1.85.

An MSRV raise is a **minor** bump and is called out in the changelog like any
other break.

## Feature flags

`default = []`. The default build compiles no optional dependency at all, and
enabling a feature only ever adds to the surface.

| Feature | What it adds | Cost |
| --- | --- | --- |
| `default` | Nothing. The empty set is deliberate — the default dependency tree is held at a fixed size and checked | None |
| `media` | Image passthrough: `Media`, `IMAGE_MEDIA_TYPES`, and the `view_image` built-in | `base64`, already compiled transitively by `reqwest` |
| `documents` | Umbrella over the five below | The union of theirs |
| `xlsx` | Spreadsheet read, generate, and preserving single-cell edit | `calamine`, `rust_xlsxwriter`, `umya-spreadsheet` — three crates because reading, writing, and round-tripping are three separate capabilities in this ecosystem |
| `docx` | Word read and generate | `docx-rs` |
| `pptx` | PowerPoint text extraction, read-only | `zip`, `quick-xml` |
| `pdf` | PDF generate, extract text, watermark, fill AcroForm fields | `lopdf`, `pdf-extract` |
| `barcode` | Barcode and QR decoding from an image | `rxing`, `image` |

Nothing here binds a C or C++ library, so no runner needs a system package.

`Workspace::read_bytes`, `Workspace::write_bytes` and
`Verification::DocumentContains` are present in **every** build. Without the
features, `DocumentContains` returns a typed error naming the missing feature
rather than the variant disappearing — a conditional enum variant is a breaking
change for every `match` a caller wrote.

## Platform support

| Platform | Status | Sandbox containment |
| --- | --- | --- |
| macOS | Supported, full suite in CI | Native, `sandbox-exec` |
| Linux | Supported, full suite in CI | Native, namespaces and rlimits |
| Windows | Supported, full suite in CI | **Portable floor only** |

On Windows the Job Object backend is designed and not implemented. Containment
there is the portable floor: an ephemeral working directory, network denied, and
the wall clock. CPU and memory caps **do not fire**. Do not read "sandboxed" on
Windows as "resource-capped".

## Limits that hold today

Stated here rather than discovered later. Each is real, each is known, and none
is fixed as of 0.19.0.

**What configuration is, and is not (0.19.0).** `io.toml` is a projection onto
the typed API and never a second path into the run loop:

- **The typed API is the authority.** Every key lands in a type this crate
  already had — `Policy`, `SandboxConfig`, `Toolchain`, `PriceTable`,
  `McpServer`, `TaskContract` — and a file can express nothing the typed API
  cannot. A test asserts every key reaches a typed field.
- **The file is read once, by the caller, before the run, and never again.**
  Nothing in this crate discovers a config on its own: `Config::discover` is the
  caller's own call. That is what makes the one guarantee here true — a config
  the agent writes *during* a run cannot widen the boundary that run is already
  under.
- **A config file is not a security boundary against the agent.** The boundary is
  the `Policy` the caller loaded; the file is where it was written down. An agent
  that can write the workspace root can write an `io.toml`, and the *next* load —
  the caller's act, not the agent's — will read it.
- **An unknown key is an error**, naming the key and the file, rather than being
  ignored. The exception is a key inside a `[[mcp]]` table, because `McpServer`
  is `#[serde(flatten)]`-based and serde refuses `flatten` beside
  `deny_unknown_fields`.
- **A failed substitution is an error, never an empty string.** An unset
  `${env:...}`, an unreadable `${file:...}`, and a value that resolves to nothing
  each fail the load, because an empty string in a boundary rule is a rule that
  matches nothing.
- **The `[toolchain]` override does not reach this crate's own run loop.** The
  harness detects for itself; `Config::toolchain(detected)` gives the embedding
  application the merged value. Wiring it into the loop needs a new
  `TaskContract` field, which is a break, and 0.19.0 carries none.
- **Scope discovery is fixed**: four scopes, no `include`, no `extends`, no
  parent-directory search, no JSON or YAML form, and no reload.

**What every recorded number is, and is not (0.18.0).** The trace now answers
what a run cost and which model spent it, and the provenance of each figure
matters more than the figure:

- A **token count is the provider's report**, not this crate's measurement.
  `Usage` is `None` where the provider reported nothing, which is not the same
  fact as zero, and `total_tokens` is taken as reported rather than re-derived
  from the parts — a vendor whose total disagrees with its own breakdown is
  billing on the total.
- A **latency is the harness's own wall clock**, bracketing `Provider::complete`.
  It includes this crate's request building and stream consumption, so it is
  slightly above what the vendor would call its own latency, by design: it is the
  wait a caller actually experiences.
- A **TTFT is `None`, never zero, where nothing measured it** — a provider that
  does not stream, or a test double. An unmeasured wait and an instant one are
  different facts.
- **Cache and reasoning counters are breakdowns**, not additions: cache tokens of
  `prompt_tokens`, reasoning tokens of `completion_tokens`. Anthropic reports its
  cache counts *beside* a prompt count that excludes them and the crate
  reconciles that at the wire boundary, so a row does not mean two things
  depending on which vendor wrote it.
- **`server_tool_requests` is zero everywhere** until the crate declares
  provider-executed tools (0.22.0). The meter exists before the spending so that
  adding them is not a second break of `Usage`.
- A **cost is derived, never stored**, from a price table the operator supplies,
  and is therefore only as right as that table. The crate ships **no prices** and
  requires an as-of date on any table, because it cannot keep a vendor's price
  list accurate on its own release schedule. An unpriced call is counted in
  `Spend::unpriced_calls` rather than costed at zero: a group with calls there is
  reporting a floor, not a total.
- **A run recorded before 0.18.0 has no rows at all**, in either new table.
  Nothing is backfilled, because the facts were never recorded. The queries
  return nothing rather than zeros.

**Lines added and removed are not a minimal diff.** `Edit::measure` compares the
file's lines before and after and trims the common head and tail. A one-line
replacement is one added and one removed; a rewrite of the middle of a file is
the size of that middle. It is a size, not a patch.

**What a passing verification gate proves.** It proves the criterion ran against
the subject and reported success. It does not prove the implementation is
correct, idiomatic, or complete. A gate is a check the caller wrote, and it
proves exactly what that check tested.

As of 0.18.0 a criterion is a command — the project's own runner, in its own
process — or `EachCompilesRust`, the one gate the harness still spawns `rustc`
for. The class of bypass 0.8.1 hardened against is therefore **structurally
gone** rather than defended against: it required a caller-supplied criterion
compiled into the subject's own crate, and no criterion is any more. What
replaces that guarantee is ordinary discipline about where the criterion lives —
a gate the agent is permitted to edit is not a gate, whatever the harness does.
`TEST_BINARY` still exists so that policies written against it compile, but
**nothing spawns it**: denying it changes nothing. See the
[verification guide](guide/verification.md).

**What the permission policy governs.** It decides whether an action is taken:
which paths are read and written, which binaries and tools may be invoked, which
hosts may be dialled. It does not govern what a thing does once it is running. A
registered `Tool` executes in the harness's own process with the embedding
program's privileges; a stdio MCP server is a separate process that, once
started, dials what it likes. The harness decides what starts, not what a started
thing then does.

**What a command the agent runs is bounded by.** As of 0.17.0 the agent can run
a command with the `exec` tool. Every call is an `Act::Exec` check on the program
*and* on the whole argv, so `allow_exec("cargo test*")` beside
`deny_exec("cargo publish*")` means what it reads. A refusal, and an approver's
decision, land in `policy_events` attributed to the rule and layer; a silent
allow does not write a row, exactly as it does not for a read or a write. What
the policy does **not** decide is what the command then does.

A command runs **in the workspace root with the embedding program's privileges,
outside the sandbox**. That is the same bound already stated above for a
registered `Tool` and a stdio MCP server, and it is deliberate: the sandbox
denies network egress and confines writes to its own workdir, which is right for
a verification gate and makes `npm install` impossible. Three consequences worth
naming. A policy written for file access does not constrain command execution —
`Act::Read`/`Act::Write` rules say nothing about `exec`, and the tier default
decides everything unnamed. A command can reach what the agent's own file rules
would have refused, because `cat secrets/prod.env` is a command and not a read.
And a timeout kills the **direct child only**: a process that child started
itself may outlive it, on every platform this crate supports — the same gap the
sandbox reports for its own wall-clock kill on the portable floor. See the
[command execution guide](guide/command-execution.md).

**Toolchain detection is a default, and it will be wrong for someone.** The
shipped table maps one marker file in the workspace root to an ecosystem and its
conventional commands, and puts that in front of the model. The harness never
runs those commands itself. Ecosystems disagree with themselves — half of all
`npm test` scripts do not run tests — and only the root is examined, so a
monorepo whose packages each carry a marker gets one detection. It is not
overridable until 0.19.0 puts a configuration file under it.

**Closed in 0.17.0: a registered tool could be silently shadowed.** The reserved
set named only the original seven built-ins while dispatch grew to twenty-six, so
a registered tool called `git_status` or `xlsx_read` passed `Toolbox::validate`
and was then permanently unreachable — the built-in answered every call. It now
names every built-in, in every build regardless of feature flags, and a
registered tool taking one of those names fails the run before the first
completion. The names of feature-gated built-ins are reserved even where the tool
itself is not compiled in, so enabling a feature can never take away a tool that
was working.

**Windows resource caps.** See the platform table above.

**`SandboxLimits::max_processes` is enforced by nothing**, on any platform. It is
deliberately not mapped to `RLIMIT_NPROC`, which is per-real-uid and would
throttle the operator's own login session rather than the sandbox; the two
backends that could scope it properly — the Linux pid-namespace active-process
limit and the Windows Job Object — are not wired up. Setting it changes nothing.

**No seccomp filter is installed.** The Linux backend is namespaces and rlimits.
Whatever syscall restriction applies is the kernel's own default under an
unprivileged user namespace, not a filter this crate installed.

**A native backend can silently become the floor.** `select` chooses its
candidate at compile time, and a backend whose primitive is unavailable at
runtime degrades to the portable floor — this is live on Ubuntu 24.04, where
`apparmor_restrict_unprivileged_userns=1` makes every `unshare` fail. It reports
the floor honestly in the returned `Selected`, so read that value rather than
assuming the platform's native backend is what ran.

**What the trace says a tree ran under.** `run_tree` and the single-file and
workspace loops record the policy they execute under, so the store answers "what
boundary was in force". `resume_tree` does not: it takes a policy as an argument
and executes under it, but leaves the recorded policy as whatever the run
started with. A tree resumed under a *widened* policy therefore leaves an audit
that understates what was permitted. `resume_tree_from_stored_policy` is
unaffected — it reads that same row back, so what is recorded is what runs — and
it is the entry point to prefer when the boundary matters.

**`git_status` output.** It uses `--porcelain=v1` without `-z`, so a path
containing a newline renders ambiguously to the model. A display concern, not a
boundary one — the path policy still sees the real path.

**Git commit idempotency across a resume.** A replayed run does not commit
twice, but that rests on git's own semantics — a replayed `add` stages nothing,
so the replayed commit finds nothing staged — rather than on a durable marker
around the commit. It is tested and it holds. A future git built-in that is not
naturally idempotent would not inherit the property.

**PDF form filling.** `fill_form` generates real appearance streams, but the
output has never been opened in a real PDF viewer. Proving that needs Pdfium,
which is a system package, which this project does not permit on any runner.

**`pdf::read_text` panic containment.** Its `catch_unwind` is a no-op under a
`panic = "abort"` profile. A malformed PDF that panics the extractor will abort
the process rather than return an error, if you build with abort.

**Document fixtures.** Every binary document fixture in the test suite was
written by this crate or by a Rust library. None was produced by Excel, Word, or
Acrobat, so real-world quirks those applications emit are untested.

## Related

- [CHANGELOG.md](../CHANGELOG.md) — the release history, with a migration note on
  every break.
- [CAPABILITIES.md](CAPABILITIES.md) — the guide index.
- [RELEASE_PROCESS.md](RELEASE_PROCESS.md) — how a release is cut.
- [public-api.txt](public-api.txt) — the enumerated public surface.
