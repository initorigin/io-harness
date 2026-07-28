# Public contract — IO Harness

What you may depend on, what may change, and what does not work today.

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. This
page exists because "pre-1.0" is usually where a library stops explaining
itself, and that is precisely when a dependent needs the explanation most.

## What is public

The public surface is everything re-exported from the crate root plus the items
reachable through the public modules it names.

The re-exported half — the 106 items a caller reaches as `io_harness::Thing` —
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
is fixed as of 0.16.0.

**What a passing verification gate proves.** It proves the criterion compiled
and ran against the subject and reported success. It does not prove the
implementation is correct, idiomatic, or complete. Since 0.8.1 a subject can no
longer shadow a macro the criterion invokes, nor delete the criterion and pass on
an empty test binary — but a gate is a check the caller wrote, and it proves
exactly what that check tested. See the
[verification guide](guide/verification.md).

**What the permission policy governs.** It decides whether an action is taken:
which paths are read and written, which binaries and tools may be invoked, which
hosts may be dialled. It does not govern what a thing does once it is running. A
registered `Tool` executes in the harness's own process with the embedding
program's privileges; a stdio MCP server is a separate process that, once
started, dials what it likes. The harness decides what starts, not what a started
thing then does.

**Windows resource caps.** See the platform table above.

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
