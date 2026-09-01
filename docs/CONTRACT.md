# Public contract — IO Harness

What you may depend on, what may change, and what does not work today.

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. This
page exists because "pre-1.0" is usually where a library stops explaining
itself, and that is precisely when a dependent needs the explanation most.

## What is public

The public surface is everything re-exported from the crate root plus the items
reachable through the public modules it names.

The re-exported half — everything a caller reaches as `io_harness::Thing` — is
enumerated in [public-api.txt](public-api.txt), which a test compares against
the live crate on every run. That is the surface the deprecation cycle below
covers and the surface every item of which carries a worked example.

The module-path half is narrower in practice and wider on paper: items such as
`io_harness::context::assemble` or `io_harness::tools::Workspace` are `pub` and
do compile, but they are not individually snapshotted. Treat them as public and
stable in the same way, and expect the snapshot to grow to cover them rather than
the items to be withdrawn.

There was a third half the snapshot could not show, because it enumerates
re-exported *names* and this was a *type*: `rusqlite` used to be a public
dependency of this crate. **As of 0.63.0 it is not.** `Error::Storage { kind,
message }` carries an owned [`StorageErrorKind`] and the message the storage
layer produced, so a `rusqlite` major bump no longer changes this crate's public
API — which is exactly what 0.23.0 was, and what will not happen again.
`Error::State(rusqlite::Error)` still exists, deprecated since 0.63.0 and
**removed in 0.65.0**; nothing constructs it, and every storage failure converts
to `Error::Storage`. The claim is enforced rather than promised: a derived check
in `tests/state_error.rs` fails if any `pub` item's surface names `rusqlite`
again, with the deprecated variant as its one declared exception, because
`public-api.txt` lists `enum Error src/error.rs` and stops there — the variant's
payload is not a line in that file and never will be.

What this did **not** buy is graph-level independence, and the difference
matters. `libsqlite3-sys` declares `links = "sqlite3"`, so only one version of it
can exist in a consumer's dependency graph whatever this crate's error type looks
like; `tests/fixtures/links-consumer/` is the standing proof of that wall.
Wrapping the error means a `rusqlite` upgrade here stops being a *type-level*
break for consumers. It does not mean they can hold a different `rusqlite`
than this crate does.

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

The MSRV is **1.95**, declared as `rust-version` in `Cargo.toml` and asserted
against this page by a test.

It moved from 1.88 in 0.23.0, and the reason is worth stating plainly because
the release that raised it is otherwise a release in which nothing changes. The
floor now comes from `libsqlite3-sys` 0.38.1, whose build script — and
`rusqlite` 0.40's own source — call the std `cfg_select!` macro, stabilised in
1.95.0. Neither crate publishes a `rust-version`, so cargo cannot catch it at
resolve time: an older toolchain fails inside the dependency's build script
with `cannot find macro cfg_select in this scope`, which reads like a toolchain
bug and is not one. It was checked rather than assumed — 1.93 and 1.94 both
fail, 1.95 builds.

There is no `rusqlite` at or above the 0.40 floor that avoids this, and below
that floor the `links = "sqlite3"` collision that 0.23.0 exists to remove comes
back. So the choice was this floor or that wall, and this floor is the one a
consumer can do something about.

The previous floor came from `rmcp`, which uses let-chains and also publishes
no `rust-version` of its own. The other floors are lower: `process-wrap` needs
1.87, `reqwest` 0.13 needs 1.85.

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
| `browser` | Driving a real browser: `BrowserConfig`, the `[browser]` table, and the six `browser_*` built-ins | **No new crate.** Implies `media`, because a screenshot is only worth taking if the model looks at it |

Nothing here binds a C or C++ library, so no runner needs a system package.

### What `browser` does and does not claim

A run drives a browser already installed on the machine — named in `[browser]`,
or resolved from a documented ordered list of executable names. **Nothing is
downloaded, ever**, and a browser that is not there is a refusal naming what was
looked for.

The browser is driven over a **pipe on the child's own descriptors**, not over a
remote debugging port. A debugging port is a TCP listener any other local process
can connect to and drive with full control of the browser; this crate opens no
such port, and needs no websocket client to avoid it.

**Every document navigation is an `Act::Net` check against its `host:port`,
decided at the paused request rather than at the URL a tool was handed.** That is
what makes the boundary hold for a navigation the model never typed — a click on
a link, a redirect, a script assigning `location`. Each decision is one
`BrowserNavigated { host, permitted }` event, so a trace records every place the
browser went *and every place it was stopped from going*.

**A URL that reaches no host is decided by its scheme instead, before the
navigation is issued (0.74.0).** Only `http`, `https`, `ws` and `wss` reduce to a
`host:port`, so only they take the check above; every other spelling produced no
request for the gate to pause and was therefore permitted by default and recorded
nowhere, which is how `file:` read a local file past `Act::Read` and past every
secret deny with no row saying it happened. The rule is an allowlist and not a
list of known-bad schemes: `about:blank` is permitted, because it is the empty
page the browser is opened on and a run that wants to leave a page has nowhere
else to go, and `file:`, `data:`, `blob:`, `javascript:` and every scheme nobody
has considered are refused. An unrecognised scheme is not a harmless one. Each of
those decisions is a `BrowserNavigated` event too, whose `host` is the **scheme**
— lowercased, with its colon — and never the URL: a `data:` URL is its own
payload and a `javascript:` URL is a program, so writing either into the trace
and into the model's observation would copy the thing that was refused into two
places it was refused from reaching.

What it does **not** claim, stated rather than left to be discovered:

- **Subresources are not individually policy-checked.** Images, stylesheets,
  fonts and XHR are the page's own traffic to a host already permitted. Document
  navigations bound where the browser *goes*; under containment everything it
  sends takes the run's own egress proxy, like every other contained command.
- **Windows drives a browser since 0.59.0**, over the same pipe transport and in
  the same suite. Until then every entry point there returned a typed
  configuration error naming the platform, because the transport needs two
  descriptors at fixed numbers and the standard library exposes no way to inherit
  them. What the child reads is not two inherited handles: Chromium turns the
  descriptors it is handed into handles itself, so they are placed in the C
  runtime's own table through `lpReserved2` on the `STARTUPINFO`.
- **One page per run.** No tabs, windows, downloads, uploads, PDF printing,
  device emulation, request mocking, or cookie and storage manipulation.
- **No waiting on arbitrary page conditions.** An action settles on the page's own
  load state, bounded by the configured timeout; the bound expiring is a normal
  outcome that still returns the page, never an error.
- **A selector that matches nothing fails, naming the selector.** It is never
  reported as a click that happened — a model cannot detect that from a
  successful-looking result.
- **`[browser]` is refused in any file inside the workspace**, like `[[hook]]`,
  because it names a program to execute: `io.toml` arrives with a `git clone`, and
  `io.local.toml` is a path the run's own agent can write. The user-scope file is
  where it goes.

`Workspace::read_bytes`, `Workspace::write_bytes` and
`Verification::DocumentContains` are present in **every** build. Without the
features, `DocumentContains` returns a typed error naming the missing feature
rather than the variant disappearing — a conditional enum variant is a breaking
change for every `match` a caller wrote.

## Platform support

| Platform | Status | Sandbox containment |
| --- | --- | --- |
| macOS | Supported, full suite in CI | Native, `sandbox-exec` |
| Linux | Supported, full suite in CI | Native, a **chain** — Landlock, `bwrap`, namespaces, floor |
| Windows | Supported, full suite in CI | Native, two backends — Job Object by default, **resources only**; AppContainer when the run calls `with_access_confinement()` (0.59.0) |

Since 0.47.0 Linux is not one backend and a fallback but an ordered chain, and
the rung a host takes is the strongest one that can enforce what the run asked
for:

| Rung | Needs | Confines writes | Denies egress |
| --- | --- | --- | --- |
| `linux-landlock` | Landlock (kernel 5.13+) | Yes | Only at ABI 4+ (kernel 6.7) |
| `linux-bubblewrap` | a working `bwrap` | Yes | Yes |
| `linux-namespaces` | unprivileged user namespaces | Yes | Yes |
| `portable-floor` | nothing | No | No |

**A workspace inside the system temporary directory is not confined**, on any
unix backend. Every one of them grants the system
temporary directory writable — the mount setup binds `${TMPDIR:-/tmp}`, the macOS
profile allows `/private/var/folders`, and the Landlock rung grants it — because a
toolchain that cannot open a temporary file cannot run at all. A workspace *located* under that directory therefore sits
inside a writable grant, and `ExecMode::ReadOnly` does not make it read-only.
This was found by the CI matrix in 0.47.0, on a test whose own workspace and
whose "outside" target were both `tempfile::tempdir()`s and so had both been
granted. It is a property of the design, not a defect in one rung: put a
workspace somewhere other than the temporary directory if the mode is meant to
bind.

**A run that denies egress is never given a rung that cannot deny egress.** That
is the one rule that can send a host below its strongest available primitive: a
kernel whose Landlock predates the network rules falls through to a rung with a
network namespace rather than taking a filesystem-only rung and leaving the
run's own policy unenforced.

The Landlock rung is the reason the chain exists. A stock Ubuntu 24.04 ships
`kernel.apparmor_restrict_unprivileged_userns=1` and refuses the namespace the
older rung needs — and `ubuntu-latest` is a stock Ubuntu 24.04 — so on the
commonest Linux CI image every contained run up to 0.46.0 took the portable
floor. Landlock needs no namespace at all. It is also the only rung that wraps
the payload in nothing: the restriction is installed in the child between fork
and exec, so the argv spawned is the argv asked for and `current_dir` means what
it says. Alongside it a small **seccomp deny-list** refuses `mount`, `umount2`,
`pivot_root`, `ptrace`, the two `process_vm` calls, the three module calls, both
`kexec` calls, `bpf` and `perf_event_open`, with `EPERM` rather than a kill. It
is a deny-list and not a jail, and it is written in the host architecture's
syscall numbers, so a process under a foreign personality is allowed through
rather than denied by coincidence.

Since 0.24.0 a Windows run is contained by a Job Object. Memory, CPU and active
process count are real bounds, the whole process tree dies when the job handle
closes, and Windows is the first backend anywhere to enforce
`SandboxLimits::max_processes`.

**A Job Object contains resources and nothing else.** There is no filesystem
facility and no network facility in one. macOS confines writes to the working
directory and denies outbound network; Linux does the same through mount and
network namespaces; the Windows default does neither. So "sandboxed" on Windows
means resource-capped and does not by itself mean access-confined, and the two
must not be read as the same claim.

**The access half is `AppContainer`, 0.26.0 built it, and since 0.59.0
`SandboxConfig::with_access_confinement()` selects it.**
`io_harness::sandbox::appcontainer` creates a container profile, derives
its SID, grants a path to it with an explicit ACE, and spawns into it through
`CreateProcessW` with a process-thread attribute list carrying **two** attributes:
`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, which puts the child in the
container, and — since 0.74.0 — `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, which
decides what crosses into it. Without the second, `bInheritHandles` is a blanket:
*every* inheritable handle this process holds is duplicated into the child
carrying the access it was opened with, and no ACL sees it, because a handle does
not go back through an access check. The list names the capture file the two
standard streams are redirected to plus whatever the caller asked for by name, and
nothing else. On the Windows CI runner a payload inside one is refused a read it
was not granted and has no route off the machine, each against a negative control
that must succeed outside the container.

**The boundary is opt-in and the Windows default has not moved.**
`sandbox::select` chooses the Job Object unless the config asked for access
confinement, and not even then under `ExecMode::FullAccess`, whose whole meaning
is that the payload may write anywhere. A run that does not call
`with_access_confinement()` gets the resource boundary and no access boundary, so
the table above is what such a run actually gets.

**A boundary asked for by name does not degrade.** Everywhere else in this crate
an unavailable primitive falls back to a weaker rung and reports it; this one is
an error naming the grant that failed, because a run that quietly took the Job
Object instead would have had no boundary at all while every assertion about it
still passed.

**Under the container there is no loopback proxy, so egress is all or nothing.**
A process inside an AppContainer cannot reach a loopback listener under any
capability set — measured on `windows-latest` with none, with `internetClient`,
with `privateNetworkClientServer` and with both — and cannot reach the host's own
network address either. A contained Windows command is therefore given no proxy
at all, and the policy's per-host rules are not enforced there: what the
container has is a capability to reach the network or no such capability.
`Backend::reaches_loopback_proxy` answers false for it and true for every other
backend, and the agent's own boundary section says so rather than claiming a
sentence it cannot back. The record is `US-IO-HARNESS-0.59.0-I03`.

**0.47.0 was specified to select the container and did not.** The Windows half
was taken out of that release whole on 2026-08-10 and shipped in **0.59.0**;
the record is `US-IO-HARNESS-0.47.0-I01`. Three real defects in the module were
found and fixed on the way and are in the tree: an ACE built from `GENERIC_ALL`
is stored verbatim by `SetEntriesInAclW` and matches no access check, so every
grant the module made between 0.26.0 and 0.47.0 was inert while being readable
back off the DACL; a tree grant must survive a descendant another process holds
open, because `CARGO_HOME` is being read by the very build that asked for the
sandbox; and a grant on a directory alone must not enumerate it, because both
`aclapi` write entry points walk the whole subtree below their target. Both
questions that release left open are answered. `CARGO_HOME` is deliberately not
in the read-execute set — it holds `credentials.toml`, and what a cargo build
needs out of it arrives as a writable cache root the run resolved. And `cmd.exe`
refusing to start a batch file named by an absolute path inside an AppContainer
is a property of the platform rather than of this spawn or of the grant set:
reading that same file by that same kind of path succeeds, and both refusals are
kept as asserted cases that fail if Windows ever changes.

The original obstacle stands and is the grant set rather than the mechanism: an
AppContainer is default-deny for reads, so the workspace is the easy part and the
executed binary, the toolchain, the redirected temporary directory and every
language's install tree are the rest. Naming those for arbitrary ecosystems is a
discovery problem 0.26.0 did not close, and a default boundary that cannot run the
payload would be worse than one a caller reaches for deliberately. Recorded in
`US-IO-HARNESS-0.26.0-I02`.

Two further differences, stated rather than left to be discovered. The job's CPU
limit counts user-mode time only, where unix `RLIMIT_CPU` counts kernel time as
well, so the cap is weaker on Windows for a kernel-heavy workload. And
`JOB_OBJECT_LIMIT_PROCESS_MEMORY` makes an allocation *fail* rather than
terminating the process: a payload over the cap is never allowed to hold the
memory, and typically dies of its own failed allocation rather than being killed.

## What `Compatible` does and does not translate (0.29.0)

`Compatible` reaches any endpoint that speaks the OpenAI chat/completions format
— twenty-one of them behind named presets, from hosted vendors to a runtime on
your own laptop. What it is **not** is a compatibility layer.

This crate sends **one wire**. There is no per-vendor request rewriting, and
there is deliberately not going to be: the whole structural claim of the
provider layer is that there is one `openai_wire` and not twenty, and a
vendor-shaped branch inside the shared body builder is how that stops being
true. Every vendor below diverges from the OpenAI wire somewhere. Those
divergences are stated here rather than papered over, because a boundary the
caller believes in and nobody enforces is worse than none.

Two of them the crate absorbs for free, and they are why the rest are
survivable: `finish_reason` is parsed as an open `String` and recorded verbatim,
and no response type uses `deny_unknown_fields`. So a vendor inventing a stop
reason or adding a field costs nothing.

### The one that fails silently — vLLM and SGLang make no tool calls

**vLLM and SGLang emit no tool calls at all unless the server was started with a
tool-call parser flag, and a client cannot set it.**

Point `Compatible::vllm(..)` or `Compatible::sglang(..)` at a server started
without it and nothing errors. The request is accepted, the model answers in
prose, `tool_calls` is empty, and the run simply never uses a tool — it looks
exactly like a model that chose to talk. There is no status code, no warning and
nothing in the trace to distinguish it from a model that declined to act.

vLLM needs `--enable-auto-tool-choice` together with a `--tool-call-parser`
naming the parser for the model being served (`--tool-parser-plugin` for a
custom one). SGLang needs its equivalent `--tool-call-parser`. Both are server
launch arguments. This crate cannot supply them, cannot detect them, and will
not guess: if an agent against a local vLLM never calls a tool, check how the
server was started before anything else.

### The rest, per vendor

**Zhipu** returns `finish_reason` values outside the OpenAI set — `sensitive`,
`network_error` and `model_context_window_exceeded` among them. They reach
`CompletionResponse::finish_reason` verbatim and the trace records them as
given. Nothing normalises them into `stop` or `length`, because a vendor's own
word for why it stopped is what its documentation explains.

**Groq** returns **HTTP 400** on a request carrying `messages[].name`. This crate
does not send that field, so it does not arise from the harness itself — it is
stated because a caller building a request by hand around this provider will
meet it.

**Mistral** requires tool-call ids matching `^[a-zA-Z0-9]{9}$` — exactly nine
alphanumeric characters — and names its JSON-schema field `schema_definition`
rather than `schema`. A tool-call id this crate did not originate is echoed as
received.

**DeepSeek** returns **HTTP 400** if `reasoning_content` is dropped from a
request that carries tools. Its own documentation is explicit: between two user
messages, when the model performed a tool call, the intermediate assistant's
`reasoning_content` *must* be passed back in every subsequent turn. This crate
sends one flattened user turn rather than a growing message array, so it does
not currently construct the shape that triggers this — but a caller assembling
multi-turn tool-calling traffic against DeepSeek must replay it.

**Ollama** accepts `tool_choice` and silently ignores it. It is listed as
unimplemented in Ollama's own OpenAI-compatibility notes. A request that pins a
particular tool is accepted and the model chooses freely anyway.

**Perplexity** serves no `/models` endpoint at all — its base has no `/v1`
segment and `GET /models` is a 404. `Compatible::perplexity(..).models()`
therefore returns the vendor's 404 as a provider error rather than an empty
catalogue, because "this vendor has no catalogue" and "this vendor serves no
models" are different facts and reporting the second would be a lie.

**What this crate deliberately does not do about any of it.** It does not strip
`messages[].name` for Groq, does not regenerate a tool-call id for Mistral, and
does not replay `reasoning_content` for DeepSeek. Each would be a vendor-shaped
branch inside the one body builder every vendor shares, and one such branch is
how twenty-one endpoints become twenty-one wires. Any one of them becomes a
change on evidence — when a consumer actually meets it — rather than on a list
somebody read.

**None of the above is a compatibility matrix.** Each is an observation of a
vendor's behaviour at the time of writing, not a support table this crate
promises to maintain, re-test each release, or keep current as twenty-one
endpoints it does not control change underneath it. There is deliberately no
such table here, because a table reads as an ongoing guarantee and this crate
cannot keep one.

### Base URLs are not uniformly shaped

The preset carries the whole prefix the vendor documents, and six of the
twenty-one are not a host plus `/v1`:

| Vendor | Base |
| --- | --- |
| Groq | `https://api.groq.com/openai/v1` |
| Zhipu | `https://open.bigmodel.cn/api/paas/v4` |
| Qwen (DashScope international) | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| Gemini (OpenAI-compatibility) | `https://generativelanguage.googleapis.com/v1beta/openai` |
| Fireworks | `https://api.fireworks.ai/inference/v1` |
| Perplexity | `https://api.perplexity.ai` (no path segment at all) |

`/chat/completions` and `/models` are appended to that base. A field that
assumed a scheme and a host would silently drop the rest and 404 against every
row above.

**A preset's base URL is a default this crate ships, not a promise the vendor
made.** Each was correct on the day this release was cut, and the vendor is under
no obligation to keep it so. `base_url` exists beside `preset` for exactly that
reason: a vendor that moves its endpoint is one line in the operator's own file,
today, rather than a release of this crate. The preset list is a convenience over
the general key and never a gate in front of it.

### What is not reachable here

**AWS Bedrock**, **Google Vertex** and **Azure OpenAI** are not. Bedrock's SigV4
signing and Vertex's service-account JWT are credential-minting protocols rather
than a header, and Gemini's *native* `interactions` API is a different wire
shape — the preset above is its OpenAI-compatibility endpoint and nothing more.
Bedrock and Vertex are nonetheless reachable today by an application that mints
a bearer token itself and passes it as the key, which is where OAuth has always
belonged in this crate. Azure is excluded on demand rather than on cost: it
needs one `Auth` variant for its `api-key` header plus a deployment name in the
path and an `api-version` query parameter, and `Auth` is `#[non_exhaustive]`
precisely so it can arrive later without a break.

### What a price means (0.29.0)

`ModelInfo::price` is `Option<Price>` and **`None` means the vendor did not
say** — it is never `Price::ZERO`. Nearly every vendor's `/models` returns
identifiers and no cost data at all, so a type that defaulted the unknown to
zero would report real spend as free. A local runtime is the one place zero is
*true*, and it is recorded as a stated zero with `PriceSource::Vendor`.

A price taken from the reference catalogue is marked
`PriceSource::Reference(host)` and **is not the vendor's price** — it is what
that aggregator charges to serve the model, which tracks the vendor's rate and
is not identical to it. Matching is an exact slug or one documented
normalisation (drop a single leading `vendor/` segment, case-insensitively) and
nothing else; a miss stays `None` and an ambiguous normalisation resolves to
nothing, because a wrong match is a wrong invoice and is worse than the gap it
filled. `Spend::unpriced_calls` is where the gap surfaces.

The reference lookup is **off by default and dials a host the caller did not
name**. When `Compatible::with_reference_prices` is set, that host appears in
`Provider::endpoints()` and the run authorises it against the policy's
`Act::Net` rules before the first step — a policy that denies it makes the run
**refuse**, not silently skip the lookup.

**A model may have more than one price.** Many vendors charge a higher rate once
a prompt passes a length threshold, and the step is usually a doubling: at the
time of writing, 44 of the 336 models the default reference catalogue serves
carry such tiers, with floors at 32k, 128k, 200k, 256k and 272k prompt tokens. A
long agentic run is exactly what crosses them. `PriceTable` carries
`PriceTier`s, the highest floor a prompt reaches prices the **whole** request —
which is how the vendors bill it, not a marginal split — and a table with no
tiers registered prices exactly as it did before they existed.

**`Fallback::models()` returns an empty list.** Which of two vendors' catalogues
a chain should report has no right answer, so it reports none rather than
picking.

**Every addition in 0.29.0 is additive.** `Provider` gains one method and
requires none: `models()` is defaulted to an empty catalogue, so an
implementation written before this release compiles unchanged and reports having
nothing to list. `ProviderSpec` gains a fourth variant behind the
`#[non_exhaustive]` 0.27.0 put on it for exactly this, so a caller who wrote the
`_ =>` arm that attribute asks for is untouched. Nothing here breaks. See the
[configuration guide](guide/configuration.md).

## What the `shell` tool will and will not run (0.24.0)

This is contract rather than implementation detail: an operator writing a policy
needs to know what their agent cannot express, and a model that discovers the
refusal set one construct at a time spends steps doing it.

`shell` takes a command line, parses it in this crate, and checks every
sub-command against `Act::Exec` and every redirect target against `Act::Write`
or `Act::Read` **before any process starts**. A line with a denied stage runs
none of its stages. There is no `sh -c` and no `cmd /c` after the parse.

**Admitted:** single quotes, double quotes, backslash escapes, `|`, `;`, `&&`,
`||`, the redirects `>` `>>` `<` `2>` `2>>` `2>&1`, and `cd`, which applies to
the remainder of the line.

**Refused, each by name and with a reason:** command substitution `$( )` and
backticks; parameter expansion `$VAR` and `${VAR}`; arithmetic `$(( ))`; process
substitution `<( )`; subshells `( )`; brace groups `{ }`; here-documents `<<`;
background `&`; the `if`, `then`, `elif`, `else`, `fi`, `for`, `while`, `until`,
`do`, `done`, `case`, `esac`, `function` and `select` keywords in command
position; and the glob characters `*` `?` `[` `]` outside quotes. Quoting is the
escape hatch — a quoted `$` is a literal, not an expansion.

The refusals are enforced by an allowlist at the lexer, not a blocklist: a
character outside the permitted set for the current state is refused, so a
construct nobody anticipated fails closed rather than being absorbed into a
word. **This set may widen in a later release and will not silently narrow.** A
line that runs today will still run.

Two consequences worth stating because they are limitations rather than
oversights. Globs are refused rather than expanded: expanding one would let the
argv the policy checked differ from the argv that ran, since the filesystem can
change in between, and passing it through unexpanded would mean something
different from what a shell would have done. Use `find` or `list_dir` to choose
paths. And `2>&1` on a stage whose stdout is piped is refused, because merging
the two streams into a pipe needs a descriptor duplication this crate does not
perform; on a final stage it merges in the captured output.

**That is a parse refusal and not an error, as of 0.73.0.** It is raised where
every other construct on this list is raised, named `a stream merge on a piped
stage`, and `run::dispatch` returns a decision beginning `shell refused:` so the
step is refused and the run continues. Up to 0.72.0 it was an `Error::Config`
raised by `apply_redirects` at spawn time, which propagated out of the run loop
and ended the run — one ordinary shell idiom cost a whole session. A `cd` stage
is exempt, because a `cd` inside a pipeline never has its redirects applied at
run time, so `cd x 2>&1 | y` still runs.

`cd` is applied when the line is planned, not when it runs, so in
`cd nope && ls > out.txt` the redirect resolves under `nope/` even though a real
shell would have failed the `cd` and written in the original directory. Both
paths are checked and neither escapes the root. The alternative — resolving at
run time — would mean the policy approved one path and the process opened
another.

## What a process handle is, and is not (0.25.0)

`shell_start` runs a command line and hands back a handle id instead of a result,
so a dev server, a log tail or a watch build can outlive the step that started
it. `shell_poll` reads what it has printed since the previous poll and whether it
is still running; `shell_kill` ends it and the processes it spawned.

The line is parsed and checked by exactly the machinery `shell` uses — the same
lexer, the same refusal set, the same per-stage `Act::Exec` check and the same
per-redirect path check, all of it before the first spawn. Everything the section
above says about what `shell` will and will not run is therefore true of
`shell_start` unchanged. A handle is a different *lifetime* for a command line,
not a second way to run one.

**A handle does not survive the process that started it.** When a run is resumed
in a new process, every handle the previous process left running is marked
orphaned, and an orphaned handle is never re-attached, polled or signalled. A
poll or a kill naming one is answered from what was recorded, and the model is
told to start again whatever it still needs. The reason is that the only thing a
checkpoint records about a live process is its pid, and a pid is not an identity:
between the crash and the resume the operating system may have given that number
to something unrelated, and no test separates the two with enough confidence to
justify signalling, because every "is it still our program" check races the
signal that follows it. This is the one way this crate could damage something
outside its own workspace, and the cost of being wrong is not a failed run but
somebody else's process. So the handle is recorded, reported, and left alone. A
harness that re-attached would be making a different trade with the operator's
machine; this one does not make it, and will not start.

**A run may have eight handles live at once** (`MAX_LIVE_HANDLES`). The ninth
`shell_start` is refused with a reason naming the cap rather than queued — a
queue is a leak with a delay — and killing one makes room for another, so the
bound is on how many run at once and not on how many a run may start.

**Every live handle is killed when the run ends, however it ends**: a finish, a
budget stop, a cancellation, an error carried out of the loop, or a panic.
`shell_kill` is for finishing with something early, not for tidying up.

A handle has no wall-clock timeout. `shell` kills a line that runs too long
because a foreground call has no other way to be told to stop; a handle has
`shell_kill` and the end of the run instead, and a dev server has nothing to be
killed at.

A handle's output goes to a capture file, and a poll returns a bounded window of
what is new since the previous one, by byte cursor rather than by re-reading. The
capture file does not outlive the run and the store does: `process_handles` — one
row per handle, with its line, its recorded pids and how it ended — and
`handle_output`, appended as each poll reads it. Like the rest of the schema they
are reached through `Store`'s methods rather than depended on directly. Both are
additive and no checkpoint layout changed, so **`CHECKPOINT_FORMAT` stays 7**: a
0.24.0 binary reading a database this release wrote never queries either table.

## What the plan gate stops, and what it does not (0.31.0)

Registering a [`PlanGate`](https://docs.rs/io-harness/latest/io_harness/trait.PlanGate.html)
on a contract opens the run in a **planning phase**. It is worth being exact
about what that phase is, because the useful claim — *nothing is written before
the approval* — is only as good as the boundary that enforces it.

**It is the policy, not the tool list.** While the phase is on, the run's
effective policy carries a `plan-gate` layer denying every `Act::Write` and every
`Act::Exec`. `Policy::explain` resolves deny-first across all layers, so this
covers `write_file`, `edit_file`, `exec`, the four shell tools, the git built-ins,
every registered `Tool` and every MCP tool — the last two because invoking one is
an exec check on its own name. A refusal during the phase appears in the trace as
an ordinary refusal attributed to the `plan-gate` layer, so it is legible to
someone who has never heard of this feature.

**Reads and the network stay open.** A plan written without looking at the
workspace is not worth reviewing, so `grep`, `find`, `read_file`, `list_dir` and
the rest are untouched. `Act::Net` is untouched too, for a blunter reason: the
provider is reached over the network, and denying it would stop the run asking
for the plan in the first place.

**`remember`, `forget` and `send_message` are the writes the policy cannot
see**, because they land in this crate's own store rather than in the workspace.
`remember` and `forget` are refused explicitly for the duration of the phase — a
withdrawal changes what later runs know exactly as a write does. `send_message`
(0.60.0) is not refused and does not need to be: the gate is one gate at the
root, a spawn during the phase is an `Act::Exec` the `plan-gate` layer refuses,
and an address naming no live agent is refused by the tool — so during the phase
there is nobody in the tree to send to. `todo_write` and `ask_question` are
neither: neither changes anything outside the run's own record of itself.

**What the phase is not:** it is not a sandbox, and it does not undo. A tool that
reads a file and has a side effect the crate cannot see — a registered `Tool`
that posts to an API from inside its `invoke` — is refused by the exec check like
any other, but a *read* tool the embedding program wrote that quietly writes
something is outside every boundary this crate has, in the phase and out of it.

**One gate, at the root, once per run.** A spawned child never holds its own
plan: a hundred children each pausing for a human is the problem the gate exists
to prevent. And the tool is withdrawn the moment a plan is approved, so a long run
that changes its mind cannot re-open the gate mid-flight.

**It is not `todo_write`.** The 0.21.0 todo tool records a plan the agent is
already executing, for an operator to watch. This one proposes a plan the run has
not started and will not start until an answer arrives. Both exist and neither
replaces the other.

## Reasoning effort, per vendor (0.31.0)

`Effort` is a tier — `Low`, `Medium`, `High` — because that is the vocabulary the
vendors share, and it is projected onto whatever each one actually accepts. It is
a **request, not a fact**, in exactly the sense `CompletionRequest::model` is: the
crate cannot know which models reason, so nothing is refused for asking, and
`Usage::reasoning_tokens` is what says whether any thinking was done and billed.

| provider | what is sent | thinking returned in `CompletionResponse::reasoning` |
| --- | --- | --- |
| `OpenRouter` | `reasoning: { effort }` | yes, from `delta.reasoning`, where the model returns it |
| `OpenAi` | `reasoning_effort` | **no** — Chat Completions does not return reasoning text at all |
| `Anthropic` | `thinking: { type: "enabled", budget_tokens }`, with `max_tokens` raised to clear the budget | yes, from `thinking_delta` blocks |
| `Compatible` | `reasoning_effort` | whatever the endpoint sends, unverified |

Three things follow that a caller should hear plainly rather than discover.

**OpenAI returns no thinking.** Asking for `Effort::High` against `OpenAi` will
change how the model behaves and will leave `reasoning` at `None`, because the
Chat Completions API this crate speaks does not return reasoning text. That is
not a defect here and it is not worked around; `Usage::reasoning_tokens` is the
only visibility available on that path.

**Anthropic has no tiers**, so the tier becomes a token budget — 1,024 / 4,096 /
16,384 — and `max_tokens` is raised above it because Anthropic refuses a request
whose budget is not strictly below the cap. A caller who needs a specific budget
rather than a tier cannot express one; that is the cost of a vendor-neutral knob.

**`Compatible` is unverified.** The key is passed through to whatever endpoint
you pointed it at. An endpoint that does not know it may ignore it or may reject
the request; this crate does not check, in keeping with everything else on that
provider.

**The thinking is shown once and billed once.** Where a provider returns it, it
reaches an `Observer` as `EventKind::Reasoning` and is **never** appended to the
observation ledger — so it is not in the prompt assembled for the next turn. A
vendor charges for thinking once as output; a harness that folded it into the
next request would be charged for it again as input, every turn, for the rest of
the run. It is also **not persisted**: `Usage::reasoning_tokens` is the durable
record, and the text is live-only.

## What attaching to a live run gives you, and what it does not (0.33.0)

A second process can [`Attach`](https://docs.rs/io-harness/latest/io_harness/struct.Attach.html)
to a run that is still going: read the events the owning process is receiving, see
what it is parked on, and answer it. The transport is the SQLite store both
processes already open — `Store::open` has set `journal_mode = WAL` and a
five-second `BUSY_TIMEOUT` since 0.12.0 — so there is no socket and no on-disk
migration. **Attaching itself still takes no lease**: reading a run's events and
answering what it is parked on requires no ownership, and 0.62.0's run lease is
about *driving* a run, which `Attach` cannot do. The two are separate questions and
the answers did not merge.

**Nothing is durable unless you broadcast.** `Observer` is still an in-process
callback. The events reach the store only through
[`Broadcast`](https://docs.rs/io-harness/latest/io_harness/struct.Broadcast.html),
which wraps another observer and writes each event on its way past. A run without
one writes no `run_events` rows at all, and an attached reader on it sees nothing.

**It reads and decides; it does not take ownership.** `Attach` has no method that
starts, resumes or steps a run. Answering writes a row the owning process reads —
it is not a transfer of control, and there is no attached steer, cancel or budget
change. That is a boundary, not an omission.

**The first answer wins, and the loser is told.** `answer_approval`,
`answer_question` and `answer_plan` each return `bool`: whether this caller's
answer is the one the run acted on. It is one conditional `UPDATE`, so two
processes answering the same approval cannot both land, and the run reads the
decision back from the row rather than from whoever raced.

**It is a poll, at both ends.** `Attach::poll` is called by you, at whatever rate
suits. The run picks up an attached answer at `ATTACH_POLL` — 200 ms — rather than
instantly. Neither end pushes.

**`runs.status` still does not tell you whether the owner is alive — the lease
does (0.62.0).** `runs.status = 'running'` has never distinguished a live process
from a crashed one and this release does not change that column. What it adds is a
different one to ask: `Store::run_lease` returns who holds a run, since when, and
whether that hold has lapsed. A run whose owning process died still reports what it
was holding, and answering it still writes a row nothing will read until somebody
resumes.

**A second process cannot drive a run its owner still holds (0.62.0).** Every
`run_*` and `resume_*` takes a lease on the run it is about to drive and releases
it on the way out, whichever way it leaves. A second driver arriving while that
lease is live is refused with `Error::Conflict` naming the holder and the moment
the lease lapses — it is refused before it drives anything, so it commits no step.
Before this release both processes proceeded and interleaved their steps into one
trace that described a run neither of them had performed, with no error and nothing
in the store afterwards to distinguish it from a real one.

**A crash is not a lock.** An acquire is refused only when all three hold: the
lease belongs to another owner, it has not lapsed, and that owner's process is
still alive. Liveness is asked of the platform against the pid carried in the
owner id — `kill(pid, 0)` on unix, where `EPERM` counts as alive because the
process is there and is somebody else's, and
`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` plus `GetExitCodeProcess` on
Windows, where a handle refused for lack of rights likewise means the process
exists and only `ERROR_INVALID_PARAMETER` means it does not. Neither costs a
dependency this crate did not already have. So a `kill -9`'d owner's run is
takeable at once rather than at the ttl, on both. The check errs towards "alive":
an owner id with no readable pid, a platform that is neither unix nor Windows,
and a Windows process that exited with code 259 — indistinguishable from a
running one, because 259 *is* `STILL_ACTIVE` — all report the owner as running.
That error direction is the bounded one: a dead owner
believed alive costs a wait until the ttl, which is also what a recycled pid
costs, whereas a live owner believed dead would hand its run to a second driver,
and that cannot arise from an absent pid. **The ttl governs exactly those cases
and nothing else.** It is `TaskContract::lease_ttl`,
defaulting to `DEFAULT_LEASE_TTL` — twice `DEFAULT_EXEC_TIMEOUT`, because the
renewal rides each step commit and so what it must outlast is one step rather than
a whole run. However the run is taken over, the generation rises by one and the
previous owner's next durable commit is refused, writing neither a `steps` row nor
a checkpoint event, because the generation is verified inside the transaction that
would have written them.

**A session head advances by compare-and-swap, and a lost turn is reported
(0.62.0).** Two processes taking a turn on one session used to both write their own
turn id; the second won outright and the first process's turn stayed in
`session_turns` with its parent intact but off the head path — answered, billed and
invisible to the next turn. The losing write now returns `Error::Conflict` and the
losing turn row is left exactly as it was. This reports a dropped turn; it does not
make both turns land, and the answer that lost is still in the store to be read or
rebased.

**`run_events` never prunes itself.** A long run's stream grows without bound
while the run lives, and nothing expires on a clock. Deleting it is still the
application's call, but since 0.58.0 the crate gives it one: `run_events` is one
of the run-keyed tables `Store::delete_session` removes and
`Store::archive_session` empties.

**A row in `pending_approvals` no longer means the run is waiting (0.33.0).** The
row is now written *before* the in-process approver is consulted, so one exists for
approvals that were answered instantly. Read
`Store::unresolved_approvals`, which is what `Attach::waiting` uses.

## What a review criterion is, and what it is not (0.34.0)

[`Verification::Review`](https://docs.rs/io-harness/latest/io_harness/enum.Verification.html)
is the first criterion in this crate whose check is a judgement. A `Reviewer` —
`ModelReviewer` over any provider, a human, or a second harness — is handed the
goal, the rubric and the files the run wrote, and returns a verdict with its
reasons.

**A review is not a proof, and it is not reproducible.** It is one model's opinion
of one change against one rubric, at one moment. It does not replace an execution
gate and it is not meant to: an exit status is a fact and a verdict is not. The two
compose — run the suite *and* have the change read.

**A model may not review its own work, and the check is by name.** With
`allow_self_review: false` (the default), a reviewer whose model equals the model
under review is refused with `Error::Config` before a request is built. The
comparison is between **strings**: two different names can be the same weights
behind a gateway, and one name can be two snapshots a month apart. It catches the
obvious case honestly and cannot catch the disguised one. The model under review is
read from `Provider::model_hint`, which is defaulted to `None` — a provider that
does not name its model makes the refusal unreachable.

**The reviewer does not see the conversation.** It reads the goal, the rubric and
what was written, deliberately: a reviewer reading the author's reasoning is a
reviewer being led. The cost is that a change whose justification lived only in the
transcript is judged without it.

**A failed review is not fed back to the run.** The verdict ends the gate; it does
not become an observation the model is asked to address. Closing that loop would
let a model optimise against the reviewer, which is the failure the criterion
exists to prevent.

## What a gate attempt records, and what `retry_gate` will and will not do (0.34.0)

Every gate evaluation writes a `gate_attempts` row: `Passed`, `Failed`, or
`Errored`. The third is the distinction the crate did not have before 0.34.0 — a
criterion that could not be evaluated at all is not one that was evaluated and said
no, and only the first is worth repeating.

**`retry_gate` re-runs the criterion and nothing else.** No step is re-executed, no
tool is called, and the only provider call is the one the criterion itself needs. A
run's steps rows, its token ledger and its files are what the run left them.

**It grades the tree as it now stands.** The criterion runs against the workspace
at the moment of the retry, not against a snapshot taken when the gate failed. If
something changed the tree in between, the retry judges the new tree. The crate
does not snapshot a workspace at gate time.

**It refuses a gate that answered.** A `Failed` attempt means the criterion ran and
said no; re-running it over an unchanged tree asks the same question until the
answer is convenient. `retry_gate` returns `Error::Resume` for that, and for a run
that never gated at all.

## What routing changes, and what it cannot (0.34.0)

[`Routing`](https://docs.rs/io-harness/latest/io_harness/struct.Routing.html) sets
`CompletionRequest.model` on the requests the run sends. That is its whole
mechanism, and its whole limit.

**It routes models, not providers.** There is no rule that swaps the provider
mid-run: `Fallback` chains providers on failure, and a rule that moved between them
would have to answer what happens to the conversation the first one was holding.

**Escalation is one-way and counts consecutive failures.** A run that escalates
does not come back down — oscillating between two models mid-run is a behaviour
nobody asked for. Escalation beats downshifting: a run whose gate keeps refusing is
not one to save money on.

**`require_primary` asks once, and only of a provider that answers.**
`Provider::reachable` is defaulted to `Ok(true)`, so a provider that does not
override it makes the rule a no-op rather than a failure. It is a point-in-time
answer with no bearing on the next minute — a provider that dies mid-run is what
`Fallback` and `RetryPolicy` are for. There is no health check, no cache, and no
re-probe.

**A model name is a request.** Naming a model the provider does not have fails the
way any wrong slug fails: at the vendor, on the next request.

**Routing governs the root agent.** A spawned child takes the model on its
`AgentDef`, exactly as it takes its `Effort` there since 0.31.0 — that is where
"search cheaply, think hard where thinking is the work" is said, and a rule that
overrode a role's own model would be the roster's author being ignored by a
counter.

## What a capability bundle contributes, and what it may not (0.35.0)

A **plugin** is a directory with a `plugin.toml` at its root, named by a
`[[plugin]]` entry in a configuration scope and loaded by `Config::plugins()`. It
contributes skills, prompt templates, agent definitions, MCP servers, lifecycle
hooks, declared executables (0.73.0) and policy layers — no more.

**A plugin contributes data, never code.** There is no dynamic loading, and there
will not be. A `Tool` is an in-process trait implementation the application
registers; `dlopen` would make every safety property of this crate a function of a
directory a stranger wrote.

**Nothing verifies that a directory is what its author published.** No signature,
no checksum, no provenance. Nothing fetches, installs or updates a bundle either:
`[[plugin]]` names a directory that already exists on this machine, and
distribution is the application's. What *is* bounded is what an untrusted bundle
may contribute, which is a different and achievable claim.

**A declaration from any file inside the workspace may not contribute a hook, an
MCP server or an executable.** All three name a program this machine would run;
`io.toml` is the file a `git clone` delivers, and `io.local.toml` is a path in the
workspace root the run's own agent can write — the 0.28.0 rule for `[[hook]]`,
applied to a new declaration site, extended to `[[bin]]` in 0.73.0 and to the local
scope in 0.74.0. Only a bundle declared from the **user-scope** file contributes
all seven kinds. The refusal is whole: a workspace-declared bundle whose manifest
declares one contributes none of its other kinds either, because a half-applied
stranger's manifest is the failure the rule exists to prevent. **A manifest is not substituted at all**:
`${env:}`, `${file:}` and `${cmd:}` are each refused in every scope, as of
0.71.0. Before that only `${cmd:}` was, which was enough while a manifest could
be reached only after an operator had written a `[[plugin]]` entry naming it —
a trust act. `Plugins::inspect` is pointed at directories nobody has agreed to
yet, so the other two became reachable on untrusted input, and reading a host's
environment or its files is the same class of act as running a program on it.

**A bundle points only at itself, and a declaration only inside the workspace
(0.74.0).** `skills` and `templates` join `[[bin]]`'s `path` under the rule that a
bundle contributes a directory it ships rather than one it points at somewhere else
on this machine: absolute, or climbing out with `..`, is refused at load, lexically,
in every scope. The cost is higher for the first two than for a `[[bin]]`, which is
a path handed back for a caller to decide about — a skills directory is *read* at
run start, and the frontmatter of every `*.md` under it is composed into the model's
system prompt on every turn. A `[[plugin]]`'s own `path` is contained under the
discovery root for the same reason, through `contain_under_root` rather than
lexically, because that directory has to exist for there to be a manifest at all
and a symbolic link is therefore a live route; one that resolves outside is
**dropped** with its reason rather than refused, like every other bundle that fails
to load.

**0.74.0 closed the `[[mcp]]` gap in `io.toml` itself.** Through 0.73.0 a
project-scoped `io.toml` could still name an MCP command directly while a
*plugin's* `[[mcp]]` was refused there — the new surface started closed and the
existing one was left alone. That asymmetry is gone: `[[mcp]]` and `[[lsp]]` are
refused sections in both workspace files, so the same declaration is refused with
a bundle around it and without one. What has not changed is that an unknown key
inside an `[[mcp]]` table is still accepted, because serde refuses `flatten`
beside `deny_unknown_fields`.

**Plugin-supplied policy may only narrow.** A `[policy]` block may carry layers of
`deny` rules; an `allow` rule, an `ask` rule or a `defaults` block drops the
bundle. A bundle takes capability away and never hands it out.

**A hook or MCP server contributed from a trusted scope is not sandboxed by having
come from a bundle.** It runs a program with this process's privileges under the
same policy any other one would.

**Namespacing changes the names a model sees.** Every contributed name becomes
`<plugin>__<name>`, which is what makes a contribution attributable in the trace
with no new column — and it means a prompt or a skill that referred to another
skill by its bare name stops matching once that skill moves into a bundle. This
crate cannot rewrite prose.

**A bundle that fails to load is dropped, and that is quiet by design.** Loading
has no error path: the reason is on `Plugins::dropped()` and in
`EventKind::PluginDropped`, and the run proceeds. An operator watching neither can
therefore run for a week believing in deny rules that were never installed. An
application that wants a broken bundle to be fatal writes one `if` — see
[the guide](guide/plugins.md).

**`version` in a manifest is documentation.** Nothing resolves it, compares two
bundles, orders their loading, or checks one against the crate.

## When a turn is answered instead of run, and what that costs (0.37.0)

A **session turn** may close without opening a run. Its own first completion
decides: stopped on text with no tool call, the turn is a `TurnKind::Reply`;
carrying a tool call, it is a `TurnKind::Run` and the loop continues from that
same completion. The decision is the model's, made inside the completion the loop
was going to make anyway.

**Only the first completion of a turn can be a reply.** A run whose fifth step
stops on text is a run that finished, which is what it has always been. Nothing
about the loop's ending changed.

**A completion carrying both prose and a tool call is work.** The call decides.
Stated here rather than left to be discovered, because the opposite reading —
prose present, therefore an answer — would swallow the work silently.

**A model that answers in prose where it should have acted costs the operator one
retype.** This is the asymmetry the design accepts by choice, and it is a real
cost rather than a theoretical one: "I'll fix that for you", with no tool call,
closes the turn having done nothing. It is a retype and not a silence — the reply
is on screen — and the alternative asymmetry is worse: running something meant as
a greeting plans it, gates it, checkpoints it and bills it.

**The system prompt for a turn's first completion changed**, and it changed for
*every* session turn rather than only for greetings. It says the operator's
message may not be work at all, that a plain answer should be written where a
plain answer is the whole of what is wanted, and that where both readings are
possible the agent should act. Every later step of a promoted turn is asked
exactly as 0.36.1 asked it.

**A reply is billed.** One completion was made and it cost money, so the run row
is written, `Store::run_summary` reports its tokens, and the per-call accounting
row carries its model and its latency. The token ceiling is applied before the
answer is served: a turn that cannot afford its own reply is refused rather than
served free.

**A reply is not resumable.** There is nothing to resume — a reply is one
completion, and a process that dies during it loses one completion, which asking
again replaces at the same price. A turn killed while it was still deciding what
it was is refused by `Store::check_resumable` rather than offered as work to
continue. A reply that *finished* is a completed run like any other and a resume
reports its outcome, unchanged.

**A contract carrying a `Verification` is never a reply.** A caller who declared
how the turn is judged has said it is work; handing back an answer instead of
running the gate would be answering a different question. A bounded contract with
no verification classifies exactly as an unbounded turn does.

**`run_with` and `run_with_observed` never classify.** A one-shot contract is work
by declaration, and an entry point that sometimes answers instead of running is a
worse contract than one that always runs.

**There is no word list.** Not in this crate, in any form — no constant, no regex,
no match over literals, no shipped data file. A list is a list in one language,
matches `hi` and not `namaste`, and answers `hi, the login page is broken`
correctly only by accident. If the classification needed a lookup table to work,
it would not work.

## What prompt caching asks for, and what it cannot promise (0.38.0, 0.44.0)

The crate marks up to **two** cache breakpoints per request.

The **first** sits at the end of the system block. On the Anthropic wire that block
is preceded by the tool schemas, so the single marker covers the tool definitions
and the instructions together — the part of a request that is identical on every
step of a run and every turn of a session.

The **second** sits at the end of the frozen transcript prefix, and only a run that
has compacted has one. When 0.43.0's fold replaces the older observations with a
written summary, everything from the top of the prompt through that summary stops
changing, and that is what this marker covers. 0.38.0 deliberately left the
transcript unmarked because assembly rewrote earlier observations every turn;
compaction is what removed the objection.

**(0.49.0) The second marker is a count of messages on a request that carries a
transcript**, and a byte offset only on one that does not. `cache_boundary` is an
offset into `user`, which a conversational request does not send;
`cache_through` names how many leading messages the caller states are stable, and
the wire marks the last content block of the message before that count. It is a
translation of the same decision, not a second one: the same guard rules on
whether the prefix has already gone out once, and the marked span is asserted to
cover exactly what the offset covered. On the OpenAI wire the marker only ever
lands on a `role: "user"` message — an assistant message's content is `null`
whenever the turn was a bare tool call, and whether that wire carries a marker on
a `role: "tool"` message through to the vendor behind it is not something this
crate can assert. Marking less costs a smaller hit; marking something the vendor
drops costs a cache write on every step.

| provider | what is sent | why |
| --- | --- | --- |
| `Anthropic` | `system` as a content-block array whose one block carries `cache_control: {"type":"ephemeral"}`; the user turn split into two text blocks with the same object on the first | that vendor's caching is request-side |
| `OpenRouter` | the same two markers, in the parts shape that wire spells them in | it translates the marker for the vendors that take one |
| `OpenAi` | **nothing** | OpenAI caches a repeated prefix by itself; there is no request-side control to use |
| `Compatible` | **nothing** | 21 endpoints this crate does not control, where an unknown body key is a 400 nobody asked for |

Nine things follow that a caller should hear plainly rather than find on an
invoice.

**This crate declares a cache; it does not operate one.** Whether anything is
cached, for how long, and what it costs are the vendor's decisions. What is sent
is a request in exactly the sense `CompletionRequest::model` is a request.

**A short prefix is silently not cached.** Every vendor that caches sets a minimum
length below which it declines, and declines without saying so: the marker is
accepted, the response is normal, and `cache_read_tokens` stays zero. An embedder
with a small system prompt will see no effect and there is no error to read.

**A miss is invisible.** A cache entry expires on the vendor's own clock, and a
request that arrives after it has gone is served fresh with no signal that it was
ever meant to hit. `Usage::cache_read_tokens` is the only observable, and its
absence means "not served from cache" without saying why.

**A prefix used exactly once costs more, not less.** Vendors bill a cache *write*
above a fresh read and a cache *read* far below one — the rate shape in this
crate's own price table is 1.25× and 0.1× against input. So the block pays for
itself from the **second** use (1.25 + 0.1 against 2.0) and a single-call run pays
about a quarter more for it than it did in 0.37.0. This is accepted deliberately:
a run makes more than one call, and a session more than one turn.

**A run cached through OpenRouter under-reports what it cost.** That wire reports
no cache-write counter, so `Usage::cache_write_tokens` is zero by construction and
the tokens that were written are priced as ordinary fresh input. Measured on the
live run that proved this release: `cache_write_tokens` came back zero on all four
calls, including the one that must have written the entry the next call read. The
crate does **not** infer the write from the prompt length — that would put a
number in the trace the invoice does not contain.

**The transcript is cached only from a compaction boundary, and only once the
prefix has repeated.** A cache breakpoint needs a byte-identical prefix, and
`context::assemble` re-derives what the model sees on every turn: a later
observation supersedes an earlier one of the same kind and target, a write
invalidates an earlier read of that path, an invalidated read is re-read and its
text replaced, and what does not fit the ceiling becomes a stub. That is still true
of everything *after* the fold, which is why the marker goes at the summary and not
past it. A run that has not compacted marks nothing in its transcript at all.

**The crate never asks a vendor to cache a prefix it has not already sent.** Even
after a fold the prefix is not immutable by construction: the memory block renders
*ahead* of the summary and is re-read from the store on every turn, so a note the
run writes about its own work moves the prefix without touching the summary. The
run loop therefore holds the previous step's candidate and marks only when this
step's is byte-identical to it. Two consequences, both deliberate: the step a fold
happens on is **never** marked, so the marker is always one turn behind the
boundary; and a note written mid-run withdraws it for exactly one step. What this
buys is that the marker cannot be billed as a cache write on a prefix that then
changes — the failure mode is lost saving, never lost money.

**`EventKind::CacheMarked` says when the crate started asking, and its absence says
it never did.** It is emitted when the marked prefix *changes* — the step it is
first offered, and again whenever a later fold moves it — and not once per step. So
a run with no `CacheMarked` marked nothing, a run with three marked three different
prefixes, and one of these beside a zero `cache_read_tokens` on the same step means
the vendor declined a marker that was sent. Without it, "why is this run getting no
cache reads" has three indistinguishable answers.

**A request carrying an image is marked on one wire and not the other.** The
Anthropic wire puts image blocks *before* the text, so there is no text prefix to
mark: a marker there would write a one-turn attachment into the cache entry, and the
next turn — which carries no image, because `Session::attach` stages for one turn
only — could never hit it. The OpenAI-shaped wire puts text first, so the two text
blocks lead, the images follow, and the boundary is honoured. Same request, two
vendors, two different answers, and the difference is a property of the orderings
rather than a policy this crate chose.

## What a contained session turn gives you, and what it does not (0.39.0)

A **session turn may fan out**. `Session::turn_contained` and
`Session::turn_contained_observed` take a `Containment` and drive the turn through
the agent-tree loop, so the agent answering it is offered `spawn_agent` and can
decompose the work into contained children. The five turn entry points that
predate this are untouched and still never offer the tool: a session that does not
pass a `Containment` behaves exactly as it did in 0.38.0.

**And it may carry a contract (0.66.0).** `Session::turn_contained_bounded` and
`Session::turn_contained_bounded_observed` take a `TaskContract` beside the
`Containment`, so a turn that may fan out can carry a plan gate, a preset or a
replaced system prompt, repository instructions, registered tools, MCP servers,
skills, a step or token budget and a verification gate — every one of which
`turn_bounded` has accepted since 0.36.0 and no contained turn could be given at
all. The contract's `root` is replaced by the session's, exactly as `turn_bounded`
replaces it. `Harness::turn_contained` and `Harness::turn_contained_with` are the
same two shapes with the host bound once.

**And it may be steered while it carries one (0.67.0).**
`Session::turn_bounded_steered` and `Session::turn_contained_bounded_steered` take
the caller's `TaskContract`, an `Observer` and a `SteerInbox` on one call, so a turn
that carries any of the above can also be corrected while it runs — and a fan-out
can be corrected at its root. Before this release the choice was exclusive:
`Session::turn_steered` takes the inbox and builds its contract internally, and the
`_bounded_observed` pair takes the contract and no inbox.

Nothing about steering changes. A message is drained at the step boundary and
nowhere else, so the step in flight completes whole and the agent reads the
correction before choosing its next action; an interrupt ends the turn as
`RunOutcome::Cancelled` on a whole step; and a fold (0.69.0) summarises the
conversation so far at that same boundary, before the step assembles its own
request. **On the contained path that boundary is
the root's own step**, which is the one point at which no child of the root is in
flight — children are awaited inside the step that spawned them. So a correction
typed while children are running lands after that step's children have finished,
not while they run. **A spawned child is handed no inbox at all**, deliberately: a
sub-agent is never steerable by an operator it has not spoken to.

Four things the tree loop reads differently from the flat one, and none is new in
0.66.0 — each has been true of `run_tree` since 0.39.0, and 0.66.0 only makes them
reachable from a session. The tree's one shared spend ceiling is built from the
`Containment`, not from the contract; `contract.max_tokens` bounds the single agent
and is capped by the tree's remaining budget. `Routing::escalate_after` and
`downshift_under` do not move the model per step, because `apply_routing` is called
from the flat workspace loop only. The preflight checks made before a flat run's
first request — a `Verification::Review` contract with no reviewer, a reviewer that
is the model under review, and `Routing::require_primary` against
`Provider::reachable` — are not made for a tree, though a model approving its own
call is still refused at the root. And `max_parallel_reads` bounds a batch that only
the flat loop builds; a contained agent dispatches its reads one at a time.

What the fan-out inherits is what `run_tree` has always given a tree — the
caller's policy narrowed per child through `Policy::contain`, one shared `Ledger`
no child contract can raise, per-tier concurrency slots with a durable queue, and
the whole graph reconstructable from `Store::agent_events`. Six things follow that
a caller should hear plainly.

**The ledger is per turn, not per session.** Each contained turn builds a fresh
ledger from the `Containment` passed to that call, so turn five gets the ceiling
turn one got and a conversation's total spend is the sum of its turns'. There is
no single ceiling across a conversation, and this crate does not offer one — it is
the same rule as "a session has no aggregate budget" below, applied to the tree.

**A child is given its goal, not the conversation.** The turn's seed — the prior
turns on the path — reaches the root agent and stops there. A child receives its
own goal, its own contract and its narrowed policy. Two reasons, and neither is
effort: forty children each carrying the transcript is the multiplied version of
the cost `ContextBudget` exists to bound, and a child that has read the
conversation is one that can act on an instruction the operator has since
withdrawn. A child's result composes back into its parent's next step, which is
where the conversation and the fan-out actually meet.

**A child is a run, never a second turn.** `Store::session_turns` returns one row
for a turn that spawned forty agents, `Session::history` renders one entry, and
`Store::turn_for_run` on a child's run id returns `None`. The children are runs
under the turn's run.

**A paused contained turn is resumed with the tree resumes.** A turn that stops
`AwaitingApproval`, `AwaitingAnswer` or `AwaitingPlan` is continued with
`resume_tree_with_decision`, `resume_tree_with_answer` or
`resume_tree_with_plan_decision` on `TurnResult::run_id` — not the flat
`resume_with_*` family, which does not rebuild the tree. **The turn row reports
the pause, not the continuation:** it is closed with what the run said when the
turn returned, so a turn parked on an approval reads `running`, and it still reads
`running` after a resume the session did not drive. The run itself is the record
that moves. There is no `Session::resume_turn`.

**Cancellation and interruption are honoured at the root's step boundary**, which
is the one point at which no child is in flight — children are awaited inside the
step that spawned them. A `Flow::Cancel` from the observer stops the whole tree
there, as it does for `run_tree`.

**The worst-case concurrency is `max_concurrent_agents × depth`**, inherited
unchanged from the per-tier slot design: a parent holds a slot at its own tier and
waits only on the tier below, which is what makes the wait graph acyclic. A
contained turn is a tree like any other in this respect.

## What contained `exec` and `shell` give you, and what they do not (0.40.0, 0.46.0)

**The project's own commands run inside the sandbox by default, since 0.46.0.**
`TaskContract::exec_sandbox` is a `SandboxConfig`, not an `Option`, and its
`ExecMode` decides where a command may write:

| `ExecMode` | A command may write to |
| --- | --- |
| `ReadOnly` | the system temporary directory, and nothing else |
| `WorkspaceWrite` *(the default)* | the workspace root, the system temporary directory, and the detected toolchain's own cache directories |
| `FullAccess` | anywhere this program's user can write |

**Every mode may also write `/dev/null`**, on every backend. The bit bucket is
where a toolchain's own scripts send output they mean to discard, and a write to
it changes nothing an observer can see — the confinement is about what a run can
*keep*. The macOS profile has allowed the device since 0.6.0 and the namespace
rung populates `/dev`; the Landlock rung grants it as of 0.48.0, having been the
one rung where a read grant alone made every git built-in fail.

`FullAccess` is what every release up to 0.45.0 did by default, and it is still
available — as a sentence rather than as an omission:
`TaskContract::with_full_access()`. The method is named the way it is so the
widest grant this crate makes is legible in a diff and findable with
`grep -r with_full_access`. `ExecMode::ReadOnly` still grants the temporary
directory, because a toolchain that cannot open a temporary file cannot start.

**The default is a boundary and not a ceiling.** The mode-derived default carries
`SandboxLimits::none()` — no CPU, wall, memory, process or file-descriptor cap at
all. Defaulting containment on is a claim about where a command may write;
defaulting the 0.6.0 ceilings on (120 wall seconds, sized for a verification gate
compiling one crate) would be a claim about how long someone else's build may
take, and this crate is not in a position to make it. The standing caps are one
call away: `with_contained_exec(SandboxConfig::new())`.

**The detected toolchain's cache directories are writable roots.** A default that
no real project can build under is a default every embedder turns off on their
first failure, so `Toolchain::cache_dirs` derives them for the ecosystem
`toolchain::detect` already found — `CARGO_HOME`, `GOMODCACHE`/`GOPATH`,
`npm_config_cache`, `PIP_CACHE_DIR`, `GRADLE_USER_HOME`, `NUGET_PACKAGES` and the
rest, each ecosystem's own environment variable winning over the conventional
path. **Only roots that exist on this host are granted**, and that filter is part
of the confinement rather than tidiness: the Linux mount setup binds every root it
is given, a bind of a path that is not there fails the setup, and a failed setup
degrades the whole backend to `PortableFloor` — so a granted path that was not
there would silently unwind the confinement it was added to preserve. A project
whose ecosystem this crate cannot name (`make`, `cmake`) gets the workspace and
the temporary directory alone.

**The verification gate takes the same roots.** It runs the project's own build
command in an ephemeral working directory, so it is the one place this crate runs
a package manager on purpose, and a gate that could not populate a registry cache
would fail for a reason that has nothing to do with the code it is judging.

**A run reports its containment once, at start.**
`EventKind::Contained { mode, backend, roots }` names the mode asked for, the
backend `sandbox::select` **actually returned**, and how many writable roots were
granted after the exists-filter. A `FullAccess` run emits it too, with
`backend: "none"` — "this run was not contained" is the first fact an audit wants
and an absent event is not a statement. The per-command `SandboxEvent` rows are
unchanged.

**An `io.toml` may name the mode**, as `[sandbox] mode = "read-only"`. It obeys
the standing trust rule: a file inside the workspace may narrow and may never
widen, so `mode = "full-access"` is refused in `io.toml` and — since 0.74.0 — in
`io.local.toml`, exactly as `force_floor = false` and `allow_network = true`
already are.

**A contained command keeps the workspace root as its working directory.** This is
what makes containment usable for a build at all. The sandbox never discarded a
working directory — the verification gate chose a `TempDir` and copied results
back out — so a contained command is simply given the workspace instead. Nothing
is copied in, nothing is copied out, and a second command sees what the first one
wrote, which is what an incremental build depends on.

**What each platform actually enforces differs, and the differences are not
cosmetic.**

| Platform | Resource caps | Writes confined | Egress denied |
| --- | --- | --- | --- |
| macOS | Yes | Yes, to what the mode grants | Yes |
| Linux | Yes | Yes, to what the mode grants (0.40.0) | Yes, on every rung but Landlock below ABI 4 |
| Windows, Job Object (the default) | Yes | **No** | **No** |
| Windows, AppContainer (`with_access_confinement()`, 0.59.0) | Yes | Yes, to the grant set derived from the run's facts | Yes, as a capability — all hosts or none, with no per-host proxy |

**The Windows default row is the one that is still open.** A Job Object contains
resources and nothing else, so a contained Windows command that did not ask for
an access boundary gets the caps and nothing more — no filesystem boundary and no
egress boundary, because a job object has neither facility. `ExecMode` is routed
and reported on that backend and enforces nothing for the filesystem.

The other row is what 0.47.0 was specified to give and did not. The Windows half
of that release — the AppContainer selected, a grant set derived from the run's
resolved facts, an empty capability array as the egress denial — was taken out of
it whole on 2026-08-10 and shipped in **0.59.0**. The record is
`US-IO-HARNESS-0.47.0-I01`, and the reason for the delay is that the half could
not be verified from the development host: ten CI rounds on `windows-latest`
found three real defects in the module, fixed them, and did not converge. It is
opt-in because the derived grant set is not complete — a toolchain reading a
machine-wide file outside it is refused — so a run that does not ask for it gets
exactly what 0.46.0 gave it.

**On the Windows Job Object and on the portable floor
the `ExecMode` is therefore routed and reported and enforces nothing for the
filesystem** — it is a statement of what the run asked for, not of what the host
delivered, and `EventKind::Contained`'s `backend` is where the difference shows. On Linux the filesystem half is new in
0.40.0: before it, the backend unshared a mount namespace and remounted nothing
into it, so only the network namespace was real. A host whose kernel refuses the
remounts degrades to `PortableFloor` and **reports the floor** rather than naming
an isolation that was never applied — the recorded backend is the one that
applied, which is the point of recording it.

**On Linux the "Yes" in that table depends on which rung of the chain the host
admits.** Three of the four rungs need an unprivileged user namespace, and Ubuntu
24.04 ships `kernel.apparmor_restrict_unprivileged_userns=1`, which refuses one.
That is the reason the Landlock rung exists and is tried first: it needs no
namespace at all, so a stock Ubuntu 24.04 host takes `Backend::LinuxLandlock` and
**does** get the filesystem confinement — with egress denied only where the
kernel's Landlock ABI carries the network rules (4 and later, kernel 6.7). A host
admitting neither Landlock nor a namespace takes `PortableFloor`, where the
resource caps still apply and **the filesystem confinement and the egress denial
do not**. Nothing is hidden — `select().backend()` answers before the run and the
`SandboxEvent` rows record it afterwards — but a caller who assumes the table's
Linux row without reading the backend is assuming a rung. Setting
`kernel.apparmor_restrict_unprivileged_userns=0` makes the namespace rungs
available too, which is what most other distributions already ship; this
repository's own CI does exactly that so its Linux legs exercise them rather than
only the rung that needs nothing.

**Egress under containment follows the policy's own host rules (0.48.0).** Up to
0.47.0 this was all hosts or none: the backends take one boolean, so a policy
allowing exactly one host gave a contained command a route to **every** host.
A run whose policy carries any `Act::Net` rule now routes its contained commands
through a loopback proxy the run owns. The sandbox permits that address and
nothing else; the proxy asks the run's own `Policy` about every `host:port` before
it connects, tunnels what is permitted, and answers what is not with `403` naming
the rule and the layer that refused it. Every dial is recorded — a `policy_events`
row with `act = "net"` and a `sandbox_events` row of kind `"dial"`.

A run whose policy names **no** host starts no proxy and is on the pre-0.48.0
path: the single boolean, unchanged. `Effect::Ask` still counts as *not*
permitted.

**What the proxy proves differs per backend, and the weaker answer is reported
rather than implied.**

| Backend | What scopes the route to the proxy | What that proves |
| --- | --- | --- |
| `macos-sandbox-exec` | SBPL names the loopback address and port exactly | Per-host. A direct dial past the proxy is refused by the kernel. |
| `linux-landlock` (ABI 4+) | `LANDLOCK_ACCESS_NET_CONNECT_TCP` permits one **port** | Port-scoped, **not** address-scoped: another host on that same port number is reachable. The port is ephemeral and chosen per run, which narrows it in practice and is not a proof. |
| `linux-namespaces`, `linux-bubblewrap` | nothing — an empty network namespace cannot reach the host's loopback | Not selected for a run that names hosts. Such a run takes the boolean and reports the backend that applied. |
| `windows-job-object`, `portable-floor` | nothing | The proxy is **advisory**: the variables are set and a command that ignores them reaches the network. The agent's own boundary section says the word "advisory". |
| `windows-appcontainer` | nothing — a process inside an AppContainer cannot reach a loopback listener under any capability set | **No proxy is started at all** (0.59.0), because pointing a command at one it cannot reach would hang every request it makes. Egress is the container's capability set, all hosts or none, and the per-host rules are not enforced. `Backend::reaches_loopback_proxy` is false for this backend alone. |

The proxy terminates no TLS and inspects no payload: a `CONNECT` names its host in
cleartext, which is the whole of what the decision needs. It is a boundary for the
agent's own commands and **not** a security barrier against another process on the
same machine — the listener is on loopback with an ephemeral port, and anything on
the host may talk to it.

**A contained command may write only under what its mode grants.** Up to 0.45.0
that was the workspace and the system temporary directory alone, and the concrete
cost was a toolchain populating a user-level cache: a cold `cargo fetch` writing
`~/.cargo/registry`, or `npm install` writing `~/.npm`, failed under containment.
0.46.0 grants the detected toolchain's own cache directories, which is what
removes that cost for a project whose ecosystem this crate can name. For one it
cannot — or for a build that writes to a path the *caller* configured outside the
workspace — the answer is `with_full_access()`, said once at the call site.

**Every spawn the run's own command tools make is contained (0.48.0).** Up to
0.47.0 this paragraph said the `shell_start` / `shell_poll` / `shell_kill`
handles were not, and it did not mention that the git built-ins were not either —
so which tool the model happened to pick decided whether the boundary applied.
Both now take the same containment every other spawn takes, per stage.

**That is not every process the crate starts, and 0.74.0 closed the two that
mattered most.** The automatic post-edit checker and the `check` tool built their
own uncontained `Exec` (0.51.0); a language server named in `[[lsp]]` is spawned
directly (0.52.0); and on unix the browser child is spawned directly (0.53.0).
Both checker spawns now take the run's own containment and are asked of the
policy first, which matters because a checker is a compiler and a compiler runs
the workspace's own `build.rs`, its proc macros and any `rustc-wrapper` — code
chosen by whoever wrote the files in the tree, which under this crate's threat
model is not the operator. A language server and the browser child are still not
wrapped by the backend `select` chose, so on those two paths the filesystem
boundary is the policy's alone. What each passes through is stated where it is
specified: starting a language server is an `Act::Exec` check, both the `check`
tool and the post-edit reflex are `Act::Exec` checks on the program and on the
whole argv, and every navigation the browser makes is an `Act::Net` check whose
egress takes the run's own proxy. Wrapping the remaining two is a later release's
work, and it is recorded here rather than left to be discovered.

The design question that paragraph deferred — what a resumed run does with a
handle whose sandbox no longer exists — is answered by construction rather than
by a mechanism: a handle's restriction lives **with its processes**, as a
`pre_exec` rule set or a wrapper argv on unix and as the Job Object the handle
already owns on Windows. There is nothing to tear down and nothing to re-enter,
so a resumed run finds the previous run's handle rows and orphans them exactly as
it did before.

**A handle takes the caps a foreground `shell` stage takes, and the wall clock
reaches neither.** `SandboxLimits::max_wall_secs` is enforced inside the sandbox's
own runner, which `exec` uses and no `shell` path reaches — so a handle cannot be
killed by it, which is also the right answer: a dev server killed at the sandbox's
ceiling would be a containment feature deleting the tool's purpose. The CPU and
open-file rlimits do apply. A handle still live when the run ends is killed by the
registry on drop, and that ending is not recorded — there is no step left to
record it on.

**Each spawning tool declares the mode it needs, and the run resolves it before
the spawn.** A call runs under the *narrower* of what it declares and what the
contract granted — the three git readers declare `read-only`, the four that write
`.git` declare `workspace-write`, and `exec` / `shell` / `shell_start` declare
nothing because they are the tools the grant was written for. A need the grant
cannot satisfy is refused with **no process started**, so the model reads a reason
naming both modes instead of decoding an errno. A registered `Tool` may declare
one too, and there the declaration is a **refusal** mechanism and not a
confinement one: the crate does not see that tool's own spawn and does not claim
to govern it.

The three paths above that no backend wraps declare nothing either, and a
declaration would have nothing to apply to: `check`, the `lsp_*` tools and the
`browser_*` tools fall through to the toolbox's own arm, which holds no entry for
a built-in, so they run under the contract's own mode with nothing narrowing
it.

**A contained `shell` stage is contained slightly less than an `exec` command.**
`shell` pipes its stages into one another, so it owns every child process and
cannot delegate the spawn to the sandbox's own runner. Each stage gets the
backend's wrapper — filesystem confinement and egress denial — and the CPU and
open-file rlimits, but **not** the memory monitor, and a stage killed by a cap is
not attributed to a `Cap` the way an `exec` command is. Every stage is wrapped,
not just the first.

**Two ceilings, and they are not the same ceiling.** `TaskContract::exec_timeout`
is the contract's bound on a wedged command and is raised with
`with_exec_timeout`; `SandboxLimits::max_wall_secs` is the sandbox's own and is
raised in the config. A command stopped by the second is reported as a cap kill
naming the resource, and one stopped by the first is reported as a timeout.

## What running read-only tool calls together guarantees, and what it does not (0.41.0)

Since 0.41.0 the loop may run several of one completion's tool calls at the same
time. This is the guarantee that goes with it, because the change is only worth
having if nothing an embedder can observe moved.

**Order is the model's call order, always.** Observations, decisions, recorded
steps, `Edit` rows, budget draws and the events an `Observer` receives are folded
back in the order the model asked for the calls, never in the order they
finished. A run's trace, its context assembly, its ledger and its replay are the
same as the serial run of the same recorded case — identical rows, not equivalent
ones. Concurrency here is an execution detail and the release treats any drift in
that as the defect it would be.

**Only read-only calls overlap.** Three built-ins are read-only: `grep`, `find`
and `read_file`. Everything else built in runs one at a time, in order, exactly as
it did before — `write_file`, `edit_file`, `exec`, the four shell tools, the git
built-ins, `list_dir`, `view_image`, spawn, `remember`, `todo_write`,
`ask_question`, `ask_questions`, `propose_plan`, and every built-in added since. That list names
the tools this paragraph was written against and is deliberately not maintained
as an enumeration: the rule is the enumeration — three names are read-only and
everything else built in is not. That includes the git readers and `list_dir`,
which change nothing: `list_dir` is outside the set the release measured, and the
git readers reach the world through a process, which a later release will decide
about with its own evidence rather than by extension.

**An MCP tool is never overlapped.** A server can advertise `readOnlyHint`, and
honouring it means issuing overlapping requests on one `McpSession`. Whether that
session multiplexes request ids over a shared stdio transport, and what a server
that does not expect it does, is a question about the MCP client. It is recorded
as an open question rather than answered by assumption.

**A registered tool decides for itself, and the default is the safe answer.**
`Tool::effect` returns `ToolEffect::Mutating` unless the implementation says
otherwise, so a toolbox assembled before 0.41.0 behaves exactly as it did.
Returning `ToolEffect::ReadOnly` is a promise the tool makes about itself, and
the harness cannot check it — the tool is arbitrary code the embedding program
compiled in. A tool that declares read-only and then writes breaks its own
invariants and nobody else's, which is the same shape as every other `Tool`
invariant and not a new class of exposure.

**The ceiling is the contract's.** `TaskContract::max_parallel_reads` defaults to
10 and clamps to a floor of 1; `0` means serial rather than an error. It bounds
calls *in flight*, so the number that actually run together is `min(cap, the
read-only calls in that completion)`. **`with_max_parallel_reads(1)` reproduces
0.40.0's execution shape exactly** — the batch path is not entered at all — which
is the supported way to rule the concurrency out while debugging something else.
It bounds tool calls inside one step of one agent and nothing else;
`Containment::max_concurrent_agents` bounds a tree's children, and the two caps
are independent.

**A pause collapses the batch.** A batch runs concurrently only while every call
in it resolves to an outright allow. The first call an approver defers ends it:
results already folded stand, the calls after it in that completion are not
started — not merely not recorded — and the step stops as it always has. Because
the collapse point is decided by the policy rather than by timing, the same
recorded case collapses at the same call every time.

**Nothing new is emitted, deliberately.** The same `EventKind::ToolCall` events,
in the same order, with the same steps and the same ledger draws. An observer
cannot tell from the event stream whether a batch ran concurrently. That is the
guarantee, not a gap.

## When a read starts, and why it is earlier than the completion (0.54.0)

0.41.0 decided which read-only calls overlap. This decides when the first of them
begins, and it is the only thing that moves: the same `ReadWork` runs the same
way under the same `max_parallel_reads`, and everything above still holds.

**A read starts as soon as its arguments are complete, not when the model stops
talking.** A provider that implements `Provider::complete_streaming_calls` reports
each finished tool call while the completion is still streaming; the harness
starts the read-only ones then. The default implementation of that method reports
nothing, so a provider that has not opted in — including `Record` and `Replay`,
and every implementation written before 0.54.0 — behaves exactly as it did.

**Only the completion's LEADING run of read-only calls, which is narrower than
what 0.41.0 batches.** A batch is any maximal run of read-only calls; speculation
stops at the first call that is not one, and never resumes for that completion. A
read started after a write that has not run yet would answer from before the
write, which is a wrong value rather than a wrong order.

**Only what the `Policy` allows outright.** A call in the grey tier is not started
early, so no approver is ever asked about a completion that may never settle, and
0.41.0's collapse-on-pause rule stays exactly where the model put it. A run with a
tool hook configured speculates nothing at all, for the same reason: a hook can
refuse a call, and asking it early would hand it a call that may not exist.

**A result is used only if the settled completion asks for it** — same position,
same name, byte-identical arguments — and what survives is kept as a contiguous
run from position zero. That one rule is what a failed attempt, a retry and a
`Fallback` fallover all reduce to: the settled completion is a different
completion, so nothing speculated against the abandoned one matches it and
nothing is kept. The exception is exact agreement — where the two completions ask
for the *same* call with the *same* arguments, the speculated result is that
call's result and is used. A discarded speculation leaves nothing behind at all:
the read happened, and the run recorded none of it.

**A registered tool needing more containment than the run grants is never started
early**, for the same reason `dispatch` refuses it before the arm that would run
it: the refusal says nothing was started, and that has to stay true.

**`max_parallel_reads` is the whole switch.** `with_max_parallel_reads(1)` turns
starting early off with the batching, so there is one escape hatch rather than
two.

**One event is new, and the reason is that this trade can lose.** 0.41.0 emitted
nothing because overlapping two reads costs nothing when it does not help;
starting a read before the model has finished asking for it costs a whole read
when the completion turns out not to want it. `EventKind::Speculated { started,
used, discarded }` is emitted once per step that started something, and never for
a step that did not. Nothing else moves: the same `ToolCall` events in the same
order, the same observations in the same ordinals, the same rows.

**Where it does not apply.** `run_with` and the other one-shot entry points do not
stream, and the tree loop that drives child agents dispatches serially — it never
took 0.41.0's batch path either. Both are unchanged by this release.

**Speculation follows streaming, and streaming follows the turn entry point.**
Only the `_observed` and `_steered` session turns stream —
`Session::turn_observed`, `Session::turn_steered`,
`Session::turn_bounded_observed`, `Session::turn_contained_observed`,
`Session::turn_contained_bounded_observed`, and (0.67.0)
`Session::turn_bounded_steered` and `Session::turn_contained_bounded_steered`. A
turn taken through `Session::turn`,
`Session::turn_bounded`, `Session::turn_contained` or
`Session::turn_contained_bounded` does not stream and therefore starts nothing
early. That
is 0.20.0's rule about where `EventKind::Token` comes from, unchanged; it is
restated here because it now decides a second thing.

## What a read returns, and what it will not return instead (0.55.0)

**A read is whole, a range you asked for, or a refusal.** It is never a shortened
version of the file. Until 0.55.0 a `read_file` whose text exceeded the
per-observation cap was cut, with a visible marker in the text saying so. That is
honest and it was not enough: what the model then held had the *shape* of a
successful read, and nothing downstream — no event, no row, no field — could tell
a whole file from the tail of one. A model that saw twelve thousand characters of
a four-hundred-thousand-character file goes on to reason as though it saw the
file.

So a read that will not fit returns **no content at all**: an error naming the
path, the size in characters, the ceiling it exceeded, **which** ceiling that
was, and the two ways to proceed.

**There are two ceilings and they are different in kind.** One is
`[run] max_read_chars` (`TaskContract::with_max_read_chars`) — a number an
operator chose, which does not move. The other is derived from the run's
[context budget](guide/context-and-memory.md) and is a share of what is still
*unspent*, so it falls as the run spends: the same file is readable at step three
and refused at step forty. A read is refused when it exceeds either. The refusal
names the one that bit, because the answers differ — raise the key, or ask for a
range now. Setting the key is what makes the boundary predictable; leaving it
unset is what every version before 0.55.0 did.

**The remedy exists, which is why the refusal is allowed to be one.** `read_file`
takes `offset` (1-based) and `limit`, in lines, and the observation header states
the range and the file's total line count. A partial read the model asked for by
number is a partial read the model knows about, which is the whole difference
from one it was handed.

**A stored read is whole in the prompt or a stub.** A read that fitted when it
happened can be squeezed later by a narrower budget share; it is then replaced by
a one-line stub naming the file and the range to re-read, never served as a
fragment. This holds for the re-read of a file a write invalidated too.

**Bounding is unchanged for everything else.** A command's output, a search's
matches, a skill body, a registered tool's result and an MCP server's reply are
still cut at the per-observation cap with the marker they have always had. A
prefix of a `grep` is not a lie about a document, because a `grep` was never one.
Document tools (`xlsx_read`, `pdf_read` and the rest) are still bounded the same
way: what they return is a rendering of a file rather than the file.

**A file that is not text is named, not decoded.** `read_file` classifies before
it reads: UTF-8, UTF-16 by byte-order mark with the encoding named, an image
routed to `view_image`, a known document routed to its own tool, and anything
else named as binary with its size and what its leading bytes look like.
Detection stops at the byte-order mark — no statistical charset guess, because a
guessed Latin-1 is the same class of confident wrong answer this whole section
exists to remove. `Workspace::read_typed` returns that classification;
`Workspace::read_file` returns the text or the typed error.

**One case is deliberately still "nothing":** a file that does not exist reads as
empty, which is what lets an agent create a file by reading it first. What
changed is that a file which *does* exist and is not text no longer answers the
same way.

## What the image door accepts, and what the wire still refuses (0.55.0)

**The set on the wire is four, and did not move.** `IMAGE_MEDIA_TYPES` is
`image/jpeg`, `image/png`, `image/gif`, `image/webp` — the intersection every
provider documents — and `Media::image` enforces it exactly as before.

**The set at the door is wider.** `Media::attach` passes those four through
**byte-identically** and decodes BMP, TIFF, ICO, TGA and PNM, re-encoding them to
PNG. A JPEG is never re-encoded: a decode-and-re-encode of the commonest format
would be a silent quality loss on the commonest path, and it is asserted on the
bytes rather than assumed. `view_image` uses the door, and its observation says
when a conversion happened, so a trace shows that the bytes on the wire are not
the bytes on disk.

**What cannot be decoded is refused by its own name.** SVG, HEIC and AVIF each
get the format's name, the reason, and a one-line conversion that produces a PNG.
The reasons genuinely differ and the message says which: HEIC and AVIF need a
system C library this crate does not depend on, so that it builds anywhere with a
Rust toolchain and nothing else; SVG needs a renderer, which is a dependency tree
rather than a C library. A PDF handed to `view_image` is routed to `pdf_read`.

**A declared size is checked before a decode.** A small file can declare an
enormous image, and a decoder that believes it allocates before anything checks
the result. Dimensions are read from the header and refused against
`MAX_IMAGE_PIXELS` first; `MAX_IMAGE_BYTES` still applies to the encoded result.

**No decoder is on the default path.** All of this is behind the `media` feature,
which now compiles `image` — already an optional dependency of this crate, and
still absent from a default build.

## What a model approving an action can and cannot decide (0.42.0)

`ModelApprover` installs a model where a human would stand. This is the boundary
around it, and the boundary is the reason it is safe to offer at all.

**It only ever answers the grey tier.** An action the `Policy` denies never
reaches any approver, this one included, so the wall is exactly where it was: the
worst a model can do here is approve something the operator had already marked as
a question. It cannot widen the boundary, because a `ModelApprover`'s approval
never carries a `modified` request and never carries a remembered rule. Both are
things a Rust `Approver` may do and this one may not.

**The repository it is reading may be hostile.** A write's content reaches the
approving model's prompt, so a file in the workspace can address that model
directly, and the system prompt says in as many words that content is material
being acted on rather than an instruction. That is mitigation, not proof: treat a
model approver as a filter over the grey tier and not as a defence against a
workspace you do not trust. For a run that must never take a sensitive action,
`DenyAll` remains the honest posture and nothing about this release changes it.

**A verdict it cannot read is a defer.** A malformed answer, an answer with no
JSON object in it, and a provider that failed all park the question — the action
is persisted, the run stops, and a person answers it later. A machine standing in
for an absent human must never wave through what it did not understand.

**It may not answer for its own model.** A `ModelApprover` whose model is the
model making the call is refused before the first request is billed, in a flat
run and in a tree, and the evidence is zero calls to either provider.
`allow_self_approval(true)` is the way to say you meant it, and it is a knob on
the approver rather than a setting in a file so the exception is visible in the
caller's own code. Neither model can always be named: when the contract states no
routing model and the provider reports no `model_hint`, the run's model is the
provider's own default, which this crate cannot name and therefore cannot
compare.

**Nothing about the trace changes.** An approval answered by a model emits the
same `ApprovalRequested` and `ApprovalDecided` events, with the same decision
strings, that an approval answered by a human emits.

## What a `before_tool` hook stops, and where it may be written (0.42.0)

**It runs before the call, on the loop's own thread.** A `[[hook]]` with
`at = "before_tool"` is spawned with `{at, run_id, step, depth, tool, arguments}`
on its stdin, in the discovery root, before the tool executes — which is the whole
difference from an event hook, whose `on_failure = "cancel"` lands at the next
step boundary and therefore after the call it objected to. A non-zero exit refuses
that one call: nothing runs, the hook's first line of stdout (up to 4096
characters) becomes the reason the model reads, and the run adapts. `on_failure`
chooses otherwise — `cancel` ends the run at the next step boundary, `continue`
lets the call through — and `refuse` is the default for a lifecycle hook.

**It is refused in any file inside the workspace, exactly as any hook is**, inside
a `[profile]` too. A hook runs an argv on this machine; `io.toml` is the file a
`git clone` delivers, and `io.local.toml` — refused since 0.74.0 — is a path in
the workspace root the run's own agent can write. One that can stop a tool is
strictly more dangerous than one that appends a log line. Write it in the
user-scope file, which is the one file no workspace can reach.

**It costs a process spawn per matching call.** The `tools` filter is how an
operator pays for the check they wanted rather than for one per read, and an
unfiltered `before_tool` hook over a read-heavy completion spawns once per call.
The gate is serial, so a slow hook slows the step it is in; the read work it
approves still runs concurrently, and 0.41.0's guarantees above are untouched.

**Nothing happens until an application installs it.**
`TaskContract::with_tool_hooks` takes the same `Hooks` a caller installs as an
`Observer`; a configuration file alone changes nothing, which is the rule every
projection of `io.toml` obeys.

**A refusal is a `Refused` event**, with the hook's program where a rule's
pattern would be and `io.toml hook` where a layer would be. There is no new event
kind, and there is no `after_tool`: that needs a tool-result shape this crate does
not have yet.

## What a reviewer is shown of a change (0.42.0)

**`ModelReviewer` is shown the change; a reviewer that overrides nothing is shown
the outcome.** `Reviewer::review_change` is defaulted to forwarding the same
`ReviewRequest` a reviewer has always received, so no existing implementation
needs an edit — and the price of that is stated rather than hidden: such a
reviewer keeps judging the files as they stand, which cannot show a deletion.
Overriding `review_change` is how a reviewer sees what was removed.

**The "before" is the state before the run first touched the file**, read from the
restore point the store has kept since 0.28.0 — not the state before the last
edit. A file the run created carries no before at all. A file whose previous
contents were over the store's snapshot cap or were not text says so in `unkept`
rather than appearing as empty, because a reviewer told a rewritten file was empty
would read every line as an addition. A file the run never wrote is not in the
list.

**It is not a diff, and nothing new is stored for it.** Both texts are what the
store already holds. Hunks are no longer future work — 0.51.0 computes one per
write and keeps it in `edits.hunk` — but a `ChangeReview` still carries the two
texts rather than the hunk. A file the
run wrote and something then removed is omitted rather than reported as empty.

## What compaction folds, what it costs, and what it never loses (0.43.0)

**When it happens.** A fold is attempted before each step's request is assembled,
and happens when the observation ledger's estimated tokens cross
`Compaction::at_share` of that turn's own effective context budget — or when the
provider has just refused the request as too large, in which case the threshold is
not consulted because the vendor has stated what it was guessing at. Under the
threshold, nothing happens and no provider is called.

**What it costs.** One ordinary completion, through the same path as every other:
one `provider_calls` row for the step it happened in, retried by the same
`RetryPolicy`, counted in `Store::spent_tokens`, and inside the run's token budget.
There is no separate summarising provider and no second model to configure — the
run's own provider writes it, because a summariser describes the run's work rather
than judging it.

**What it never loses.** Every folded observation stays in `ledger_observations`
and is still returned by `Store::observations` and rendered by
`Session::transcript`. What a fold changes is what the *next request* carries, not
what the trace holds. The paragraph itself is a `summaries` row keyed on how many
entries it stands in for, and `restore_ledger` replays those rows — so a resumed,
branched or replayed run reconstructs the same fold instead of buying it again.

**What it is not.** It is not a guarantee that the summary is right. A model
writing about a model's work can miss the decision that mattered, and a paragraph
is less visibly incomplete than a stub. `keep_recent` keeps the newest
observations whole beside it, and the transcript is how a person checks. The
summariser is given no tools, its output is one observation among others rather
than an instruction, and the `Policy` is untouched and remains the wall: a summary
cannot widen a boundary, approve an act, or call anything — which is the whole
answer to a folded observation containing text from a repository the agent did not
author.

**Off is a setting, not an absence.** `Compaction { at_share: 1.0, .. }` never
folds, and that includes the overflow recovery: a caller who turned folding off
asked for 0.42.0's behaviour, and an over-window request being terminal is part of
what they asked for.

## A fold the caller asked for (0.68.0)

**There are three triggers and one machinery.** `Compaction` decides the folds
nobody asked for. `TaskContract::fold_now` is how somebody asks before the turn
starts: set it on a turn and that turn's first step folds before it assembles its
first request, whatever the threshold says. `Steer::fold` (0.69.0) is how somebody
asks while the turn is already running. Everything else is identical — the same
summariser, the same cached `summaries` row, the same `EventKind::Compacted`, the
same "what it never loses". An interface should not have to care which trigger
fired, which is why there is no second event.

**It lands before the turn's first request, and that is the whole promise.** The
alternative available before 0.68.0 was to lower `at_share` for one turn and wait
for the ledger to cross it, which mutates the caller's own setting to fake a
request and can say neither when it will land nor whether it will. A request that
is honoured at a stated point is a promise an interface can pass on to an
operator.

**Three boundaries on the contract flag, each deliberate.** The request is consumed **once**, at the
turn's first step — a contract reused for every turn would otherwise fold every
turn, and a flag on a contract is a property of the turn rather than of the
moment. It does **not** override an off setting, for the reason stated directly
above: one trigger reversing "off" would make the word mean two things. And it
does **not** reach a spawned child — a contract reaches the whole tree, but a
child's ledger is its own work with no conversation seeded into it, so folding it
would fold something the operator never saw. That is the same boundary steering
draws.

**A steered fold lands at the next step boundary, and folds before that step's own
request (0.69.0).** `Steer::fold` is the third way in, beside `say` and
`interrupt`, and it promises a point rather than a moment: the step that drains the
inbox raises the loop's standing fold request, and that request is read by the
compaction attempt of the *same* step iteration, a few lines further down. So a
fold asked for mid-step lands on the request built after that boundary, not the one
after that. The request is recorded as a `ContextEvent::steered` line at the step
that read it, so a trace says when the operator asked as well as when the fold
happened.

**Four boundaries on the steered fold, each deliberate.** It is **not immediate** —
like a message and an interrupt it lands at the next step boundary, because a tool
call in flight is not a safe place to change the conversation out from under. It
does **not** override an off setting, for the reason stated above: `Compaction {
at_share: 1.0, .. }` never folds, this trigger included. It does **not** reach a
spawned child, which is the boundary `fold_now` and steering both already draw. And
it **loses to an interrupt drained at the same boundary**: the interrupt is
answered first, the turn ends as `RunOutcome::Cancelled`, and no summariser call is
spent on a turn nobody is going to read.

**A fifth boundary, and it is the one an operator meets first.** A request made
when there is nothing to fold is spent and nothing happens: a fold keeps
`keep_recent` observations whole and may only replace ones the store already holds,
so a conversation shorter than that has no prefix to stand in for. The request is
recorded either way, which is what makes it visible after the fact — but an
interface that reports "compacted" because it sent one is reporting something this
contract does not promise. The `Compacted` event is the fact; the request is not.

One request is one fold, and the unit is the boundary rather than the call: two
asks that reach the same boundary are one fold, because the second would summarise
a ledger the first has just replaced with a paragraph. Two asks separated by a
boundary are two folds. Asking once does not put the turn into a mode where every
step folds.

**A fold outlives the turn that made it (0.69.0).** `summaries` is keyed on
`run_id` and every session turn is its own run, so a fold used to buy one turn of
relief: the next turn's seed rebuilt the conversation from the turn rows and put
back whatever the fold had just replaced, on every trigger. `Session::seed` now
finds the newest turn on the path whose run folded and seeds that turn's newest
summary paragraph in place of the conversation entries the fold consumed — the
first summary row stands in for `folded` entries and each later one for `folded - 1`,
because a later fold's prefix begins with the paragraph the earlier one wrote, and
the total is capped at what was seeded before that turn. Newest wins and the walk
stops there, since an earlier paragraph was part of what a later fold summarised.

**It does not replace every earlier turn**, and the transcript does not move at
all. A fold keeps the newest `keep_recent` entries whole, so the seed keeps that
tail whole too, and every turn after the folding one is seeded as it always was.
`Session::transcript` still renders every prompt and reply, and the folded
observations are still in `ledger_observations` — what gets shorter is the seed the
model reads, and nothing else. The paragraph is seeded under the public
`SEED_SUMMARY` target with the same `[earlier work, summarised]` framing an in-turn
fold writes, so a folded span reads identically whether it was folded three steps
ago or three turns ago, and it reaches the model as `Piece::Prose` rather than as
either party's words. Nothing is stored that was not stored before: it is a join
over `session_turns.run_id` and `summaries.run_id`, the join `Session::transcript`
already makes, with no schema change and `CHECKPOINT_FORMAT` still at 7. A session
that never folded pays one indexed lookup per turn on its path and gets the seed it
had.

**The conversation is made durable before the first step, and it has to be.** A
fold may only replace entries the store already holds. Until 0.68.0 the seeded
conversation sat above that watermark for the whole of step one — it became
durable at the *end* of the step, while a fold is attempted at the start of it —
so a session turn seeded with a long conversation could not fold at its first step
on any trigger, the overflow recovery included. The turn most likely to exceed the
window was the one the recovery could not help. The seed is now written before the
loop, which is not a relaxation of the rule that an observation must not outlive a
step that never committed: the seed belongs to no step of the run.

## What a context overflow does now (0.43.0)

**It is classified from the vendor's own words.** `ProviderErrorKind::from_response`
answers `ContextOverflow` for a 400 or 413 whose message carries one of a short
list of known wordings, and `from_status` for everything else. The list is
deliberately conservative and is not exhaustive: a wording it does not know costs
exactly what an overflow cost on 0.42.0, while a false positive would make the loop
re-send a request the server had already read and refused.

**It is not retryable, and the recovery is not a retry.** `is_retryable()` is
`false`, because re-sending the same bytes cannot make them fit. What answers it is
a different request: the loop folds the ledger and asks once more. That happens at
most once per step; a second overflow escalates with both attempts in the trace.

## What a transcript is (0.43.0)

`Session::transcript` is a **read**. It calls no provider, writes no row, and can
be called as often as you like on a finished session for the same cost.

It renders the **whole session tree**, not the path the model sees. A
`Session::branch_from` leaves earlier turns off that path, and those are exactly
the turns no other surface will show you; `TranscriptTurn::on_path` marks which
ones the model can still see. `Transcript::to_markdown` returns a `String` — the
crate does not choose your file, its encoding or its pagination.

## What `Session::attach` attaches, and for how long (0.43.0)

Staged images ride **the next turn only**, and the staging is cleared once that
turn has been driven whatever its outcome. A screenshot is about the thing being
said now; re-sending it every later turn would bill for it every turn.

`TaskContract::with_images` is unchanged and still means what it always did — for
the whole run — so a `turn_bounded` whose contract carries images and whose session
has staged one sends both. A provider whose `accepts_images` is false refuses the
turn before anything is sent, which is 0.42.0's refusal reached through a new door.
`media` feature only; without it, nothing here exists.

## What the system prompt says, and who may change it (0.45.0)

The system prompt is composed once per run, before the first request, from parts
in a fixed order:

1. the description of the agent and its tools — the crate's, or the caller's when
   `TaskContract::prompt` is `SystemPrompt::Replace`;
2. the extra-tool catalogue and the skills catalogue, as since 0.20.0;
3. the planning directive, when the plan gate is on;
4. the caller's own text, when `TaskContract::prompt` is `SystemPrompt::Append`;
5. the repository's own guidance, when `[instructions]` discovered any;
6. the boundary section, when the run enforces a policy or is contained — which since 0.46.0 is every run that has not asked for `ExecMode::FullAccess`;
7. **the crate's ending sentence, last, always** — in the workspace loop. The
   single-file loop composes without one, because there is no turn to classify
   there.

**Nothing a caller or a repository supplies is emitted after step 7.** The ending
of a classifying turn's opening is the sentence that lets a turn answer instead of
working, and the guarantee it produces — a `TurnKind::Reply` stages no step, no
gate, no checkpoint, no snapshot and no approval (0.37.0) — is one this document
makes to a reader who never sees the embedder's prompt. A composable prompt that
could contradict its own runtime's contract would not be a feature. What this
crate asserts is the composition: in the workspace loop the sentence is present,
byte-exact, and last, under every `SystemPrompt` including `Replace("")`, and
under any text a repository carries. **What a model then does with a prompt is not a claim this
crate can make.**

**The ending moved in 0.45.0.** Until 0.44.0 it sat inside the base description,
which put the tool and skill catalogues after it. Every sentence a 0.44.0 prompt
carried is still carried, in the same words; one of them is in a different place.

`SystemPrompt::Replace` replaces the description and nothing else — the
catalogues, the guidance, the boundary and the ending are still composed around
it. There is no preset catalogue and there will not be one: a preset shipped by a
library is an opinion about model behaviour the library cannot test and cannot
withdraw.

**A preset is a manner appended to a framing, never a replacement for one
(0.60.3).** Which world the agent is in — a task to meet, a conversation to answer,
a tree it may fan out across — is chosen by the loop and by the classification.
How the work is done and reported is chosen by the embedder. They are separate
axes and they compose, so `SystemPrompt::Preset` names no tools and describes no
agent: the framing beside it already did both, once. Up to 0.60.2 a preset carried
a whole description and stood where `Replace`'s text stands, which meant selecting
`Concise` on a session turn silently reframed a greeting as work.

**Why the framings differ at all.** The one-shot entry points are task-framed
because their caller declared work in code — a `TaskContract` with a goal and a
`Verification` is a statement that there is something to meet. The session entry
points are conversation-framed on the first completion of a turn carrying
`Verification::None`, because their operator declared nothing: what they typed may
be work and may be a question, and the model is the thing best placed to tell them
apart. Every block composed for that turn is held to the same standard as a result
— **it must be true of the turn being taken**. That is one rule with three
consequences already paid for: the user block stopped telling a classifying turn to
call a tool (0.48.0), the system block stopped telling it that it had a
specification (0.49.0), and the plan directive stopped ordering it to plan before it
was permitted to answer (0.60.3).

## What the boundary section tells the agent, and what it leaves out (0.45.0)

When a run enforces a policy, the system block carries one line per act — read,
write, execute, network — naming that tier's default and the patterns the layers
rule on, grouped by what `Policy::explain` returns for each and attributed, on a
refusal, to the layer that produced it. It is the same vocabulary a `Refused`
event carries, so the prompt and the refusal name the same thing.

- **A permissive policy renders nothing *when the run is not contained*.** Since
  0.46.0 a run is contained unless it asked for `ExecMode::FullAccess`, so a
  permissive contained run still gets the one line naming its mode and backend.
  Single-file mode renders no section at all, because it enforces no policy. A
  section describing an enforcement that does not happen would be worse than
  silence.
- **At most 24 patterns per act are named**, and the line says how many it did not
  name. The unnamed rules are enforced exactly the same.
- **`Effect::Ask` is rendered as itself** — allowed once a human or an approver
  says yes. Neither "allowed" nor "refused" is true of it, and both mislead.
- **A rule an approver remembers mid-run is not reflected**, because the prompt is
  composed once. The remembered rule *widens* what is permitted, so the section
  stays conservative rather than wrong. A plan gate is reflected: the narrowed
  policy is what the planning prompt describes, and the loop already switches
  prompts when the phase ends. **Both blocks of a gated turn describe the same
  narrowed policy (0.60.3)** — the classifying opening was handed the post-plan
  boundary until then, so a turn under the gate read one thing while `plan_lock`
  refused another.
- **The section is not the boundary.** The `Policy` is, enforced in the tool and
  verification layers before any call runs. Telling the agent is an optimisation
  against paying a step per refusal, and no prompt text widens anything.

One further line names the run's `ExecMode` and the backend `sandbox::select`
**actually returned** on this host — not the one that was asked for. Where that is
the portable floor or a Windows Job Object, the line says the resource caps apply
and filesystem and outbound-network confinement do not — the truth an agent would
otherwise have to discover (0.40.0). A stock Ubuntu 24.04 is no longer an example
of it: that host takes the Landlock rung, which needs no namespace. A run under
`ExecMode::FullAccess` gets the line too, saying it is not contained: since 0.46.0
that is a decision the caller made, and an agent that may write anywhere should
know it rather than infer it from a write that happened to succeed.

## What a repository's own guidance is, and where it now rides (0.45.0)

`[instructions]` is discovered **by default** from 0.45.0: `Config::discover`
looks for `AGENTS.md` whether or not an `[instructions]` table is present, and
`[instructions] files = []` is how a project says no. Nothing became implicit that
was not already — the caller still chooses to read the configuration, once, before
the run.

What it finds lands in `TaskContract::instructions` and rides in the **system
block**. From 0.27.0 to 0.44.0 it landed in `TaskContract::constraints` and rode
in the user turn on every step. The two are different things: a constraint is a
rule the goal is checked against, and this is guidance the agent reads.

**It is untrusted text, and 0.45.0 moved it somewhere more authoritative.** A
repository is not the operator. What bounds it is structural rather than
advisory: the text is delimited, framed to the model as the repository's own
guidance that grants nothing and does not change how the turn ends, and emitted
**before** both the boundary section and the crate's ending, so it cannot be the
last word. It grants nothing because the policy is enforced before any call runs.
Treat a workspace whose instructions file you have not read the way you would
treat its code.

## What a prompt family changes (0.45.0)

`Provider::prompt_family` classifies the model answering — from the provider for
the two built-in vendors, from the model slug for `OpenRouter` and `Compatible`,
and `PromptFamily::Generic` for everything this crate does not recognise, which
includes every `Compatible` endpoint it does not control.

**It decides delimiters and nothing else.** Every family is given the same
sections, in the same order, with the same words, ending with the same sentence;
the crate asserts that by stripping the delimiters and comparing the rest byte for
byte. Today the Anthropic family wraps sections in tagged blocks, as that vendor's
own prompting guidance asks for, and the others read the same text plainly. No
claim is made anywhere that one family's wording performs better than another's.

## How a child comes back, and what a parent gives up to stop waiting (0.50.0)

A `spawn_agent` call takes two optional arguments beside the ones it always took.
`wait` defaults to `true`; `background_after_secs` names a wall clock. Naming
neither is the spawn every tree written before this release made: the parent waits,
the results are folded into the step that asked for them in the order the model
asked, and the trace is reproducible.

`"wait": false` means the parent takes its next step immediately and the child's
report reaches it later. `background_after_secs` means the parent waits, and stops
waiting when the clock runs out. **The child is not cancelled in either case.**
That is the difference between this and a timeout: dropping the child's future
would cancel it mid-step and leave its run row `running` for ever, which nothing
can tell apart from a crashed process. The two combined — a child you are not
waiting for cannot cross a clock — is a contradiction, and it is answered with a
typed observation naming both arguments, before any child is registered, admitted
or written.

**What a parent gives up.** For the calls that use them, the trace is no longer
step-for-step reproducible: which step a report lands on depends on how long the
child took. Two guarantees survive. Reports fold in the order the children were
spawned, not the order they finished, so two children racing leave the same
ledger. And a run that detaches nothing is byte-identical to one on 0.49.0.
`TaskContract::without_detached_spawns` refuses detachment outright for an
embedder who wants the old guarantee unconditionally; the refusal is stated in the
parent's own ledger, never silent.

**What a parent gets back.** Through 0.49.0 a finished child was composed as
`[child 7 "goal" -> Success { steps: 4 }]` — a `Debug`-printed outcome and a step
count, because `RunOutcome::Success` carries no text. A parent that fanned out to
investigate four subsystems learned that four runs succeeded and none of what they
found, and the only way a finding could travel was a file the parent then read.
A child now reports what it concluded: the text of its last completion, beside its
steps and its tokens, bounded by the same per-observation cap as everything else.
An agent's own words are durable for the first time, as one `agent_events` row per
step that said something; a child that never spoke says so rather than reporting
an empty answer.

**Concurrency, exactly.** A detached child is a future on its parent's own task,
polled while the parent waits for its own completion — not a spawned task. It
cannot be one: `rusqlite::Connection` is `Send` and not `Sync`, so the store
cannot cross a task boundary, which is the same constraint that decided 0.41.0's
read batch. The consequence is worth stating: a detached child makes progress
while the parent is waiting on a provider, and it does not make progress while the
parent is inside a synchronous stretch of its own.

**Caps are unchanged.** A detached child holds its tier's slot until it finishes,
so `Containment::max_concurrent_agents` throttles exactly as it did and
`EventKind::Fleet`'s `working` counts it. A tree drained of its children reports
zero.

**Across a restart.** A detached child's step commits, so a resume starts after it
and the spawn call is never replayed. The parent therefore takes its children back
before its first step, resuming each from its own checkpoint through the ordinary
spawn path — the same run row, not a second child. The whole spawn call is
recorded to make that possible, because the `spawns` row holds five of the nine
arguments and a rebuild from those five would resume a child under a wider policy
than it was given.

**The one window that is not covered.** The drain runs after the loop has recorded
the parent's ending, so a process that dies *during* the drain leaves the parent
`completed` and a child `running`. Resuming the parent then reports the terminal
outcome and re-adopts nothing. The child is not lost — it is resumable by its own
run id — but it is not picked up automatically, and closing that would mean
deferring the run's own ending until its children are finished.

## What a change is kept as, and what an undo can reach (0.51.0)

Every `write_file`, `edit_file` and `patch_file` records the change as a unified
diff of the **whole file**, in `edits.hunk`, beside the two line counts it has
recorded since 0.18.0. `Store::edits` hands it back and `Store::patch` renders a
run's whole change as a step-ordered patch series — one `--- a/path` / `+++ b/path`
header pair per edit, in the order the run made them.

**A series, not one diff.** Two edits to the same file take their line numbers
from that file as it stood at each of them, so the second hunk is only correct
once the first has been applied. It applies as a sequence, the way a multi-commit
diff does. Joining the hunks under one pair of headers would look like a patch and
would not apply.

**The counts did not change, and that is deliberate.** An `edit_file`'s
`lines_added` and `lines_removed` still measure the fragment it replaced, not the
file — which is what they have measured since 0.18.0. The two answers genuinely
differ when a replacement does not begin and end on a line boundary: deleting a
substring *inside* a line is nothing added and one line removed measured over the
fragment, and one and one measured over the file. Computing both from the same
texts would have been tidier and would have silently renumbered every trace ever
recorded.

**The hunk is one hunk, and not a minimal diff.** It is produced by trimming the
common head and the common tail — the computation the counts already perform. For
an `edit_file`, which is one contiguous replacement by construction, that *is* the
minimal diff. For a `write_file` that rewrote two distant regions it is one hunk
spanning both: a valid unified diff that reverse-applies exactly, and not the
shortest one. A minimal diff is a dependency or several hundred lines of
algorithm, and it buys shorter output rather than a capability.

**Three reasons a hunk is absent, and none of them is "nothing happened".** The
row was written before 0.51.0; the file's previous contents were not kept, so
there was nothing to diff against — over the 1 MiB snapshot cap, or not UTF-8,
and the reason is on that path's `snapshots` row; or the rendered diff would
itself have exceeded that cap. An absent hunk is reported as absent everywhere it
is read. It is never treated as an empty patch, because an empty patch reverts
cleanly and reverts nothing.

**`patch_file` is all or nothing.** Every hunk is matched against the file as it
stands, at its own recorded position against the *original*, before anything is
written. A patch whose third hunk does not fit leaves the file byte-identical,
writes no `edits` row and no restore point, and says which hunk and what it
expected. The match is exact: a fuzzy match is how a file gets quietly corrupted.
One path per call, and it cannot create a file — a patch is anchored to text that
already exists, and creating is `write_file`'s job.

**`check` is the project's own checker as a question.** The same ecosystem
type-check that has run automatically after every successful write since 0.20.0,
callable before one. It takes no arguments, so what runs is the detection's
answer and not the model's. It reports and never blocks: a failing check does not
undo an edit.

**Both checkers are `Act::Exec`-gated, and they differ only in what they do with
a refusal (0.74.0).** Each is checked on the program *and* on the whole argv,
exactly as `exec` is — `deny_exec("cargo")` and `deny_exec("cargo check*")` both
reach either — and each runs what it does run inside the run's own containment.
Until 0.74.0 the automatic one was ungated and uncontained on the argument that it
was the crate's own reflex after a write the policy had already allowed; that
argument was wrong, because `cargo check` compiles and compiling runs the
workspace's `build.rs`, so a run that wrote a `Cargo.toml` and then wrote a build
script reached host execution through two calls an approver saw as writes.

The tool reports a refusal and the reflex is silent about one, and only
`Effect::Allow` runs the reflex — an `Effect::Ask` is a skip rather than a
question, because this path has no approver to route one to and a write that
paused on an approval prompt would be the very thing the paragraph above forbids:
a successful write turned into something else by what happens after it.
**`Policy::default()` sets `exec: Ask`, so under it the post-edit check does not
run at all.** A run that wants it names the checker with `allow_exec`; the write
itself is unaffected and still cannot fail, and the diagnostics are simply absent.
The other difference is older: when there is no checker, the tool says so and the
automatic path stays silent. Silence costs nothing when nobody asked and reads as
"your project is clean" when somebody did.

**`rewind_step` undoes one step, and you walk backwards.** `rewind` puts a file
back to before the run's **first** write to it and `rewind_run` does that for a
whole run; neither can undo step eighteen of twenty. `rewind_step` reverse-applies
that step's stored hunks, one entry per path it wrote.

Reverse-application is order-sensitive and the API says so rather than hiding it.
A step reverted while a later step's change still sits on top of it finds context
that has moved, and the answer is `Reverted::Stale` and an **untouched** file. To
walk a run back, revert the newest step first and descend. `Reverted::NoHunk` is
the other way nothing happens, and it is a different fact: there is nothing to
undo with, and reverting the later steps first will not change that — `rewind` is
what puts such a file back.

Writing goes through `Workspace::write_file`, so the same path policy the edit
obeyed governs the undo. Nothing in the trace is deleted, and the revert is itself
written down: `Store::rewinds` reports it with `undid_step` set, which is what
distinguishes it from a whole-run rewind, and one `EventKind::Reverted` is emitted
carrying the count of paths actually put back.

**What `rewind_step` does not do.** It does not replace `rewind`: a snapshot is a
stronger restore than a chain of reverse-applies, and a run whose hunks are absent
must still be fully undoable. It does not touch memory or the queue — those are
`rewind_run`'s, and a step did not create them. And it is an operator-facing API,
not a tool: no model can call it.

## Limits that hold today

Stated here rather than discovered later. Each is real, each is known, and each
is open at the release named beside it. The list is maintained rather than
stamped — it carried an "as of 0.35.0" for twenty-five releases while entries
dated 0.49.0, 0.56.0, 0.57.0 and 0.60.0 were added below it.

**The concurrency cap is per tier, not per tree (0.32.0).**
`Containment::max_concurrent_agents` bounds how many agents work at once *at one
nesting level*. Each tier holds its own set of slots, which is what makes the cap
safe rather than what makes it convenient: a parent holds a slot at its own tier
while it waits for children at the tier below, so the wait graph runs strictly
downward and cannot contain a cycle. One tree-global pool would deadlock the first
time the agent holding the last slot spawned a child, because only that child
could free it. The consequence, plainly: a tree of depth *d* can hold up to
`max_concurrent_agents * d` agents working at once, not `max_concurrent_agents`.
Bound the whole tree with `max_total_agents`, which refuses, and with
`max_total_tokens`, which halts.

**The queue is FIFO and has no other policy (0.32.0).** A child queued behind
`max_concurrent_agents` starts when a slot frees, in the order it queued. There is
no priority, no reordering, no way for an application to promote or cancel a
waiting child, and no fairness rule between the parents feeding one tier. A queue
with a policy is a scheduler; this is not one.

**A queued child is a row, not an agent (0.32.0).** It has no run id, no steps and
no spend, which is what "a queued child that never started is not charged" means —
and also what it costs: `Store::children` will not list it, `Store::agent_events`
has nothing to say about it, and it emits no events of its own until it is
admitted. `Store::queued_agents` is the only place it appears, by goal.

**The plan gate is a boundary on the agent, not on the caller (0.31.0).** The
`plan-gate` policy layer holds for the duration of the run's own loop. The
embedding program holding the `Store` and the workspace is not the run and is not
refused anything — the same distinction 0.30.0 drew for a pinned memory entry.

**A cancelled plan is final, and a pending one is not resumable twice (0.31.0).**
`Store::decide_plan` refuses a second verdict with `Error::Resume`, so two
processes racing to approve the same plan means one of them hears about it. Two
processes each resuming the same run after a single approval is guarded as well
since 0.62.0, by the run's lease rather than by the plan: the second resume is
refused with `Error::Conflict` before it drives a step. `Store::check_resumable`
still answers only whether the checkpoint can be continued at all, never by whom.

**Reasoning text is live-only (0.31.0).** `EventKind::Reasoning` is the only
place the thinking appears. It is not written to the trace, so a run whose
`Observer` was not attached — or a run being read after the fact — can see what
the thinking *cost* through `Usage::reasoning_tokens` and cannot see what it
*said*. Persisting it would grow the trace by the most verbose thing a model
produces, for a value nobody has asked for yet.

**An aggregate is one indexed query, not a constant-time one (0.30.0).**
`runs_by_outcome`, `runs_by_day`, `gate_failures_by_phase`, `first_try` and
`recovery` each reach their rows through an index rather than scanning a table,
which is asserted on the query plan rather than on a clock. What they are *not*
is independent of how much history the store holds: counting every row is what
the answer is, so the cost is linear in rows however it is served. Measured on a
debug build over an in-memory store, at 20,000 finished runs: 1.8ms, 2.5ms,
7.6ms and 1.2ms — four figures for the five queries named above, which is how
this sentence has read since 0.30.0. Which query the missing figure belongs to
was never recorded, so no pairing is claimed here; the spread is roughly 90 to
380 nanoseconds per run. A caller
refreshing a panel every second over a very large trace should cache the answer;
this crate does not cache it for them.

**Eviction is ordered by evidence, not by the write clock (0.56.0).** At a cap
the store drops the entry with the fewest **distinct runs** behind it in
`memory_recalls`, then the one least recently carried, then — for everything with
no recalls at all — the oldest, which is 0.10.0's order kept as the tie-break so
the unproven cohort behaves exactly as it did. The count is of runs and not of
rows: a recall row is written once per carried key per *step*, so rows measure
steps elapsed since the write and would make the order monotone in age again. A
recall means the entry was **carried into a prompt**, which is the strongest
signal this crate can observe; nothing here claims the model read it.

**Which notes a turn carries is decided by the turn, and the order they are
printed in is not (0.57.0).** When the store does not fit the memory block's
share, the entries that survive the fit are ranked by three terms: how many
normalised words the entry's key and value share with what this turn is about —
the words of the run's goal, plus every path or subject a tool has already named
in this run — then how many **distinct runs** have carried the entry, then the
order the store returned, which is `(created_at, key)`. An entry with no signal
and no evidence therefore keeps exactly the position it had before this release,
so a turn about nothing the store knows selects as 0.56.0 did.

The block is nonetheless always **printed** in the store's own order, never in
rank order, and that is a guarantee rather than an implementation detail: the
memory block is a byte-prefix of the user turn, and the second cache breakpoint
(0.44.0) is withheld unless that prefix repeats byte-identically. A store that
fits its share therefore assembles a byte-identical prompt however the turn
moves, and only the over-cap regime — the one raising a cap creates — sees any
change at all. Normalisation is: lowercase, split on anything that is not
alphanumeric, drop anything shorter than three characters. Nothing is scored by
a model and nothing leaves the process, so a replayed run selects what the run
it replays selected.

**A note that restates one already held is reported where it is written
(0.57.0).** `remember` writes by key, so the same fact learned twice under two
names leaves two entries that disagree, both carried, and the model acting on
whichever it read last. On a write whose text closely overlaps an entry already
stored **in the same scope under a different key**, the tool result names that
key and quotes what it holds, bounded. The write still lands: this is a report
and never a refusal, because a harness that declined a write because two strings
overlapped would be guessing at intent and one that merged them would be writing
a fact nobody stated. Rewriting the same key is not reported — that is the
replacement writing by key has meant since 0.10.0 — and a workspace note
restating a **global** one is not reported either, because that is the override
the second scope exists for. The comparison is a normalised token overlap
computed in this process; `Store::memory_similar` is the same answer for a
caller.

**The three caps are the operator's (0.56.0).** `MEMORY_MAX_ENTRIES` (64),
`MEMORY_MAX_CHARS` (16,000) and `MEMORY_MAX_ENTRY_CHARS` (an eighth of that)
remain the defaults and are now movable, through `[memory]` in `io.toml` or
`TaskContract::with_memory_limits`. All three narrow at project scope and never
widen. Raising one is coupled to what a prompt can carry: the memory block gets a
quarter of a turn's effective tokens, and the defaults were chosen so a whole
store fits inside that share — past it, selection begins deciding what the model
sees, which is only safe because selection is now by evidence. Since 0.57.0 that
selection is by what the turn is about, so raising a cap costs time rather than
relevance: ranking is linear in entries and measured in `docs/MEASUREMENTS.md`.

**Memory has two scopes, and the specific one wins (0.56.0).** A workspace's
canonical path, and `GLOBAL_MEMORY_WORKSPACE` above it, which every run over
every workspace recalls. `remember` and `forget` take `scope`, defaulting to the
workspace, so a run may promote a fact it believes is universal and withdraw it
the same way. Where a key exists in both, **the workspace's entry is carried and
the global one is not rendered at all**. Each scope holds its own caps, its own
pins and its own eviction, so a run recalling both may carry up to twice one
scope's characters inside a block ceiling that has not grown.

**A run can unlearn (0.56.0).** `forget` removes one entry. A pinned entry is
refused, exactly as a write to one is; a key that was never there is reported as
absent rather than as a removal. The restore point is taken before the entry
goes, so `rewind_run` puts it back — and a forget takes the key's recall rows
with it where an eviction leaves them, because the run said the fact was wrong
while a cap said only that the store was full.

**A pin binds a run, not a person, and not another process's caller (0.30.0).**
`MemoryEntry::pinned` stops the *agent* overwriting an entry through the
`remember` tool, stops it withdrawing one through `forget`, and stops the caps
evicting it. It is not access control:
`Store::memory_write` from the embedding program is refused the same way, but
`Store::memory_pin(.., false)`, `memory_delete` and `memory_clear` are the
caller's and are not refused — the pin is a statement about what a run may
change, and the program holding the store is not the run. A refused write is
recorded as a `memory_refused` row and handed back to the model, so the failure
mode the flag exists to prevent — an agent proceeding as though its correction
landed — is visible in the trace and to the agent itself.

**Gate failures group by phase, not by criterion (0.30.0).** The trace records
which *phase* of the gate failed — `compile`, `criterion-compile`, `test-run` —
and not the text of the criterion that failed, so `gate_failures_by_phase`
reports phases. Counted per failure, not per run: a run that failed the same
phase three times contributes three. "How many runs failed this phase" is a
different question and is not answered rather than answered ambiguously.

**`Recovery` has no escalation count (0.30.0).** Nothing in the trace records an
escalation as an event, and an escalation is in any case the opposite of a
rescue — it is the run handing the problem back to the caller. Counting it beside
fallbacks and replans would read as a success. When something records it, it gets
its own row rather than a place in the total.

**A `MemoryEntry` is a returned row, and it gained two fields in 0.30.0.**
`kind` and `pinned` are additive for every caller that reads one, which is what
this crate does with the type — it is returned by `memory_get` and `memory_list`
and taken by nothing. A caller who nonetheless *constructs* one in a struct
literal has to name the two new fields. Stated here rather than left to be
found; the same warning as the two provider structs below, at a much smaller
blast radius.

**`CompletionRequest` and `CompletionResponse` are not `#[non_exhaustive]`, and
adding a field to either is a break (0.24.0).** `EventKind`, `Backend` and `Cap`
were marked `#[non_exhaustive]` in 0.24.0, so a variant added to any of them is
no longer a break. These two structs were considered for the same treatment and
deliberately left out. The attribute forbids struct-literal construction —
including `..Default::default()` — from outside this crate, and
`Provider::complete` must *return* a `CompletionResponse`, so marking it would
leave the crate's primary extension point constructible only through
`default()`-then-assign or through a builder API that does not exist. That is a
permanent cost on everyone who implements `Provider`, paid to avoid a break
whose fix is mechanical.

So the position is: **these two structs may gain fields in a later minor, and
that will break struct literals of them.** It is written here rather than left to
be discovered, and 0.22.0 is the precedent — it added three fields and broke
exactly that. If you construct either in a mock provider or a test, prefer
`..Default::default()` so that a new field costs you nothing.

**0.49.0 is that minor, and it added two: `messages` and `cache_through`.**

## What a message is, and what `user` is still for (0.49.0)

`CompletionRequest::messages` is the conversation the request carries, in order:
a user turn, an assistant turn holding the calls that step made, and one batch of
results answering it. Each built-in wire maps it onto that vendor's own block
types one to one. Empty — the default, and every caller before 0.49.0 — means
every built-in wire sends the body it sent in 0.48.0, byte for byte.

Before this release a request could not express a conversation at all. It held
one `system` string and one `user` string, both wires emitted a single
`role: "user"` message, and a step's results were re-rendered as bracketed prose
inside the next one. So the crate parsed `tool_calls` off a response and then
discarded the protocol on the way back in, and the model read a third-person
account of its own past actions. That is off the distribution every model this
crate targets was post-trained on, and what it produces is not an error but
degraded instruction following: restating plans, narrating intent instead of
acting, losing that a tool has already been called.

**The ids are minted, not remembered.** A vendor correlates a `tool_use` id with
the `tool_result` id answering it *within one request*, and this crate rebuilds
the whole request on every step — so the id the vendor originally issued is never
needed again, `ToolCall` gained no field for it, and nothing is stored. They are
derived from the message's position and the call's, so the same transcript
assembles to the same bytes, which is what a cache prefix requires. Nine
characters and alphanumeric, the strictest id rule any vendor this crate plans to
reach states.

**A result names its call by position in the turn before it**, and a result that
names a call that turn did not make is dropped from the body rather than sent
with an invented id — a `tool_result` correlating with nothing is a 400 on at
least one vendor. A message that would carry no blocks at all is dropped whole,
for the same reason.

**`user` is derived and still carried.** The loop fills it with exactly the
string it filled before 0.49.0, so a `Provider` that reads it receives what it
always received and is honestly non-conversational; a built-in wire ignores it
whenever `messages` is non-empty. 0.49.0 described it as kept for one release;
eleven minors later it is still here and no removal is scheduled, and when one is
it will be a marked break carrying a migration note. The two
are not built separately: the assembler emits one sequence of pieces, the flat
string is those pieces concatenated, and the conversation is those same pieces
interleaved with the assistant turns — which is why they cannot drift into two
accounts of one run.

**Two cases were sent as prose. One is left, and it is deliberate.** A step whose
results do not line up with the calls it made falls back to prose, because
correlating them positionally would answer the wrong call and a `tool_result`
naming a call that turn did not make is a 400 on at least one vendor. That is not
a regression — it is exactly what 0.48.0 sent — and it is not silent: `messages`
is empty or short, and a reader can see it.

**The other was a resumed run, and 0.64.0 closed it.** Until then a resumed run
rebuilt its ledger from stored observation text, which holds every tool *result*
but no record of the calls they answer, so its pre-resume history stayed in the
first user message and only the steps after the resume point were role-tagged.
The missing half was never the results and never the ordinals — ordinals are
recomputed positionally from the restored ledger, elided entries included. It was
the assistant turn: what the model wrote and the calls it made, which the run loop
held in memory and lost with the process.

That turn is now durable. `step_turns(run_id, step, text, calls)` holds it — one
row per committed step, written by the same transaction that writes the step, with
the calls stored as this crate's own `ToolCall` JSON and `text` nullable so that
*wrote nothing* and *wrote the empty string* stay different facts. Both loops, flat
and tree, restore it beside the ledger. **Given the same committed state and the
same responses, a resumed run assembles the same `messages` an uninterrupted run
assembles** — same roles, same assistant turns, same result batches, nothing
normalised away.

**What this does not change.** Ids are still minted from position and still never
stored, so the same transcript still assembles to the same bytes and a cache prefix
still holds. Nothing is stored that a stored `Vec<Message>` would have been: the
flat `user` string and the conversation stay two views of one emission, and what a
turn carries stays a per-turn decision against that turn's context budget rather
than a snapshot frozen at the step that wrote it.

**A store written before 0.64.0 has no turns to restore, and a run resumed out of
one behaves exactly as it did then** — prose for what it cannot pair, role-tagged
from the resume point on. Absent is not empty: a step with no row falls back, and
a step whose row carries no calls and no text is a real turn that did nothing and
is sent as one.

**`rusqlite::Error` was in the public API. It was taken out in 0.63.0.** From
0.23.0 until then, `Error::State(#[from] rusqlite::Error)` carried the storage
dependency's own error type, which made `rusqlite` a *public* dependency of this
crate: every `rusqlite` major bump was a breaking change here, whether or not
anything about this crate's behaviour changed. That is exactly what happened in
0.23.0, whose entire content is a dependency move and which still had to be
published as a break.

It is wrapped now. `Error::Storage { kind, message }` carries a
[`StorageErrorKind`] this crate owns — `Busy`, `Constraint`, `Corrupt`, `Other`,
with only `Busy` retryable — and the message the storage layer produced, kept
whole. The classification happens once, in the `From<rusqlite::Error>`
conversion every `?` in the storage layer already went through, which is why the
change cost no call site.

The wrap was deliberately not done in 0.23.0: a migration release has to be
reviewable for exactly one property — that nothing behaves differently — and an
error-type redesign in the same diff destroys that.

**What this means if you are writing code today.** Match `Error::Storage` and
branch on the kind:

```rust,ignore
Err(Error::Storage { kind, message }) if kind.is_retryable() => retry(),
Err(Error::Storage { message, .. }) => report(message),
```

`Error::State` still exists, **deprecated since 0.63.0 and removed in 0.65.0**.
Nothing constructs it, so no failure this crate produces arrives as that variant
any more; code matching it compiles with a warning naming the replacement, for
the length of the cycle.

What the wrap did **not** buy: `libsqlite3-sys` declares `links = "sqlite3"`, so
a consumer's graph can still hold only one version of it. A `rusqlite` upgrade
here is no longer a *type-level* break; it is still a graph-level constraint, and
`tests/fixtures/links-consumer/` is what proves that wall is real.

**What a provider-executed search or fetch is, and is not (0.22.0).** A
`WebAccess` on the contract asks the *provider* to look something up inside the
completion:

- **The provider dials the URL, so this process never does.** No socket is opened
  here for a search or a fetch, `Act::Net` is therefore never consulted for one,
  no `Approver` sees it, and the sandbox is not involved. The boundary is
  **declared, not enforced by this crate**: `allowed_domains` and
  `blocked_domains` fill in the vendor's own filter and are enforced there. It is
  the arrangement already stated below for a stdio MCP server — the harness states
  a boundary another process enforces, and records what it stated. Enforcement in
  *this* process means not turning the feature on and using a tool the harness
  executes itself.
- **A declaration a vendor cannot carry is refused, not narrowed.** OpenAI's Chat
  Completions takes an allow-list and has no fetch tool; OpenRouter's web plugin
  takes no domain filter and has no fetch tool. Either mismatch is an
  `Error::Config` before the first request is sent, so a `WebAccess` is not
  automatically portable between providers.
- **A citation is what the provider returned, and is not verified here.** The
  crate does not fetch the cited URL, does not check that the page says what the
  model claimed, and does not rank what it was given. A `citations` row is
  evidence about the answer, not about the world, and nothing in the run consults
  one.
- **A paused turn resumes as a fresh request and may repeat a search.** A
  `pause_turn` stop reason is a continuation, so the loop takes another step — but
  the crate does not echo the vendor's partial assistant blocks back. (The request
  was one flattened user turn from 0.1.0 to 0.48.0 and carries `messages` since
  0.49.0; neither shape carries those blocks.) The provider may therefore
  re-run, and re-charge for, a search it already performed. `WebAccess::max_uses`
  is the only lever against it.
- **A spawned child inherits the root's declaration and cannot ask for its own.**
  The spawn tool copies the root contract's `WebAccess` onto the child and never
  reads one from the spawn arguments, so the model cannot widen it; there is no
  per-child narrowing either. A plain `Session::turn` has no declaration, because
  it builds its own contract — `turn_bounded` carries one.

See the [web guide](guide/web.md).

**What a session is, and is not (0.20.0).** A [`Session`] is a conversation over
the runs, not a second execution path:

- **A turn is a run.** Each turn has its own `runs` row, steps, refusals, budget
  draws and checkpoint, and carries every guarantee a run carries — it is
  auditable, resumable by its `run_id` through the ordinary `resume*` entry
  points, and bounded by the same `Policy`. A session adds the tree and nothing
  else.
- **The tree is append-only.** Branching makes an earlier turn the parent of the
  next one; it edits, deletes and rewrites nothing, so an abandoned branch stays
  readable. There is no turn edit, no history rewrite, and no session-level
  compaction — what bounds a long conversation is the `ContextBudget`, which
  elides what the model sees and never what the store holds.
- **A streamed delta is provisional until the completion returns.**
  `EventKind::Token` is what the model has said so far, not a decision it has
  made: the turn may still fall over to another provider, be retried, or be
  interrupted, and text already emitted is not withdrawn. Render it; do not act on
  it. The committed step is the settled fact.
- **The harness itself acts on one thing before the completion returns, and only
  one (0.54.0).** A *finished* read-only tool call — in the completion's leading
  run of read-only calls, allowed outright by the `Policy` — is started as soon as
  its arguments are complete. That is a narrow exception to the rule above and it
  does not weaken it, because the result is used **only** if the settled
  completion carries that same call, with the same name and byte-identical
  arguments, at that same position. Anything else is discarded unused: no
  observation, no step row, no `PolicyEvent`, no ledger draw, no
  `EventKind::ToolCall`. A consumer's rule is unchanged — render a delta, do not
  act on it — because a consumer cannot tell a finished call from a half-received
  one, and the harness can: the arguments parse as a JSON object, and every proper
  prefix of one does not.
- **A `Provider` that does not override `complete_streaming` streams nothing.**
  The default emits the finished text as one delta, which keeps a consumer
  rendering, and is not incremental. The four built-in providers and `Fallback`
  override it.
- **Steering is text, not authorization.** An operator's mid-turn message reaches
  the model exactly as a `TaskContract` constraint does, and every tool call it
  leads to is checked against the same policy by the same code. A `Steer` cannot
  change the policy, the budgets, the sandbox or the contract of a turn in flight.
  `Steer::fold` (0.69.0) is the same rule from the other side: it changes what the
  next request carries and nothing else — not the trace, which keeps every folded
  observation, and not a boundary.
- **A steer, a fold and an interrupt land at the next step boundary**, never where
  they were sent — the same rule `Flow::Cancel` has always had, for the same reason:
  in between, a tool call is in flight and a file may be half-written. An interrupt
  drained at the same boundary as a fold wins, and the fold is not bought.
- **One session, one driver, and the loser is told (0.62.0).** Two processes
  taking turns on the same session id concurrently still do not both land on the
  head path. What no longer happens is one of them being dropped silently: the
  head advances by compare-and-swap, so the losing write returns
  `Error::Conflict` with its turn row left intact, to be read or rebased. The run
  behind each turn is leased besides, so two processes driving one run is refused
  outright.
- **A session has no aggregate budget.** Every turn is a fresh run with its own
  ceilings, so `max_steps` on one turn does not bound the next. A conversation-wide
  limit is the caller's to enforce, per turn, from `Store::run_summary`.

**What configuration is, and is not (0.19.0, extended in 0.27.0, 0.28.0 and 0.29.0).** `io.toml` is
a projection onto the typed API and never a second path into the run loop:

- **The typed API is the authority.** Every key lands in a type this crate
  already had — `Policy`, `SandboxConfig`, `Toolchain`, `PriceTable`,
  `McpServer`, `TaskContract` — and a file can express nothing the typed API
  cannot. A test asserts every key reaches a typed field. `[[provider]]` (0.27.0)
  is the same rule and not an exception to it: it yields a **`ProviderSpec`**, a
  value the application constructs a provider from, never a provider. `Provider::complete`
  returns `impl Future`, so the trait is not dyn-compatible and there is no
  `Box<dyn Provider>` for an accessor to return. `kind = "compatible"` (0.29.0) is
  a fourth `ProviderSpec` variant and not a fourth mechanism — it names an
  endpoint where the other three name a vendor, and it arrives behind the
  `#[non_exhaustive]` 0.27.0 put on that enum for exactly this, so a caller who
  wrote the `_ =>` arm is unbroken.
- **A resolved key can say which file decided it, and that is all it says
  (0.30.0).** `Config::origin(key)` reports the scope and the path that won a
  dotted key; `Config::origins()` reports every key a file set. It is an addition
  beside `Config::sources()`, which answers "which files were read" and keeps
  answering exactly that. Three bounds, each deliberate. **An empty answer is the
  crate's default** — no file named the key — and is never dressed up as a file.
  **`policy.layers` and `agent` report every contributing file**, because those
  arrays append across scopes and naming one winner for a value three files built
  would be false. And **a substituted value reports the file the substitution was
  written in**, not the environment variable or the command it read, because the
  file is what a reader can go and change. Provenance describes the resolution and
  never alters it, and there is still **no configuration writer**: reporting which
  scope set a key is not editing that file, and this crate does not edit `io.toml`
  for you.
- **The file is read once, by the caller, before the run, and never again.**
  Nothing in this crate discovers a config on its own: `Config::discover` is the
  caller's own call. That is what makes the one guarantee here true — a config
  the agent writes *during* a run cannot widen the boundary that run is already
  under. `[instructions]` (0.27.0) reads the repository's own `AGENTS.md` inside
  that same call, before the run, and never during one.
- **A config file is not a security boundary against the agent.** The boundary is
  the `Policy` the caller loaded; the file is where it was written down. An agent
  that can write the workspace root can write an `io.toml`, and the *next* load —
  the caller's act, not the agent's — will read it.
- **A file inside the workspace may narrow the boundary and may never widen it
  (0.27.0, extended to `io.local.toml` in 0.74.0).** Five keys are refused when, and
  only when, the value written is the widening one: `policy.defaults.exec = "allow"`,
  `policy.defaults.net = "allow"`, `sandbox.allow_network = true`,
  `sandbox.force_floor = false` and `sandbox.mode = "full-access"`. So is
  `${cmd:...}` in `io.toml`, and since 0.74.0 `${file:...}` there too — its argument
  is joined onto the file's own directory, and an absolute one replaces that
  directory outright — including inside a `[profile]`. **So are five whole sections**,
  and the rule they implement is one sentence rather than a list: *anything that
  names a program to run, or names an endpoint a credential is sent to, is refused
  outside the user scope.* `[[hook]]` (0.28.0), `[browser]` (0.53.0), and
  `[[provider]]`, `[[mcp]]` and `[[lsp]]` (0.74.0). A section is refused whole rather
  than by its hazardous key: a rule that permits half a table is a rule a reader has
  to hold two halves of, and the next key added to that table lands on the permitted
  side by default. And two keys that name a **directory** are held to the root:
  `run.skills` and `run.templates` may not be absolute and may not climb out with
  `..` (0.74.0), because both are joined onto the discovery root and the frontmatter
  of every `*.md` under them is composed into the model's system prompt on every
  turn — a read, not an act, and one that happens before any `Policy` exists to have
  an opinion about it. A `[[plugin]]`'s own `path` is held to the same boundary,
  through `contain_under_root`, so a symbolic link out of the workspace is caught
  too; such a declaration is dropped with its reason rather than refused, like every
  other bundle that fails to load.

  **`io.local.toml` is held to all of it since 0.74.0.** It is the operator's own
  file in intent; in fact it is a path in the workspace root, and a run's agent
  writes paths in the workspace root. One `write_file` of an unremarkable name
  declared a `[[hook]]`, an `[[mcp]]` command or a `[[provider]]` endpoint that the
  next `Config::discover` would act on, outside the `Policy` and outside the sandbox,
  and nothing about that write looked like an escalation. The **user scope** is the
  one file no workspace can reach — `$IO_CONFIG`, else `$IO_CONFIG_HOME/io.toml`,
  else `$XDG_CONFIG_HOME/io/io.toml` or `~/.config/io/io.toml`; on Windows
  `%IO_CONFIG%`, else `%IO_CONFIG_HOME%\io.toml`, else `%APPDATA%\io\io.toml` — so it
  is the one still trusted to widen, and every refusal above names it. The narrowing
  value of each of the five keys stays legal in both workspace files, because a
  project file denying `exec` is exactly what the scope is for. **A number has no
  widening value, so five keys are
  held to the lower of the two instead** — `run.max_read_chars` (0.55.0),
  `run.max_wait_secs` (0.60.0) and the three `[memory]` caps (0.56.0): a project file may tighten an operator's ceiling
  and may not loosen it, while `io.local.toml` and the user scope set it outright. **This does not claim that a cloned repository is
  safe** — `[toolchain]` still names an argv, a policy layer can still allow what the
  defaults did not, and `${cmd:...}` and `${file:...}` are refused in `io.toml` and
  not in `io.local.toml`. It is a specific narrowing of specific hazards: the keys
  whose effect is to remove containment, and the sections that name a program or a
  credentialled endpoint, in a file the workspace supplies. The boundary against the
  agent is still the `Policy` the caller loaded.
- **An unknown key is an error**, naming the key and the file, rather than being
  ignored. There are exactly two exceptions and they are stated together: a key
  inside a `[[mcp]]` table, because `McpServer` is `#[serde(flatten)]`-based and
  serde refuses `flatten` beside `deny_unknown_fields`; and anything under `[app]`
  (0.27.0), which this crate stores and deliberately never validates so that the
  applications built on it keep their own settings in the same file. A typo inside
  a `[profile.<name>]` is an error even when that profile is never selected.
- **A failed substitution is an error, never an empty string.** An unset
  `${env:...}`, an unreadable `${file:...}`, a `${cmd:...}` whose program is missing
  or whose exit status is non-zero, and a value that resolves to nothing each fail
  the load, because an empty string in a boundary rule is a rule that matches
  nothing. `${cmd:...}` (0.27.0) runs with **no shell** — the value is split on
  whitespace and the first word is the program — and has no timeout, which is stated
  rather than fixed: it runs at the caller's own `Config::discover`, with the
  caller's own privileges, before any run exists.
- **`[instructions]` is discovery, and the one place "resolve or fail" does not
  apply (0.27.0).** A named file that is absent is skipped rather than failing the
  load. What it finds lands in `TaskContract::instructions` — its own public field since
  0.45.0, having landed in `constraints` from 0.27.0 to 0.44.0 — and it is
  **untrusted text from the repository**: it reaches the model verbatim
  and grants nothing.
- **The `[toolchain]` override does not reach this crate's own run loop.** The
  harness detects for itself; `Config::toolchain(detected)` gives the embedding
  application the merged value. Wiring it into the loop needs a new
  `TaskContract` field, which is an addition rather than a break — the type is
  `#[non_exhaustive]`, and 0.62.0 added `lease_ttl` that way. The wiring is a
  later release's work, not a break this one is avoiding.
- **Scope discovery is fixed**: four scopes, no `include`, no `extends`, no
  parent-directory search, no JSON or YAML form, and no reload. `IO_CONFIG` (0.27.0)
  names the user-scope *file* outright, ahead of `IO_CONFIG_HOME` and every platform
  convention — it names a scope rather than bypassing the merge, so a project file
  still wins the keys it names and the count stays four. A `[profile.<name>]` is an
  overlay of the same file that was already read, applied by an explicit
  `Config::with_profile` call: not a fifth scope, not a second file, not a reload.

**What a lifecycle hook is, and is not (0.28.0).** A `[[hook]]` table is an
`Observer` that came from a file instead of from a Rust program. It changes nothing
about how a run is watched; it changes who can write one.

- **It is the caller's to install.** `Config::hooks()` builds a `Hooks` and hands it
  back; the caller passes it to `run_observed`, `resume_observed`, or any of the tree
  forms, exactly as it passes its own. Nothing in this crate installs an observer the
  caller did not install, and that is the same "nothing is loaded implicitly" rule
  every other projection in `io.toml` obeys.
- **It fires on the events the channel already emits**, named by the wire tags
  `EventKind` serializes to. The list of valid names is a census of the enum, asserted
  by a test that reads the source, so a name that is not there is a load error rather
  than a hook that installs and never fires.
- **It runs inside the run loop and blocks it.** `Observer::event` is synchronous and
  returns a `Flow` the loop acts on immediately. That is what makes
  `on_failure = "cancel"` able to stop anything at all, and it is why an executing
  hook is bounded by `timeout_ms` and killed past it. Hooking a hot event — `token`
  is emitted per streamed token — with a `run` action is a decision to spawn a
  process that often.
- **There is no shell.** A hook's `run` is a TOML array and reaches the process as
  argv, unsplit and uninterpreted, which is the discipline `${cmd:...}` and the `exec`
  tool already hold.
- **It grants nothing.** A hook is not a permission mechanism: it cannot approve or
  deny an action, and the boundary is still the `Policy` the caller loaded. It can
  only end the run, and only when the operator wrote `on_failure = "cancel"`.
- **A hook's output is discarded.** `stdout` and `stderr` go to `null`, so a hook
  talks by exiting non-zero. A failure is traced at warn level with the hook's index
  and the reason, and never with the event, which may carry a goal or a target.
- **Hooks do not accumulate across scopes.** Unlike `[[policy.layers]]` and
  `[[agent]]`, the array is not in `APPENDING`: a later scope replaces it whole. Since
  0.74.0 only the user scope may declare it, so there is no second file left for that
  rule to arbitrate between — what a run gets is that file's hooks, and then whatever
  a user-scope bundle contributed, which `Plugins::apply_to_hooks` **appends** to
  them.

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
- **`server_tool_requests` is only as complete as the vendor's reporting.** It is
  non-zero from 0.22.0, where a provider both executes a tool and reports a count
  for it — Anthropic does. OpenAI and OpenRouter report no such counter in the
  shape the crate reads, so the meter is zero on those providers even for a run
  whose `server_tool_calls` rows say a search ran. A provider-executed request is
  billed per request rather than per token, and a `PriceTable` prices tokens: no
  derived cost includes it.
- A **cost is derived, never stored**, from a price table the operator supplies,
  and is therefore only as right as that table. The crate ships **no prices** and
  requires an as-of date on any table, because it cannot keep a vendor's price
  list accurate on its own release schedule. An unpriced call is counted in
  `Spend::unpriced_calls` rather than costed at zero: a group with calls there is
  reporting a floor, not a total.
- **A run recorded before 0.18.0 has no rows at all**, in either new table.
  Nothing is backfilled, because the facts were never recorded. The queries
  return nothing rather than zeros.

**What `rewind` puts back, and what it cannot (0.28.0).** `rewind` restores a path
to the state it was in before *this run* first wrote it.

- **One restore point per file per run**, taken at the first write. Not a per-step
  history of contents: there is no redo, and a restore returns the file to the
  state before this run's first write. Reversing one step's change is a different
  mechanism with its own answers — `rewind_step` (0.51.0), which reverse-applies
  that step's hunks.
- **It is durable, and it is a new table.** `CHECKPOINT_FORMAT` stays 7, an older
  store opens and resumes unchanged, and a run that predates the release answers
  `NotRecorded` rather than restoring nothing quietly.
- **Four answers, and the fourth is the point.** `Restored`, `Removed` for a file
  the run created, `NotKept` for one whose previous contents were over the 1 MiB cap
  or were not UTF-8, and `NotRecorded` for a path this run never wrote. `NotKept`
  and `NotRecorded` change nothing at all, and a `NotKept` file is left exactly as
  the run left it — never truncated. Collapsing the two would tell a caller a file
  was untouched when the run had rewritten it and the harness cannot undo that.
- **Only the write tools snapshot** — `write_file`, `edit_file` and `patch_file`
  (0.51.0). A file changed by `shell`, by `exec`, or by a git built-in has no
  restore point. So does one whose bookkeeping
  row could not be written, which is warned about and swallowed exactly as an edit
  row is — `NotRecorded` means "no restore point", not "the run did not write it".
- **It obeys the policy the edit obeyed.** Restoring goes through
  `Workspace::write_file`; removing checks `Act::Write` itself and refuses anything
  that is not an outright allow, because a write is inspectable afterwards and a
  delete is not.
- **One path per call.** `rewind_run` (0.36.0) is the whole-run form; neither is
  transactional.

**What `rewind_run` puts back, and what it cannot (0.36.0).** `rewind_run` widens
the above from a path to a run: every file it wrote, every memory entry it wrote,
and the spawn backlog it left queued, in one call.

- **Memory goes back to the value before this run's FIRST write to that key**, on
  the same one-restore-point-per-run rule and the same guard files have. An entry
  the run created is removed, because "the way it was" for an entry that did not
  exist is not existing.
- **The pin does not apply to an undo.** Restoring bypasses `memory_write`, so an
  entry pinned *after* the run wrote it is still put back. The alternative is
  telling a caller a rewind happened when it had not.
- **Nothing in the trace is deleted.** Steps, the durable event stream, the spawn
  records and the ledger are untouched. The spend happened, and an undo that
  erased the rows would make the ledger disagree with the invoice. What the rewind
  took is recorded before it goes and is read back through `Store::rewinds`.
- **A commit is not un-committed.** `git reset` is unreachable from this crate by
  construction and stays so. A push is not recalled, a migration is not reversed,
  a provider call is not un-billed. A rewind restores a working tree, memory and a
  queue — nothing outside them.
- **One run, not a tree.** A caller wanting a subtree loops over it. Choosing an
  ordering over children whose written paths may overlap is a decision this crate
  does not make for you.
- **Two additive tables**, `memory_snapshots` and `rewinds`. `CHECKPOINT_FORMAT`
  stays 7, no existing table is altered, and a run from before this release has no
  restore points to find. The unconditional cost is one restore-point row the
  first time a run writes a given memory key — bounded by keys touched per run,
  exactly as file snapshots are bounded by paths written.

**What a branch and a worktree do and do not promise (0.36.0).** `git_branch`
renders `git switch --create=<name>` and `git_worktree` renders
`git worktree add -b <name> -- <path>`.

- **`switch --create` cannot discard a working-tree change.** The ref starts at
  `HEAD`, git refuses a name that already exists, and the tree is carried across
  rather than replaced. That is the whole reason it is reachable while `checkout`,
  `reset`, `rebase`, `stash`, `push`, `fetch`, `clone`, `remote` and
  `filter-branch` remain unreachable by construction.
- **Branch names are validated in this crate, more narrowly than git validates
  them.** Letters, digits, `.`, `_`, `/` and `-`; at most 100 characters; no
  leading `-`, no `..`, no empty or `.lock` path component. A name git would
  accept and this refuses costs an observation; a name git would read as an option
  costs the property the whole module exists for.
- **Nothing removes a worktree or deletes a branch.** Removing a worktree deletes
  the work a child was spawned to produce, so it stays the operator's call.
  Worktrees a run created accumulate until someone removes them.
- **A worktree is visible to the parent's own `git_status`**, as one untracked
  entry — `?? .worktrees/` — because git summarises an untracked directory rather
  than descending into it. A `git_add` naming `.` in the parent therefore stages
  the children's trees. This crate does not write to `.git/info/exclude`,
  `.gitignore` or any other repository metadata on your behalf; the operator adds
  `/.worktrees/` to `.git/info/exclude` if they want it hidden.
- **`AgentDef::worktree` fails the spawn when a worktree cannot be made** — no
  `git`, not a repository, or the policy refusing that path. It does not fall back
  to the parent's tree, because that fallback is the collision the field exists to
  remove.
- **Version floors this crate does not probe for:** `git switch` needs git 2.23,
  `git worktree` needs git 2.5. An older git surfaces as the message git itself
  prints, as an observation rather than a crate error.

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
started, dials what it likes; and a provider-executed search or fetch (0.22.0) is
dialled by the provider, so no `Act::Net` decision is taken for it at all and its
domain filter is the vendor's rather than the policy's. The harness decides what
starts, not what a started thing then does.

**What a command the agent runs is bounded by.** As of 0.17.0 the agent can run
a command with the `exec` tool. Every call is an `Act::Exec` check on the program
*and* on the whole argv, so `allow_exec("cargo test*")` beside
`deny_exec("cargo publish*")` narrows an allowlist to the sub-command it names.
An argv deny is sound **inside** an allowlist and nowhere else: it holds where
`defaults.exec` is `Deny` or `Ask` and explicit `allow_exec` rules say what may
run, and it is not a blocklist over a permissive default. A joined argv can be
spelled in more ways than a pattern can enumerate — `["git","-c","x","push"]`
puts a flag between the program and the sub-command, and `["env","rm"]` and
`["busybox","rm"]` reach the program under another name — so a denylist over a
permissive tier is a boundary with an unbounded number of ways around it, and no
pattern makes it complete. Write the allowlist, then narrow it. A refusal, and an
approver's decision, land in `policy_events` attributed to the rule and layer; a
silent allow does not write a row, exactly as it does not for a read or a write.
What the policy does **not** decide is what the command then does.

A command runs with the **workspace root as its working directory**, and since
0.46.0 it runs **contained by default**: `ExecMode::WorkspaceWrite` is the
default `exec_sandbox` mode, so `exec` and every `shell` tool is wrapped by the
backend `sandbox::select` chooses and may write inside the workspace root, the
system temporary directory and the detected toolchain's own cache directories —
and nowhere else. The grant every release up to 0.45.0 gave by default is still
available and is now a sentence rather than an omission:
`TaskContract::with_full_access`. This is where a command differs from a
registered `Tool` and a stdio MCP server, which do run unwrapped at the
embedding program's privileges: the harness starts the command's process itself,
so it can wrap it, and it does not start theirs. What the containment is worth is
per platform and the platform table above is the statement of it — macOS and
Linux confine writes and deny egress, a Windows AppContainer does both when the
run asked for access confinement (0.59.0), and the Windows default Job Object
and the portable floor apply the resource caps and have no filesystem facility at
all, so there the mode is routed and reported and enforces nothing for the
filesystem.

Three consequences worth naming. The mode changes none of them; the second one
varies by platform. A policy written for file access does not constrain command
execution — `Act::Read`/`Act::Write` rules say nothing about `exec`, and the tier
default decides everything unnamed. A command can read what the agent's own file
rules would have refused, because `cat secrets/prod.env` is a command and not a
read: containment confines *writes*, and on macOS and Linux the tree is bound or
remounted read-only rather than unreadable, so it takes nothing away from that.
The one platform where it does is a Windows AppContainer, which is default-deny
for reads and so bounds the read to the derived grant set as well. And a timeout
kills the **direct child only**: a process that child started itself may outlive
it, on every platform this crate supports — the same gap the sandbox reports for
its own wall-clock kill on the portable floor. See the
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
and was then permanently unreachable — the built-in answered every call. It named
every built-in as of that release, in every build regardless of feature flags,
and a registered tool taking one of those names fails the run before the first
completion. The names it holds are reserved even where the tool itself is not
compiled in, so enabling a feature can never take away a tool that was working.

**It reopened once, because the fix was a list rather than an invariant, and
0.61.0 closed it as a rule.** Every built-in added after 0.17.0 reopened the
defect by one name — the worktree tool, `patch_file`, `check`, the five `lsp_*`
tools, the six `browser_*` tools, `forget`, and the mailbox pair — eighteen in
all, each validating cleanly and then never reached. The set now holds all 53
names the harness answers, and what keeps it holding them is a test rather than
diligence: `every_name_the_harness_answers_is_reserved` derives the built-in set
from the crate's own `*_TOOL` constants and fails when `RESERVED_TOOL_NAMES` does
not hold it, in either direction. Adding a built-in without reserving its name is
a red test.

Two consequences of that release a caller can see. The six `browser_*` name
constants are no longer `#[cfg(feature = "browser")]` — a name the harness owns
is owned in every build, which is the same reason 0.17.0 ungated the image and
document names, and it is what lets one unconditional list hold them. And
`send_message` / `read_messages` are reserved in **every** run shape, including a
flat run that is never offered them: before 0.61.0 a registered tool could take
one of those names and work in a flat run while being shadowed inside a tree,
which made the safe set of names depend on which run shape a program happened to
start. That is `spawn_agent`'s own precedent, and it is the one place where this
release takes away a configuration that worked rather than one that was quietly
broken.

**Windows resource caps.** See the platform table above.

**`SandboxLimits::max_processes` is enforced on Windows and nowhere else.** Since
0.24.0 the Job Object's `ActiveProcessLimit` bounds the active process count per
sandbox, and like the job's memory cap it is not a kill: the job refuses the
`CreateProcess` that would cross the limit, so the run fails because its own
spawn failed rather than because the payload was terminated. macOS and Linux
enforce nothing. Neither maps it to `RLIMIT_NPROC`, which is per-real-uid and
would throttle the operator's own login session rather than the sandbox, and the
other backend that could scope it properly — the Linux pid-namespace
active-process limit — is not wired up. Setting it on a unix host changes
nothing.

**Killing a process handle reaches the whole tree, on every platform (0.26.0).**
`shell_kill` ends the processes the handle started *and* every descendant they
produced, including a grandchild whose own parent has already exited — the shape
a real dev server has, and the one no kill built on walking the process table can
reach, because the parent/child link it would have followed is gone.

That is a guarantee rather than a best effort, and it rests on a different
mechanism per platform:

- **macOS and Linux (since 0.25.0)** — each stage of a handle's line is spawned as
  the leader of its own process group, and the kill signals the group. Membership
  is inherited across `fork` and outlives every parent in the chain.
- **Windows (since 0.26.0)** — there is no process group. Each stage is created
  suspended, assigned to a per-handle Job Object, and only then resumed, and the
  kill closes the job. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` walks nothing, so
  membership cannot be escaped or outlived.

This is *not* the same claim as the sandbox's. A handle's job carries the
teardown guarantee and no resource limits, because a handle is a lifetime rather
than a boundary — a twenty-minute build is exactly the workload the sandbox's
caps exist to kill. A handle's processes are governed by the policy checks its
command line passed before anything spawned, not by `SandboxLimits`.

Until 0.26.0 the Windows half was genuinely open, and until this paragraph was
rewritten this document said the unix half was too. It was not: process groups
closed it in 0.25.0 and there has been a test and a negative control for it since.
Under-claiming a shipped boundary is the same defect as over-claiming one — either
way the file cannot be trusted — and it is recorded here because this is the
second consecutive release in which this document has done it.

The run-end sweep and the drop backstop go through the same kill, so both inherit
the guarantee.

**The seccomp filter is a deny-list, not a jail (0.46.0).** The Landlock rung
installs one alongside its rule set: a short list of syscalls that would let a
payload undo its own confinement or reach into another process is refused with
`EPERM`, and it says nothing about the thousands it does not name. An allow-list
a real toolchain survives is a research problem whose failure mode is a broken
build, and that trade was made at specification time. On the namespace rungs
whatever else applies is the kernel's own default under an unprivileged user
namespace, not a filter this crate installed.

**A native backend can silently become the floor.** `select` chooses its
candidate at compile time, and a backend whose primitive is unavailable at
runtime degrades to the next rung and ultimately to the portable floor. Ubuntu
24.04's `apparmor_restrict_unprivileged_userns=1` makes every `unshare` fail,
which rules out three of the four Linux rungs — that host lands on Landlock, not
on the floor, and the floor is where a host without Landlock either ends up. It
reports what ran honestly in the returned `Selected`, so read that value rather than
assuming the platform's native backend is what ran.

**What the trace says a tree ran under.** `run_tree`, `resume_tree` and the
single-file and workspace loops all record the policy they execute under, so the
store answers "what boundary was in force". `resume_tree` did not, until 0.32.0:
it took a policy as an argument and executed under it while leaving the recorded
policy as whatever the run started with, so a tree resumed under a *widened*
policy left an audit that understated what was permitted. Fixed in 0.32.0, which
was already in that function for the fleet queue.

**What a plan is, and is not (0.21.0).** The `todo_write` tool holds the agent's
plan in a `todos` table, replaced wholesale on every write.

- **A plan is never enforced.** Nothing verifies it, no `RunOutcome` depends on
  it, and no refusal consults it. An item whose state is `done` is the agent's
  claim, not a fact the harness checked. What a plan buys is a long run that can
  be recognised as going the wrong way before it ends.
- **A plan is not gated, and neither is a question.** Both write into the
  harness's own store rather than the workspace, the network or a binary, so
  there is no `Act` to check them against. There is deliberately no fifth `Act`
  variant: a permission rule in front of the channel whose purpose is to ask a
  human something would be a category error.
- **A plan is readable while the run is going.** The write is one transaction, so
  a reader on another connection sees the previous plan or the next one and never
  half of each.

**An answer to a question is text, never authorization (0.21.0).** The approval
path asks whether an action is *permitted* and its answer can only narrow what
happens. `ask_question` asks what the operator *wanted*, and its answer is
delivered to the model as an observation. Every tool call that follows one is
checked against the same `Policy` by the same code — the rule steering has
followed since 0.20.0. A human answering "write the file, I authorize it" does
not make a denied write permitted.

- **A paused question's step is committed, so the asking call is not replayed.**
  `resume_with_answer` therefore delivers the answer as a ledger observation
  rather than by re-running the tool. `Store::answered_question` is a query for
  reconstructing a run, not the resume mechanism.
- **`answered_by` distinguishes a machine from a person.** A `Responder` in the
  run's own process and a human answering after a pause are different facts about
  a run, and the trace keeps them apart.
- **Several independent questions may be asked in one call (0.72.0).**
  `ask_questions` takes an array of question objects, parsed strictly per index
  with the failing index named, at most ten per call and capped-and-told rather
  than truncated. `ask_question` is unchanged and remains the tool for one
  question. Questions whose answers depend on each other belong in separate
  calls: the harness cannot detect a dependent pair and does not try, the tool
  descriptions say so, and this is a known limitation rather than a solved
  problem.
- **A batch is one parked question.** One `pending_questions` row, one
  `question_id`, one `RunOutcome::AwaitingAnswer`, one `Waiting::Question`, and
  the same four `resume_*_with_answer` functions with the signatures they have.
  Answering is all-or-nothing — one conditional update on one row, which is what
  a submitted answer set means. The row's question text is the whole ask, so a
  reader that predates batching sees all of it rather than the first of it.
- **`Responder::answer_all` is defaulted, so no implementor had to change.** Its
  default body loops `answer` once per question in order; an interface that wants
  one overlay for several questions overrides it. The trait stays
  dyn-compatible.
- **An offer can explain itself (0.72.0).** `Question::choices` is
  `Vec<Choice>` — a label with an optional sentence and an optional preview —
  and the deserializer reads the array of plain strings every earlier release
  wrote, so no store needs a migration. A preview is bounded at twelve lines or
  eight hundred bytes and cut at a line boundary, with control characters and
  escape sequences stripped, because a model writes it and a terminal draws it.
  `Question::multiple` says several offers may be taken and
  `Question::answer_of` spells such an answer, once, so two interfaces produce
  the same text. An answer is still not obliged to be one of the offers.
- **A child's question pauses the whole tree**, as a child's deferred approval
  does, and `resume_tree_with_answer` takes the *root's* run id.

**What a named agent definition can and cannot do (0.21.0).** An `AgentDef` gives
a spawned child a role, a model and a narrower boundary.

- **A definition can only narrow.** `deny_write` and `deny_net` compose through
  `Policy::contain`, which has bounded every child since 0.5.0 — allows
  intersect, denies union, at any depth. There is no `allow_write` and no
  `allow_net`, and there must never be one: a roster in a configuration file that
  could grant would be a privilege-escalation path. A definition silent about a
  path its parent denies still yields a child that is refused it.
- **A model is a request, not a fact.** `AgentDef::model` travels as
  `CompletionRequest::model`, which a vendor may substitute or alias. What
  actually served a call is `CompletionResponse::model`, and that is what the
  trace records.
- **One provider serves a whole tree.** A definition names a model *string*; it
  cannot carry its own provider, vendor or API key. Two definitions naming models
  from different vendors is not supported.
- **A definition cannot carry its own skills directory.** The tree's skill
  catalogue is the root's and is shared, as it has been since 0.5.0.
- **The roster is advertised in the spawn tool's description, not as a schema
  `enum`**, so an unknown name is a recoverable error observation naming what is
  available rather than a malformed call.

**What a prompt template is (0.21.0).** `Templates::render` substitutes
`{{placeholder}}` values and routes the remainder to `$ARGUMENTS`, and returns a
`String`.

- **Rendering can set nothing.** No policy, no budget, no toolbox, no model, no
  verification criterion — it returns text, which is what makes a shared template
  directory safe to read.
- **A placeholder with no argument is an error, never an empty string** — the rule
  0.19.0 set for `${env:}`, for the same reason: a goal with a hole in it still
  reads like a goal.
- **There is no template language.** No conditionals, loops, includes, partials or
  nesting, and substitution is single-pass: a value containing `{{x}}` is emitted
  literally rather than re-read.

**Repetition detection does not survive a resume (0.21.0).** There are now two
stall signals: the 0.11.0 window (the workspace stayed still *and* a call
repeated) and bare repetition (the same call `window` times in a row, whether or
not the workspace moved — the shape a parent respawning one child takes, which
the window could never see because a child that ran sets `changed`
unconditionally). Both live in `Progress`, which is in-memory, so a resumed run
starts its window at zero exactly as it did before.

**`git_status` output.** It uses `--porcelain=v1` without `-z`, so a path
containing a newline renders ambiguously to the model. A display concern, not a
boundary one — the path policy still sees the real path.

**A call the harness cannot inspect is not replayed on a resume (0.65.0).** Every
registered `Tool` and every MCP call is classified on a recovery axis —
`ToolRecovery::Replayable` or `ToolRecovery::Indeterminate` — separately from
`ToolEffect`, which answers concurrency and not repeatability. `Tool::recovery`
defaults from `Tool::effect`: a tool declaring `ToolEffect::ReadOnly` states that
it observes and changes nothing, so it is replayable; `ToolEffect::Mutating`,
which is what a tool declaring nothing gets, states only that it must run alone,
so it is indeterminate. An indeterminate call is journalled in `tool_attempts`
before it is made and closed after it returns, on its own rather than at the step
boundary — the one thing in this crate written to outlive a step that never
committed. A resume that finds an open attempt returns
`RunOutcome::AwaitingRecovery` and drives nothing; `resume_with_recovery` carries
the operator's decision, which is to retry the call, to record it as completed
with the account the model is then given, or to abort the run.

**What it does not close.** Between the decision to make a call and the journal
row committing there is nothing on disk, and no journal closes that gap, because
the write and the call are not one act. A process dying in that window replays the
call exactly as it did before 0.65.0. What the release narrows the window to is
the width of one committed `INSERT`, and the crate does not claim more than that.
Nothing here undoes an effect, compensates for one, or sends anything to the
service on the tool's behalf; the crate records what was started and asks.

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

## What a deletion takes, what it leaves, and what expires on its own (0.58.0)

**Nothing expires on its own.** There is no background job, no default retention
window and no age at which anything in a store becomes eligible for removal. The
crate's position since 0.1.0 — history has no expiry, and the embedding program
decides — is unchanged by this release. What 0.58.0 adds is the instrument, and
every one of its calls must be made by name. A program upgraded to 0.58.0 and
left alone behaves exactly as it did on 0.57.0.

**The unit is a session.** `Store::delete_session` takes the session's turns, the
runs those turns drove, and every run those runs spawned, transitively, under
`runs.parent_run_id` — then every row across the schema keyed to that run set,
then the turns, then the session row, in one transaction. There is no removal of
a single run, a single turn or a named string. A turn's run may have spawned
children, and a half-removed tree is unreachable rows that nothing counts.

**What a deletion leaves.** A `memory` entry is never removed: a note is a
workspace asset that outlives the run that wrote it, which 0.56.0 made explicit
by giving it a scope above the workspace. A run reachable from no session — one
started by `run` or `run_with` rather than by a session turn — is named by
nothing and is not reached.

**What a deletion changes elsewhere.** `memory_recalls` rows for removed runs
*do* go, because they name a run that no longer exists. Since 0.56.0 memory
eviction ranks candidates by `COUNT(DISTINCT run_id)` over that table, so pruning
a session lowers the standing of every note that session had drawn on, and a
later write may evict a note it would previously have kept. That is the behaviour
and not a defect — evidence from a run that no longer exists is not evidence —
and `Store::memory_pin` is what holds a note regardless.

**A resumable run is never swept by a date.** `Store::sweep_sessions(before)`
removes every session whose `created_at` is **strictly** before the cutoff; a
session whose `created_at` equals it survives. A session holding any run in
`RunStatus::Running` or `RunStatus::Paused` is **refused** — left byte-identical,
with its id in `Pruned::refused` — because a date is a policy applied to sessions
nobody looked at. `delete_session` carries no such refusal: naming one id is
somebody's decision about that session. The cutoff is a string because
`sessions.created_at` is a `strftime('%Y-%m-%dT%H:%M:%fZ','now')` text column and
a string comparison is what the storage performs.

**The preview is the receipt, not an estimate of it (0.70.0).**
`Store::sweep_preview(before)` returns the `Pruned` that `sweep_sessions(before)`
would produce, and deletes nothing. It can be trusted as equality rather than as
a bound because both go through the same two private steps: one selection, which
resolves the candidates and applies the resumable refusal, and one measurement,
which counts and sizes what was selected. The deletion then consumes the very
sets the measurement returned rather than resolving them again — a preview
computed one way and a deletion performed another is precisely the defect this
exists to avoid. The one thing it cannot promise is quiescence: a run that starts
or finishes between the two calls moves the answer, because the refusal is a
question about the store's state and not about the cutoff.

`Store::session_created_at` exposes the column the cutoff is compared against.
The nearest substitute — a session's first turn — is always **later** than the
session row, so a preview built on it understates the deletion, which is the
dangerous direction for a caller deciding whether to proceed.

**What an archive keeps.** `Store::archive_session` keeps every row and empties
every column that holds words. The counts, timings, tokens, cost, file paths,
line counts, verdicts, statuses and kinds all survive; the prompts, replies, tool
results, summaries, snapshot contents and edit hunks do not. It is not confined
to the conversation table and must not be: `provider_calls` is the only pure
accounting table in the schema, and the user's own words are in `steps.prompt`,
every tool result in `ledger_observations.text`, whole file contents in
`snapshots.before`. An archive that emptied only `session_turns` would report a
removal it had not performed. Archiving is idempotent, and it does not refuse a
resumable session — a run whose words are gone can still be resumed.

**An archived restore point can no longer restore.** The `snapshots` row stays
and records that its content was archived, and a restore reaching it reports
`Rewind::NotKept` naming the archive rather than writing an empty file over a
real one. (`Reverted::Stale` is `rewind_step`'s answer to a hunk that no longer
applies — a different question with a different type.)

**A session's size is content bytes; a store's size is pages.**
`Store::session_size` reports the summed `length()` of the session's own text and
blob columns with the row counts beside it, and `None` for an id the store does
not hold. It is not a per-session page figure and will not become one: `dbstat`
attributes a page to a b-tree — a table — and a page holds rows belonging to any
number of sessions. `Store::store_size` is where the file's arithmetic lives:
`page_size × page_count`, the freelist, and a per-table breakdown.

**Freed pages stay in the file until `compact` is called.** SQLite frees pages
into the file rather than out of it, so a prune leaves the file the size it was
and raises `StoreSize::free_bytes`. `Store::compact` runs `VACUUM` and returns
the bytes the file shrank by. It rewrites the whole database, needs free disk
space of roughly the file's own size while it runs, and cannot run inside a
transaction — which is why it is a separate call. `PRAGMA incremental_vacuum` is
not an alternative: every store this crate has created was created without
`auto_vacuum`, so it does nothing on any existing file.

**A deletion cannot be undone by this crate**, and nothing here is a tool a model
can call. There is no trash and no recovery path; an operator whose recovery
position matters copies the file first. And what these calls remove is what is in
the database — they say nothing about the operator's own logs, their provider
account, or their filesystem.

## Related

## What a language server is asked, and what it is not trusted with (0.52.0)

A run that names a server in `[[lsp]]` — or on the contract with
`TaskContract::with_lsp` — is offered five tools: `lsp_definition`,
`lsp_references`, `lsp_symbols`, `lsp_hover` and `lsp_rename`. A run that names
none is offered **none of them**, and its composed prompt is byte-identical to the
one it had on 0.51.0.

**Starting one is an `Act::Exec` check on its program**, the same check an
`[[mcp]]` stdio server passes, and it happens before any spawn is attempted.
Without `allow_exec` naming that binary the run ends in `Error::Lsp`/`Error::Refused`
and no process exists. A server that cannot be spawned fails the run rather than
being skipped.

**A server is named or there is no server.** Nothing is downloaded at run time,
nothing is resolved from `PATH` by ecosystem, and the detected toolchain is
deliberately not consulted — mapping an ecosystem to a binary would be a guess
about the operator's machine.

**The `path` a navigation names is an `Act::Read` check, taken before the server
is told anything (0.74.0).** Until this release the path was only ever joined onto
the workspace root — which an absolute argument discards outright and a `../..`
climbs out of — and then read from disk and shipped to the server as a `didOpen`,
so `lsp_hover {"path": "../../../../etc/shadow"}` moved a file across the boundary
`read_file` would have refused, and left no row saying so. The check is
`Workspace::check_path`, so it refuses what `resolve` refuses — an absolute path,
and a `..` that leaves the root whether or not the file exists — before it grades
anything, and the refusal is written through the same gate every other refusal in
a run is: one `policy_events` row, attributed to the rule and layer. A read tier
of `Ask` therefore **prompts** on a navigation where it used to pass silently,
which is the treatment `read_file` already gets. `Policy::default()` allows reads,
so the common configuration is unchanged.

**`lsp_rename` writes nothing, and renders a patch only for files the policy
allows reading outright.** It answers with a patch series in `patch_file`'s
format. Every byte that reaches the workspace goes through `write_file`,
`edit_file` or `patch_file` and their gates: one `Act::Write` check per path,
all-or-nothing per file. The reading side is the server's choice rather than the
model's — a `WorkspaceEdit` names whatever files the server decided the rename
touches — and rendering a diff for one puts its removed lines in the model's
context, so each is resolved under the workspace root and must be `Effect::Allow`
(0.74.0). An `Ask` is not permission here: there is no approver on this path to
answer the question, and an unanswered question is not a yes. Under a policy that
asks about reads a rename renders no patch and says so; a run that wants the patch
grants `allow_read` over the tree it is renaming in, which is the grant applying
that patch already needs.

**Positions are 1-based** on the way in and out, as `read_file` shows them and a
compiler reports them. The protocol's zero base is an internal detail. Line 0 is
refused by name rather than clamped.

**A file is re-sent from disk on every request that names it**, so a run that edits
a file and then asks about it is never answered from the text as it was before its
own edit.

**Diagnostics augment and never replace.** Where a server advertises the pull
capability, its findings are appended to what the project's own checker reported,
attributed to the server's id. The compiler's stream is never filtered: a language
server's own analysis omits borrow-check errors, monomorphisation errors and every
lint. Push diagnostics are not used — they have no completion signal, so an empty
result cannot be told from a slow one.

### Stated plainly

- **A server indexes the whole root it is pointed at, including files a
  `deny_read` rule covers.** What this crate does about that is refuse to carry
  those locations into the model's context: every location handed back passes the
  same `Act::Read` check `read_file` passes, and an omission is counted in the
  answer rather than leaving a silently shorter list. What it does **not** do is
  stop the server process reading those files, and it cannot.
- **A language server runs at this process's own privilege**, like an MCP stdio
  server and unlike a command run through `exec` or `shell`. It is not placed
  inside the run's execution sandbox.
- **An empty answer cannot always be told from an unready one.** The protocol has
  no readiness signal, and a server still building its index answers `[]` rather
  than erroring. This crate tracks the work a server announces through
  `$/progress` and retries an empty answer once that work settles, bounded by the
  server's `timeout_secs` — but a server that reports no progress for a start-up
  phase can still return an empty answer that means "not yet".
- **Where two servers claim the same file suffix, the first in declaration order
  answers.** A workspace-wide question, which names no file, goes to the first
  configured server.
- **Completion, signature help, code lens, semantic tokens and call hierarchy are
  not offered.** A model does not type, and the hierarchy requests are named
  roadmap work rather than added here to round out a list.

- [CHANGELOG.md](../CHANGELOG.md) — the release history, with a migration note on
  every break.
- [CAPABILITIES.md](CAPABILITIES.md) — the guide index.
- [RELEASE_PROCESS.md](RELEASE_PROCESS.md) — how a release is cut.
- [public-api.txt](public-api.txt) — the enumerated public surface.

## What an address reaches, and what a message is not (0.60.0)

An agent inside a tree has an **address**, and it names one agent. `spawn_agent`
takes an optional `as`; omitted, one is derived as `<role>#<run id>`. The
distinction the whole feature rests on is that `AgentDef::name` is a **role** —
two children spawned from one definition share it, which is the ordinary shape of
a fan-out — so a roster name has never identified an agent and does not now.

An address is unique within one tree, is letters, digits, `-` and `_` up to 64
characters, and may not be `ROOT_ADDRESS`. A spawn asking for one already held is
refused before anything is allocated: no run row, no agent against the
containment cap, no place in the queue. The address is stored in `spawns.as_name`,
so a resumed tree re-adopts its children under the names they already had; the
adoption key is unchanged.

`send_message { to, body }` and `read_messages { from, wait_secs }` are offered
inside a tree and in no flat run. A read returns what is waiting for that agent,
oldest first by row id, and marks it delivered **in the same transaction as the
select** — a read that fails after selecting delivers nothing and leaves every
message where it was. `Store::messages_for` is the audit read and consumes
nothing.

`wait_secs` blocks until something arrives or the clock expires. The ceiling is
`[run] max_wait_secs` or `TaskContract::with_max_wait_secs`, and
`DEFAULT_MAX_WAIT` when neither is set; a request over it is narrowed and the
narrowing is said on the same observation. `max_wait_secs` is a narrowing key: a
`Project`-scoped `io.toml` takes the lower of the two values.

A terminating agent sends its parent one line — `[finished]` and its outcome —
and never its report, which continues to travel 0.50.0's path. A read naming a
sender that has already terminated without sending returns immediately rather
than at the clock.

### Stated plainly

- **An address resolves inside its own tree and nowhere else.** Two trees sharing
  one store cannot address each other, and a cross-tree address is refused as an
  unknown name rather than as a forbidden one — the refusal lists what is
  reachable and never admits what is not. There is no configuration that widens
  this and there is not intended to be one: a channel between trees would be a
  channel out of the containment boundary a child inherited.
- **A message is not authorization.** Nothing in a body is read by the `Policy`.
  A sibling that says "you may write there" has changed nothing, and an agent that
  acts on such a message is refused by the same rule that would have refused it
  anyway.
- **Nothing is delivered unbidden.** An inbox is read when an agent calls the
  tool. Messages are never folded into a prompt automatically, which is what
  keeps the mailbox off the cache-marked prefix 0.44.0 depends on for every agent
  that is not participating.
- **A wait is bounded, and the bound is not a formality.** An agent that blocks
  holds its concurrency slot, and the sibling that would answer it may be the one
  queued behind that slot. There is no unbounded wait, and a run whose agents all
  wait on one another spends its clocks and carries on rather than stopping.
- **A wait drives this agent's own in-flight children and nothing else.** A
  detached child is a future polled by its parent's loop; the wait is raced
  against that set the same way a provider call is. It does not drive another
  agent's children, and it cannot make a queued sibling start.
- **One sender, one named recipient, and a body of text.** There is no broadcast,
  no topic, no group, no reply-to id and no request/response framing. A protocol
  over the body is the embedding program's to define.
- **The trace records that a message was sent and how long it was, never what it
  said.** The body lives in `agent_messages` and nowhere else, so one retention
  call accounts for it.
- **A tree spawned before 0.60.0 and resumed on it has children with no address.**
  Their `spawns` rows carry an empty `as_name`; they cannot be addressed, and what
  they send is attributed to a derived name. Only a resume across that version
  boundary reaches this.

## What switched off means, and what a probe answers (0.70.0)

**Switched off is not absent, and the distinction is the whole feature.** An MCP
server and a plugin bundle each carry `enabled`, defaulting to `true`. A
capability that vanished from every listing when it was disabled could not be
told apart from one that was never declared, and nothing would be left for an
operator to switch back on. So a disabled thing contributes nothing and is still
listed, marked.

**Where the flag is honoured decides what it means.** For a server it is read at
the head of the connect loop, not over the assembled roster: the server is never
started, no socket is dialled, no session entry exists, and the namespaced tool
names it would have offered belong to nobody. Filtering the roster instead would
have left a disabled server running and its tools callable — the defect wearing a
fix's clothes. For a bundle the same reasoning puts it in a third collection
beside the loaded and the dropped, so all six contribution sites keep reading the
loaded set and none of them can forget the check.

**A disabled bundle is still validated, and still held to its scope's trust
rule.** It is loaded, so a broken one is reported as broken whether or not it is
switched on, and a workspace-declared bundle declaring a hook is refused even
while disabled — switching it on is a one-character edit, and a refusal that can
be sidestepped by shipping something switched off is not a refusal.

**And it claims no id.** A bundle's id is what namespaces the names it
contributes, and a disabled one contributes nothing, so holding the id against it
would be reserving a name nobody uses — and would break the swap this flag exists
to make easy. Switching `tools-v1` off and declaring `tools-v2` beside it is a
one-line edit; if the disabled entry held the id, the new bundle would be dropped
as a duplicate and neither would contribute, with the failure reported against
the entry the operator did not touch. Two bundles sharing an id collide only when
**both are switched on**, which is a real clash over a live namespace.

**The `[[mcp]]` exemption stays, and one key inside it is checked anyway.** That
table cannot carry `deny_unknown_fields` — `McpServer` is `#[serde(flatten)]`-based
and serde refuses the two together — which is what keeps a newer server key
forward-compatible with an older binary. But `enabled` is the one key whose
misspelling *inverts* the operator's intent rather than merely being ignored, so a
near-miss spelling of that key alone is refused by name while an unrelated unknown
key in the same table is still accepted. The narrow check is what lets the broad
exemption survive.

**The two halves of the flag have opposite downgrade shapes, and the dangerous one
is silent.** A 0.69.0 binary reading a file that disables a *server* ignores the
key — under the exemption — and runs the server the operator switched off. A
0.69.0 binary reading a file that disables a *bundle* refuses the whole file,
because `[[plugin]]` is not exempt. Neither can be fixed from here: 0.69.0 is
published. An operator who downgrades removes the `enabled` keys first.

**A probe answers a different question from a preflight.** A policy preflight says
whether a server is *permitted* to start; a wrong command and an unreachable host
both pass it. `probe_mcp` starts one configured server, reports whether it
answered and what it offered, and shuts it down — reporting refused, not-started,
unreachable, timed out and answered apart, because those are four different
problems with four different fixes. It is bounded by the server's own
`timeout_secs` **including the handshake**, which the run loop's own connect does
not bound, so a server that accepts a connection and then says nothing is reported
rather than hanging the caller. A disabled server answers without being started.

## What an asking posture asks, and where it still cannot (0.70.0)

**`Effect::Ask` on `Act::Exec` reaches an approver.** It had been compared
against `Allow` and refused anything else, so `Ask` behaved as `Deny` — and
because `Policy::default()` sets `exec = Ask`, every git built-in was refused out
of the box with an error naming the program, which reads as a missing binary. The
comparison appeared at four sites and all four are fixed: the git spawn, every
MCP tool invocation, and both checks in a spawned agent's worktree creation (the
write *and* the `git` exec beneath it — gating only the first would have left
`worktree = true` dead under the default policy).

**A git built-in's `Act::Exec` target is the program alone.** The string put to
the policy and to the approver is `git`, so `deny_exec("git")` reaches every one
of them and `deny_exec("git commit*")` reaches none — no built-in ever presents a
joined argv the way `exec` does, and it could not: the argv these tools build
carries the `-c` hardening flags between the program and the sub-command, so the
text an operator would have to write is one this crate composes rather than one
they chose. A sub-command is denied by naming what it touches instead —
`deny_write(".git")` stops `git_commit` and `git_branch`, and a `deny_read` on a
path stops `git_add` staging it. Undocumented until 0.74.0, and stated here rather
than changed: adding a joined-argv target would break every policy written
`allow_exec("git")` under a deny-by-default tier.

**`git_worktree`'s path is contained, absolute spellings included (0.74.0).** It
is the one built-in that *creates* the path the model named, so it is asked of
`Workspace::check_path` directly rather than only of the gate, whose relaxation
for an absolute read or write target exists so `read_skill` can reach a bundle
outside the root. Under a policy with broad writes, `{"path":"/tmp/escaped"}` put
a full checkout outside the workspace and wrote an allow-shaped row to match. An
absolute path and a `..` that climbs out are now the same refusal, with the same
row, as every other path in this crate.

**A `Deny` posture still refuses without asking, and that is the arm that
matters.** The fix is not `!= Deny`; that would turn `Ask` into `Allow` for any
caller reaching the tool without a run loop. `Git` carries a flag saying its
caller has already gated, so the ungated path keeps the whole check and the gated
path cannot re-derive an `Ask` underneath an approval a human has just given.

**Two places still refuse an `Ask`, both on purpose.**

- **Starting a configured MCP server.** Connecting is configuration, not an act a
  human is standing by to approve, so the server's own binary must be allowed
  rather than asked. The consequence is worth stating plainly: under a bare
  `Policy::default()` no MCP server starts, so the tool-call approval above is
  only reachable once the server itself may run.
- **A verification gate's own commands.** `ExecGuard` has no approver, and there
  is no channel for a pause: `Verification::passes_guarded` is public and returns
  `Result<bool>`, and two of its four call sites are outside any run loop.
  `Policy::default()` allow-lists `rustc` and the test binary by name for exactly
  this reason. What 0.70.0 changes there is only the *reason given* — an `Ask`
  had been reported as "the policy forbids this", sending an operator hunting for
  a deny rule that does not exist.

**An approved exec has no filesystem effect, and approving it grants the
program.** A deferred `exec` resumes the way a deferred `net` does and not the
way a `write` does: nothing is written, the pending row is resolved, and the
program is allowed for the remainder of the run. Both halves are load-bearing —
routing an `exec` through the write path would create an empty file named after
the program and resume without ever running the command, and without the grant
the model re-issues the call and the approver is asked again for what they have
just allowed.

## What a step cap means now, and what a failed gate tells the next step (0.70.0)

**`StepCapReached` means only that nothing judged the work.** A run that reached
its cap having failed its criterion answers `RunOutcome::VerificationFailed`
instead. The two had been one answer, and a caller reading it as "raise
`max_steps`" was wrong precisely when the criterion was the problem. A run with
no `Verification`, and a run that never reached its gate, still answer
`StepCapReached` — `Verification::None` reports `Ok(false)` from the gate, so
"the gate returned false" is not "the criterion failed" and the distinction is
made explicitly rather than inferred.

**It is not terminal to `resume`.** Every outcome `terminal_outcome` maps is one
nobody in-process can undo: a pass, or a person's no. A failed criterion is
neither — the gate is re-run from scratch on the next step, so a raised budget, a
repaired machine or an edit in the workspace can turn it green, and a
`Verification::Command` failing because its runner is absent is a machine to fix
rather than a verdict on the work. Nothing here can know that a criterion will
never pass, and terminality needs evidence the crate does not have.

**A failing gate now tells the next step what it said.** The failing phase and
the recorded output arrive as an ordinary `ObsKind::Error` observation on the
ledger — not as a new prompt section, so the cached prefix keeps its shape — and
never on the last step, which has no request left to inform. It is bounded twice,
by a line count and by a character cap, because a tail is what is useful about a
runner's output and a cap is what makes it safe when a single line is enormous.
The trace records which attempt was informed and which was blind.

**A repeated failure is carried once.** The ledger accumulates for the whole run,
so a criterion failing the same way at every step would append a near-identical
block per step and re-send all of them on every request thereafter — a context
leak with a plausible-looking cause. Each append is compared against the last
one, and the comparison is on **what the gate said** — its phase and the tail of
its output — rather than on the section, which opens by naming the step and so
would never match itself. A failure that changes is carried again. The ledger is
never shortened to achieve this: it is tracked by a watermark index, and anything
that shortens it in place corrupts the store.

**Two gaps are stated rather than closed.** A failing `Verification::Review`
writes its reasons to `gate_attempts` and nothing to `sandbox_events`, so a
review still tells the model nothing. And the single-file loop has no ledger, so
it gains the outcome and not the feedback.

**A run's recorded path is readable (0.70.0).** `Store::run_file` returns the
value written when the run began — for a child spawned under `worktree = true`,
the child's **own** worktree rather than its parent's root, which is the whole
reason the column was written in 0.36.0 and the reason reconstructing it from the
parent's root is wrong. It is named after the column and not after the tree case:
for a single-file run it holds that file's path, so it is not always a directory.

## What a `[[hook]]` block commits this crate to (0.71.0)

`Hook` and `OnFailure` are public as of 0.71.0, and that is a wider promise than
the two names suggest. `Hook` derives `Serialize`, `Deserialize` and
`#[serde(deny_unknown_fields)]`, so **the shape of a `[[hook]]` block in a
configuration file or a plugin manifest is now part of this crate's public
contract**: its seven keys — `on`, `at`, `tools`, `append`, `run`, `on_failure`
and `timeout_ms` — are named here, and renaming one, changing what one accepts,
or making an existing file stop parsing is a break under the rules above.

This is recorded rather than discovered on purpose. The shape was already load
bearing before the type was public: operators author these blocks by hand,
`docs/guide/hooks.md` documents them, and `deny_unknown_fields` means a key this
crate stops recognising is a hard parse error rather than a warning. Making the
type public did not create the commitment; it made it visible, and this section
is where a future release is expected to look before editing the struct.

Adding a key remains compatible — `Hook`'s fields are private, reached through
accessors, so a new field breaks no struct literal, and a new optional key breaks
no existing file. `OnFailure` is `#[non_exhaustive]` for the same reason: it has
three variants today and a fourth is foreseeable, so a downstream `match` must
carry a wildcard arm from the start rather than acquire one in a later release.

Two things are deliberately *not* promised. The `Debug` rendering of a `Hook` is
not a format — nothing may parse it — and the order `Hooks::declarations()`
returns is declaration order within a scope, not a stable global ordering across
scopes.

## What a `[[bin]]` declares, and what it does not (0.73.0)

A `plugin.toml` may carry an array of `[[bin]]` tables, each with a `name` and a
`path` relative to the plugin root — the shape `[[agent]]`, `[[mcp]]` and
`[[hook]]` already have. `Plugin::bin()` returns each entry's name beside its
path joined onto the plugin root, absolute, in declaration order.

**Declaring an executable is not permission to run it.** This crate says what a
bundle contributes; it does not install a program, put one on a `PATH`, or hand
one to `exec`. Where a host places a contributed binary, and whether the policy
lets the agent invoke it, are the host's decisions and are governed by
`Act::Exec` like any other program.

**Only the user scope may contribute one.** `[[bin]]` joins `[[hook]]` and
`[[mcp]]` as a contribution a bundle declared from a file **inside the workspace**
may not make, for the same reason: it names a program this machine would run.
`io.toml` arrives with a `git clone`, and `io.local.toml` — on the trusted side of
this line until 0.74.0 — is a path in the workspace root the run's own agent can
write, which is how two ordinary writes carried a `[[hook]]` in with no refusal
anywhere on the path. Declaring a bundle from either file is still permitted; the
refusal is on what the *manifest* contributes. A manifest declaring one from such
a scope is refused **whole** and the bundle lands on `Plugins::dropped()` — none of
its other kinds are applied either.

**The path is validated lexically, and nothing is stat'd at load.** An absolute
`path`, or one climbing out of the plugin root with `..`, is refused at load. A
path that merely does not exist is not: an executable a bundle ships is
ordinarily produced by the bundle's own build, and a manifest that was valid on
Tuesday and dropped on Wednesday because a `target/` directory was cleaned is a
worse contract than one that reports what was declared. What a missing file
means is the caller's to decide.

**The name is not namespaced.** Every other contributed id — skills, templates,
agents, policy layers, MCP servers — becomes `<plugin>__<name>` as it loads. A
`bin` name does not, because it is the program a human or a model actually
invokes, and `rust-review__review` is not a name anyone types.

**`[[bin]]` is additive forward only, and this is the one break in the format's
history.** A `plugin.toml` written before 0.73.0 loads on 0.73.0 exactly as it
always did. The other direction does not hold: `Manifest` carries
`#[serde(deny_unknown_fields)]`, so a manifest declaring `[[bin]]` is refused by
io-harness 0.72.0 and earlier as an unknown field, and the bundle is dropped
**whole** — every skill, template, agent, hook, MCP server and deny layer in it,
not merely the `[[bin]]`. A bundle that must load on both ships two manifests or
requires `io-harness >= 0.73.0`.

`Plugin::contributions()` gained `"bin"` as its seventh name, ordered after
`"hooks"` and before `"policy"`. That vector is also
`EventKind::PluginLoaded`'s `contributions` field, so a consumer matching on it
sees a name it has not seen before.

## What `read_skill` reaches with a `path` (0.73.0)

`read_skill` takes an optional `path` beside its required `name`. Without one,
the tool behaves byte for byte as it did in 0.72.0: it reads the skill's own
body. With one, it reads a companion file the skill points at — a checklist, a
worked example, a longer reference — or lists a directory.

**The path resolves under the skill's root and may not leave it.** For a
plugin-contributed skill that root is the *bundle's* root, not its `skills/`
directory, so a bundle keeping `shared/` beside `skills/` is in reach of every
skill it contributes. For a standalone skill discovered through
`TaskContract::with_skills` it is the skill's own directory. `Skill::root`
carries it.

**Escape is refused, never resolved.** An absolute path, any `..` component, and
a symlink whose target canonicalises outside the root are each refused with an
observation rather than an error, and no read happens. The refusal names what
was asked for and never where it resolved to.

**The resolved path goes through the same `Act::Read` gate the body passes.** A
policy that denies the bundle's directory denies the companion file too; there
is no second door.

**A directory comes back as a listing.** Its entries, sorted, one per line, under
the same result cap a body is subject to. This is deliberate: it saves the model
the turn it would otherwise spend guessing a filename.

**A path that is not there is reported as not there**, distinctly from a refusal,
and does not enumerate the directory. A skill pointing at a file it no longer
ships is a typo, and reporting it as a refusal would send an operator hunting for
a breach that did not happen.
