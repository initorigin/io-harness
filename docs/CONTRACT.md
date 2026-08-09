# Public contract — IO Harness

What you may depend on, what may change, and what does not work today.

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. This
page exists because "pre-1.0" is usually where a library stops explaining
itself, and that is precisely when a dependent needs the explanation most.

## What is public

The public surface is everything re-exported from the crate root plus the items
reachable through the public modules it names.

The re-exported half — the 150 items a caller reaches as `io_harness::Thing` —
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
| Linux | Supported, full suite in CI | Native, a **chain** — Landlock, `bwrap`, namespaces, floor |
| Windows | Supported, full suite in CI | Native, AppContainer **and** Job Object |

Since 0.47.0 Linux is not one backend and a fallback but an ordered chain, and
the rung a host takes is the strongest one that can enforce what the run asked
for:

| Rung | Needs | Confines writes | Denies egress |
| --- | --- | --- | --- |
| `linux-landlock` | Landlock (kernel 5.13+) | Yes | Only at ABI 4+ (kernel 6.7) |
| `linux-bubblewrap` | a working `bwrap` | Yes | Yes |
| `linux-namespaces` | unprivileged user namespaces | Yes | Yes |
| `portable-floor` | nothing | No | No |

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

**`remember` is the one write the policy cannot see**, because it lands in this
crate's own store rather than in the workspace. It is refused explicitly for the
duration of the phase. `todo_write` and `ask_question` are not: neither changes
anything outside the run's own record of itself.

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
five-second `BUSY_TIMEOUT` since 0.12.0 — so there is no socket, no lock, no lease
and no on-disk migration.

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

**The crate does not know whether the owner is alive.** `runs.status = 'running'`
has never told a live process from a crashed one and this does not change it. A run
whose owning process died still reports what it was holding, and answering it
writes a row nothing will read until somebody resumes. Conversely, a run that is
genuinely live is not detected either: `resume_*` will refuse a request that has
already been decided, but it will not refuse one that is still being held by a
process that is still running.

**`run_events` is never pruned.** A long run's stream grows without bound. Deleting
it is the application's call; the crate has never pruned anything in the trace and
did not start with its newest table.

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
hooks and policy layers — no more.

**A plugin contributes data, never code.** There is no dynamic loading, and there
will not be. A `Tool` is an in-process trait implementation the application
registers; `dlopen` would make every safety property of this crate a function of a
directory a stranger wrote.

**Nothing verifies that a directory is what its author published.** No signature,
no checksum, no provenance. Nothing fetches, installs or updates a bundle either:
`[[plugin]]` names a directory that already exists on this machine, and
distribution is the application's. What *is* bounded is what an untrusted bundle
may contribute, which is a different and achievable claim.

**A project-scoped declaration may not contribute a hook or an MCP server.** Both
name a program this machine would run, and `io.toml` is the file a `git clone`
delivers — the 0.28.0 rule for `[[hook]]`, applied to a new declaration site. The
refusal is whole: a project-scoped bundle whose manifest declares one contributes
none of its other kinds either, because a half-applied stranger's manifest is the
failure the rule exists to prevent. `${cmd:}` inside a manifest is refused in
every scope.

**This does not narrow the standing `[[mcp]]` gap in `io.toml` itself.** A
project-scoped `io.toml` may still name an MCP command directly, and an unknown
key inside an `[[mcp]]` table is still accepted, because serde refuses `flatten`
beside `deny_unknown_fields`. A *plugin's* `[[mcp]]` is refused there, so the new
surface is stricter than the old one — deliberately: new surface starts closed,
existing surface is not narrowed under a release nobody asked to break.

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
the standing trust rule: a project-scoped file may narrow and may never widen, so
`mode = "full-access"` is refused there exactly as `force_floor = false` and
`allow_network = true` already are.

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
| Windows | Yes | Yes, to what the mode grants (0.47.0) | Yes (0.47.0) |

**Windows changed in 0.47.0 and it is the larger of the release's two surprises.**
Up to 0.46.0 a Job Object contained resources and nothing else, so a contained
Windows command got the caps and nothing more — no filesystem boundary and no
egress boundary, because a job object has neither facility. A contained run now
also gets an **AppContainer**: a low-box token that answers *no* to every
securable object it was not granted, with an explicit ACE per granted path, plus
the job object's limits and kill-on-close. Both halves, one backend,
`windows-appcontainer`.

What is granted is derived from what the run already resolved — the workspace
(read-only under `ExecMode::ReadOnly`), the system temporary directory and the
detected toolchain's cache directories — plus read-execute on the program's own
directory and the system root, which a process needs in order to start at all.
**The user's profile directory is deliberately not granted**: that is where
credentials live. Egress is the capability array: exactly `internetClient` when
the run's policy permits egress, and empty when it does not, so the denial is
the token's own.

**A program written against 0.46.0 on Windows has never had a filesystem
boundary**, and may be reading configuration or a sibling checkout from outside
the workspace without anything having refused it. Those reads now fail. The
remedy is `TaskContract::with_full_access()` or a mode that says what the run
actually needs. A host where the container cannot be built falls back to the Job
Object alone and reports `windows-job-object`.

One observable difference, stated rather than left to be found: on the
AppContainer backend **standard error arrives merged into `stdout`**. That
backend owns its own spawn — the container SID reaches a child only through a
process-thread attribute list, which no stable `Command` can carry — and
redirects both streams to one file rather than draining two pipes. **On Windows and on the portable floor
the `ExecMode` is therefore routed and reported and enforces nothing for the
filesystem** — it is a statement of what the run asked for, not of what the host
delivered, and `EventKind::Contained`'s `backend` is where the difference shows. On Linux the filesystem half is new in
0.40.0: before it, the backend unshared a mount namespace and remounted nothing
into it, so only the network namespace was real. A host whose kernel refuses the
remounts degrades to `PortableFloor` and **reports the floor** rather than naming
an isolation that was never applied — the recorded backend is the one that
applied, which is the point of recording it.

**On Linux the "Yes" in that table depends on the host's kernel policy, and one
common distribution says no by default.** The backend needs an unprivileged user
namespace. Ubuntu 24.04 ships
`kernel.apparmor_restrict_unprivileged_userns=1`, which refuses one, so on a
stock Ubuntu 24.04 host a contained command takes `PortableFloor`: the resource
caps still apply, and **the filesystem confinement and the egress denial do
not**. Nothing is hidden — `select().backend()` answers `PortableFloor` before
the run and the `SandboxEvent` rows record it afterwards — but a caller who
assumes the table's Linux row without reading the backend will not get what it
says. An operator who wants the real backend sets
`kernel.apparmor_restrict_unprivileged_userns=0`, which is what most other
distributions already ship; this repository's own CI does exactly that so its
Linux legs exercise the backend rather than the fallback.

**Egress under containment is all hosts or none.** The backends take one boolean:
a network namespace either exists or it does not. So the run's `Policy` decides
whether a contained command has a route out — `true` when the policy would permit
any `Act::Net`, `false` otherwise — and a policy that allows exactly one host
gives a contained command a route to **every** host. Per-host filtering is
unchanged for the crate's own tools and is not, and cannot cheaply be, applied at
the sandbox wall. `Effect::Ask` counts as *not* permitted: an approver answers
about one action at the moment it is attempted, and a namespace is built before
the command starts and cannot be renegotiated afterwards.

**A contained command may write only under what its mode grants.** Up to 0.45.0
that was the workspace and the system temporary directory alone, and the concrete
cost was a toolchain populating a user-level cache: a cold `cargo fetch` writing
`~/.cargo/registry`, or `npm install` writing `~/.npm`, failed under containment.
0.46.0 grants the detected toolchain's own cache directories, which is what
removes that cost for a project whose ecosystem this crate can name. For one it
cannot — or for a build that writes to a path the *caller* configured outside the
workspace — the answer is `with_full_access()`, said once at the call site.

**The `shell_start` / `shell_poll` / `shell_kill` handles are not contained.** A
handle outlives the call that made it, and what a resumed run should do with a
handle whose sandbox no longer exists is a design question rather than an
extension of this one. Only the foreground `shell` line and `exec` are contained.

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
`ask_question` and `propose_plan`. That includes the git readers and `list_dir`,
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

**It is refused in a project-scoped `io.toml`, exactly as any hook is**, inside a
`[profile]` too. A hook runs an argv on this machine and `io.toml` is the file a
`git clone` delivers; one that can stop a tool is strictly more dangerous than one
that appends a log line. Write it in `io.local.toml` or the user-scope file.

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

**It is not a diff, and nothing new is stored.** Both texts are what the store
already holds; computing and keeping hunks is a later release's work. A file the
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
7. **the crate's ending sentence, last, always.**

**Nothing a caller or a repository supplies is emitted after step 7.** The ending
of a classifying turn's opening is the sentence that lets a turn answer instead of
working, and the guarantee it produces — a `TurnKind::Reply` stages no step, no
gate, no checkpoint, no snapshot and no approval (0.37.0) — is one this document
makes to a reader who never sees the embedder's prompt. A composable prompt that
could contradict its own runtime's contract would not be a feature. What this
crate asserts is the composition: the sentence is present, byte-exact, and last,
under every `SystemPrompt` including `Replace("")`, and under any text a
repository carries. **What a model then does with a prompt is not a claim this
crate can make.**

**The ending moved in 0.45.0.** Until 0.44.0 it sat inside the base description,
which put the tool and skill catalogues after it. Every sentence a 0.44.0 prompt
carried is still carried, in the same words; one of them is in a different place.

`SystemPrompt::Replace` replaces the description and nothing else — the
catalogues, the guidance, the boundary and the ending are still composed around
it. There is no preset catalogue and there will not be one: a preset shipped by a
library is an opinion about model behaviour the library cannot test and cannot
withdraw.

## What the boundary section tells the agent, and what it leaves out (0.45.0)

When a run enforces a policy, the system block carries one line per act — read,
write, execute, network — naming that tier's default and the patterns the layers
rule on, grouped by what `Policy::explain` returns for each and attributed, on a
refusal, to the layer that produced it. It is the same vocabulary a `Refused`
event carries, so the prompt and the refusal name the same thing.

- **A permissive policy renders nothing**, and single-file mode never renders it,
  because single-file mode enforces no policy. A section describing an
  enforcement that does not happen would be worse than silence.
- **At most 24 patterns per act are named**, and the line says how many it did not
  name. The unnamed rules are enforced exactly the same.
- **`Effect::Ask` is rendered as itself** — allowed once a human or an approver
  says yes. Neither "allowed" nor "refused" is true of it, and both mislead.
- **A rule an approver remembers mid-run is not reflected**, because the prompt is
  composed once. The remembered rule *widens* what is permitted, so the section
  stays conservative rather than wrong. A plan gate is reflected: the narrowed
  policy is what the planning prompt describes, and the loop already switches
  prompts when the phase ends.
- **The section is not the boundary.** The `Policy` is, enforced in the tool and
  verification layers before any call runs. Telling the agent is an optimisation
  against paying a step per refusal, and no prompt text widens anything.

One further line names the run's `ExecMode` and the backend `sandbox::select`
**actually returned** on this host — not the one that was asked for. Where that is
the portable floor or a Windows Job Object, the line says the resource caps apply
and filesystem and outbound-network confinement do not, which on a stock Ubuntu
24.04 is the truth an agent would otherwise have to discover (0.40.0). A run under
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

## Limits that hold today

Stated here rather than discovered later. Each is real, each is known, and none
is fixed as of 0.35.0.

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
processes racing to approve the same plan means one of them hears about it. What
is *not* guarded is two processes each resuming the same run after a single
approval; that is the same property every other resume in this crate has, and
`Store::check_resumable` is the only lock there is.

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
7.6ms and 1.2ms respectively — roughly 90 to 380 nanoseconds per run. A caller
refreshing a panel every second over a very large trace should cache the answer;
this crate does not cache it for them.

**A pin binds a run, not a person, and not another process's caller (0.30.0).**
`MemoryEntry::pinned` stops the *agent* overwriting an entry through the
`remember` tool, and stops the caps evicting it. It is not access control:
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
  rendering, and is not incremental. The four built-in providers and `Fallback`
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
