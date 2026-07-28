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
    Verification::WorkspaceFileContains { file: "site.css".into(), needle: "grid".into() },
)
.with_images([Media::image("image/png", png_bytes)?]);
```

Accepted media types are `image/jpeg`, `image/png`, `image/gif` and `image/webp`
(`IMAGE_MEDIA_TYPES`). Anything else is a typed error at construction, not a
request the provider rejects later.

Two size bounds are enforced here rather than by the vendor. `MAX_IMAGE_BYTES` is
5MB per image — Anthropic's documented limit, and the smaller of the two vendor
limits, because an image one vendor would refuse and another accept is worse
refused here than refused there as an HTTP 400 that reads like a transport
failure. `MAX_REQUEST_IMAGE_BYTES` is 20MB across all images on one request,
because the per-image bound does not compose: sixteen images each under the
single-image limit is a request no budget anticipated. The bytes themselves are
not parsed — whether they are a valid PNG is the vendor's judgement, and guessing
would mean adding an image decoder to the default path.

The agent looks at one itself with `view_image`, which is gated on `Act::Read`
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

Five tools: `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`.

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

Commits are local: there is no push, no fetch, no branch switching and no history
rewriting. Repository hooks do not run — `.git/hooks/*` is arbitrary code carried
by the repository the agent was pointed at, and nothing in the permission model
covers it.

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
