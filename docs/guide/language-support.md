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

`CompilesRust`, `RustTestPasses` and `WorkspaceTestPasses` are deprecated in
0.17.0 and **removed in 0.18.0**. Each one becomes a `Command`:

```rust
use io_harness::Verification;

// Before (0.16.2, deprecated in 0.17.0, gone in 0.18.0):
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

The behaviours are not identical, and the difference is worth knowing before you
move. The old variants compile the named files in a throwaway directory with your
`test_src` appended as a module, so they reach *private* items and need no cargo
project. `cargo test` runs the project's real suite, which is stronger in every
way that matters and cannot check a criterion the project does not contain. If
you were using `test_src` to assert something the repository does not test, write
that test into the repository — which is where a caller reading the deprecation
warning should end up anyway.

`EachCompilesRust` is **not** deprecated: it has no `Command` equivalent, since
"each of these files compiles on its own" is not something a project's own
tooling asks.

## See also

- [Command execution](command-execution.md) — what `exec` does and does not bound
- [Verification](verification.md) — what a passing gate proves, and what it does not
- [Execution sandbox](sandbox.md) — the container a `Command` criterion runs in
