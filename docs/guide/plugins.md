# Capability bundles

*A directory is a bundle of capabilities, and everything it contributes says so.*

Since 0.35.0 a directory holding a `plugin.toml` can contribute skills, prompt
templates, agent definitions, MCP servers, lifecycle hooks and deny-only policy
in one piece. Before it, each of those had a discovery path of its own — a
`with_skills` directory here, a `Templates::discover` there, three arrays in
`io.toml`, and a policy stack the application assembled — so handing a coherent
set of them to somebody else meant six manual steps and no record afterwards
that any of it came from anywhere but the operator.

## The manifest

```toml
# bundles/rust-review/plugin.toml
name = "rust-review"                 # the id; `[a-z0-9][a-z0-9-]{0,31}`
description = "Everything our Rust reviews need."
version = "1.2.0"                    # documentation only

skills = "skills"                    # a directory, relative to this file
templates = "templates"

[[agent]]
name = "reviewer"
model = "cheap-model"
deny_write = true

[[mcp]]
id = "docs"
transport = "stdio"
command = "docs-mcp-server"

[[hook]]
on = ["refused"]
append = "audit.jsonl"

[policy]
layers = [{ name = "no-secrets", rules = [
    { act = "write", effect = "deny", pattern = "secrets/**" },
] }]
```

Every contribution type here is the one `io.toml` already deserializes. A
manifest is the configuration file's vocabulary rather than a second one, so an
unknown key is refused by name the same way and for the same reason.

## Declaring one

```toml
# io.local.toml
[[plugin]]
path = "bundles/rust-review"
```

`[[plugin]]` entries **accumulate** across the three scopes, the way
`policy.layers` and `[[agent]]` do: a project's bundles and an individual's own
are both wanted. A relative `path` resolves against the discovery root — the
project the harness was pointed at, not the directory the declaring file happens
to live in.

## Installing what it contributed

Loading is the caller's, once, before the run:

```rust,ignore
let config = Config::discover(root)?;
let plugins = config.plugins();

let contract = plugins.apply_to(TaskContract::workspace(goal, root));  // skills, agents, MCP
let policy = plugins.apply_to_policy(my_policy);                       // deny layers
let hooks = plugins.apply_to_hooks(config.hooks(), root);              // lifecycle hooks
let templates = plugins.templates()?;                                  // rendered before a run
```

Three calls rather than one because this crate has three installation points and
never installs anything implicitly. A bundle that contributed nothing of a kind
makes that call the identity.

## Who may contribute what

`io.toml` is committed and arrives with a `git clone`, which is why 0.28.0
refuses `[[hook]]` there outright. A bundle is a stranger's directory one step
further out, so the same rule governs it:

| Declared in | skills, templates, agents, deny policy | `[[hook]]`, `[[mcp]]` |
| --- | --- | --- |
| `io.toml` (project) | yes | **no** |
| `io.local.toml` (local) | yes | yes |
| user-scope file | yes | yes |

The refusal is **whole**. A project-scoped bundle whose manifest declares a hook
contributes none of its other five kinds either — a half-applied stranger's
manifest is the failure the rule exists to prevent.

Two rules apply in every scope. `${cmd:}` inside a manifest is refused wherever
the declaring file lives, because a bundle is a third party's file however it was
named. And plugin-supplied policy may only **narrow**: a `[policy]` block may
carry layers of `deny` rules and nothing else, so an `allow` rule, an `ask` rule
or a `defaults` block drops the bundle.

## Looking at a directory before declaring it (0.71.0)

An installer downloads a bundle and then has to write a `[[plugin]]` line into
somebody's configuration before anything will tell it whether that bundle loads.
That is backwards: the operator finds out by running a job. `Plugins::inspect`
answers first.

```rust,ignore
use io_harness::config::Scope;
use io_harness::Plugins;

let plugin = Plugins::inspect(Scope::User, "downloads/rust-review")?;
println!("{} contributes {:?}", plugin.id(), plugin.contributions());
```

No declaration file is written and `Config::discover` is never called. Every
check a load runs, runs here: the id grammar, the trust rule for `scope`, the
narrowing rule on `[policy]`, the `[[hook]]` validator, and `${cmd:}` refused in
a manifest wherever it came from. The error is the string that would have
appeared on `Plugins::dropped()`, so a preflight and a load cannot disagree.

It is **fallible** where loading a declared set is not, and deliberately: a set
that dropped one bundle still has the others, while a caller asking about one
directory is asking a yes-or-no question.

### `scope` is the answer, not a formality

It is the scope the caller intends to *declare* the bundle from, and the result
differs by it — this is the table above as an API rather than a quirk of the
loader:

| `scope` | A manifest carrying `[[hook]]` or `[[mcp]]` |
| --- | --- |
| `Scope::User`, `Scope::Local` | returned like any other contribution — these are the operator's own files |
| `Scope::Project` | **refused whole**, not shortened — `io.toml` arrives with a `git clone` |

A bundle that would load from one file and not the other is exactly what an
installer has to tell an operator *before* it writes anything, and marketplace
install semantics are the reason: "this bundle wants to run a program on your
machine, so it can only go in your own file" is a sentence somebody has to be
shown. `${cmd:}` in a manifest is refused at either scope, so no choice of scope
buys it.

What comes back is the same `Plugin` a load produces, with an accessor per
contribution kind — `skills_dir`, `templates_dir`, `agents`, `mcp_servers`,
`hooks`, `policy_layers` — beside `id`, `description`, `version` and
`contributions`. `hooks()` is 0.71.0's: `contributions()` has advertised
`"hooks"` since 0.35.0 and there was no way to read the tables behind it, so an
installer could say a bundle contributes hooks and not say what they do. The
same accessors are on `Hook` itself; see [Hooks](hooks.md#reading-the-hooks-that-are-installed-0710).

A bundle's hooks are not namespaced, and nothing was left out: a `[[hook]]`
contributes no name for an id to prefix — it names events, a path and an argv,
and all three belong to the operator's tree rather than to the bundle's.

## Attribution

Every contributed name is namespaced `<plugin>__<name>` as it loads — skills,
templates, agents, policy layers and MCP server ids alike. Nothing was added to
the store to make a contribution traceable; the plugin is simply inside the
strings the trace has recorded since 0.4.0:

| Ask | Answered by |
| --- | --- |
| which bundle refused this write? | `PolicyEvent.layer` — `rust-review__no-secrets` |
| which bundle's server was called? | `McpEvent.server` — `rust-review__docs`, and the tool `mcp__rust-review__docs__search` |
| which bundle's agent spent this? | the child run's agent name — `rust-review__reviewer` |
| what did this run load at all? | `EventKind::PluginLoaded` / `PluginDropped`, on step 0 |

It also makes a collision impossible rather than unlikely: a bundle cannot occupy
a name the operator already uses, and two bundles cannot occupy each other's,
because ids are unique, bounded, and may not themselves contain `__`.

## A broken bundle costs exactly itself

`Config::plugins()` has no error path. A directory with no manifest, unparseable
TOML, an unknown key, a malformed or duplicate id, or a contribution its
declaring scope may not make is **dropped**: recorded on `Plugins::dropped()`
with the reason, reported as `EventKind::PluginDropped`, and skipped. Every
bundle that did load is applied and the run proceeds.

An application that wants a broken bundle to be fatal writes one `if`:

```rust,ignore
if let Some(bad) = plugins.dropped().first() {
    return Err(Error::Config(format!("{}: {}", bad.id, bad.error)));
}
```

## The limits, stated plainly

**Nothing verifies that a directory is what its author published.** There is no
signature, no checksum and no provenance. The trust rule bounds what an
*untrusted* bundle may contribute, which is a different claim and the only one
this crate can honestly make.

**`Plugins::inspect` checks the manifest, not the author.** It answers "would
this load, and what would it contribute" — which is the whole of what a preflight
can know. A directory that inspects cleanly is still an unsigned directory, and
every limit on this page holds for it unchanged.

**Nothing fetches, installs or updates a bundle.** A `[[plugin]]` names a
directory that already exists on this machine. Distribution is the application's.

**A plugin contributes data, never code.** There is no dynamic loading and there
will not be: a `Tool` is an in-process trait implementation the application
registers, and `dlopen` would make every safety property of this crate a function
of a directory.

**A hook or MCP server contributed from a trusted scope runs a program with this
process's privileges.** Being introduced by a bundle neither sandboxes it nor
changes the policy that governs it.

**Namespacing changes the names a model sees.** A prompt or a skill that referred
to another skill by its bare name stops matching once that skill moves into a
bundle. This crate cannot rewrite prose.

**A dropped bundle is quiet by design.** It is reported on two channels and it
does not stop the run, so an operator who watches neither can run for a week
without deny rules they believe are installed.

**The standing `[[mcp]]` gap in `io.toml` itself is unchanged.** A project-scoped
`io.toml` may still name an MCP command directly, and an unknown key inside an
`[[mcp]]` table is still accepted because serde refuses `flatten` beside
`deny_unknown_fields`. A *plugin's* `[[mcp]]` is refused at project scope, so the
new surface is stricter than the old one — deliberately: new surface starts
closed.

**`version` in a manifest is documentation.** Nothing resolves it, compares it,
or checks it against the crate.
