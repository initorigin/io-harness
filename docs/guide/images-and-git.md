# Images and git

Two capabilities that let the agent see what it is working on and commit what it
produced: image passthrough to a provider whose model accepts them, and five git
built-ins with argv the model cannot shape.

```toml
io-harness = { version = "0.15", features = ["media"] }
```

`media` is what images need — its only dependency is `base64`, already in every
build through `reqwest`, so the default dependency tree is unchanged. Git needs
no feature and no dependency at all: it shells out to the `git` already on the
machine. Git is a **runtime** capability, never a build dependency — no `git` on
the machine is an observation the model adapts to and the run carries on.

## Images

The caller attaches them to the task with `TaskContract::with_images`, and they
ride every request:

```rust
use io_harness::{Media, TaskContract, Verification};

let contract = TaskContract::workspace(
    "the layout in this screenshot is wrong; fix the CSS",
    "/path/to/repo",
)
.with_verification(Verification::WorkspaceFileContains {
    file: "site.css".into(),
    needle: "grid".into(),
})
.with_images([Media::image("image/png", png_bytes)?]);
```

Accepted media types **on the wire** are `image/jpeg`, `image/png`, `image/gif`
and `image/webp` (`IMAGE_MEDIA_TYPES`) — the intersection every provider
documents. Anything else is a typed error at construction, not a request the
provider rejects later.

**The door is wider than the wire (0.55.0).** `Media::attach` takes an image in
whatever format it arrived in and produces one a provider will accept: those four
pass through **byte-identically**, and BMP, TIFF, ICO, TGA and PNM are decoded
and re-encoded to PNG. A JPEG is never re-encoded — a round trip through a
decoder on the commonest path would be a silent quality loss.

```rust
// A BMP from a scanner, a TIFF from a camera: accepted at the door, PNG on the wire.
let media = Media::attach("image/bmp", scanner_bytes)?;
assert_eq!(media.media_type, "image/png");
```

What the crate cannot decode is refused **by name**, with the reason and a
one-line conversion — not with the vendors' four-type list, which is a true
statement about three APIs and reads, at the doorstep, as this crate being unable
to open a photograph. HEIC and AVIF need a system C library this crate does not
depend on, so that it builds anywhere with a Rust toolchain and nothing else; SVG
needs a renderer, which is a dependency tree rather than a C library; a PDF is
routed to `pdf_read`. `Media::source_type_for` is the extension table that names
all of them, beside `Media::media_type_for`, which still answers the narrower
"may this go on the wire".

A declared size is checked before a decode: a small file can declare an enormous
image, and a decoder that believes it allocates before anything checks the
result. `MAX_IMAGE_PIXELS` bounds what will be decoded from the header alone.

Two size bounds are enforced here rather than by the vendor. `MAX_IMAGE_BYTES` is
5MB per image — Anthropic's documented limit, and the smaller of the two vendor
limits, because an image one vendor would refuse and another accept is worse
refused here than refused there as an HTTP 400 that reads like a transport
failure. `MAX_REQUEST_IMAGE_BYTES` is 20MB across all images on one request,
because the per-image bound does not compose: sixteen images each under the
single-image limit is a request no budget anticipated. The bytes of one of the
four wire types are **not** parsed — whether they are a valid PNG is the vendor's
judgement, and there is still no image decoder on the default path. A format the
door converts is necessarily decoded, which is why that path is behind the
`media` feature and its pixel bound is checked from the header first.

The agent looks at one itself with `view_image`, which accepts everything the
door accepts and says in its observation when a conversion happened, and which is
gated on `Act::Read`
against the path the model named — this is the model choosing which of the user's
files to send to a third party, so it is authorised per call on the real path
rather than once by tool name. A viewed image rides one request and is then
dropped.

`Provider::accepts_images` defaults to `false`, so a provider written before
images existed inherits a refusal rather than a silent drop. The refusal happens
before the request body is built and before anything is spent.

Video and audio are not supported, and are not on any roadmap; the reasoning is
in [Documents](documents.md#video-is-off-the-roadmap).

## Git

Seven tools: `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, and
since 0.36.0 `git_branch` and `git_worktree`.

The reachable surface is closed by construction rather than by an allow-list.
The exec policy enforces a program *name* and records argv without checking it,
so `Act::Exec("git")` cannot tell `git log` from `git push --force`; each
built-in therefore constructs its own complete argv, and every model-supplied
path is passed after `--` with a leading `-` refused outright.

The path policy governs git on the paths git touches. Staging copies a file's
bytes into the object store, so `git_add` requires `Act::Read` on each path it
stages — a file the policy denies cannot reach a commit — and `git_commit`
requires `Act::Write` on `.git`. **A run under a narrow write policy must allow
`.git` or its commits are refused.**

```rust
use io_harness::Policy;

let policy = Policy::default()
    .layer("app")
    .allow_read("src/*")
    .allow_write("src/*")
    .allow_write(".git")     // without this, every commit is refused
    .allow_exec("git");
```

The committing identity is the caller's to set, with
`TaskContract::with_commit_identity`, rather than whatever the machine's global
git config happens to say.

Commits are local: there is no push, no fetch and no history rewriting.
Repository hooks do not run — `.git/hooks/*` is arbitrary code carried by the
repository the agent was pointed at, and nothing in the permission model covers
it.

## Landing on a branch (0.36.0)

Without `git_branch`, a run commits onto whatever branch it found — an agent
asked to fix a test lands its work on `main` because `main` was checked out, and
a human reviewing it has to move it themselves.

`git_branch` renders `git switch --create=<name>`: a branch at the current
commit, moved onto, carrying the working tree across. It is the only shape of a
checkout this crate builds, and the only one that cannot discard a change — an
existing name is refused by git, and nothing is replaced. `checkout` stays on
the forbidden list, and the test that holds that list is unchanged by this
release rather than relaxed to fit.

Branch names are validated here before git sees them: letters, digits, `.`, `_`,
`/` and `-`, at most 100 characters, no leading `-`, no `..`, and no empty or
`.lock` path component. A refused name is an observation the agent can act on.
The allowlist is deliberately narrower than git's own rules — the set of names
an agent has reason to ask for is small, so the safe subset is the one that can
be enumerated.

## A working tree per agent (0.36.0)

Every agent in a tree shares one checkout, so two children editing the same file
are one overwriting the other. `git_worktree` renders
`git worktree add -b <name> -- <path>`: another checkout of the same repository,
on a new branch, at a path checked for `Act::Write` like every other
model-supplied path.

A roster can ask for one without the model deciding:

```rust
use io_harness::AgentDef;

// Each child of this definition is rooted at its own worktree under
// `.worktrees/`, on its own branch, created before its first step.
let worker = AgentDef::new("worker").with_worktree();
```

The path is derived from the key a spawn is adopted by, so a resumed tree
continues in the worktree it already made rather than re-creating it and
throwing the child's work away. If one cannot be made — no git, not a
repository, the policy refusing that path — the spawn fails and says why, because
quietly sharing the parent's tree is the collision the flag exists to prevent.

**Two honest costs.** Nothing here removes a worktree: removing one deletes the
work the child was spawned to do, so it is the operator's call
(`git worktree remove`). And the parent's own `git_status` reports the directory
as untracked — one line, `?? .worktrees/`, because git summarises an untracked
directory rather than descending into it — so a `git_add` naming `.` stages the
children's trees. This crate does not write to `.git/info/exclude` on your
behalf; the one line an operator can add themselves is:

```sh
echo '/.worktrees/' >> .git/info/exclude
```

`git switch` needs git 2.23 and `git worktree` needs git 2.5. An older git
surfaces as the message git itself prints, as an observation, not as a crate
error.

## Why this raises the stakes on resume

A commit is the first irreversible action the harness can take. A crashed run may
therefore already have committed under a policy the resuming caller cannot name,
which is why `resume_from_stored_policy` exists and why plain `resume` refuses a
policy-bearing run rather than continuing without a boundary. See
[Durable runs](durable-runs.md) and
[Verification](verification.md#reading-the-trace-and-resuming).

## See also

- [Permissions and approval](permissions.md) — the per-path gate on `view_image`, `git_add` and `git_commit`
- [Durable runs](durable-runs.md) — resuming a run that may already have committed
- [Documents](documents.md) — the other optional-feature capability, and the video decision
- [Tools and skills](tools-and-skills.md) — why fixed-argv built-ins beat a registered tool here
- [MCP and network egress](mcp-and-network.md) — what the policy does and does not govern once a process is running
- [The contract](../CONTRACT.md) — the crate's stability and API promises
- [README](../../README.md)
