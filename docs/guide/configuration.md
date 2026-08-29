# Configuration — `io.toml`

Since 0.19.0, an operator configures the harness in a file instead of in Rust.
One format, four scopes, and every key lands in a type the crate already had.

```toml
# io.toml — committed, and inherited by everyone on the team.
[policy.defaults]
read = "allow"
write = "ask"
exec = "deny"
net = "deny"

[[policy.layers]]
name = "ops-baseline"
rules = [
  { act = "write", effect = "deny", pattern = "infra/*" },
  { act = "exec",  effect = "allow", pattern = "cargo" },
]

[run]
max_steps = 30
max_tokens = 400000
```

```rust
use io_harness::{run_with, ApproveAll, Config, TaskContract, Verification};

// One call reads the file. Nothing in this crate does it for you.
let config = Config::discover(&root)?;

let policy = config.policy().unwrap_or_default();
let contract = TaskContract::workspace("make the suite pass", &root)
    .with_verification(Verification::Command {
        argv: vec!["cargo".into(), "test".into()],
        expect_exit: 0,
    });
let contract = config.apply_to(contract);

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

`examples/config_live.rs` is that, end to end, against a real provider.

## The four scopes

Later wins, key by key:

| # | Scope | Where | Meant to be |
| --- | --- | --- | --- |
| 1 | defaults | whatever the typed API produces with no file | — |
| 2 | `user` | `$IO_CONFIG_HOME/io.toml`, else `$XDG_CONFIG_HOME/io/io.toml` or `~/.config/io/io.toml`; `%APPDATA%\io\io.toml` on Windows | one person's own machine |
| 3 | `project` | `io.toml` in the workspace root | **committed** |
| 4 | `local` | `io.local.toml` in the workspace root | **gitignored** |

That split is the point of the feature: a project sets a boundary its
collaborators inherit, and an individual overrides one key of it without editing
a shared file. **Commit `io.toml`. Never commit `io.local.toml`** — the crate's
own `.gitignore` carries it, and yours should too.

Discovery reads the root it is given and does **not** walk upward out of it. A
run's configuration comes from the directory the caller named, never from
somewhere above it that the caller did not choose.

`Config::sources()` reports which files were merged, in the order they were
applied, so an operator whose setting did not take effect can see which file won
rather than guess.

### How a value merges

| Shape | Rule |
| --- | --- |
| a scalar | the later scope replaces it |
| a table (`[sandbox.limits]`, `[prices.models]`, `[toolchain.<eco>]`) | merged key by key; a key the later scope never named is untouched |
| `[[policy.layers]]` | **appended** in scope order — a later scope adds a layer, it does not rewrite the boundary |
| any other array (`args`, a toolchain command) | replaced whole; a half-merged argv is not a command |
| `[[mcp]]` | replaced whole; a half-merged server definition is not a server |

Appending layers does not weaken anything. The `Policy` type's own rule still
holds across the seam: a later layer may add capability and may **never**
re-allow an earlier deny.

## Which file decided one key (0.30.0)

`Config::sources()` answers "what was read". The question an operator actually
arrives with is narrower — *the value is not the one I set, so who won it?* — and
four files merging key by key means the answer is per key, not per file.

```rust
use io_harness::config::Origin;

let at: &[Origin] = config.origin("policy.defaults.exec");
for o in at {
    println!("{:?} — {}", o.scope, o.path.display());
}
```

The key is a dotted path, spelled the way the file spells it:
`run.max_steps`, `sandbox.limits.max_wall_secs`, `policy.defaults.exec`,
`toolchain.cargo.test`. `Origin` is `{ scope, path }` and lives in
`io_harness::config` beside `Scope`, not at the crate root — it describes a file,
and the crate root is where the run's own types live.

**An empty slice is an answer.** No file set the key, so the value is this
crate's default, and a default has no file. Naming one would be an invention, and
"the harness decided this" is exactly what the operator needs to be told. Since
0.71.0 some of those defaults have *names* — `run.max_steps` unset is
`DEFAULT_MAX_STEPS`, in the `[run]` section below — and that changes nothing
here: a constant is not a file, and an origin reports files. A value you
can name and a value nobody wrote down are still the same answer.

**One entry, except where a key genuinely has more than one author.**
`[[policy.layers]]` and `[[agent]]` append across scopes rather than replace, so
`origin("policy.layers")` lists **every** contributing file in apply order. Both
files built that value; picking one as the winner would be a lie about a boundary
assembled from two.

**The deciding scope, not the last file read.** A key set only in the user file
reports the user file, even when a project file exists further down the merge and
names other keys entirely. The distinction is invisible when a key is set
everywhere and is the whole point when it is not — the case this exists for is
0.27.0's: an operator allowed `exec` in their own file, a cloned repository
narrowed it to `deny`, and until now nothing said so.

A `${env:...}` or `${cmd:...}` value reports **the file the substitution was
written in**, not the environment variable and not the helper. The file is what
decided to ask; the answer arriving from elsewhere is the mechanism. A literal and
a substituted value written at the same key in the same file report the identical
origin.

After `with_profile(name)`, a key the profile set reports the file the
`[profile.<name>]` block was written in — the profile decided it, so the profile's
file is the origin. The overlaid configuration reports no `profile.*` keys at all,
because it is no longer a configuration with a profile in it; it is the result of
applying one.

`Config::origins()` is the whole-list form, yielding `(&str, &[Origin])` in key
order, for a caller rendering every setting a workspace resolved to with the file
beside each. Keys no file named are absent rather than present-and-empty, so what
it yields is exactly what somebody wrote down.

`Config::from_toml` reports no origins, for the same reason `Config::sources()`
returns none: parsed text has no file behind it.

## Every key

### `[policy]` → [`Policy`](permissions.md)

```toml
[policy.defaults]
read = "allow"   # "allow" | "ask" | "deny"
write = "ask"
exec = "deny"
net = "deny"

[[policy.layers]]
name = "ops-baseline"          # name it after who wrote it
rules = [{ act = "read", effect = "deny", pattern = "infra/*" }]
```

`act` is `read`, `write`, `exec` or `net`. `effect` is `allow`, `ask` or `deny`.
`pattern` is the same glob the typed API takes.

The base is `Policy::default()` — the tiered default, with the secret patterns
already denied — not `Policy::permissive()`. A file that names a layer and
forgets a default must not end up enforcing *less* than a caller who wrote no
file at all.

### `[sandbox]` → [`SandboxConfig`](sandbox.md)

```toml
[sandbox]
allow_network = false    # `true` is refused in a project file — see the trust rule below
force_floor = true       # `false` is refused there too

[sandbox.limits]
max_cpu_secs = 60
max_wall_secs = 120
max_memory_bytes = 2147483648
max_processes = 0        # 0 means NO CAP, not a cap of zero
max_open_files = 512
```

Caps merge one key at a time onto the defaults, so lowering the wall clock keeps
the default memory cap. TOML has no null, and "absent" already means "inherit" —
so **`0` means no cap**.

### `[run]` → [`TaskContract`](../../README.md), via `Config::apply_to`

```toml
[run]
max_steps = 30
max_duration_secs = 900
max_tokens = 400000
max_retries = 2
exec_timeout_secs = 120
max_read_chars = 40000   # the largest file one read may carry (0.55.0)
skills = ".io/skills"

[run.retry]
base_ms = 500        # milliseconds, not {secs, nanos}
max_ms = 30000

[run.stall]
window = 3           # 0 switches stall detection off
max_replans = 1

[run.context]
max_tokens = 24000
share = 0.5

[run.commit_identity]
name = "io-harness agent"
email = "agent@io-harness.invalid"
```

A key this table omits falls to the contract's own default, and since 0.71.0
three of those defaults are named constants rather than literals buried in a
constructor:

| Constant | Value | Where it lands |
| --- | --- | --- |
| `DEFAULT_MAX_STEPS` | 8 | `TaskContract::new` |
| `DEFAULT_WORKSPACE_MAX_STEPS` | 12 | `TaskContract::workspace` |
| `DEFAULT_MAX_RETRIES` | 2 | both |

Two step budgets, kept apart on purpose: a repo-wide task spends turns finding
the files a single-file task is handed. The constructors read the constants, so
the number written here and the number a contract carries cannot drift. And a
caller who wants "the default, plus a little" writes it rather than guessing:

```rust
use io_harness::{TaskContract, Verification, DEFAULT_MAX_STEPS};

let patient = TaskContract::new("fix the parser", "src/parse.rs", Verification::None)
    .with_max_steps(DEFAULT_MAX_STEPS * 2);
```

`max_read_chars` is the one key here whose absence is not simply a default. Left
unset, the ceiling a read is measured against is derived from `[run.context]` —
a share of the token budget still *unspent*, so it falls as the run spends and
the same file is readable at step three and refused at step forty. Setting this
makes it a number that does not move, which is what a refusal a run's behaviour
turns on has to be. Both ceilings apply to every read, and the refusal names the
one that bit.

It is one of four keys a project-scoped `io.toml` may only **lower** — the other
three are the `[memory]` caps below. The keys a cloned repository may not widen
are otherwise refused by their widening value (`exec = "allow"`); a number has no
such value, so the smaller of the two wins instead. `io.local.toml` and the
user-scope file set them outright.

## `[memory]` — what a workspace's durable notes may hold

```toml
[memory]
max_entries = 64       # notes one workspace may hold
max_chars = 16000      # characters across all of them
max_entry_chars = 2000 # characters in any one note
```

These were the crate's own constants until 0.56.0 and they are still the
defaults, so a file that omits the table changes nothing. Each key is applied on
its own: naming one leaves the other two where they were.

**Read this before raising one.** The numbers are not a guess at what fits in a
database — they are chosen against what fits in a *prompt*. The memory block gets
a quarter of a turn's effective tokens, and at these caps the whole store fits
inside that share, so recall carries everything and nothing has to be selected.
Past that point selection begins, and what the model sees stops being "your
notes" and becomes "the notes that fit". **Which notes those are is no longer the
objection it was.** Since 0.57.0 the entries that survive the fit are chosen by
what the turn is about — the words of the run's goal and every path a tool has
already named — then by how many separate runs have carried them, then by the
store's own order, so a turn's own subject decides what it is handed. The block is
still printed in `(created_at, key)` order, which is why a store that fits its
share assembles the same bytes it did before. From a *selection* standpoint,
raising these is now safe.

What is left is time, and it is per turn rather than per write. Ranking a scope
normalises every entry it holds into a token set on every turn, because the
ranking is computed from the store and the turn rather than stored: about 1.106 ms
at the default 64 entries, 11.088 ms at 512 and 119.171 ms at 4,096, on the
machine named in [docs/MEASUREMENTS.md](../MEASUREMENTS.md). A `remember` — the
same work plus the duplicate check, which reads those entries again — is 1.946,
21.172 and 201.369 ms at those sizes, and that figure already contains the
eviction ranking a capped write does, measured alone at about 73 ms at 4,096 in
0.56.0. Both are linear in
`max_entries` and flat in the size of the recall table. A millisecond a turn is
nothing beside a provider call; 120 ms a turn, on every turn, is the honest reason
not to raise `max_entries` past what a workspace needs. Raise them because you
measured the block being short, not on principle.

The table bounds one *scope*. The scope above the workspace holds its own, so a
run recalling both can carry up to twice `max_chars` — inside the same block
ceiling, with the workspace's own notes taking the space first.

What the file does **not** set is the task: `goal`, `file`, `root` and `verify`
are what the caller is asking for now, not a property of the project.

### `[toolchain.<ecosystem>]` → [`Toolchain`](language-support.md)

```toml
[toolchain.cargo]
manager = "cargo"
test = ["cargo", "nextest", "run"]
```

Keyed on the detected ecosystem (`cargo`, `node`, `python`, `go`, …), so one file
carries an override for every ecosystem a team works in and only the matching one
applies. A command the file does not name keeps the shipped default.

### `[prices]` → [`PriceTable`](accounting.md)

```toml
[prices]
as_of = "2026-07-29"          # required: a price list with no date has no expiry

[prices.models."some-vendor/some-model"]
input = 3000000               # micro-units per MILLION tokens
output = 15000000
cache_read = 300000
cache_write = 3750000
per_server_tool_request = 10000
```

This is where a price comes from. The crate ships none — it cannot keep a
vendor's list accurate on its own release schedule — so until an operator writes
one down, every call is reported as unpriced rather than as free. A dimension the
file omits is an explicit zero.

### `[[provider]]` → `ProviderSpec`, via `Config::provider_spec` (0.27.0)

```toml
[[provider]]                              # the first entry is the provider
kind = "openrouter"                       # "openrouter" | "anthropic" | "openai" | "compatible"
model = "anthropic/claude-sonnet-4"
api_key = "${env:OPENROUTER_API_KEY}"     # optional: absent means the provider's own variable

[[provider]]                              # each later entry is the next link in the chain
kind = "anthropic"
model = "claude-sonnet-4"
```

`Config::provider_spec()` is the first entry and `Config::fallback_specs()` is the
rest, **in the order written** — the order is the configuration, not a detail of
it. The application builds from the spec:

```rust
let provider = match config.provider_spec() {
    Some(ProviderSpec::OpenRouter { model, api_key }) => /* ... */,
    Some(ProviderSpec::Anthropic { model, api_key }) => /* ... */,
    _ => OpenRouter::from_env()?,
};
```

A **spec**, not a provider. `Provider::complete` returns `impl Future`, so the
trait is not dyn-compatible and there is no `Box<dyn Provider>` for an accessor to
hand back. `Fallback` is generic over two type parameters and nests —
`Fallback::new(a, Fallback::new(b, c))` — so the caller assembles the chain the
file named in three lines of their own code.

`ProviderSpec` is `#[non_exhaustive]`: match it with a `_ =>` arm, because a later
release adds a variant.

**Printing a spec does not print the key (0.71.0).** `Debug` is hand-written:
every field is verbatim except `api_key`, which renders as `<redacted>` when the
file wrote one and `None` when it did not.

```text
Anthropic { model: "claude-sonnet-4", api_key: <redacted> }
```

That distinction is the whole operator-facing value of the field — "a key was
supplied and it was still wrong" and "no key was supplied, so the provider read
its own environment variable" are different misconfigurations with different
fixes — and nothing beyond it is said, not a length, not a prefix. `Serialize` is
deliberately **unchanged** and still writes the real key: a tool that saves the
operator's settings file must not replace their credential with a placeholder.

Unlike `[[policy.layers]]` and `[[agent]]`, the chain is **replaced** by a later
scope rather than appended to. A half-appended fallback chain is not a chain.

#### `kind = "compatible"` — any OpenAI-shaped endpoint (0.29.0)

A fourth kind names an endpoint instead of a vendor: a base URL, an auth style, a
key and a model are the whole of what an OpenAI-shaped API needs, so a hosted
vendor nobody has heard of and a model running on this laptop are the same entry
with different values.

```toml
[[provider]]
kind = "compatible"
model = "llama-3.3-70b-versatile"        # required
preset = "groq"                          # exactly one of preset / base_url
base_url = "http://localhost:8000/v1"    # exactly one of preset / base_url
api_key = "${env:GROQ_API_KEY}"          # optional; omit for a local runtime
auth = "bearer"                          # optional: "bearer" | "none"
name = "lab"                             # optional trace label
reference_prices = false                 # optional, default false
```

Those two middle lines are a choice and not a pair. **Exactly one of `preset` and
`base_url` is required**: writing both, or neither, is refused naming the entry's
index in the array, because "a provider entry is wrong" is not something an
operator with four of them can act on.

`preset` is a vendor this crate already knows the endpoint of:

```
cerebras deepseek fireworks gemini groq minimax mistral moonshot perplexity
qwen together xai zhipu jan koboldcpp llamacpp lmstudio localai ollama sglang
vllm
```

An unknown `preset` is refused **listing the ones that exist**, so a typo is one
read away from its own fix rather than a search through this page.

`auth` says how the key is presented and defaults to `"bearer"`; a preset supplies
its own, so a local runtime that wants no `Authorization` header at all is already
`"none"` without the key being written. `api_key` is optional for the same reason —
a runtime on `localhost` has nothing to authenticate to. `name` is a label for the
trace and changes no behaviour.

`reference_prices = true` opts into an outbound request to a host **the file did
not name**, to ask what the endpoint's models cost. That host is governed by the
same `Act::Net` rules as every other endpoint this crate dials: a policy that does
not allow it means the run refuses, rather than the lookup quietly not happening.
It is off by default.

### `[app]` — stored, never validated (0.27.0)

```toml
[app.cli]
theme = "dark"
width = 100

[app.studio]
open_tabs = ["trace", "policy"]
```

The one section this crate keeps and does not understand, so an application layer and
your own program keep their settings in the same file instead of inventing a
second format beside it. Read it into your own type:

```rust
let cli: CliSettings = config.app("cli")?.unwrap_or_default();
```

**Nothing here is validated.** An unknown key under `[app]` is your business, not
an error. Every other section still rejects what it does not know — this is one
hole with a wall around it, and the wall is the point.

### `[profile.<name>]` — a named overlay (0.27.0)

```toml
[run]
max_steps = 30
max_retries = 4

[profile.cheap]
run = { max_steps = 5 }

[profile.careful]
run = { max_steps = 120 }
policy = { defaults = { write = "ask" } }
```

```rust
let config = Config::discover(&root)?.with_profile("cheap")?;
```

A profile is the same file format again, overlaid through the same merge the
scopes use: a scalar replaces, a table merges key by key, an array replaces whole.
**Scopes merge first, and the profile applies to the result** — so a profile in any
scope beats a base key in every scope.

A name the file does not carry is an error naming it: a `--profile` argument that
silently does nothing is the same failure as a typo in a key. A typo *inside* a
profile is rejected at load even when that profile is never selected, because a
profile body is validated as the file format. Profiles do not compose and do not
nest.

### `[instructions]` — discovering a repository's own rules (0.27.0, 0.45.0)

```toml
# Nothing at all: `AGENTS.md` is discovered anyway, as of 0.45.0.

[instructions]
files = ["AGENTS.md", "docs/HOUSE-STYLE.md"]   # or name your own, relative to the root

[instructions]
files = []                # ...and this is how a project opts out
```

`Config::discover` reads each file that exists and `Config::apply_to` lands them in
`TaskContract::instructions`, one per file, each naming the file it came from — so
the instructions a repository already carries reach the model without being pasted
into a goal string. They ride in the **system block**, inside a delimited section
framed as the repository's own guidance.

**Since 0.45.0 the search runs whether or not this table is present**, so a
repository carrying the file every other agent reads and no `io.toml` at all is
read. An explicit `files = []` is the opt-out, and it is distinct from an absent
table.

**Before 0.45.0 the text landed in `TaskContract::constraints`** and rode in the
user turn on every step. A constraint is a rule the goal is checked against; this
is guidance the agent reads. A caller that read `constraints` to find a
repository's `AGENTS.md` reads `instructions` now.

A named file that does not exist is **skipped**. This is discovery, not
substitution: the "resolve or fail" rule that governs `${...}` deliberately does
not apply here.

### `[[hook]]` → `Hooks`, via `Config::hooks` (0.28.0)

```toml
[[hook]]                       # an audit log: every event, one JSON line each
append = "audit.jsonl"

[[hook]]                       # a formatter, after every step that changed a file
on = ["step"]
run = ["cargo", "fmt"]

[[hook]]                       # a local policy check that can stop the run
on = ["tool_call"]
run = ["./scripts/allowed.sh"]
on_failure = "cancel"
timeout_ms = 2000
```

Each table names the events it wants and one thing to do with them. `Config::hooks()`
returns a `Hooks`, which **is** an `Observer`, so you install it exactly as you would
install your own — and nothing in the run loop changed to make that work.

`on` names events by the wire tags `EventKind` serializes to, and an absent `on` is
every event. `append` writes one JSON line per matching event, `run` spawns a fixed
argv with that JSON on the child's stdin, and exactly one of the two is required.
Both resolve against the discovery root, which is also the child's working
directory. There is no shell: the argv is an array and reaches the process unsplit.

**`[[hook]]` is refused in the project scope**, whole, for the reasons in
[A project file may narrow, and may never widen](#a-project-file-may-narrow-and-may-never-widen-0270)
below. The full page is [Hooks](hooks.md).

### `[[mcp]]` → [`McpServer`](mcp-and-network.md)

```toml
[[mcp]]
id = "github"
transport = "stdio"
command = "github-mcp-server"
args = ["stdio"]
timeout_secs = 60

[[mcp]]
id = "search"
transport = "http"
url = "https://mcp.example.com"
[mcp.headers]
Authorization = "Bearer ${env:SEARCH_TOKEN}"
```

Declaring a server does not start it. Its binary is still an `Act::Exec` check
and its host still an `Act::Net` check, so without a policy rule naming them the
run refuses before the server process exists.

### `[web]` → `WebAccess`, via `Config::apply_to`

```toml
[web]
search = true                          # let the provider run a search
fetch = false                          # let the provider fetch a URL
max_uses = 5                           # cap on provider-executed requests per completion
allowed_domains = ["docs.rs"]          # empty means the vendor's default: anywhere
blocked_domains = ["evil.test"]        # empty means no block-list
```

The table lands on the same `WebAccess` the programmatic builder produces, and it
merges key by key like any other table: a local scope writing `search = false`
switches it off without dropping the project's `max_uses` or domain lists.

Nothing here is on by default, and writing the table is not the same as switching
it on — a file that carries `[web]` with `search = false` has stated a decision,
and the contract records it as one.

**The boundary is declared, not enforced.** The provider dials the URL, so
`Act::Net` never sees it and the domain lists are filling in the *vendor's* filter.
A caller who needs the boundary enforced in this process must leave this off.

## Secrets: `${env:...}`, `${file:...}` and `${cmd:...}`

Any string value may name an environment variable, a file, or a command to run:

```toml
[[mcp]]
id = "search"
transport = "http"
url = "https://mcp.example.com"
[mcp.headers]
Authorization = "Bearer ${env:SEARCH_TOKEN}"
X-Key = "${file:./secrets/mcp-key}"
```

`${cmd:...}` runs a credential helper and takes its trimmed stdout:

```toml
# io.local.toml — gitignored, and yours.
[[mcp]]
id = "search"
transport = "http"
url = "https://mcp.example.com"
[mcp.headers]
Authorization = "Bearer ${cmd:op read op://vault/mcp/token}"
```

There is **no shell**. The value is split on whitespace and the first word is the
program, so a `;`, a `|` or a backtick in it is an argument rather than a second
command. A non-zero exit is a failure, because a helper that failed did not produce
a credential.

**`${cmd:...}` is refused in the project scope.** `io.toml` is committed and arrives
with a `git clone`, and a run-this primitive in that file would run on the first
`Config::discover` of a repository you have not read. Write it in `io.local.toml`
or in your user-scope file. `Config::from_toml` is the project scope too, and
refuses it for the same reason.

`${file:...}` resolves against the directory of the file that wrote it, and its
contents are trimmed. All three forms **resolve or fail**: an unset variable, an
unreadable file, and a value that resolves to nothing are each an error naming
the key and the file. None of them is ever an empty string — an empty string in a
boundary rule is a rule that matches nothing, and a config that silently disarms
itself is the worst thing this feature could do.

## Printing a config does not print what is in it (0.71.0)

A resolved `Config` holds everything the substitutions above fetched: the
provider key, the `Authorization` header a `${cmd:}` helper produced, whatever a
`${file:}` read. Until 0.71.0 `Debug` was derived, so one
`tracing::debug!("{config:?}")` wrote every one of them out — *twice*, once from
the typed `File` and once from the merged `raw` table that both hold the same
resolved string. Every release before this one did that.

`Debug` on `Config`, on the `File` behind it and on `ProviderSpec` is hand-written
now, and none of the three prints a leaf a substitution could have filled:

| Printed | As |
| --- | --- |
| which files were read, in merge order | verbatim — `sources` holds paths, not values |
| `origins` | verbatim — a map from a dotted key *name* to the files that decided it |
| a section a file set (`[policy]`, `[run]`, `[web]`, …) | `<set>`, or `None` where no file mentioned it |
| the merged `raw` table, and `[app]` | key names, nesting and leaf **kinds** — `string`, `integer`, `boolean` — never a leaf |
| `[[mcp]]`, `[[lsp]]`, `[[agent]]` | their ids and names only; each carries headers, a child environment or an argv |
| `[[hook]]`, `[[plugin]]` | counted |
| `[[provider]]` | in full, through `ProviderSpec`'s own impl, which withholds the key |

`[[provider]]` is the one array rendered whole because the model a run is about
to use is the most-asked question of a configuration, and that impl already
holds the key back. A section's contents are omitted rather than filtered key by
key, because a list of the fields safe to print goes stale the next time a
section gains a field.

The shape is still there — an operator debugging a config wants to know that
`policy.defaults.exec` was set, in which file, and to what *kind* of value — so
this is a narrower print rather than a blank one. And it is `Debug` alone:
`Serialize` writes what the operator typed, for the reason given above.

## A project file may narrow, and may never widen (0.27.0)

`io.toml` is committed. It travels with a `git clone`, and until 0.27.0 a cloned
repository's file could switch off the parts of the boundary that stop it mattering.
Four keys are therefore refused in the **project** scope, and only when the value
written is the one that widens:

| Key | Refused in `io.toml` when it says | Still legal there |
| --- | --- | --- |
| `policy.defaults.exec` | `"allow"` | `"ask"`, `"deny"` |
| `policy.defaults.net` | `"allow"` | `"ask"`, `"deny"` |
| `sandbox.allow_network` | `true` | `false` |
| `sandbox.force_floor` | `false` | `true` |

Plus `${cmd:...}` anywhere in the file, and — since 0.28.0 — the whole `[[hook]]`
array. Not the executing half of it: a hook that runs an argv is the `${cmd:...}`
primitive arriving one release later, and a hook that appends is a write to a path
a cloned repository chose, which is the same hazard by a shorter route. The refusal
names the key, the file, and where to write it instead — `io.local.toml` or your
user-scope file, where all of them are accepted unchanged. A widening key or a hook
hidden inside `[profile.<name>]` is refused too; the profile is applied later, and a
check that only looked at the base would let it reach the same place by a different
path.

Value-dependent rather than key-dependent, deliberately: a project file *denying*
`exec` is exactly what the project scope is for, and a rule that refused the key
outright would forbid the good half to stop the bad one.

**What this does not claim.** Not that a cloned repository is safe. `[[mcp]]` still
names a command, `[toolchain]` still names an argv, and a `[[policy.layers]]` entry
can still allow what the defaults did not. This is a specific narrowing of a
specific hazard — four keys, one substitution and one array, no more — and it is the
file half of a boundary whose enforcing half is still the `Policy` you loaded.

## An unknown key is an error

```
io.toml: key `run.max_stepz`: unknown field `max_stepz`
```

A typo in a permission rule that is silently ignored leaves an operator believing
in a boundary that is not there. Every section rejects what it does not know.

## The limits, stated plainly

**A config file is not a security boundary against the agent.** The boundary is
the `Policy` the caller loaded. A file is where that policy was written down. If
the agent can write to the workspace root, it can write an `io.toml` — and what
stops that mattering is *when* the file is read, not the file's permissions.

**Nothing is loaded implicitly, and that is the guarantee.** No entry point in
this crate discovers a config on its own; the caller calls `Config::discover` and
decides what to do with the result. The harness never re-reads it, so a config
the agent writes *during* a run cannot widen the boundary that run is already
under. A config the agent wrote is picked up by the *next* load, which is the
caller's own act — so treat a workspace the agent can write as a workspace whose
`io.toml` the agent can propose.

**The `[toolchain]` override is for the embedding application, not for this
crate's run loop.** The harness detects a project's ecosystem for itself and does
not consult a config. Reaching it would mean a new `TaskContract` field, which is
an addition rather than a break — the type is `#[non_exhaustive]`, and 0.62.0 added
`lease_ttl` that way. `Config::toolchain(detected)` gives every caller the merged
value today; the run loop wiring is a later release's job all the same.

**Two sections do not reject an unknown key, and they are these two.** A
`[[mcp]]` table does not, because `McpServer` is `#[serde(flatten)]`-based and
serde refuses `flatten` beside `deny_unknown_fields`. `[app]` does not, because
that is the whole point of it — the crate stores it and never looks inside. Every
other section rejects what it does not know. Two exceptions listed together are a
rule with edges; one listed and one hidden is a rule nobody can trust.

**One key inside `[[mcp]]` is checked anyway, and 0.70.0 says why.** The exemption
stays — it is what keeps a newer server key forward-compatible with an older
binary — but `enabled` is the one key whose misspelling silently *inverts* what the
operator asked for. `enabld = false` under the exemption is swallowed, and the
server they meant to switch off runs. So a near-miss spelling of that one key is
refused by name, while an unrelated unknown key in the same table is still
accepted. The narrow check exists precisely so the broad exemption can survive.

**`${cmd:...}` has no timeout.** A credential helper that hangs hangs your own
`Config::discover`, before any run exists, with your own privileges. That is
visible rather than silent, and it is the reason there is no timeout knob rather
than an excuse for one: when a consumer meets it, a timeout is a key argued on its
own terms.

**A named instructions file that is absent is skipped silently.** `[instructions]`
is discovery, so a file that is not there is not an error and a typo in a filename
is not reported. This is the one place the "resolve or fail" rule above does not
apply, and it is deliberate: `AGENTS.md` is present in some repositories and not in
others, which is the normal case rather than the failure.

**A discovered `AGENTS.md` is untrusted text, and since 0.45.0 it rides somewhere
more authoritative.** It comes from the repository, it reaches the model verbatim
in the system block, and it grants nothing: the boundary is still the `Policy` you
loaded, enforced before any call runs. What bounds it is structural — the text is
delimited, framed as the repository's guidance rather than the operator's
instruction, and emitted before both the boundary section and the crate's own
ending, so it cannot be the last word. Treat a workspace whose instructions file
you have not read the way you would treat any other text a stranger wrote into
your prompt.

**A project file may narrow and never widen, and that is all it does.** See the
section above for the four keys, and for the sentence it is not.

**An origin reports the merge and does not take part in it.** `Config::origin`
changed what a caller can *see* about a resolution in 0.30.0 and changed no
resolution: every key still lands on the value it landed on before. Nor is it half
of a writer — there is still nothing in this crate that edits an `io.toml`, so
"which file would I change" is a question it answers and not one it acts on.

**A hook runs inside the run loop and blocks it.** `Observer::event` is synchronous,
which is what lets `on_failure = "cancel"` stop anything at all — and it means a
`run` hook on a hot event, `token` above all, spawns a process that often. An
executing hook is bounded by `timeout_ms` and killed past it; nothing bounds how
many events you point one at. See [Hooks](hooks.md) for the rest.

**Hooks do not accumulate across scopes.** `[[hook]]` is not in the appending set
that `[[policy.layers]]` and `[[agent]]` are in, so a later scope replaces the array
whole: one hook in `io.local.toml` discards every hook your user-scope file
declared.

**A `preset`'s base URL is a default this crate ships, not a fact about the
vendor.** It was right on the day the release was cut and the vendor is under no
obligation to keep it so. That is why `base_url` exists beside `preset` and takes
the same entry: a vendor that moves its endpoint is one line in your own file,
today, rather than a release of this crate and a version bump you wait for. The
preset list is a convenience over the general key, never a gate in front of it.

**`reference_prices = true` turns on egress to a third host.** The file names one
endpoint; this key dials a different one to ask what that endpoint's models cost,
and a price it returns is that host's opinion rather than an invoice. It is off by
default, it is `Act::Net`-checked like anything else this crate dials, and a policy
that denies that host fails the run instead of silently reporting every call
unpriced.

**Redaction is `Debug`'s alone, and it is not a secret store.** The hand-written
impls keep a resolved credential out of a log line. They do not keep it out of
memory, out of `Serialize`, or out of the accessor that hands it to you —
`config.provider_spec()` returns the real key, because that is what a caller
asks for it. One specific leak is closed, formatting, and nothing else is
claimed. A `${cmd:}` helper's output is still a string in this process.

**`${` always begins a substitution.** There is no escape, so a literal `${` in a
value — in a glob pattern, say — is not expressible. An unknown prefix is an
error rather than a passthrough, which is what keeps a typo from reaching a
policy rule as text.

**The scopes are still four.** There is no `include`, no `extends`, no `$schema`,
no JSON or YAML form, no parent-directory search, and no reload. `IO_CONFIG` names
the **user-scope file** outright, ahead of `IO_CONFIG_HOME` and every platform
convention — it names a scope rather than bypassing the merge, so a project file
still wins the keys it names. A profile is a section of the same file that was
already read, not a fifth scope and not a second read.

## `[[lsp]]` — language servers (0.52.0)

```toml
[[lsp]]
id = "rust"
command = "rust-analyzer"
args = []
extensions = [".rs"]
timeout_secs = 60
```

One table per server. `extensions` decides which files it answers for; an empty
list answers for every file, and where two servers claim one suffix the first in
declaration order wins. `timeout_secs` bounds every request to it, and it is also
what bounds how long the run will wait for a slow start-up.

Allowed in a project-scoped `io.toml`, for the reason `[[mcp]]` is: starting the
server is an `Act::Exec` check on `command`, so the boundary is the policy the
caller loaded rather than the scope of the file that named the binary. Unlike
`[[mcp]]`, a misspelled key here **is** rejected by name — there is no
`#[serde(flatten)]` in this table to forbid it.

A narrower scope replaces the whole set rather than appending to it, the way
`[[hook]]` and `[[provider]]` do: the servers that run are the servers of one file.
