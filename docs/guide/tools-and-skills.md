# In-process tools and skills

Implement `Tool` for something your product already knows how to do and the model
is offered it beside the built-ins; point the contract at a directory of markdown
and those files shape *how* the agent works without touching Rust.

[MCP](mcp-and-network.md) makes the harness extensible **out of process**. That is
the right boundary for a capability that already lives elsewhere and the wrong one
for a capability already linked into the same binary: a second process, a
transport, and a serialization hop to call a function that is one `await` away.
The `Tool` trait is the in-process half. **Skills** are the other half of this
page — instructions rather than code.

## Register a tool your program already has

```rust
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{run_with, ApproveAll, Policy, TaskContract, ToolSpec, Verification};
use serde_json::json;

struct LookupOrder { db: OrderDb }

impl Tool for LookupOrder {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lookup_order".into(),
            description: "Look up an order by id. Returns its status and line items.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        // Read defensively: a model can send a missing or mistyped field, and
        // that is an observation to adapt to, not a crash.
        let id = arguments.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Box::pin(async move {
            Ok(match self.db.order(&id).await {
                Some(order) => format!("{order}"),
                None => format!("no order with id {id:?}"),
            })
        })
    }
}

let contract = TaskContract::workspace(
    "Write the status of order 4471 into REPORT.md.",
    "/path/to/repo",
)
.with_verification(Verification::WorkspaceFileContains {
    file: "REPORT.md".into(),
    needle: "4471".into(),
})
.with_tools(Toolbox::new().with(LookupOrder { db }));

let policy = Policy::default()
    .layer("app")
    .allow_read("*")
    .allow_write("*")
    // Registering the tool offers it. This is what allows it to be called.
    .allow_exec("lookup_order");

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

- **Registration is availability, not authority** — a call is an `Act::Exec`
  check on the tool's name, decided by the same deny-first stack that decides
  paths, binaries, and hosts. Hand an agent a toolbox and still refuse one tool
  in it; the refusal is an observation the agent adapts to, with the deciding
  rule and layer in the trace. An `ask_exec` routes to the `Approver` and
  survives a restart like any other [deferred approval](durable-runs.md).
- **Nothing may shadow anything** — a registered tool cannot take a name the
  harness reserves, cannot use the `mcp__` prefix reserved for server tools, and
  two registered tools cannot share a name. Each is an `Error::Config` raised
  **before the provider is called once**, not a silent shadowing found at
  dispatch. The reserved set is `RESERVED_TOOL_NAMES` in `src/tools/custom.rs`,
  and that list is the statement of it rather than this page — a set retyped into
  prose is a set that goes stale. It holds the feature-gated built-ins it names
  in every build, including builds that do not compile them, so enabling a
  feature can never take away a tool that was working. It does **not** yet hold
  every name dispatch answers: the `browser_*` and `lsp_*` tools are among
  eighteen that are dispatched and not reserved, and because dispatch matches
  every built-in before it reaches the toolbox, a registered tool taking one of
  those validates and is then never reached. 0.61.0 closes that gap. Until it
  does, do not name a tool after anything the harness already answers.
- **A failing tool is an observation** — returning `Err` puts the message in the
  observations and the run continues, the same treatment `grep` gives a bad
  regex. Only the model can tell "try another id" from "give up on this
  approach".
- **It cannot flood the context** — a result over the cap is truncated with a
  visible marker before it enters the observations, and the truncated form is
  what the trace records. The cap is not a constant of its own: it is derived per
  turn from the run's [context budget](context-and-memory.md), so the
  per-result ceiling and the whole prompt's ceiling are one unit from one source.
- **Inherited by the tree** — a [child agent](composition.md) is offered its
  parent's toolbox and calls it under the child's own narrowed policy.
  Inheritance grants the tool; `Policy::contain` still decides the call.
- **Traced like a built-in** — same decision, argument, and observation rows, so
  an audit does not have to distinguish extension from core.

Run it live: `cargo run --example custom_tool`.

### Say whether your tool changes anything (0.41.0)

`Tool` has one more method and it is defaulted, so nothing above needs editing:

```rust,ignore
use io_harness::tools::ToolEffect;

impl Tool for Customers {
    // spec() and invoke() exactly as above, plus:
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}
```

When a completion carries several tool calls, the loop runs the read-only ones at
the same time and everything else one at a time. `effect()` is how your tool joins
the first group. It defaults to `ToolEffect::Mutating`, so a toolbox written
before 0.41.0 keeps running exactly as it did, and a tool becomes concurrent only
because its author said it could.

Three things are worth knowing before you change it:

- **The declaration is a promise the harness cannot check.** Your tool is code the
  crate compiled in; if it says `ReadOnly` and then writes, it has broken its own
  invariants and nobody else's. Say `ReadOnly` when a call observes and changes
  nothing — a lookup, a fetch, a query.
- **Order does not change.** However the calls run, the observations, decisions,
  steps and ledger draws are recorded in the order the model asked for them. An
  observer cannot tell the difference, and that is the point.
- **The ceiling is on the contract.** `TaskContract::with_max_parallel_reads`
  bounds how many are in flight; it defaults to 10, and `1` puts the run back on
  the pre-0.41.0 path exactly, which is the way to rule the concurrency out while
  you are debugging something else.
- **Since 0.54.0 a read-only call may also start *early*** — while the model is
  still producing the completion that asks for it, rather than after. Declaring
  `ReadOnly` is what opts your tool into that as well, and the same promise is what
  makes it safe: a call that observes and changes nothing can be run against a
  completion that turns out never to settle, and the result simply thrown away.
  Two consequences worth knowing:
  - Your tool may be invoked and its result discarded, with nothing recorded about
    it. That happens when the completion fails and is retried, falls over to
    another provider, or settles on different arguments than it streamed. Write
    `invoke` so a wasted call costs only its own work — which a genuinely read-only
    call already does.
  - It only applies to a *leading* run of read-only calls, and only in a session
    turn served by a provider that reports its finished calls. Anything after the
    first mutating call in a completion waits, so a read cannot observe the state
    before a write that has not run yet.

## What `read_file` gives back (0.55.0)

Three answers, and only three: the file, the range of lines you asked for, or a
refusal that says why.

```json
{ "path": "src/run.rs", "offset": 4200, "limit": 200 }
```

`offset` counts from 1 and the observation header states the range and the
file's total line count — `[read src/run.rs lines 4200-4399 of 15012]` — so a
slice reads as a slice. An `offset` past the end is an error naming the total,
not an empty success.

**A file too large to carry is refused rather than shortened.** It used to be
cut, with a marker saying so; what the model then held had the shape of a whole
file. The refusal names the path, the size, the ceiling and both remedies. There
are two ceilings: `[run] max_read_chars`, which an operator sets and which does
not move, and one derived from the [context budget](context-and-memory.md),
which falls as the run spends. The message names the one that bit, because
raising a key and asking for a range are different answers.

**A file that is not text is named, not decoded.** An image says to call
`view_image`; a spreadsheet, a Word document, a slide deck or a PDF names the
tool that reads it, and says so plainly when that tool's feature is not compiled
into the build. Anything else is named as binary with its size and what its
leading bytes look like. UTF-16 with a byte-order mark is decoded and the
encoding is named; without the mark it is binary, because a guessed encoding is
a confident wrong answer rather than a read. An SVG is text — a model reading one
wants the markup.

Every other tool's output is still bounded rather than refused. A command's
output and a search's matches were never documents, and a prefix of one is not a
lie.

## What `remember` reports back (0.57.0)

`remember` writes one keyed note against a scope — the workspace by default, or
the scope above it — and the result says the note was stored. It writes *by key*,
which is the whole of its failure mode: the same fact learned twice under two
names leaves two entries that disagree, both carried into later prompts, and the
model acting on whichever it read last. Nothing about a keyed write notices that.

So a write whose text closely overlaps an entry already stored **in the same scope
under a different key** comes back naming that key and quoting what it holds,
bounded with the same `…[truncated]` marker every other bounded result carries.
The write still lands: this is a report and never a refusal, because declining a
write because two strings overlapped would be guessing at intent, and merging them
would write a fact nobody stated. The model has the conflicting key and its text
in the same turn, so it settles the contradiction with another `remember` or a
`forget` rather than leaving it for a later run to trip over.

Two writes are deliberately not reported. Rewriting the **same key** is the
replacement writing by key has meant since 0.10.0. A workspace note restating a
**global** one is the override the second scope exists for, which is how a wrong
global note is corrected locally — so the check only ever looks inside the scope
being written.

The comparison is a normalised token overlap — shared words over union words, on
the lowercased alphanumeric tokens of three characters or more — computed in this
process on the write path already running. No embedding, no model call, nothing
over a network. `Store::memory_similar` is the same answer for the embedding
program; what it costs at each cap is in
[docs/MEASUREMENTS.md](../MEASUREMENTS.md), and what the note is worth to a later
turn is in [Context and memory](context-and-memory.md).

## Skills: instructions, not code

Point the contract at a directory of markdown. Both conventions in common use
are accepted, so a directory written for another agent tool usually works
unchanged:

```text
skills/
  migrations.md          -> skill "migrations"
  api-style/
    SKILL.md             -> skill "api-style"
```

```rust
let contract = TaskContract::workspace("Add the `orders` table migration.", "/path/to/repo")
.with_verification(Verification::EachCompilesRust(vec!["migrations/003_orders.rs".into()]))
.with_skills("skills");   // discovered once per run, not once per step
```

Optional YAML frontmatter names and describes a skill; without it the name comes
from the file stem (or the containing directory, for a `SKILL.md`) and the
description from the first prose line:

```yaml
---
name: migrations
description: How to write a reversible database migration in this repo.
---

Always write the down-migration first...
```

- **Names and descriptions reach the prompt; bodies do not** — twenty skills
  would otherwise be paid for on every turn of every run. The agent is told what
  exists and calls the built-in `read_skill` tool for the one it judges
  relevant, which enters the observations once. The harness does not rank,
  match, or auto-inject — automatic relevance selection is a context-construction
  question and is deliberately not here.
- **Reading one is an ordinary policy-checked read** — a policy denying
  `Act::Read` over the skills directory keeps the catalogue in the prompt and the
  bodies out of the context, with the refusal in the trace. An unknown skill name
  returns an observation listing what does exist, not an error.
- **A bad directory fails honestly** — a missing path, a path that is not a
  directory, more than `MAX_SKILLS` (64) skills, or two skills with the same name
  is an `Error::Config` at run start. A rejected set, not a silently truncated one
  the caller believes is complete.

`read_skill` is offered only when the contract configures skills, so a run
without them does not pay a tool slot for one that could do nothing but fail.

Run it live: `cargo run --example skills_run`.

## The boundary, stated plainly

A registered tool runs **in the harness's own process, with the embedding
program's privileges**. The policy governs whether it is *called*; it does not
govern what it does once running — no sandbox, no path scoping, and no egress
control applies inside it. This is exactly the bound already stated for a
[stdio MCP server](mcp-and-network.md#the-limit-stated-plainly), and for the same
reason: the harness decides what starts, not what a started thing then does. A
tool that shells out, writes outside the workspace, or dials a host has done so
with your full authority.

A **skill** is instructions with no execution of its own. A skill saying "run
`rm -rf /`" is a sentence the model reads, and any action it then takes passes
the same policy every other action does. Anything that should actually *do*
something is a `Tool`, where the permission layer can see it.

This boundary is why the crate's own file-touching capabilities are built-ins
rather than registered tools. A registered tool is authorised once, by an exec
check on its name; a tool whose whole job is reading and writing files in the
user's workspace is dispatched as a built-in instead, gated per call on the real
path it names — so `deny_write("secrets/*")` refuses it for exactly the reason it
refuses `write_file` to the same path.

## See also

- [MCP and network egress](mcp-and-network.md) — the out-of-process half, and the
  same boundary stated there first
- [Permissions and approval](permissions.md) — the `Act::Exec` check that decides
  every tool call
- [Agent composition](composition.md) — toolbox inheritance and narrowing
- [Context and memory](context-and-memory.md) — the budget the result cap derives
  from
- [Documents](documents.md) and [Images and git](images-and-git.md) — built-ins
  whose names the toolbox does not reserve
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
