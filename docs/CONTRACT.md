# Public contract — IO Harness

What you may depend on, what may change, and what does not work today.

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. This
page exists because "pre-1.0" is usually where a library stops explaining
itself, and that is precisely when a dependent needs the explanation most.

## What is public

The public surface is everything re-exported from the crate root plus the items
reachable through the public modules it names.

The re-exported half — the 145 items a caller reaches as `io_harness::Thing` —
is enumerated in [public-api.txt](public-api.txt), which a test compares against
the live crate on every run. That is the surface the deprecation cycle below
covers and the surface every item of which carries a worked example.

The module-path half is narrower in practice and wider on paper: items such as
`io_harness::context::assemble` or `io_harness::tools::Workspace` are `pub` and
do compile, but they are not individually snapshotted. Treat them as public and
stable in the same way, and expect the snapshot to grow to cover them rather than
the items to be withdrawn.

There is a third half the snapshot cannot show, because it enumerates re-exported
*names* and this is a *type*: **`rusqlite` is a public dependency of this crate.**
`Error::State(#[from] rusqlite::Error)` carries that crate's own error type, so a
`rusqlite` major bump changes this crate's public API whether or not anything here
behaves differently — which is exactly what 0.23.0 was. It is written down here
because `public-api.txt` lists `enum Error src/error.rs` and stops there: the
variant's payload is not a line in that file and never will be. The intent to
wrap it is under [Limits that hold today](#limits-that-hold-today).

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
| Windows | Supported, full suite in CI | **Native resource containment only** |

Since 0.24.0 a Windows run is contained by a Job Object. Memory, CPU and active
process count are real bounds, the whole process tree dies when the job handle
closes, and Windows is the first backend anywhere to enforce
`SandboxLimits::max_processes`.

**A Job Object contains resources and nothing else.** There is no filesystem
facility and no network facility in one. macOS confines writes to the working
directory and denies outbound network; Linux does the same through mount and
network namespaces; Windows does neither. So "sandboxed" on Windows means
resource-capped and does not mean access-confined, and the two must not be read
as the same claim.

**The access half is `AppContainer`, and 0.26.0 built it without making it the
default.** `io_harness::sandbox::appcontainer` creates a container profile,
derives its SID, grants a path to it with an explicit ACE, and spawns into it
through `CreateProcessW` with a `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
attribute list. On the Windows CI runner a payload inside one is refused a read it
was not granted and has no route off the machine, each against a negative control
that must succeed outside the container.

`Sandbox::select` still chooses the Job Object on Windows, so **the table above is
what a run actually gets** and is unchanged by this release. The obstacle is the
grant set, not the mechanism: an AppContainer is default-deny for reads, so the
workspace is the easy part and the executed binary, the toolchain, the redirected
temporary directory and every language's install tree are the rest. Naming those
for arbitrary ecosystems is a discovery problem 0.26.0 did not close, and a
default boundary that cannot run the payload would be worse than one a caller
reaches for deliberately. Recorded in `US-IO-HARNESS-0.26.0-I02`.

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

The preset carries the whole prefix the vendor documents, and five of the
twenty-one are not `/v1`:

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

## Limits that hold today

Stated here rather than discovered later. Each is real, each is known, and none
is fixed as of 0.23.0.

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

**`rusqlite::Error` is in the public API, and the intent is to take it out
(0.23.0).** `Error::State(#[from] rusqlite::Error)` carries the storage
dependency's own error type, which makes `rusqlite` a *public* dependency of
this crate: every `rusqlite` major bump is a breaking change here, whether or
not anything about this crate's behaviour changes. That is exactly what
happened in 0.23.0, whose entire content is a dependency move and which still
had to be published as a break.

The intent is to wrap it, so that the variant carries a type this crate owns
and a future `rusqlite` bump stops being a consumer break. It is stated here
rather than done in 0.23.0 on purpose: a migration release has to be reviewable
for exactly one property — that nothing behaves differently — and an
error-type redesign in the same diff destroys that. It is not yet slotted to a
version.

**What this means if you are writing code today.** Matching the variant and
ignoring its payload is safe and will stay safe:

```rust,ignore
Err(Error::State(_)) => { /* the store failed; the payload will change */ }
```

Reaching *through* the variant to use the payload as your own
`rusqlite::Error` is the thing that will break when the wrap lands, and it
already breaks on every `rusqlite` bump.

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
  the request has been one flattened user turn since 0.1.0 and the crate does not
  echo the vendor's partial assistant blocks back. The provider may therefore
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
- **A `Provider` that does not override `complete_streaming` streams nothing.**
  The default emits the finished text as one delta, which keeps a consumer
  rendering, and is not incremental. The three built-in providers and `Fallback`
  override it.
- **Steering is text, not authorization.** An operator's mid-turn message reaches
  the model exactly as a `TaskContract` constraint does, and every tool call it
  leads to is checked against the same policy by the same code. A `Steer` cannot
  change the policy, the budgets, the sandbox or the contract of a turn in flight.
- **A steer and an interrupt land at the next step boundary**, never where they
  were sent — the same rule `Flow::Cancel` has always had, for the same reason: in
  between, a tool call is in flight and a file may be half-written.
- **One session, one driver.** Two processes taking turns on the same session id
  concurrently is unsupported and undefended beyond SQLite's own busy timeout;
  their turns would interleave into one tree in an order nobody chose.
- **A session has no aggregate budget.** Every turn is a fresh run with its own
  ceilings, so `max_steps` on one turn does not bound the next. A conversation-wide
  limit is the caller's to enforce, per turn, from `Store::run_summary`.

**What configuration is, and is not (0.19.0, extended in 0.27.0 and 0.28.0).** `io.toml` is
a projection onto the typed API and never a second path into the run loop:

- **The typed API is the authority.** Every key lands in a type this crate
  already had — `Policy`, `SandboxConfig`, `Toolchain`, `PriceTable`,
  `McpServer`, `TaskContract` — and a file can express nothing the typed API
  cannot. A test asserts every key reaches a typed field. `[[provider]]` (0.27.0)
  is the same rule and not an exception to it: it yields a **`ProviderSpec`**, a
  value the application constructs a provider from, never a provider. `Provider::complete`
  returns `impl Future`, so the trait is not dyn-compatible and there is no
  `Box<dyn Provider>` for an accessor to return.
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
- **A project-scoped file may narrow the boundary and may never widen it
  (0.27.0).** Four keys are refused in `io.toml` when, and only when, the value
  written is the widening one: `policy.defaults.exec = "allow"`,
  `policy.defaults.net = "allow"`, `sandbox.allow_network = true`, and
  `sandbox.force_floor = false`. So is `${cmd:...}` anywhere in that file, including
  inside a `[profile]`. **So is the whole `[[hook]]` array (0.28.0)** — not its
  executing half: a hook that runs an argv is the `${cmd:...}` primitive arriving one
  release later, and a hook that appends is a write to a path a stranger chose, which
  is the same hazard by a shorter route. Each is accepted unchanged in
  `io.local.toml` and in the user scope, and the narrowing value of each of the four
  keys stays legal in `io.toml`. **This does not claim that a cloned repository is
  safe** — `[[mcp]]` still names a command, `[toolchain]` still names an argv, and a
  policy layer can still allow what the defaults did not. It is a specific narrowing
  of a specific hazard: the keys whose effect is to remove containment from a file
  that arrives with a `git clone`.
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
  load. What it finds lands in `TaskContract::constraints` — no new public field —
  and it is **untrusted text from the repository**: it reaches the model verbatim
  and grants nothing.
- **The `[toolchain]` override does not reach this crate's own run loop.** The
  harness detects for itself; `Config::toolchain(detected)` gives the embedding
  application the merged value. Wiring it into the loop needs a new
  `TaskContract` field, which is a break, and no release so far carries one.
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
  `[[agent]]`, the array is not in `APPENDING`: a later scope replaces it whole, so
  one `[[hook]]` in `io.local.toml` discards every hook the user-scope file declared.

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
  history: there is no undo of the last edit, no rewind to step 4, and no redo.
- **It is durable, and it is a new table.** `CHECKPOINT_FORMAT` stays 7, an older
  store opens and resumes unchanged, and a run that predates the release answers
  `NotRecorded` rather than restoring nothing quietly.
- **Four answers, and the fourth is the point.** `Restored`, `Removed` for a file
  the run created, `NotKept` for one whose previous contents were over the 1 MiB cap
  or were not UTF-8, and `NotRecorded` for a path this run never wrote. `NotKept`
  and `NotRecorded` change nothing at all, and a `NotKept` file is left exactly as
  the run left it — never truncated. Collapsing the two would tell a caller a file
  was untouched when the run had rewritten it and the harness cannot undo that.
- **Only `write_file` and `edit_file` snapshot.** A file changed by `shell`, by
  `exec`, or by a git built-in has no restore point. So does one whose bookkeeping
  row could not be written, which is warned about and swallowed exactly as an edit
  row is — `NotRecorded` means "no restore point", not "the run did not write it".
- **It obeys the policy the edit obeyed.** Restoring goes through
  `Workspace::write_file`; removing checks `Act::Write` itself and refuses anything
  that is not an outright allow, because a write is inspectable afterwards and a
  delete is not.
- **One path per call.** Undoing a whole run is a loop the caller writes, and it is
  not transactional.

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
