# Language support

Nothing in the harness is Rust-aware any more. A run's definition of done can be
any command, the agent can run any command, and the harness ships a table of what
each ecosystem's commands conventionally are so the agent does not spend its
first three turns finding out.

```rust
use io_harness::{TaskContract, Verification};

// Go.
let go = Verification::Command {
    argv: vec!["go".into(), "test".into(), "./...".into()],
    expect_exit: 0,
};
// Python.
let py = Verification::Command {
    argv: vec!["uv".into(), "run".into(), "pytest".into()],
    expect_exit: 0,
};
// Or no gate at all, for work with no checkable criterion.
let open_ended = Verification::None;
# let _ = (go, py, open_ended);
```

## What the harness knows about a project

One marker file in the workspace root decides the ecosystem, and the detection is
put in front of the model on every turn — not only the first, because the first
turn is exactly what `ContextBudget` compacts away on a long run.

```rust
use io_harness::toolchain;

# fn demo() -> std::io::Result<()> {
let dir = tempfile::tempdir()?;
std::fs::write(dir.path().join("package.json"), "{}")?;
std::fs::write(dir.path().join("pnpm-lock.yaml"), "")?;

let found = toolchain::detect(dir.path()).unwrap();
assert_eq!(found.ecosystem, "node");
assert_eq!(found.manager, "pnpm");     // the lockfile chose
assert_eq!(found.test, ["pnpm", "test"]);
# Ok(()) }
# demo().unwrap();
```

| Marker | Ecosystem | Test command |
| --- | --- | --- |
| `Cargo.toml` | cargo | `cargo test` |
| `deno.json`, `deno.jsonc` | deno | `deno test` |
| `package.json` | node | `<manager> test` |
| `go.mod` | go | `go test ./...` |
| `pyproject.toml`, `requirements.txt` | python | `<manager> pytest` |
| `pom.xml` | maven | `mvn -B test` |
| `build.gradle`, `build.gradle.kts` | gradle | `gradle test` |
| `mix.exs` | elixir | `mix test` |
| `Gemfile` | ruby | `bundle exec rake test` |
| `composer.json` | php | `composer test` |
| `Package.swift` | swift | `swift test` |
| `CMakeLists.txt` | cmake | `ctest --test-dir build` |
| `*.csproj`, `*.fsproj`, `*.sln` | dotnet | `dotnet test` |
| `Makefile` | make | `make test` |

Each entry also carries an install, build, lint, format and run argv, where the
ecosystem has one. An empty vector means it does not: C projects driven by `make`
have no standard formatter, and a Go module needs no install step. Nothing is
shown to the model for a command that does not exist.

**The package manager comes from the lockfile.** `bun.lockb`/`bun.lock` → `bun`,
`pnpm-lock.yaml` → `pnpm`, `yarn.lock` → `yarn`, otherwise `npm`; `uv.lock` →
`uv`, `poetry.lock` → `poetry`, otherwise the interpreter directly. This is not
tidiness: running `npm install` in a pnpm workspace does not merely waste a turn,
it writes a second lockfile the project did not ask for.

**Order resolves the ambiguity.** `deno.json` is checked before `package.json`,
because a Deno project may carry both and `npm test` is wrong for it. `Makefile`
is checked **last**, because a great many projects have one beside their real
build system.

**A directory with no marker reports nothing.** Not a guess. The agent would run
`cargo test`, watch it fail, and have learned less than if it had been told to go
and look.

## The project's own checker, after an edit — and, since 0.51.0, before one

Detection also decides what runs *after* an edit. As of 0.25.0 a successful
`edit_file` in a project whose ecosystem `toolchain::detect` recognises runs that
project's own type-check command and appends what it reported to the observation
the model was going to read anyway.

Since 0.51.0 the agent can also ask. The `check` tool runs the same command over
the workspace on demand, so "is this tree already broken, and where" is a question
before a write rather than a note after one. It takes no arguments: what runs is
the detected command in the table below and not the model's guess at it.

Both are gated the same way, and since 0.74.0 both run inside the run's own
containment. Each is an `Act::Exec` check on the resolved command — the program
*and* the whole argv, as `exec` is — because `cargo check` compiles, and compiling
runs the workspace's `build.rs`, its proc macros and any `rustc-wrapper` named in
`.cargo/config.toml`: code chosen by whoever wrote the files in the tree, which is
not necessarily the operator. Until 0.74.0 the automatic one was ungated and
uncontained on the argument that it was the crate's own reflex after a write the
policy had already allowed.

They differ in what they do with a refusal. `check` reports one, because somebody
asked; the reflex is silent, because the model called `write_file` and a refusal
it cannot act on is context spent on a decision that was not its own. And only
`Effect::Allow` runs the reflex — an `Effect::Ask` is a skip rather than a
question, since this path has no approver and a write that paused on an approval
prompt would be a write turned into something else by what happens after it.
**`Policy::default()` sets `exec: Ask`, so under it nothing is checked after an
edit.** Name the checker with `allow_exec` for the projects you want checked; the
write is unaffected either way and still cannot fail. And when this ecosystem has
no checker, `check` says so where the automatic path stays silent — an empty
answer to a direct question reads as "your project is clean".

What that buys is when the error arrives, not that it arrives. Until now an agent
learned that its edit did not compile only by deciding to find out — call `exec`,
wait for a build, read the log — and a model that has just written a
plausible-looking function has no signal telling it to doubt one. So a type error
introduced at step 4 was discovered at step 20 by the verification gate, with
sixteen steps built on top of it. The check costs the model no decision.

| Ecosystem | Check command |
| --- | --- |
| cargo | `cargo check --message-format=json` |
| deno | `deno check .` |
| node | `tsc --noEmit` |
| go | `go build ./...` |
| python | `pyright` |

**Every other ecosystem in the detection table is skipped on purpose.** maven,
gradle, dotnet, swift, cmake, elixir, ruby, php and make have no type-check step
separate from their build, and running a build after every single edit would make
editing unusable. These commands are held to a much stricter bar than the
`build` argv of the same detection is — which is why `cargo check` is here and
`cargo build` is not. An ecosystem with no cheap checker is a fact about the
ecosystem rather than a failure of the harness: nothing runs, and nothing is said
to the model. A root with no marker file is skipped for the same reason `detect`
reports nothing there.

The check covers the workspace, not the edited file. None of these tools can
meaningfully type-check one file in isolation, because the edited file's
correctness depends on every file that uses it — and a caller broken by a changed
signature is the most valuable finding of the lot. The findings say so, and may
name files the edit did not touch. Since the cost of a check is the cost of the
project's own type-check, it is bounded twice: by the contract's exec timeout,
and by the run's per-observation output cap, the same ceiling every other tool
result obeys.

**It can never turn a successful edit into a failed one.** The write is on disk
before the checker starts. A checker missing from `PATH`, killed by the timeout,
or broken for a reason of its own reports that the edit is *unchecked* and the
run carries on. Unchecked is deliberately not the same observation as clean: a
model reading silence as approval is the failure this exists to prevent, and "the
checker found nothing" and "nothing checked your work" must not look alike.

Two smaller choices worth stating. It is `tsc` and not `npx tsc`, because `npx`
will reach the network and install a package to satisfy an invocation, which is
not something an edit should be able to trigger — a project with TypeScript
installed has `tsc` reachable, and one that does not is told its edit is
unchecked. And only cargo's output is parsed, because `--message-format=json` is
the only way to get its diagnostics without its progress bars; every other
checker's console output is passed through exactly as it printed it, since the
rendered diagnostic is what a developer reads and there is nothing to do with one
here except show it.

## The limits, stated plainly

**It is a default offered as information, never an instruction.** The harness
does not run these commands. The agent decides, calls `exec`, and that call is
checked against the `Policy` on the program and on the whole argv like any other.
A wrong entry costs a turn; it cannot widen a boundary.

**It will be wrong for someone on day one.** Ecosystems disagree with themselves:
half of all `npm test` scripts do not run tests, a Python project with a
`pyproject.toml` may really be driven by a `Makefile`, and `composer test` only
exists if the project defined it. When the default is wrong the agent finds out
the same way a human does — it runs and fails — and the goal text is the
workaround until it becomes overridable.

**Only the workspace root is examined.** A monorepo whose packages each carry
their own marker gets one detection, for the root. That is a real case this
deliberately does not handle.

**It is not overridable yet.** The table is shipped Rust data with no
configuration file under it. Writing a file format now would mean writing it
twice, because its shape is determined by this release and the accounting release
after it. `Toolchain` is `Serialize`/`Deserialize` today so that when the file
arrives it deserializes into *this* type rather than a second one.

## Choosing a criterion for a non-Rust project

`Verification::Command { argv, expect_exit }` runs its argv inside the execution
sandbox with the workspace as its working directory, and compares the exit
status. Two things follow from the sandbox:

- **No network.** A gate that needs to fetch dependencies will not work; run the
  install through `exec` first, where the agent works, and keep the gate to the
  command that tests.
- **A killed command never passes.** A signal, or a sandbox cap, reports as no
  exit at all, so no `expect_exit` can match it. A command that was cut short is
  not a command that met your criterion.

`expect_exit` is a status rather than a bool because "the linter found nothing"
and "the command ran" are different claims and some tools say so with a number.

When there is no checkable criterion — "work out why the nightly deploy fails and
write up what you find" — use `Verification::None`. The run ends when the agent
replies without calling a tool, reported as `RunOutcome::Finished`, which is a
different outcome from a step cap, a stall and a budget stop. Nothing checked the
work; that is what choosing it means. Read the result rather than shipping it the
way a `Success` may be shipped.

## Migrating off the Rust-specific criteria

`CompilesRust`, `RustTestPasses` and `WorkspaceTestPasses` were deprecated in
0.17.0 and are **removed as of 0.18.0**. Each one becomes a `Command`:

```rust
use io_harness::Verification;

// Before (0.16.2, deprecated in 0.17.0, gone as of 0.18.0):
//     Verification::CompilesRust
// After:
let compiles = Verification::Command {
    argv: vec!["cargo".into(), "build".into()],
    expect_exit: 0,
};

// Before:
//     Verification::WorkspaceTestPasses { files, test_src }
// After — the repository's own suite over the whole crate, rather than a
// concatenation of the files you listed:
let tested = Verification::Command {
    argv: vec!["cargo".into(), "test".into()],
    expect_exit: 0,
};
# let _ = (compiles, tested);
```

The behaviours are not identical, and the difference is worth knowing as you
move. The old variants compiled the named files in a throwaway directory with your
`test_src` appended as a module, so they reached *private* items and needed no
cargo project. `cargo test` runs the project's real suite, which is stronger in
every way that matters and cannot check a criterion the project does not contain.
If you were using `test_src` to assert something the repository does not test,
write that test into the repository — which is where a caller reading the
deprecation warning should have ended up anyway.

**If you upgraded straight from 0.16.2 you never saw the warning.** The
deprecation lived for one minor release, which is the shortest cycle this crate's
contract allows, and a reader who upgrades one minor at a time is not the only
reader. The table above is the whole migration.

`EachCompilesRust` is **not** deprecated: it has no `Command` equivalent, since
"each of these files compiles on its own" is not something a project's own
tooling asks.

## See also

- [Command execution](command-execution.md) — what `exec` does and does not bound
- [Verification](verification.md) — what a passing gate proves, and what it does not
- [Execution sandbox](sandbox.md) — the container a `Command` criterion runs in

## Navigating with a language server (0.52.0)

`grep` answers "which lines contain this text". An editor answers "where is this
defined", "who calls it", "what is this", and it answers them from a resolution
rather than from a match. 0.52.0 gives the agent the second kind of question.

Name a server in the user-scope `io.toml` — since 0.74.0 `[[lsp]]` is refused in
both workspace files, because a language server is a command, an argv and an
environment this process spawns at run start, and the spawn gate is an `Act::Exec`
check on the binary name alone:

```toml
[[lsp]]
id = "rust"
command = "rust-analyzer"
extensions = [".rs"]
timeout_secs = 60
```

or on the contract:

```rust,ignore
use io_harness::{LspServer, TaskContract};

let contract = TaskContract::workspace("rename Ledger to Tally", "/repo")
    .with_lsp([LspServer::new("rust", "rust-analyzer").with_extensions([".rs"])]);
```

That adds five tools, and only for a run that configured a server:

| Tool | Question |
| --- | --- |
| `lsp_definition` | Where is the symbol at this position defined? |
| `lsp_references` | Everywhere it is used — resolved, not matched |
| `lsp_symbols` | What is in this file, or — with `query` — where a symbol is in the workspace |
| `lsp_hover` | What is this: its type, signature and documentation |
| `lsp_rename` | Rename it everywhere, **as a patch you apply yourself** |

Positions are 1-based, the way `read_file` shows them.

### What it costs, and what it saves

The server starts once per run, in the background, so a cold index is paid once
rather than inside a tool call. Starting it is an `Act::Exec` check on the binary
named — without `allow_exec` for it the run refuses before the process exists.

The `path` a question names is an `Act::Read` check of its own, taken before the
server is told anything (0.74.0). The file is about to be read from disk and sent
to a third-party process, and until this release the path was only joined onto the
workspace root — which an absolute argument discards and a `../..` climbs out of —
so `lsp_hover {"path": "../../../../etc/shadow"}` crossed the boundary
`read_file` would have refused and left no row. A read tier of `Ask` now prompts
here, exactly as it does for `read_file`; `Policy::default()` allows reads, so the
common configuration is unchanged.

Measured over one question ("where is this defined and which call sites use it")
against the same repository: **three provider calls and 6,052 prompt bytes**
through the server, against **six calls and 11,901 bytes** through
`grep`/`find`/`read_file`.

### Rename writes nothing

`lsp_rename` returns a patch series — one `--- a/<path>` / `+++ b/<path>` header
and its hunks per file. Apply the parts you want with `patch_file`, one path per
call. That keeps every byte reaching the workspace behind an `Act::Write` check on
the path it lands in, and keeps the all-or-nothing guarantee `patch_file` already
gives.

**A file is rendered only if the policy allows reading it outright** (0.74.0). The
server decides which files a `WorkspaceEdit` names, not the model, and rendering
one is not naming it — every removed line lands in the model's context. So each
path is resolved under the workspace root and must be `Effect::Allow`: an `Ask` is
not permission, because there is no approver on this path to answer it. A file
that does not clear the bar is omitted, with a count and the reason, rather than
dropped silently. A run that wants the patch grants `allow_read` over the tree it
is renaming in — the grant applying that patch already needs.

### Diagnostics

Where a configured server answers diagnostic requests, its findings are appended
to what `check` and the automatic post-edit checker already report, attributed to
the server. They are never a substitute: a language server's analysis omits
borrow-check errors, monomorphisation errors and every lint — precisely the errors
a model writes.

### Stated plainly

- A server indexes the whole root, including files a `deny_read` rule covers. Every
  location handed back is checked against `Act::Read` and an omission is counted in
  the answer — but the server process has still read those files. The bar depends on
  what is being handed back: a path that is only *printed* is omitted on an outright
  `Deny`, since naming a path is not reading it, while a file whose *contents* this
  crate is about to render — `lsp_rename`'s patch — needs an `Allow`.
- The server runs at the harness's own privilege, like an MCP stdio server. It is
  not inside the execution sandbox.
- A server still indexing answers `[]` rather than an error. This crate waits for
  the work a server announces and asks again, bounded by its `timeout_secs`, but a
  server that announces nothing about a start-up phase can still answer "nothing"
  when it means "not yet".
