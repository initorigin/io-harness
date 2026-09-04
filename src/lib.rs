//! An embeddable agent runtime for Rust. Any task, any provider, in your process
//! — with a permission boundary, a sandbox, and a durable trace you own.
//!
//! You hand it a [`TaskContract`]: the task, the workspace it may touch, and what
//! it may read, write, run and dial. The harness runs the loop — observe, reason,
//! act, check, stop — and returns a [`RunResult`] whose [`RunOutcome`] says why it
//! stopped, with every step, refusal, and budget draw in a SQLite trace
//! ([`Store`]) you can read afterwards. No daemon and no CLI.
//!
//! The agent can run the project's own toolchain, so the language a project is
//! written in is not this crate's business. [`Verification`] is optional and
//! language-agnostic: a criterion can be `cargo test`, `npm test`, `go test
//! ./...` or [`Verification::None`] when the task has no checkable criterion at
//! all.
//!
//! # Quickstart
//!
//! ```no_run
//! use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};
//!
//! #[tokio::main]
//! async fn main() -> io_harness::Result<()> {
//!     let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
//!     let store = Store::memory()?;
//!
//!     let contract = TaskContract::workspace(
//!         "the test suite is failing; find out why and fix it",
//!         "/path/to/repo",
//!     )
//!     // The project's own command decides whether the work is done. Nothing
//!     // on this path is Rust-aware.
//!     .with_verification(Verification::Command {
//!         argv: vec!["npm".into(), "test".into()],
//!         expect_exit: 0,
//!     });
//!
//!     // src/ is writable, secrets/ is refused outright and never reaches a human,
//!     // and the agent may run the test runner but nothing that publishes.
//!     let policy = Policy::default()
//!         .layer("app")
//!         .allow_read("*")
//!         .allow_write("src/*")
//!         .deny_read("secrets/*")
//!         .allow_exec("npm*")
//!         .deny_exec("npm publish*");
//!
//!     let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
//!     println!("{:?}", result.outcome);
//!     Ok(())
//! }
//! ```
//!
//! [`run`] is the same loop with no policy — it uses [`Policy::permissive`],
//! which enforces nothing. Every entry point has an observed twin taking an
//! [`Observer`] as its last argument ([`run_with_observed`], and so on).
//!
//! # What it does
//!
//! **The loop.** [`TaskContract::workspace`] gives the agent `grep`, `find`,
//! `read_file`, `write_file`, `edit_file` and `patch_file` across a repository
//! root; [`TaskContract::new`] names one file to edit. A read returns the file,
//! the range of lines it asked for, or a refusal saying why — never a shortened
//! file wearing the shape of a whole one, and never an empty string for a binary
//! ([`tools::FileContent`]). A change touching several
//! places in one file is one `patch_file` call taking a unified diff, applied as
//! a unit or not at all ([`tools::PATCH_FILE_TOOL`]), and the project's own
//! type-check is a question the agent can ask before it writes rather than only
//! a note it gets afterwards ([`tools::CHECK_TOOL`]).
//!
//! **What a change is kept as.** Every write records the change as a unified diff
//! of the whole file ([`Edit::hunk`]), so a trace can show *what* a step changed
//! and not only how many lines it changed: [`Store::patch`] renders a run's whole
//! change as a step-ordered patch series a human reviews. That is also what makes
//! undo finer than a run — [`rewind_step`] reverse-applies one step's hunks, and
//! walking a run back is that call newest step first.
//!
//! **Navigating the code.** Point the harness at a language server — in `io.toml`
//! or with [`TaskContract::with_lsp`] — and the agent asks the questions an editor
//! answers rather than the ones a text search answers: `lsp_definition`,
//! `lsp_references`, `lsp_symbols`, `lsp_hover` and `lsp_rename`
//! ([`tools::LSP_DEFINITION_TOOL`] and its four siblings). The server starts once
//! per run in the background, so a cold index is paid once rather than inside a
//! tool call, and starting it is an [`Act::Exec`] check on the binary the operator
//! named ([`LspServer`]). `lsp_rename` **writes nothing**: it answers with a patch
//! series the model applies with `patch_file`, one gate check per file. Configure
//! no server and none of the five schemas exists.
//!
//! **Driving a browser.** Behind the `browser` feature, off by default, a run can
//! open a page and use it — [`tools::BROWSER_NAVIGATE_TOOL`] and its five siblings
//! — reading the text the page actually renders and taking a screenshot the model
//! is shown. Console output and uncaught page errors ride the observation of the
//! action that produced them. **Every document navigation is an [`Act::Net`] check
//! against its host, decided at the paused request**, so a navigation a click or a
//! script causes is gated by the same code as one the model typed. It speaks over a
//! pipe on the child's own descriptors and opens no debugging port, and the browser
//! is one already installed ([`BrowserConfig`]) — nothing is ever downloaded.
//!
//! **Commands, under the same boundary as everything else.** The agent runs the
//! project's own build, tests, linter or package manager through an `exec` tool
//! ([`tools::EXEC_TOOL`]) taking a fixed argv and never a shell string, so there
//! is no `;`, `&&` or `$( )` to parse and therefore none to get wrong. Every call
//! is an [`Act::Exec`] check on the program *and* on the whole argv, so
//! `allow_exec("cargo test*")` beside `deny_exec("cargo publish*")` means what it
//! reads. By default a command runs in the workspace with the embedding
//! program's privileges and is **not** sandboxed — see [`tools::exec`] for the
//! whole of that bound, and [`DEFAULT_EXEC_TIMEOUT`] for the ceiling on one that
//! wedges. A contract that wants the narrower thing asks for it:
//! [`TaskContract::with_contained_exec`] puts every command `exec` and the
//! foreground `shell` start inside the [`Sandbox`] backend this host offers,
//! keeping the workspace as the working directory so an incremental build
//! survives between commands.
//!
//! **Verification in any language, or none.** [`Verification::Command`] runs a
//! caller-supplied command in the sandbox and asserts its exit status — one
//! variant covering every language the machine has a toolchain for.
//! [`Verification::None`] is a run with no gate, ended by an assistant turn that
//! calls no tool and reported as [`RunOutcome::Finished`]. What a passing gate
//! proves is narrower than it reads, and [`Verification`] states it exactly.
//! [`toolchain::detect`] tells the agent what this project's commands
//! conventionally are, so it does not spend turns finding out.
//!
//! **A permission boundary.** A [`Policy`] of layered, deny-first [`Rule`]s
//! decides what the agent may read, write, execute ([`Act::Exec`]) and connect
//! to ([`Act::Net`]), enforced in the tool and verification layers rather than
//! in the prompt. Anything marked [`Effect::Ask`] goes to an [`Approver`], which
//! may approve (optionally rewriting the action or remembering a rule), deny, or
//! [`Defer`](Decision::Defer) — persisting the pending action so a human can
//! decide after this process has exited, with [`resume_with_decision`] and
//! [`resume_tree_with_decision`] continuing on that answer. Every refusal is in
//! the trace, attributed to the rule and the layer that produced it.
//!
//! **Budgets and stop conditions.** Steps, wall-clock time, and token spend are
//! capped by the contract. A whole tree of agents draws from one shared
//! [`Ledger`] that no spawned contract can raise.
//!
//! **Agent composition.** [`run_tree`] runs a workspace contract as the root of
//! a tree: the agent gains [`SPAWN_TOOL`], which launches a contained sub-agent
//! over the same workspace, and the child's result composes back into the
//! parent's next turn. Children nest, and many work at once up to
//! [`Containment::max_concurrent_agents`] — a spawn past that cap queues rather
//! than failing, and the queue survives a restart, while
//! [`Containment::max_total_agents`] still refuses. A child inherits its parent's
//! policy and can only narrow it ([`Policy::contain`]: allows intersect, denies
//! union, at any depth). [`run`] and [`run_with`] never expose the spawn tool.
//!
//! **An execution sandbox.** Commands the verification gate runs execute in an
//! ephemeral [`Sandbox`]: an isolated workdir, resource caps that *kill* rather
//! than throttle ([`SandboxLimits`]), outbound network denied by default, and
//! guaranteed teardown. One trait, a native [`Backend`] per platform over a
//! portable floor that runs everywhere, chosen by [`select`] and recorded in the
//! trace.
//!
//! **Durable, unattended runs.** After every completed step the trace, the
//! budget draw, and a checkpoint commit in one transaction, so a crash leaves
//! either a whole step or none of it. [`resume`] and [`resume_tree`] reconstruct
//! the run from the store and continue every agent from its own last committed
//! step: completed steps are not re-run, the [`Ledger`] is restored from durable
//! totals rather than reset or double-charged, and an irreversible edit already
//! applied is re-observed rather than repeated. [`resume_from_stored_policy`]
//! and [`resume_tree_from_stored_policy`] read the boundary the run was started
//! under back out of the store instead of trusting the caller to pass the same
//! one again — prefer them when the policy is what matters.
//! [`Store::run_status`] and [`RunStatus`] report where a run stands; a resume
//! against a missing or newer-format checkpoint is a typed [`Error::Resume`],
//! never a panic or a half-resume.
//!
//! **A request that can express a conversation.** [`CompletionRequest`] carries
//! [`messages`](CompletionRequest::messages) — an ordered transcript of user
//! turns, assistant turns holding the calls the model made, and batches of
//! results answering them — and each built-in wire maps it onto that vendor's own
//! block types one to one. Before 0.49.0 a run's own history reached the model as
//! prose describing it, which is off the distribution these models are trained
//! on. [`CompletionRequest::user`] is still filled, derived and byte-identical to
//! what it was, so a [`Provider`] that reads it keeps working; it is retained for
//! one release.
//!
//! **A parent that chooses how a child comes back.** [`SPAWN_TOOL`] takes `wait`
//! and `background_after_secs`: a parent can carry on while a sub-agent works and
//! read its report at a later step, and a child that outlasts a stated wall clock
//! moves to the background rather than holding the tree. It is never cancelled —
//! a parent that stops waiting is not a parent that stops the work — and the tree
//! still does not return while a child it started is running. What a child hands
//! back is what it concluded, beside what it cost;
//! [`TaskContract::with_spawn_background_after`] and
//! [`TaskContract::without_detached_spawns`] are the operator's ceiling on both.
//!
//! **Agents in a tree talking to each other.** Everything above is a *vertical*
//! edge: a parent starts a child, a child reports to its parent. Two children
//! investigating two subsystems had no way to tell each other what they found.
//! Now every agent in a tree has an **address** — [`SPAWN_TOOL`] takes an `as`,
//! and one is derived from the role and the run id when it is omitted — and that
//! address names *one* agent, unlike [`AgentDef::name`], which is a role several
//! children may share. [`SEND_MESSAGE_TOOL`] tells a named agent something;
//! [`READ_MESSAGES_TOOL`] returns what was sent to this one, oldest first,
//! delivered exactly once, optionally narrowed to one sender and optionally
//! blocking. Every wait is bounded by [`TaskContract::with_max_wait_secs`] or
//! [`DEFAULT_MAX_WAIT`], because an agent that blocks holds its concurrency slot
//! and the sibling that would answer it may be queued behind that slot; a request
//! over the ceiling is narrowed and said. A terminating agent posts one short line
//! to its parent, so waiting for a named child and waiting for a message are the
//! same call. Messages are [`AgentMessage`] rows, so a resumed tree reads the same
//! ones in the same order, once — and an address reaches inside its own tree and
//! nowhere else.
//!
//! **Providers, with fallback.** [`OpenRouter`], [`Anthropic`] and [`OpenAi`]
//! behind one [`Provider`] trait, over the crate's own HTTP+SSE client.
//! [`provider::Fallback`] moves to the next configured provider when one is down
//! or rate-limited, and failures are classified ([`ProviderErrorKind`]) so a
//! caller can tell a retryable transport error from a terminal one.
//! [`RetryPolicy`] governs the backoff and [`StallPolicy`] detects a run that is
//! repeating itself rather than progressing.
//!
//! **Extensibility, in-process and out.** Implement the object-safe [`Tool`]
//! trait for something the embedding program already does, collect them in a
//! [`Toolbox`], and register it with [`TaskContract::with_tools`] — no second
//! process, transport, or serialization hop. Or point the harness at MCP servers
//! with [`TaskContract::with_mcp`]: [`McpServer`]s spawned as child processes
//! ([`McpTransport::Stdio`]) or dialled over streamable HTTP
//! ([`McpTransport::Http`]), offered to the model under `mcp__<server>__<tool>`
//! so a server can never shadow a built-in. Either way registration makes a tool
//! *available*, not authorized: every call is an [`Act::Exec`] check on its
//! name. [`TaskContract::with_skills`] adds [`Skills`] — markdown instruction
//! files that shape how the agent approaches a class of task, loaded through
//! [`read_skill`](tools::READ_SKILL_TOOL) as an ordinary policy-checked read,
//! with no Rust at all.
//!
//! **Provider-executed web search and fetch.** [`TaskContract::with_web`] takes a
//! [`WebAccess`]: search, optionally fetch, a cap on provider-executed requests,
//! and the hosts to allow or block. One declaration, three translations —
//! Anthropic's dated server tools, OpenAI's `web_search_options`, OpenRouter's
//! `web` plugin — and what a vendor cannot express is an [`Error::Config`] before
//! the request is sent rather than a boundary quietly dropped.
//! [`WebAccess::from_policy`] derives the domain lists from the run's own
//! [`Act::Net`] rules. What the model drew on comes back as [`Citation`]s and is
//! recorded in the trace ([`Store::citations`]), beside a [`ServerToolCall`] row
//! per provider-executed call so a search that *broke* stays distinguishable from
//! one that found nothing. The provider dials the URL, so this crate opens no
//! socket for it: the domain filter is the vendor's and [`Act::Net`] never sees
//! the connection.
//!
//! ```
//! use io_harness::{TaskContract, WebAccess};
//!
//! let contract = TaskContract::workspace(
//!     "update the install line to the current release",
//!     "/path/to/repo",
//! )
//! .with_web(WebAccess::search().max_uses(3).allow("crates.io"));
//!
//! assert!(contract.web.is_some());
//! ```
//!
//! **Context that stays relevant.** Each turn is assembled to fit a stated share
//! of the token budget ([`ContextBudget`]): superseded observations are
//! compacted, and an observation a later write invalidated is re-read rather
//! than trusted. Durable memory keyed to the workspace ([`MemoryEntry`],
//! [`Store::memory_list`]) survives between runs, as a fact or a decision
//! ([`MemoryKind`]) and pinnable ([`Store::memory_pin`]) so a run cannot
//! overwrite a correction — a refused write is recorded and told to the agent
//! rather than swallowed. [`Store::memory_recalls`] says which entries a given
//! run actually drew on, which is a different question from what it knew.
//!
//! **What the trace adds up to.** One row per provider call and one per file
//! change, with money derived at query time from a [`pricing::PriceTable`] the
//! operator owns rather than stored. [`Store::spend_by_model`] and its two
//! siblings group the cost; [`Store::runs_by_outcome`], [`Store::first_try`],
//! [`Store::gate_failures_by_phase`] and [`Store::recovery`] group what the runs
//! *did* — how often a gate passed first time, which phase fails most, and how
//! many runs a fallback, a replan or a resume carried through. Grouped rows out;
//! the rendering is the consuming application's.
//!
//! **Observation and replay.** Register an [`Observer`] and be called as the run
//! happens — [`RunEvent`]s covering steps, tool calls, approvals, refusals,
//! spend draws, retries, fallbacks and outcomes — instead of polling the store;
//! [`Flow`] lets an observer ask the run to stop. [`provider::Record`] captures
//! a case and [`provider::Replay`] runs it back identically.
//!
//!
//! **Durable conversations.** [`Session`] holds one instead of firing a task:
//! open a session over a workspace, take a turn, and the next turn reads the ones
//! before it. A turn **is** a run — its own trace, budgets, boundary and
//! checkpoint — so a session survives a crash for the reason a run does, and
//! [`Session::reopen`] picks it up in a later process from the id alone. The
//! conversation is an append-only tree: [`Session::branch_from`] takes the next
//! turn from any earlier one without disturbing what came after it. An observed
//! turn streams the model's text as [`EventKind::Token`] while it is still being
//! produced, and a [`Steer`] lets an operator say something else mid-turn or
//! interrupt — both honoured at the next step boundary, and neither an
//! authorization: a steer reaches the model as text, and every call it leads to is
//! checked against the same [`Policy`].
//!
//! **Configuration in a file.** [`Config::discover`] reads one `io.toml` across
//! four scopes — the crate's defaults, a user file, a committed project file, and
//! a gitignored local one — and projects it onto this API: a [`Policy`], a
//! [`SandboxConfig`], the run budgets applied through [`Config::apply_to`], the
//! [`toolchain`] commands, a [`pricing::PriceTable`], and [`McpServer`]s.
//! `${env:...}` and `${file:...}` keep a credential out of the file, an unknown
//! key is an error rather than a shrug, and nothing is loaded implicitly — the
//! caller reads the file, before the run, once, which is what stops an agent
//! widening the boundary it is running under. [`Config::origin`] reports which
//! scope and which file decided a given key, for the operator whose setting did
//! not take effect.
//! **Documents and images**, behind opt-in features: spreadsheets, Word,
//! PowerPoint text, PDF, and barcode decoding, each gated on [`Act::Read`] or
//! [`Act::Write`] against the real path the model named, and verified with
//! [`Verification::DocumentContains`] rather than a container read as the empty
//! string; plus image passthrough to any provider whose model accepts one.
//!
//! **Git**, as fixed-argv built-ins: status, diff, log, add, commit (under a
//! caller-supplied [`Identity`]), branch and worktree, so a run ends as a
//! reviewable commit on a branch of its own rather than a working tree someone
//! has to reconstruct. The model supplies paths, a message and a branch name,
//! never a subcommand or a flag, so push, fetch, reset and rebase are
//! unreachable by construction — `git switch --create`, the one checkout that
//! cannot discard a change, is the only one of them that is reachable. An
//! [`AgentDef`] can ask for its own worktree, so concurrent children stop
//! overwriting each other's files.
//!
//! **Reads that start before the model stops talking.** A tool call inside a
//! completion is complete long before the message is. When a provider implements
//! [`Provider::complete_streaming_calls`] and reports a finished call while it is
//! still streaming, a streaming session turn starts the read-only ones then
//! rather than after — so the reads land in the time the model spends narrating
//! what it is about to do. It is bounded by what can be taken back: only the
//! completion's leading run of read-only calls, so a read never answers from
//! before a write that has not run; only what the [`Policy`] allows outright, so
//! no approver is asked about a turn that may be abandoned; and the result is used
//! only if the finished completion carries that same call with the same
//! arguments, otherwise it is discarded with nothing recorded. The trace and the
//! observations are identical either way, and a provider that does not implement
//! the method — including [`provider::Replay`] — starts nothing early.
//!
//! What none of it governs: a stdio MCP server and a registered [`Tool`] both
//! run outside the sandbox with the privileges of whoever started them, and a
//! provider-executed search or fetch is dialled by the provider, so no
//! [`Act::Net`] decision is taken for it at all. The harness decides what may
//! *start* and what may be *called* — not what a started thing then does.
//!
//! # Feature flags
//!
//! `default = []`. The default build compiles no optional dependency at all.
//!
//! | Feature | What it adds |
//! | --- | --- |
//! | `media` | Images to providers that accept them: the four types they document, plus BMP/TIFF/ICO/TGA/PNM converted to PNG on the way |
//! | `documents` | Umbrella over the five below |
//! | `xlsx` | Spreadsheet read, generate, and preserving single-cell edit |
//! | `docx` | Word read and generate (no in-place edit, deliberately) |
//! | `pptx` | PowerPoint text extraction (read-only, no writer) |
//! | `pdf` | PDF generate, extract text, watermark, fill AcroForm fields |
//! | `barcode` | Barcode and QR decoding from an image |
//!
//! # Minimum supported Rust
//!
//! **MSRV: Rust 1.95.** The floor comes from `libsqlite3-sys`, which publishes
//! no `rust-version` of its own, so cargo cannot catch it at resolve time — on
//! 1.94 the build fails inside that dependency's build script rather than
//! here, with an error about a missing `cfg_select` macro. It rose from 1.88 in
//! 0.23.0, and there is no `rusqlite` at or above that release's floor which
//! avoids it.
//!
//! # Platform support
//!
//! | Platform | Sandbox containment |
//! | --- | --- |
//! | macOS | Native, `sandbox-exec` |
//! | Linux | Native, namespaces and rlimits |
//! | Windows, default | Native resource containment (memory, CPU, process count, tree kill); no filesystem or network boundary |
//! | Windows, opt-in | Native access confinement: an AppContainer inside that job, writes confined and egress denied (0.59.0) |
//!
//! The Windows row is deliberately not the word "Native" on its own, because it
//! would not mean there what it means in the two rows above it. Since 0.24.0 a
//! Windows run is contained by a Job Object, so [`Backend::WindowsJobObject`] is
//! what the sandbox reports, [`Cap::Memory`], [`Cap::Cpu`] and the new
//! [`Cap::Processes`] are real bounds rather than fields nothing applies, and
//! Windows is the first backend anywhere to enforce
//! [`SandboxLimits::max_processes`]. But **a Job Object contains resources and
//! nothing else** — there is no filesystem facility and no network facility in
//! one. macOS confines writes to the working directory and denies outbound
//! network; Linux does the same through mount and network namespaces; Windows
//! does neither.
//!
//! The Linux half of that sentence became true in **0.40.0** and was not before
//! it, which is worth stating rather than quietly correcting. Until then the
//! backend unshared a mount namespace and remounted nothing into it: the
//! namespace existed, the filesystem view was the host's, and a write outside the
//! working directory landed. Only the network namespace was doing real work. The
//! backend now remounts the tree read-only inside its namespace and binds back
//! the working directory and the system temporary directory — the same two places
//! the macOS profile has always allowed — so "confines writes to the working
//! directory" describes both platforms. A host whose kernel refuses the remounts
//! degrades to [`Backend::PortableFloor`] and **reports the floor**, because the
//! one thing worse than no boundary is a boundary that is named and absent.
//!
//! **Windows has an access half since 0.59.0, and the caller chooses it.** By
//! default a contained Windows run still gets a Job Object: memory, CPU, active
//! processes and a tree kill on close, reported as
//! [`Backend::WindowsJobObject`], with no filesystem facility and no network
//! facility, so [`ExecMode`] is routed and reported there and enforces nothing.
//! [`SandboxConfig::with_access_confinement`] selects `sandbox::appcontainer`
//! instead — a low-box token that is default-deny on every securable object and
//! reaches only the paths this run resolved, reported as
//! [`Backend::WindowsAppContainer`], which both
//! [`Backend::confines_writes`] and [`Backend::denies_egress`] answer true for.
//!
//! It is the caller's decision because the grant set is derived from the run's
//! own facts and derived is not complete: a toolchain reading a machine-wide file
//! outside it is refused, and a default boundary that cannot run an arbitrary
//! payload is worse than one a caller reaches for deliberately. And unlike every
//! other backend here it does not degrade — a boundary asked for by name that
//! cannot be applied is an error naming the grant that failed, because a run that
//! silently took the Job Object instead would have no boundary at all while every
//! assertion about it still passed.
//!
//! Linux likewise stopped being one backend and a fallback. It is a chain —
//! Landlock, `bwrap`, the namespace backend, the floor — and the rung a host takes
//! is the strongest that can enforce what the run asked for, with one rule that can
//! send it lower: a run denying egress is never given a rung that cannot deny
//! egress. The Landlock rung needs no namespace, which is the whole point, because
//! a stock Ubuntu 24.04 refuses the one the older rung needs.
//!
//! **0.48.0 finished the sentence "everything a run starts is contained", and made
//! egress mean what the policy says.** A backgrounded `shell_start` handle and
//! the git built-ins now take the same containment every other spawn takes, so the
//! boundary no longer depends on which tool the model picked; each spawning tool
//! declares the mode it needs and a call runs under the narrower of that and what
//! the contract granted, with an unsatisfiable need refused before anything is
//! started. And a run whose [`Policy`] names hosts routes its contained commands
//! through a loopback proxy the run owns, which asks that policy about every
//! `host:port`. What the proxy proves differs per backend and the weaker answer is
//! reported: address-scoped on macOS, **port-scoped** under Landlock,
//! **advisory** on the portable floor and under a Windows Job Object — where the
//! agent's own boundary section uses that word rather than implying a boundary it
//! does not have — and **absent** under Windows access confinement, where a
//! process cannot reach a loopback listener at all under any capability set, so
//! egress is the capability rather than the route: all of the network, or none of
//! it.
//!
//! Two smaller differences worth knowing rather than discovering: the job's CPU
//! limit counts user-mode time only, where unix `RLIMIT_CPU` counts kernel time
//! too, so the cap is genuinely weaker there; and the memory limit makes an
//! allocation *fail* rather than terminating the process, so a payload usually
//! dies of its own failed allocation rather than being killed outright.
//!
//! The full suite runs on all three in CI.
//!
//! # Guides
//!
//! Longer prose than a doc comment should carry, one page per capability:
//!
//! - [Permissions and approval](https://github.com/initorigin/io-harness/blob/main/docs/guide/permissions.md)
//! - [Command execution](https://github.com/initorigin/io-harness/blob/main/docs/guide/command-execution.md)
//! - [Language support](https://github.com/initorigin/io-harness/blob/main/docs/guide/language-support.md)
//! - [Verification](https://github.com/initorigin/io-harness/blob/main/docs/guide/verification.md)
//! - [Agent composition](https://github.com/initorigin/io-harness/blob/main/docs/guide/composition.md)
//! - [Execution sandbox](https://github.com/initorigin/io-harness/blob/main/docs/guide/sandbox.md)
//! - [Durable runs](https://github.com/initorigin/io-harness/blob/main/docs/guide/durable-runs.md)
//! - [MCP and network egress](https://github.com/initorigin/io-harness/blob/main/docs/guide/mcp-and-network.md)
//! - [Tools and skills](https://github.com/initorigin/io-harness/blob/main/docs/guide/tools-and-skills.md)
//! - [Context and memory](https://github.com/initorigin/io-harness/blob/main/docs/guide/context-and-memory.md)
//! - [Resilience](https://github.com/initorigin/io-harness/blob/main/docs/guide/resilience.md)
//! - [Observability and replay](https://github.com/initorigin/io-harness/blob/main/docs/guide/observability.md)
//! - [Sessions](https://github.com/initorigin/io-harness/blob/main/docs/guide/sessions.md)
//! - [Agency](https://github.com/initorigin/io-harness/blob/main/docs/guide/agency.md)
//! - [Web search and fetch](https://github.com/initorigin/io-harness/blob/main/docs/guide/web.md)
//! - [Configuration](https://github.com/initorigin/io-harness/blob/main/docs/guide/configuration.md)
//! - [Accounting](https://github.com/initorigin/io-harness/blob/main/docs/guide/accounting.md)
//! - [Documents](https://github.com/initorigin/io-harness/blob/main/docs/guide/documents.md)
//! - [Images and git](https://github.com/initorigin/io-harness/blob/main/docs/guide/images-and-git.md)
//!
//! [The public contract](https://github.com/initorigin/io-harness/blob/main/docs/CONTRACT.md)
//! states what is stable, what may change, and the limits that hold today. The
//! crate is pre-1.0: a minor release may break the contract, and when it does it
//! is marked in
//! [CHANGELOG.md](https://github.com/initorigin/io-harness/blob/main/CHANGELOG.md)
//! with a migration note. That file is where the release history lives.
//!

// docs.rs builds with every feature on and sets the `docsrs` cfg (see
// Cargo.toml). This labels each gated item with the feature it needs, so a
// reader browsing the rendered docs is never shown an item that would not exist
// in their build without being told why. Nightly-only, and reached only under
// that cfg — a stable `cargo doc` is unaffected.
//
// `doc_cfg`, not `doc_auto_cfg`: the latter was removed in 1.92.0 (rust-lang
// PR 138907) and merged into `doc_cfg`, which now does the automatic labelling
// itself. 0.16.1's docs.rs build failed on the removed feature name.
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod agent;
pub mod approve;
pub mod attach;
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub mod browser;
#[cfg(feature = "codeact")]
#[cfg_attr(docsrs, doc(cfg(feature = "codeact")))]
pub mod codeact;
pub mod config;
pub mod containment;
pub mod context;
mod contract;
mod diff;
mod error;
mod harness;
pub mod hooks;
pub mod lsp;
pub mod mcp;
#[cfg(feature = "mcp-server")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp-server")))]
pub mod mcp_server;
pub mod net;
pub mod observe;
#[cfg(feature = "otel")]
#[cfg_attr(docsrs, doc(cfg(feature = "otel")))]
pub mod otel;
pub mod plugin;
pub mod policy;
pub mod pricing;
pub mod provider;
pub mod resilience;
mod run;
pub mod sandbox;
pub mod schema;
pub mod session;
pub mod skills;
mod state;
pub mod template;
pub mod toolchain;
pub mod tools;
mod verify;
pub mod web;

pub use approve::{
    AcceptPlan, AnswerFuture, AnswersFuture, ApprovalContext, ApproveAll, Approver, Choice,
    Decision, DenyAll, FixedResponder, ModelApprover, Plan, PlanGate, PlanGateNone, PlanReview,
    PlanStep, PlanVerdict, Question, Request, Responder, ResponderNone, StdinApprover,
    StdinPlanGate, StdinResponder,
};
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser::BrowserConfig;
#[cfg(feature = "codeact")]
#[cfg_attr(docsrs, doc(cfg(feature = "codeact")))]
pub use codeact::{
    CodeActConfig, CODEACT_CANDIDATES, CODEACT_MIN_PYTHON, CODEACT_UNCALLABLE,
};
pub use config::{Config, ProviderSpec};
pub use containment::{Containment, Draw, FleetTally, Ledger, SpawnRefusal};
// `Origin` joins them in 0.77.0 for a reason the other three do not have:
// `EventKind::ToolCall`, a root-level type, now names it in a field. A caller
// matching on that field would otherwise have to reach into `context` for a type
// the root already hands them the container of.
pub use context::{Collapse, Compaction, ContextBudget, Origin};
pub use contract::{
    Preset, Routing, SystemPrompt, TaskContract, DEFAULT_MAX_RETRIES, DEFAULT_MAX_STEPS,
    DEFAULT_WORKSPACE_MAX_STEPS,
};
pub use error::{Error, ProviderErrorKind, Result, StorageErrorKind};
pub use harness::Harness;
pub use hooks::{Hook, Hooks, OnFailure};
pub use lsp::LspServer;
pub use mcp::{probe_mcp, McpProbe, McpServer, McpTransport, MCP_TOOL_PREFIX};
#[cfg(feature = "mcp-server")]
#[cfg_attr(docsrs, doc(cfg(feature = "mcp-server")))]
pub use mcp_server::{
    serve_mcp, serve_mcp_with, McpServerConfig, MCP_SERVER_PROTOCOL_VERSION, MCP_SERVER_UNSERVED,
};
#[cfg(feature = "otel")]
#[cfg_attr(docsrs, doc(cfg(feature = "otel")))]
pub use otel::{OtelConfig, OtelExporter, GENAI_CONVENTIONS, OTEL_DEFAULT_ENDPOINT};
// The `net` module is public as of 0.71.0, but it exports exactly one function —
// `target` — and the default request deadline keeps this crate-root re-export it
// has always had. A caller overriding it with `with_timeout` should be able to
// name the value they are overriding without reaching into a provider's
// namespace, or into a module whose only other public item is a policy helper.
pub use attach::{Attach, Waiting, POLL_LIMIT};
pub use net::{target, REQUEST_TIMEOUT};
pub use observe::{Broadcast, EventKind, Flow, Ignore, Observer, RunEvent};
pub use plugin::{Dropped, Plugin, Plugins, MAX_ID, NAMESPACE, PLUGIN_FILE};
pub use policy::{Act, Defaults, Effect, Layer, Policy, Rule, Verdict};
pub use provider::{
    Anthropic, Auth, Compatible, CompletionRequest, CompletionResponse, Effort, Message, ModelInfo,
    OpenAi, OpenRouter, PriceSource, PromptFamily, Provider, Reference, ToolCall, ToolResult,
    ToolSpec, Usage,
};
#[cfg(feature = "media")]
pub use provider::{Media, IMAGE_MEDIA_TYPES};
pub use resilience::{Progress, Progressing, RetryPolicy, StallPolicy};
// Each entry point has an observed twin: a separate function rather than an extra
// parameter on the existing seven, so 0.11.0 code compiles unchanged against
// 0.12.0. The observer is this release's headline, not a reason to break every
// caller that does not want one.
pub use run::{
    resume, resume_from_stored_policy, resume_from_stored_policy_observed, resume_observed,
    resume_tree, resume_tree_from_stored_policy, resume_tree_from_stored_policy_observed,
    resume_tree_observed, resume_tree_with_answer, resume_tree_with_answer_observed,
    resume_tree_with_decision, resume_tree_with_decision_observed, resume_tree_with_plan_decision,
    resume_tree_with_plan_decision_observed, resume_with, resume_with_answer,
    resume_with_answer_observed, resume_with_decision, resume_with_decision_observed,
    resume_with_observed, resume_with_plan_decision, resume_with_plan_decision_observed,
    resume_with_recovery, resume_with_recovery_observed, retry_gate, retry_gate_observed, rewind,
    rewind_run, rewind_run_observed, rewind_step, rewind_step_observed, run, run_observed,
    run_tree, run_tree_observed, run_with, run_with_observed, RecoveryDecision, Reverted, Rewind,
    Rewound, RunOutcome, RunResult, DEFAULT_LEASE_TTL, DEFAULT_MAX_WAIT, READ_MESSAGES_TOOL,
    SEND_MESSAGE_TOOL, SPAWN_TOOL,
};
pub use sandbox::{
    copy_back, select, Backend, Cap, ExecMode, Sandbox, SandboxConfig, SandboxLimits,
    SandboxOutcome, Selected,
};
// The type only. `schema::extract_json` stays in its module: at the crate root
// it would be a name with no subject, and the one caller that wants it is the
// run loop, which is already naming the module to build the schema.
pub use schema::OutputSchema;
pub use session::{
    Session, Steer, SteerInbox, Steering, Transcript, TranscriptTurn, TurnKind, TurnResult,
};
pub use skills::{Skill, Skills};
// `AgentEvent` and `SpawnRow` were `pub` inside this private module but were not
// re-exported, so `Store::agent_events` and `Store::find_spawn` returned values an
// external caller could hold and could not name — which made `agent_events`, the
// only audit of per-step budget draws against the shared tree ledger, unreadable
// through the public API. Exported in 0.12.0: an observability release cannot ship
// leaving its own audit table reachable only by opening the SQLite file.
pub use agent::{AgentDef, Agents};
pub use state::{
    AgentEvent, AgentMessage, Archived, AssistantTurn, CheckpointEvent, ContextEvent, Edit,
    FirstTry, GateAttempt, GateOutcome, Lease, LeaseRow, McpEvent, MemoryEntry, MemoryForget,
    MemoryKind, MemoryLimits, MemoryRecall, MemoryWrite, Pending, PendingPlan, PendingQuestion,
    PolicyEvent, ProcessHandle, ProviderCall, Pruned, Recovery, RewindRecord, RunStatus,
    RunSummary, SandboxEvent, SessionSize, SpawnRow, StepAttribution, StepRecord, Store, StoreSize,
    Summary, Tally, TodoItem, TodoState, ToolAttempt, Turn, BUSY_TIMEOUT, CHECKPOINT_FORMAT,
    GLOBAL_MEMORY_WORKSPACE, MEMORY_MAX_CHARS, MEMORY_MAX_ENTRIES, MEMORY_MAX_ENTRY_CHARS,
    ROOT_ADDRESS, SUCCESS_OUTCOME, TODO_MAX_ITEMS, TODO_TEXT_CAP, UNKNOWN_MODEL,
};
pub use template::{Template, Templates};
pub use tools::git::Identity;
pub use tools::{
    Tool, ToolEffect, ToolFuture, ToolMask, ToolRecovery, Toolbox, ASK_QUESTIONS_TOOL,
    ASK_QUESTION_TOOL, DEFAULT_EXEC_TIMEOUT, PROPOSE_PLAN_TOOL, TODO_WRITE_TOOL,
};
pub use verify::{
    ChangeReview, ExecGuard, FileChange, ModelReviewer, Review, ReviewRequest, Reviewer, Reviewing,
    Verification, TEST_BINARY,
};
pub use web::{Citation, ServerToolCall, WebAccess};
