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
allow_network = false
force_floor = false

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

## Secrets: `${env:...}` and `${file:...}`

Any string value may name an environment variable or a file:

```toml
[[mcp]]
id = "search"
transport = "http"
url = "https://mcp.example.com"
[mcp.headers]
Authorization = "Bearer ${env:SEARCH_TOKEN}"
X-Key = "${file:./secrets/mcp-key}"
```

`${file:...}` resolves against the directory of the file that wrote it, and its
contents are trimmed. Both forms **resolve or fail**: an unset variable, an
unreadable file, and a value that resolves to nothing are each an error naming
the key and the file. None of them is ever an empty string — an empty string in a
boundary rule is a rule that matches nothing, and a config that silently disarms
itself is the worst thing this feature could do.

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
not consult a config, because reaching it would mean a new `TaskContract` field —
a break this release does not carry. `Config::toolchain(detected)` gives io-cli,
io-studio and any other caller the merged value today; the run loop wiring is a
later release's job.

**An unknown key inside a `[[mcp]]` table is not rejected.** `McpServer` is
`#[serde(flatten)]`-based and serde refuses `flatten` beside
`deny_unknown_fields`. Every other section rejects what it does not know; that
one accepts and ignores it.

**`${` always begins a substitution.** There is no escape, so a literal `${` in a
value — in a glob pattern, say — is not expressible. An unknown prefix is an
error rather than a passthrough, which is what keeps a typo from reaching a
policy rule as text.

**The scopes are fixed.** There is no `include`, no `extends`, no `$schema`, no
JSON or YAML form, no parent-directory search, and no reload. Each would be a
second mechanism doing the job the four scopes already do.
