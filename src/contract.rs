//! The task contract: what the agent is asked to do and how success is judged.

use std::path::PathBuf;
use std::time::Duration;

use crate::context::{Compaction, ContextBudget};
use crate::resilience::{RetryPolicy, StallPolicy};
use crate::verify::Verification;

/// What the caller wants the system prompt to say (0.45.0).
///
/// Until 0.45.0 the system prompt was three private `const`s an embedder could
/// neither read nor change, so a program built on this crate could not give its
/// agent its own voice without forking. This is the whole of the answer, and it
/// is deliberately three states rather than a catalogue: a preset shipped by a
/// library is an opinion about model behaviour that library cannot test and can
/// never withdraw, and a preset written by the embedder is a `const` in the
/// embedder's own crate.
///
/// What no variant can reach is the crate's own ending — the sentence that
/// decides what a turn is. It is emitted **last**, after everything here, because
/// the guarantee it produces ([`TurnKind::Reply`](crate::TurnKind::Reply) stages
/// no step, no gate, no checkpoint and no approval) is one `docs/CONTRACT.md`
/// makes to a reader who never sees the embedder's prompt:
///
/// ```
/// use io_harness::{SystemPrompt, TaskContract};
///
/// let contract = TaskContract::workspace("port the parser", "/tmp/repo")
///     .with_system_prompt(SystemPrompt::Append(
///         "You are Acme's release bot. Prefer the smallest diff that works.".into(),
///     ));
///
/// assert!(matches!(contract.prompt, SystemPrompt::Append(_)));
/// ```
///
/// `#[non_exhaustive]` from birth: a fourth state is a thing a minor may add, and
/// this way it costs nobody a recompile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SystemPrompt {
    /// The crate's own description of the agent and its tools. Every release
    /// before 0.45.0, and still the default.
    #[default]
    Builtin,
    /// The crate's description, then this text, then the crate's own sections and
    /// its ending.
    Append(String),
    /// This text **instead of** the crate's description. The tool catalogue, the
    /// skills catalogue, the repository's instructions, the boundary section and
    /// the ending are all still composed around it — replacing the description
    /// does not replace what the crate has to say about the request it is
    /// building.
    Replace(String),
}

/// A single unit of work handed to the harness.
///
/// The agent edits one file to meet [`Verification`], bounded by budgets. v0.2
/// adds the time and cost (token) budgets and the retry limit; a 0.1.0 caller
/// that only set `goal`, `file`, `verify`, and `max_steps` still compiles —
/// the new bounds default to unbounded / two retries.
///
/// Every entry point in the crate takes one of these, so it is where a run's
/// definition of done and its ceilings are decided — before the model is asked
/// anything, and independently of which provider will serve it:
///
/// ```
/// use io_harness::{TaskContract, Verification};
/// use std::time::Duration;
///
/// let contract = TaskContract::workspace(
///     "make `parse` return an error on empty input instead of panicking",
///     "/path/to/repo",
/// )
/// // The criterion is checked by running the project's own suite, so a
/// // plausible-looking stub cannot satisfy it. This is the half of the
/// // contract that decides whether the run *succeeded*, as opposed to merely
/// // stopping. It is opt-in: a contract that never asks for one is unverified,
/// // which is the honest description of a task with nothing to check.
/// .with_verification(Verification::Command {
///     argv: vec!["cargo".into(), "test".into()],
///     expect_exit: 0,
/// })
/// // And this is the half that decides when it stops regardless. All three are
/// // independent stops with their own `RunOutcome`, so a run that ran out of
/// // money is distinguishable afterwards from one that ran out of ideas.
/// .with_max_steps(20)
/// .with_time_budget(Duration::from_secs(900))
/// .with_token_budget(200_000)
/// // Surfaced to the model verbatim. A constraint is guidance, not a boundary —
/// // what the agent may actually touch is the `Policy`'s job, because the model
/// // can ignore a sentence and cannot ignore a refusal.
/// .with_constraint("do not change the public signature of `parse`");
///
/// assert_eq!(contract.max_steps, 20);
/// assert!(contract.root.is_some()); // workspace mode: grep, find, read, write
/// ```
///
/// [`TaskContract::new`] is the other constructor: one file, one tool, and no
/// policy enforcement — a policy passed to a single-file run is refused with
/// [`Error::Config`](crate::Error::Config) rather than silently ignored. Reach
/// for [`TaskContract::workspace`] for anything with a boundary.
///
/// The `with_*` builders that add *capability* rather than bounds —
/// [`with_mcp`](TaskContract::with_mcp), [`with_tools`](TaskContract::with_tools),
/// [`with_skills`](TaskContract::with_skills) — are workspace-mode only and are
/// validated at run start, so a duplicate tool name or an unreadable skills
/// directory fails the run before the first completion is billed.
///
/// `#[non_exhaustive]` since 0.35.0, which added [`TaskContract::plugins`]. A
/// contract is built with [`TaskContract::new`] or [`TaskContract::workspace`]
/// and narrowed with the `with_*` builders — every documented caller — and those
/// are untouched; an external struct literal or an exhaustive destructuring is
/// what stops compiling, once, so that the next field costs nobody anything.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskContract {
    /// Plain-language goal, e.g. "add a `hello` function that returns 42".
    pub goal: String,
    /// The one file the agent may read and write in single-file mode. In
    /// workspace mode (`root` is `Some`) it is unused.
    pub file: PathBuf,
    /// Workspace root for multi-file mode. `None` (the 0.1/0.2 default) runs the
    /// single-file loop over `file`. `Some(dir)` runs the workspace loop, where
    /// the agent greps/finds/reads/writes several files under `dir`.
    pub root: Option<PathBuf>,
    /// Extra rules the agent must respect, surfaced to the model verbatim.
    pub constraints: Vec<String>,
    /// What the system prompt says (0.45.0). [`SystemPrompt::Builtin`] by
    /// default, which is every release before it.
    pub prompt: SystemPrompt,
    /// A repository's own guidance, carried in the system block (0.45.0).
    ///
    /// What `[instructions]` discovers (`AGENTS.md` by default) lands here rather
    /// than in [`TaskContract::constraints`], where it landed from 0.27.0 to
    /// 0.44.0. The two are different things: a constraint is a rule the goal is
    /// checked against, and this is guidance the agent reads. It is also
    /// **untrusted text** — a repository is not the operator — so it is carried in
    /// a delimited section, framed as guidance rather than instruction, and
    /// emitted before both the boundary section and the crate's ending, which it
    /// therefore cannot displace.
    pub instructions: Vec<String>,
    /// The checkable success criterion. The run succeeds when this passes.
    ///
    /// [`TaskContract::new`] takes it positionally; [`TaskContract::workspace`]
    /// starts it at [`Verification::None`] and leaves it to
    /// [`TaskContract::with_verification`], since most workspace tasks have
    /// nothing to check and should not have to say so.
    pub verify: Verification,
    /// Step budget: hard cap on loop iterations. The run stops when reached.
    pub max_steps: u32,
    /// Time budget: the run stops if it runs longer than this. `None` = unbounded.
    pub max_duration: Option<Duration>,
    /// Cost budget, measured in total tokens summed across completions (no
    /// price telemetry exists, so cost is counted in tokens). `None` = unbounded.
    pub max_tokens: Option<u64>,
    /// How many times a failing provider/tool step is retried before the run
    /// escalates the error. Defaults to 2.
    pub max_retries: u32,
    /// MCP servers to connect for this run. Their tools are offered to the model
    /// beside the built-ins, namespaced `mcp__<server>__<tool>`.
    ///
    /// Empty by default, so a 0.7.0-era contract behaves exactly as before.
    /// Workspace mode only: single-file mode has one tool and no tool layer to
    /// extend.
    #[allow(clippy::doc_markdown)]
    pub mcp: Vec<crate::mcp::McpServer>,
    /// Images handed to the agent alongside the goal, shown to the model on
    /// every step of the run.
    ///
    /// This is the caller's half of the image capability: the task is *about*
    /// these, so they persist for the whole run rather than being attached once.
    /// The agent's own half — looking at an image already in the workspace — is
    /// the `view_image` built-in, which is gated on the path the model names.
    ///
    /// Empty by default, so a 0.14.0-era contract behaves exactly as before. A
    /// provider that does not accept images refuses a run carrying any, before
    /// anything is sent; see [`crate::Provider::accepts_images`].
    #[cfg(feature = "media")]
    pub images: Vec<crate::provider::Media>,
    /// Who a commit the agent makes is attributed to.
    ///
    /// Defaults to an agent identity at a domain reserved so it can never exist.
    /// `git commit` fails outright with no `user.email` configured, so this
    /// cannot be left to the machine, and inheriting the repository's identity
    /// would attribute the agent's commit to whichever human configured that
    /// checkout.
    pub commit_identity: crate::tools::git::Identity,
    /// Tools the embedding program supplies itself, offered to the model beside
    /// the built-ins and governed by the same policy and trace.
    ///
    /// Empty by default, so a 0.8.1-era contract behaves exactly as before. In
    /// process, unlike [`TaskContract::mcp`]: see [`crate::tools::Tool`] for what
    /// registration does and does not authorize.
    pub tools: crate::tools::Toolbox,
    /// How long to wait between provider attempts, and how long a wait may grow.
    ///
    /// Applies only to a failure that
    /// [`is_retryable`](crate::error::ProviderErrorKind::is_retryable); an
    /// authentication failure escalates on its first occurrence however patient
    /// this is.
    pub retry: RetryPolicy,
    /// When to decide the agent has stopped making progress, and how many times to
    /// tell it. `StallPolicy { window: 0, .. }` switches detection off.
    pub stall: StallPolicy,
    /// How much of each request the observation log may occupy.
    ///
    /// Defaults to [`ContextBudget::default`]. Separate from
    /// [`TaskContract::max_tokens`] because they bound different things: that is
    /// what the whole run may *spend*, this is what one request may *carry* —
    /// though the two are related, since the share is taken of what the spend
    /// budget has left.
    pub context: ContextBudget,
    /// When the run's history is folded into a written summary (0.43.0).
    ///
    /// Defaults to [`Compaction::default`], which folds — the failure it replaces
    /// is a prompt the caller never sees, so it is not one an embedder can opt
    /// into fixing. `Compaction { at_share: 1.0, .. }` is 0.42.0's behaviour
    /// exactly, and is a setting rather than an absence.
    pub compaction: Compaction,
    /// How long a command the agent runs with the `exec` tool may take before it
    /// is killed and reported as a timeout.
    ///
    /// Defaults to [`DEFAULT_EXEC_TIMEOUT`](crate::DEFAULT_EXEC_TIMEOUT). Set it
    /// with [`TaskContract::with_exec_timeout`]. Separate from
    /// [`TaskContract::max_duration`] because they bound different things: that is
    /// how long the whole run may take, this is how long any one command may hang
    /// before the run gets its turn back — without it, a wedged command consumes
    /// the run's whole time budget and the run reports a budget stop, which is
    /// the wrong diagnosis for what happened.
    pub exec_timeout: Duration,
    /// Where this run's own commands may write, and under what caps.
    ///
    /// **Contained by default since 0.46.0.** The default is
    /// [`ExecMode::WorkspaceWrite`](crate::ExecMode::WorkspaceWrite) with
    /// [`SandboxLimits::none`](crate::sandbox::SandboxLimits::none): every command
    /// `exec` and the `shell` tools start is wrapped by the backend
    /// [`select`](crate::sandbox::select) chooses, may write inside the workspace
    /// root, the system temp directory and the detected toolchain's own cache
    /// directories — and nowhere else — with **no** resource cap at all. The
    /// **workspace root** is its working directory, so nothing is copied to a
    /// temporary directory and nothing is discarded, and an incremental build
    /// survives between commands.
    ///
    /// Up to 0.45.0 this was an `Option` defaulting to `None`, which ran every
    /// command at the embedding program's own privileges. That grant is still
    /// available and is now a sentence rather than an omission:
    /// [`TaskContract::with_full_access`]. The caps are likewise a decision —
    /// [`TaskContract::with_contained_exec`] with
    /// [`SandboxConfig::new`](crate::sandbox::SandboxConfig::new) asks for the
    /// standing CPU, wall, memory and file-descriptor ceilings on top of the
    /// boundary.
    ///
    /// What the mode does **not** decide, and each is documented in
    /// `docs/CONTRACT.md`: egress, which comes from this run's [`Policy`] and is
    /// still one boolean per run, so a policy allowing one host permits all of
    /// them under containment; the `shell_start` / `shell_poll` / `shell_kill`
    /// handles, which are not contained because a handle outlives the call that
    /// made it; and what a host can actually enforce — macOS and Linux confine
    /// writes and deny egress, while a Windows Job Object and the portable floor
    /// apply the resource caps and have no filesystem facility at all, so there
    /// the mode is routed and reported and enforces nothing for the filesystem.
    ///
    /// [`Policy`]: crate::Policy
    pub exec_sandbox: crate::sandbox::SandboxConfig,
    /// Directory of skill files to offer the agent, or `None` (the default) for
    /// no skills.
    ///
    /// The *path* is held rather than the discovered set, because reading a
    /// directory is fallible and a builder method is not: discovery happens at
    /// run start, so a directory that does not exist fails the run with
    /// [`Error::Config`](crate::Error::Config) naming the path — the same point
    /// and the same way [`TaskContract::tools`] is arbitrated.
    pub skills: Option<PathBuf>,
    /// The capability bundles this run loaded (0.35.0).
    ///
    /// Empty by default, which is every release before 0.35.0. Set it with
    /// [`Plugins::apply_to`](crate::Plugins::apply_to), which also folds each
    /// bundle's agents and MCP servers into the fields above — this field carries
    /// what neither of those can: the skills directories, which are one per
    /// bundle where [`TaskContract::skills`] is one per contract.
    ///
    /// It is why this type is `#[non_exhaustive]` as of 0.35.0. Adding a public
    /// field is a break for an external struct literal, and paying that once here
    /// makes the next contract field free.
    pub plugins: crate::plugin::Plugins,
    /// Named agent definitions a spawn may ask for by name (0.21.0).
    ///
    /// Empty by default, which is exactly the spawn behaviour of every release
    /// before 0.21.0. A definition can only ever *narrow* the child's boundary —
    /// composed through [`Policy::contain`](crate::Policy::contain), which has
    /// bounded every child since 0.5.0 — so registering a roster grants nothing.
    pub agents: crate::agent::Agents,
    /// Who answers the agent's questions about intent, in this process (0.21.0).
    ///
    /// `None` — the default — means nobody does, so a question persists and pauses
    /// the run for a human, which is the honest default for unattended work.
    ///
    /// Carried on the contract rather than passed to every entry point, the way a
    /// [`Toolbox`](crate::Toolbox) is: adding an argument to `run`, `run_with`,
    /// `run_tree` and their observed and resume variants would break every existing
    /// call site to add something almost all of them would pass `None` for.
    ///
    /// Behind an `Arc` so a whole tree shares one responder, exactly as it shares one
    /// [`Approver`](crate::Approver).
    pub responder: Option<std::sync::Arc<dyn crate::approve::Responder>>,
    /// What the provider may look up on the agent's behalf (0.22.0).
    ///
    /// `None` — the default — is every release before 0.22.0: nothing is declared
    /// and no vendor is asked to search. Set it with [`TaskContract::with_web`].
    ///
    /// Carried on the contract rather than passed to an entry point, the way a
    /// [`Toolbox`](crate::Toolbox) is, and governing the whole tree: a spawned
    /// child searches under the same declaration its parent did.
    pub web: Option<crate::web::WebAccess>,
    /// Who reviews the plan the agent proposes, before it acts on any of it
    /// (0.31.0).
    ///
    /// `None` — the default, and every release before 0.31.0 — is no plan gate at
    /// all: the run starts working immediately, exactly as it always has.
    /// `Some(gate)` opens the run in a planning phase where every
    /// [`Act::Write`](crate::Act::Write) and [`Act::Exec`](crate::Act::Exec) is
    /// denied under a `plan-gate` policy layer, and the only way out is a
    /// [`Plan`](crate::Plan) the gate approves.
    ///
    /// Workspace mode only, and the root agent only. A spawned child does not hold
    /// its own gate: a hundred children each pausing on a plan is the problem the
    /// gate exists to prevent, not a feature of it.
    ///
    /// Behind an `Arc` for the same reason [`Self::responder`] is.
    pub plan_gate: Option<std::sync::Arc<dyn crate::approve::PlanGate>>,
    /// How hard the root agent's model should think (0.31.0).
    ///
    /// `None` — the default, and every release before 0.31.0 — asks for nothing and
    /// sends the body 0.30.0 sent, leaving the vendor's own default in place. A
    /// spawned child takes the tier on its [`AgentDef`](crate::AgentDef) instead,
    /// which is where "search cheaply, write carefully" is said.
    pub effort: Option<crate::provider::Effort>,
    /// Who answers a [`Verification::Review`](crate::Verification::Review)
    /// criterion (0.34.0).
    ///
    /// `None` — the default, and every release before 0.34.0 — is no reviewer, and
    /// a contract that carries a review criterion without one fails at run start
    /// rather than at the gate: the mistake is a configuration error and it should
    /// cost nothing to find.
    ///
    /// Behind an `Arc` for the same reason [`Self::responder`] and
    /// [`Self::plan_gate`] are — one reviewer for a whole tree.
    pub reviewer: Option<std::sync::Arc<dyn crate::verify::Reviewer>>,
    /// The operator's own `before_tool` checks, from `io.toml` (0.42.0).
    ///
    /// `None` — the default — is no lifecycle gate at all. The same
    /// [`Hooks`](crate::Hooks) value that an application installs as an
    /// [`Observer`](crate::Observer) is what goes here: as an observer it ignores
    /// the `at` tables, as a gate it ignores the `on` ones, so an operator writes
    /// one file and the application makes one decision about whether to honour it.
    ///
    /// Nothing is implicit. A configuration describing a `before_tool` hook does
    /// nothing until an application installs it, which is the rule every other
    /// projection of that file already obeys.
    pub tool_hooks: Option<std::sync::Arc<crate::hooks::Hooks>>,
    /// Rules that change which model the run asks, while it is running (0.34.0).
    ///
    /// `None` — the default — is every release before 0.34.0: whichever model the
    /// provider was built with answers every step. See [`Routing`](crate::Routing).
    ///
    /// The **root** agent's, like [`Self::effort`]. A spawned child takes the model
    /// on its [`AgentDef`](crate::AgentDef), which is where a role's own model is
    /// said.
    pub routing: Option<crate::contract::Routing>,
    /// How many read-only tool calls from one completion may be in flight at once
    /// (0.41.0).
    ///
    /// Defaults to 10. It caps calls *in flight*, not
    /// calls attempted, so the number that actually run together is
    /// `min(this, read-only calls in that completion)` — a completion carrying
    /// four reads runs four whatever this says.
    ///
    /// `1` is every release before 0.41.0, exactly: one call at a time, in the
    /// order the model asked. That is the point of the floor being 1 rather than
    /// 2 — an embedder who suspects the concurrency while debugging something
    /// else sets it and is back on 0.40.0's execution path without changing
    /// anything else. Set it with [`TaskContract::with_max_parallel_reads`].
    ///
    /// It bounds tool calls inside one step of one agent, and nothing else.
    /// [`Containment::max_concurrent_agents`](crate::Containment) bounds a tree's
    /// children; the two are independent by design.
    pub max_parallel_reads: usize,
}

impl TaskContract {
    /// Minimal contract: goal, target file, and a success criterion.
    /// Defaults to 8 steps, no time/token budget, 2 retries, no constraints.
    pub fn new(goal: impl Into<String>, file: impl Into<PathBuf>, verify: Verification) -> Self {
        Self {
            goal: goal.into(),
            file: file.into(),
            root: None,
            constraints: Vec::new(),
            prompt: SystemPrompt::Builtin,
            instructions: Vec::new(),
            verify,
            plan_gate: None,
            effort: None,
            reviewer: None,
            tool_hooks: None,
            routing: None,
            max_steps: 8,
            max_duration: None,
            max_tokens: None,
            max_retries: 2,
            mcp: Vec::new(),
            commit_identity: crate::tools::git::Identity::default(),
            #[cfg(feature = "media")]
            images: Vec::new(),
            tools: crate::tools::Toolbox::new(),
            context: ContextBudget::default(),
            compaction: Compaction::default(),
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            exec_timeout: crate::tools::DEFAULT_EXEC_TIMEOUT,
            exec_sandbox: crate::sandbox::SandboxConfig {
                limits: crate::sandbox::SandboxLimits::none(),
                ..crate::sandbox::SandboxConfig::new()
            },
            skills: None,
            plugins: crate::plugin::Plugins::none(),
            agents: crate::agent::Agents::new(),
            responder: None,
            web: None,
            max_parallel_reads: 10,
        }
    }

    /// A workspace task: the agent may grep, find, read, and write several files
    /// under `root`.
    ///
    /// Verification defaults to [`Verification::None`] and stays there until
    /// [`with_verification`](TaskContract::with_verification) says otherwise,
    /// because a checkable criterion is the exception in workspace work rather
    /// than the rule. "Work out why the deploy fails", "summarise what this module
    /// does", "port the parser and tell me what you found" are runs with an answer
    /// and no gate, and while this constructor demanded a criterion every one of
    /// those callers had to type `Verification::None` to say the thing that was
    /// already true. Defaulting it puts the argument back where it belongs: on the
    /// calls that really do have something to check, spelled out at the point they
    /// ask for it.
    ///
    /// When there *is* a gate it should be a multi-file variant —
    /// [`Verification::EachCompilesRust`], or a [`Verification::Command`] running
    /// the project's own suite — since a workspace run touches more than the one
    /// file a single-file criterion knows about.
    ///
    /// [`TaskContract::new`] is deliberately unchanged and still takes its
    /// criterion positionally: a single-file task names one file and one thing that
    /// must become true of it, so there is no honest default to fall back to there.
    ///
    /// The rest of the defaults match [`TaskContract::new`] (12 steps here, since
    /// repo tasks take more turns), no time/token budget, 2 retries.
    pub fn workspace(goal: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            goal: goal.into(),
            file: root.clone(),
            root: Some(root),
            constraints: Vec::new(),
            prompt: SystemPrompt::Builtin,
            instructions: Vec::new(),
            verify: Verification::None,
            plan_gate: None,
            reviewer: None,
            tool_hooks: None,
            routing: None,
            effort: None,
            max_steps: 12,
            max_duration: None,
            max_tokens: None,
            max_retries: 2,
            mcp: Vec::new(),
            commit_identity: crate::tools::git::Identity::default(),
            #[cfg(feature = "media")]
            images: Vec::new(),
            tools: crate::tools::Toolbox::new(),
            context: ContextBudget::default(),
            compaction: Compaction::default(),
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            exec_timeout: crate::tools::DEFAULT_EXEC_TIMEOUT,
            exec_sandbox: crate::sandbox::SandboxConfig {
                limits: crate::sandbox::SandboxLimits::none(),
                ..crate::sandbox::SandboxConfig::new()
            },
            skills: None,
            plugins: crate::plugin::Plugins::none(),
            agents: crate::agent::Agents::new(),
            responder: None,
            web: None,
            max_parallel_reads: 10,
        }
    }

    /// Decide what "succeeded" means for this run.
    ///
    /// The other half of what [`TaskContract::workspace`] leaves open: that
    /// constructor starts a contract at [`Verification::None`], and this is how a
    /// caller with something to check says so. Setting a criterion changes what the
    /// *outcome* is allowed to be, not what the agent is allowed to do — the
    /// boundary is the [`Policy`](crate::Policy)'s job, and a gate that passes says
    /// nothing about how the workspace got that way. [`Verification`] documents
    /// exactly what each variant does and does not prove.
    ///
    /// A named builder rather than a positional argument, for the same reason every
    /// other capability on this type is one: every entry point takes a
    /// `TaskContract`, so setting it here works with all sixteen of them and changes
    /// no signature anywhere else.
    ///
    /// ```
    /// use io_harness::{TaskContract, Verification};
    ///
    /// // Nothing to check: the deliverable is an answer, and the run ends when the
    /// // agent stops calling tools.
    /// let unverified = TaskContract::workspace("work out why the deploy fails", "/repo");
    /// assert!(matches!(unverified.verify, Verification::None));
    ///
    /// // Something to check: the project's own suite decides, not the model.
    /// let gated = TaskContract::workspace("make the failing test pass", "/repo")
    ///     .with_verification(Verification::Command {
    ///         argv: vec!["cargo".into(), "test".into()],
    ///         expect_exit: 0,
    ///     });
    /// assert!(matches!(gated.verify, Verification::Command { .. }));
    /// ```
    #[must_use]
    pub fn with_verification(mut self, verify: Verification) -> Self {
        self.verify = verify;
        self
    }

    /// Connect these MCP servers for the run and offer their tools to the model.
    ///
    /// Each server is authorized before it is reached — spawning a stdio server
    /// is an exec check on its binary, dialling an HTTP one is a network check
    /// on its host — so configuring a server here does not grant access to it.
    pub fn with_mcp<I>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = crate::mcp::McpServer>,
    {
        self.mcp = servers.into_iter().collect();
        self
    }

    /// Let the provider look things up on the agent's behalf (0.22.0).
    ///
    /// The *provider* runs the search and dials the URL, so this crate opens no
    /// socket for it and [`Act::Net`](crate::Act::Net) never sees one. The domain
    /// lists on the declaration are handed to the vendor's own filter: a boundary
    /// declared here and enforced there, exactly as `docs/CONTRACT.md` describes
    /// for a stdio MCP server. Use
    /// [`WebAccess::from_policy`](crate::WebAccess::from_policy) to derive them
    /// from the run's policy rather than writing the same hosts twice.
    ///
    /// ```
    /// use io_harness::{TaskContract, Verification, WebAccess};
    ///
    /// let contract = TaskContract::workspace(
    ///     "update the README's install line to the current release",
    ///     "/path/to/repo",
    /// )
    /// .with_verification(Verification::WorkspaceFileContains {
    ///     file: "README.md".into(),
    ///     needle: "install".into(),
    /// })
    /// .with_web(WebAccess::search().max_uses(3).allow("crates.io"));
    ///
    /// assert!(contract.web.is_some());
    /// ```
    #[must_use]
    pub fn with_web(mut self, web: crate::web::WebAccess) -> Self {
        self.web = Some(web);
        self
    }

    /// Hand the agent images to look at, alongside the goal.
    ///
    /// A new named method rather than a parameter on any of the sixteen entry
    /// points: every one of them takes a `TaskContract`, so attaching here works
    /// with all of them and changes no existing signature.
    ///
    /// Construct each [`crate::Media`] with [`crate::Media::image`], which
    /// refuses a media type no provider documents and refuses an image over the
    /// per-image size bound. The total carried by one request is bounded too —
    /// see [`crate::provider::MAX_REQUEST_IMAGE_BYTES`].
    #[cfg(feature = "media")]
    #[must_use]
    pub fn with_images<I>(mut self, images: I) -> Self
    where
        I: IntoIterator<Item = crate::provider::Media>,
    {
        self.images.extend(images);
        self
    }

    /// Attribute the agent's commits to this name and address.
    ///
    /// Replaces the default agent identity. Neither may be empty or contain a
    /// control character — both reach the commit object and the reflog.
    #[must_use]
    pub fn with_commit_identity(
        mut self,
        name: impl Into<String>,
        email: impl Into<String>,
    ) -> Self {
        self.commit_identity = crate::tools::git::Identity {
            name: name.into(),
            email: email.into(),
        };
        self
    }

    /// Register in-process tools for the run and offer them to the model.
    ///
    /// Registration makes a tool available; it does not authorize it. Each call
    /// is an [`Act::Exec`](crate::Act::Exec) check on the tool's name, and a
    /// registered tool runs with the embedding program's own privileges — see
    /// [`crate::tools::Tool`] for the full bound.
    ///
    /// A name that shadows a built-in, uses the `mcp__` prefix, or duplicates
    /// another registered tool fails the run with [`Error::Config`](crate::Error::Config)
    /// before the first completion.
    ///
    /// Workspace mode only, like [`TaskContract::with_mcp`]: single-file mode has
    /// one tool and no tool layer to extend.
    pub fn with_tools(mut self, tools: crate::tools::Toolbox) -> Self {
        self.tools = tools;
        self
    }

    /// Offer the agent the skills in `dir` — see [`crate::skills`] for the
    /// layout.
    ///
    /// The directory is read at run start, not here, so a path that does not
    /// exist, is not a directory, or holds more than
    /// [`MAX_SKILLS`](crate::skills::MAX_SKILLS) skills fails the run with
    /// [`Error::Config`](crate::Error::Config) naming it, before the first
    /// completion.
    ///
    /// A skill is instructions the model may choose to read. Offering one grants
    /// nothing: the read goes through the policy when it happens, and anything
    /// the model then does is checked as it always is.
    pub fn with_skills(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills = Some(dir.into());
        self
    }

    /// Say what the system prompt says (0.45.0).
    ///
    /// [`SystemPrompt::Builtin`] is the default and is every release before this
    /// one. Neither [`SystemPrompt::Append`] nor [`SystemPrompt::Replace`] can
    /// reach the crate's ending sentence, the repository's instructions section or
    /// the boundary section — those are composed after the caller's text, in that
    /// order, and the ending is last.
    ///
    /// ```
    /// use io_harness::{SystemPrompt, TaskContract};
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_system_prompt(SystemPrompt::Replace("You are Acme's bot.".into()));
    /// assert_eq!(contract.prompt, SystemPrompt::Replace("You are Acme's bot.".into()));
    /// ```
    pub fn with_system_prompt(mut self, prompt: SystemPrompt) -> Self {
        self.prompt = prompt;
        self
    }

    /// Carry one piece of repository guidance in the system block (0.45.0).
    ///
    /// This is what [`Config::apply_to`](crate::Config::apply_to) calls with what
    /// `[instructions]` discovered, and it is public for a caller that has its own
    /// source of the same kind of text. It is **not** a constraint: a constraint is
    /// a rule the goal is checked against ([`with_constraint`](TaskContract::with_constraint)),
    /// and this is guidance the agent reads, carried in a delimited section that
    /// cannot displace the crate's own rules.
    ///
    /// ```
    /// use io_harness::TaskContract;
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_instruction("Project instructions from `AGENTS.md`:\nprefer small diffs");
    /// assert_eq!(contract.instructions.len(), 1);
    /// assert!(contract.constraints.is_empty());
    /// ```
    pub fn with_instruction(mut self, text: impl Into<String>) -> Self {
        self.instructions.push(text.into());
        self
    }

    /// Carry these capability bundles through the run (0.35.0).
    ///
    /// Sets the field and nothing else. [`Plugins::apply_to`](crate::Plugins::apply_to)
    /// is the call that also folds each bundle's agents and MCP servers in, and
    /// is what a caller normally reaches for; this exists for a caller assembling
    /// a contract in a different order.
    ///
    /// ```
    /// use io_harness::{Plugins, TaskContract};
    ///
    /// let contract = TaskContract::workspace("tidy the crate", "/repo")
    ///     .with_plugins(Plugins::none());
    /// assert!(contract.plugins.is_empty());
    /// ```
    pub fn with_plugins(mut self, plugins: crate::plugin::Plugins) -> Self {
        self.plugins = plugins;
        self
    }

    /// Register the named agent definitions a spawn may ask for (0.21.0).
    ///
    /// Offering a roster grants nothing, for the same reason offering a skill does
    /// not: a definition's `deny_write`/`deny_net` are composed through
    /// [`Policy::contain`](crate::Policy::contain), so it has no way to express an
    /// allow. A definition silent about a path its parent denies still yields a
    /// child that is refused it.
    ///
    /// Only the tree entry points ([`run_tree`](crate::run_tree) and friends) offer
    /// the spawn tool at all, so a roster on a contract handed to
    /// [`run_with`](crate::run_with) is inert rather than a hidden capability.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents, TaskContract};
    ///
    /// let contract = TaskContract::workspace("find the bug, then fix it", "/repo")
    /// .with_agents(
    ///     Agents::new()
    ///         .with(AgentDef::new("searcher").with_model("cheap-model").deny_write())
    ///         .with(AgentDef::new("author").with_model("strong-model")),
    /// );
    ///
    /// assert_eq!(contract.agents.len(), 2);
    /// assert!(contract.agents.get("searcher").unwrap().deny_write);
    /// ```
    pub fn with_agents(mut self, agents: crate::agent::Agents) -> Self {
        self.agents = agents;
        self
    }

    /// Require the agent to propose a plan and have it reviewed before it acts
    /// (0.31.0).
    ///
    /// The run opens in a planning phase: the agent may read the workspace and may
    /// change nothing in it — every [`Act::Write`](crate::Act::Write) and
    /// [`Act::Exec`](crate::Act::Exec) is denied under a `plan-gate` policy layer,
    /// which covers registered and MCP tools too because those are exec checks —
    /// and the only way out is a [`Plan`](crate::Plan) the gate approves.
    ///
    /// Without a gate the run works immediately, exactly as every release before
    /// 0.31.0 did. This is not [`TODO_WRITE_TOOL`](crate::TODO_WRITE_TOOL): that is
    /// a plan the operator *watches* while it executes, this is one that executes
    /// nothing until an answer arrives.
    ///
    /// When nothing in this process answers, the plan is persisted and the run stops
    /// with [`RunOutcome::AwaitingPlan`](crate::RunOutcome::AwaitingPlan), so the
    /// process may exit and
    /// [`resume_with_plan_decision`](crate::resume_with_plan_decision) continues it
    /// later under the same run id.
    ///
    /// ```
    /// use io_harness::{PlanGateNone, TaskContract};
    /// use std::sync::Arc;
    ///
    /// // Unattended and honest: every plan pauses for a human, and nothing under
    /// // /repo is touched while it waits.
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_plan_gate(Arc::new(PlanGateNone));
    ///
    /// assert!(contract.plan_gate.is_some());
    /// ```
    pub fn with_plan_gate(mut self, gate: std::sync::Arc<dyn crate::approve::PlanGate>) -> Self {
        self.plan_gate = Some(gate);
        self
    }

    /// Register who answers a
    /// [`Verification::Review`](crate::Verification::Review) criterion (0.34.0).
    ///
    /// ```
    /// use io_harness::{Review, ReviewRequest, Reviewer, Reviewing, TaskContract, Verification};
    /// use std::sync::Arc;
    ///
    /// #[derive(Debug)]
    /// struct Strict;
    /// impl Reviewer for Strict {
    ///     fn review<'a>(&'a self, _r: ReviewRequest) -> Reviewing<'a> {
    ///         Box::pin(async { Ok(Review::failed(["the goal asked for two files"])) })
    ///     }
    ///     fn model(&self) -> Option<&str> { None }
    /// }
    ///
    /// let contract = TaskContract::workspace("split the module", "/repo")
    ///     .with_verification(Verification::Review {
    ///         rubric: "the module is two files and both compile".into(),
    ///         allow_self_review: false,
    ///     })
    ///     .with_reviewer(Arc::new(Strict));
    ///
    /// assert!(contract.reviewer.is_some());
    /// ```
    pub fn with_reviewer(mut self, reviewer: std::sync::Arc<dyn crate::verify::Reviewer>) -> Self {
        self.reviewer = Some(reviewer);
        self
    }

    /// Honour the `before_tool` hooks a configuration declared (0.42.0).
    ///
    /// ```
    /// use std::sync::Arc;
    /// use io_harness::{Config, TaskContract};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// // A check an operator writes instead of compiling: nothing is published
    /// // from this repository by an agent, whatever it decides.
    /// std::fs::write(
    ///     dir.path().join("io.local.toml"),
    ///     "[[hook]]\nat = \"before_tool\"\ntools = [\"exec\"]\nrun = [\"./no-publish\"]\n",
    /// )?;
    ///
    /// let hooks = Config::discover(dir.path())?.hooks();
    /// let contract = TaskContract::workspace("cut the release", dir.path())
    ///     .with_tool_hooks(Arc::new(hooks));
    /// assert!(contract.tool_hooks.is_some());
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn with_tool_hooks(mut self, hooks: std::sync::Arc<crate::hooks::Hooks>) -> Self {
        self.tool_hooks = Some(hooks);
        self
    }

    /// Let the run change which model it asks, while it is running (0.34.0).
    ///
    /// ```
    /// use io_harness::{Routing, TaskContract};
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_routing(Routing::new().escalate_after(2, "big-model").require_primary());
    ///
    /// assert!(contract.routing.is_some());
    /// ```
    pub fn with_routing(mut self, routing: Routing) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Ask for a reasoning tier on the root agent's completions (0.31.0).
    ///
    /// A request rather than a fact, in the sense a model slug is: each vendor is
    /// asked in its own dialect and one that cannot be asked ignores it.
    /// [`Usage::reasoning_tokens`](crate::Usage::reasoning_tokens) is what says
    /// whether anything was thought. A spawned child takes the tier on its
    /// [`AgentDef`](crate::AgentDef) instead.
    ///
    /// ```
    /// use io_harness::{provider::Effort, TaskContract};
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_effort(Effort::High);
    ///
    /// assert_eq!(contract.effort, Some(Effort::High));
    /// ```
    pub fn with_effort(mut self, effort: crate::provider::Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Register who answers the agent's questions about intent (0.21.0).
    ///
    /// Without one, a question is persisted and the run pauses with
    /// [`RunOutcome::AwaitingAnswer`](crate::RunOutcome::AwaitingAnswer) for a human
    /// to answer through [`resume_with_answer`](crate::resume_with_answer).
    ///
    /// An answer is text the model reads. It authorizes nothing: every tool call that
    /// follows one is checked against the same [`Policy`](crate::Policy) by the same
    /// code, which is the rule steering has followed since 0.20.0.
    ///
    /// ```
    /// use io_harness::{FixedResponder, TaskContract};
    /// use std::sync::Arc;
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo")
    ///     .with_responder(Arc::new(FixedResponder::new("use io.local.toml")));
    ///
    /// assert!(contract.responder.is_some());
    /// ```
    pub fn with_responder(
        mut self,
        responder: std::sync::Arc<dyn crate::approve::Responder>,
    ) -> Self {
        self.responder = Some(responder);
        self
    }

    /// Discover the configured skills, and every loaded bundle's (0.35.0).
    ///
    /// Called at run start by every entry point, alongside
    /// [`Toolbox::validate`](crate::tools::Toolbox::validate) — once, before the
    /// first completion, never per step. A bundle's names are namespaced as they
    /// are read, so a contributed skill cannot occupy a name the contract's own
    /// directory uses and two bundles cannot collide with each other.
    pub(crate) fn discover_skills(&self) -> crate::Result<crate::skills::Skills> {
        let mut skills = match &self.skills {
            Some(dir) => crate::skills::Skills::discover(dir)?,
            None => crate::skills::Skills::none(),
        };
        for (id, dir) in self.plugins.skill_dirs() {
            skills = skills.merged(crate::skills::Skills::discover(dir)?.namespaced(&id))?;
        }
        Ok(skills)
    }

    /// Override the step budget.
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Set the time budget.
    pub fn with_time_budget(mut self, max_duration: Duration) -> Self {
        self.max_duration = Some(max_duration);
        self
    }

    /// Set the cost budget, in total tokens across all completions.
    pub fn with_token_budget(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set how long a command the agent runs with `exec` may take.
    ///
    /// A new named builder rather than a parameter, like every other capability
    /// this crate has added: every entry point takes a `TaskContract`, so setting
    /// it here works with all of them and changes no existing signature.
    ///
    /// ```
    /// use io_harness::{TaskContract, DEFAULT_EXEC_TIMEOUT};
    /// use std::time::Duration;
    ///
    /// // A repository whose cold build is slower than the default ceiling. Raise
    /// // it rather than watching honest work be killed as if it had hung.
    /// let patient = TaskContract::workspace("build and test it", "/monorepo")
    ///     .with_exec_timeout(Duration::from_secs(2400));
    ///
    /// // And the other direction: an unattended fleet job that would rather give
    /// // up on a command than sit behind it.
    /// let impatient = TaskContract::workspace("lint it", "/repo")
    ///     .with_exec_timeout(Duration::from_secs(60));
    ///
    /// assert!(patient.exec_timeout > DEFAULT_EXEC_TIMEOUT);
    /// assert!(impatient.exec_timeout < DEFAULT_EXEC_TIMEOUT);
    /// ```
    #[must_use]
    pub fn with_exec_timeout(mut self, exec_timeout: Duration) -> Self {
        self.exec_timeout = exec_timeout;
        self
    }

    /// Set how many read-only tool calls from one completion may run at once
    /// (0.41.0).
    ///
    /// `0` is clamped to `1` rather than rejected: a caller who computed this
    /// from a configuration file should get the safe reading — serial, which is
    /// what every release before 0.41.0 did — and not a failed run.
    ///
    /// ```
    /// use io_harness::TaskContract;
    ///
    /// // A model that reads eight files before it edits one gets them in the
    /// // time of the slowest read rather than the sum of all of them.
    /// let wide = TaskContract::workspace("port the parser", "/repo")
    ///     .with_max_parallel_reads(16);
    /// assert_eq!(wide.max_parallel_reads, 16);
    ///
    /// // And the way back to 0.40.0's execution shape, for anyone ruling the
    /// // concurrency out while debugging something else.
    /// let serial = TaskContract::workspace("port the parser", "/repo")
    ///     .with_max_parallel_reads(1);
    /// assert_eq!(serial.max_parallel_reads, 1);
    ///
    /// // Zero means serial too, not an error.
    /// let zero = TaskContract::workspace("port the parser", "/repo")
    ///     .with_max_parallel_reads(0);
    /// assert_eq!(zero.max_parallel_reads, 1);
    ///
    /// // The default, which no caller has to say.
    /// assert_eq!(TaskContract::workspace("port the parser", "/repo").max_parallel_reads, 10);
    /// ```
    #[must_use]
    pub fn with_max_parallel_reads(mut self, max_parallel_reads: usize) -> Self {
        self.max_parallel_reads = max_parallel_reads.max(1);
        self
    }

    /// Run the project's own commands inside the [`Sandbox`](crate::Sandbox).
    ///
    /// Opt-in, and the default stays what it has been since 0.17.0: `exec` and
    /// `shell` run in the workspace root at the embedding program's privileges,
    /// with the [`Policy`](crate::Policy) deciding what may *start* and nothing
    /// bounding what a started process then does. Ask for this when the commands
    /// this run invokes do not need what containment takes away.
    ///
    /// A contained command keeps the **workspace root** as its working directory,
    /// so nothing is copied out and nothing is discarded and an incremental build
    /// survives from one command to the next. What it loses is everything outside
    /// that root: on macOS and Linux its writes are confined to the workspace, and
    /// its egress is denied unless this run's policy would permit
    /// [`Act::Net`](crate::Act::Net).
    ///
    /// ```
    /// use io_harness::sandbox::{SandboxConfig, SandboxLimits};
    /// use io_harness::{ExecMode, TaskContract};
    ///
    /// // The default caps, egress denied, and the strongest backend this host
    /// // offers — the same configuration the verification gate uses. Since
    /// // 0.46.0 this adds the *ceilings*; the boundary was already there.
    /// let contained = TaskContract::workspace("run the test suite", "/repo")
    ///     .with_contained_exec(SandboxConfig::new());
    ///
    /// assert_eq!(contained.exec_sandbox.mode, ExecMode::WorkspaceWrite);
    /// assert_eq!(contained.exec_sandbox.limits.max_wall_secs, Some(120));
    /// assert!(!contained.exec_sandbox.allow_network);
    ///
    /// // A run that never asks is confined just the same, and pays no ceiling.
    /// let default = TaskContract::workspace("install and build", "/repo");
    /// assert_eq!(default.exec_sandbox.mode, ExecMode::WorkspaceWrite);
    /// assert_eq!(default.exec_sandbox.limits, SandboxLimits::none());
    ///
    /// // Caps are the config's, so a run that wants a tighter memory ceiling than
    /// // the default says so here rather than anywhere else.
    /// let tight = TaskContract::workspace("run the fuzzer", "/repo")
    ///     .with_contained_exec(SandboxConfig {
    ///         limits: SandboxLimits {
    ///             max_memory_bytes: Some(256 * 1024 * 1024),
    ///             ..SandboxLimits::default()
    ///         },
    ///         ..SandboxConfig::new()
    ///     });
    /// assert_eq!(
    ///     tight.exec_sandbox.limits.max_memory_bytes,
    ///     Some(256 * 1024 * 1024)
    /// );
    /// ```
    #[must_use]
    pub fn with_contained_exec(mut self, sandbox: crate::sandbox::SandboxConfig) -> Self {
        self.exec_sandbox = sandbox;
        self
    }

    /// Run this run's commands at the embedding program's own privileges.
    ///
    /// The escape hatch from 0.46.0's default, and the reason it is a named method
    /// rather than a field left unset: a run that may write anywhere the host user
    /// can write is the widest grant this crate makes, and it should be legible in
    /// a diff and findable with `grep -r with_full_access`. Every release up to
    /// 0.45.0 did this by default.
    ///
    /// Reach for it when the commands this run invokes genuinely need the machine —
    /// a toolchain installer, a system package manager, a build that writes to a
    /// path the caller configured outside the workspace. Everything else is better
    /// served by the default, whose writable roots already include the detected
    /// toolchain's own caches.
    ///
    /// ```
    /// use io_harness::{ExecMode, TaskContract};
    ///
    /// let wide = TaskContract::workspace("upgrade the toolchain", "/repo")
    ///     .with_full_access();
    ///
    /// assert_eq!(wide.exec_sandbox.mode, ExecMode::FullAccess);
    /// assert!(!wide.exec_sandbox.mode.is_contained());
    /// ```
    ///
    /// It leaves the caps alone. A `FullAccess` command reaches no backend at all,
    /// so nothing in [`SandboxLimits`](crate::sandbox::SandboxLimits) applies to it
    /// — the contract's `exec_timeout` is what still bounds it, exactly as it did
    /// before 0.46.0.
    #[must_use]
    pub fn with_full_access(mut self) -> Self {
        self.exec_sandbox.mode = crate::sandbox::ExecMode::FullAccess;
        self
    }

    /// Set the [`ExecMode`](crate::ExecMode) without touching the caps.
    ///
    /// [`TaskContract::with_full_access`] is the one worth spelling out, so it has
    /// its own method; this is how a run asks for the third mode.
    ///
    /// ```
    /// use io_harness::{ExecMode, TaskContract};
    ///
    /// // A run that reads a codebase and reports on it has nothing to write.
    /// let audit = TaskContract::workspace("summarise the architecture", "/repo")
    ///     .with_exec_mode(ExecMode::ReadOnly);
    ///
    /// assert_eq!(audit.exec_sandbox.mode, ExecMode::ReadOnly);
    /// ```
    #[must_use]
    pub fn with_exec_mode(mut self, mode: crate::sandbox::ExecMode) -> Self {
        self.exec_sandbox.mode = mode;
        self
    }

    /// Set how much of each request the observation log may occupy.
    ///
    /// Sits beside [`TaskContract::with_token_budget`] because they are the two
    /// halves of one thing: that bounds what the run may spend, this bounds what
    /// any one request carries of what it has already observed.
    pub fn with_context_budget(mut self, context: ContextBudget) -> Self {
        self.context = context;
        self
    }

    /// Set when the run's history is folded into a written summary (0.43.0).
    ///
    /// The companion to [`TaskContract::with_context_budget`]: that decides how
    /// much of the history one request may carry, this decides what happens to
    /// the rest of it. Without a fold the remainder becomes one-line stubs, which
    /// say a read happened and not what it taught the run.
    ///
    /// ```
    /// use io_harness::{Compaction, TaskContract};
    ///
    /// // Fold sooner and keep less whole: a small window, or large observations.
    /// let tight = TaskContract::workspace("port the parser", "/repo")
    ///     .with_compaction(Compaction { at_share: 0.6, keep_recent: 4 });
    /// assert!(tight.compaction.enabled());
    ///
    /// // Or never, which is what 0.42.0 did.
    /// let never = TaskContract::workspace("port the parser", "/repo")
    ///     .with_compaction(Compaction { at_share: 1.0, ..Compaction::default() });
    /// assert!(!never.compaction.enabled());
    /// ```
    #[must_use]
    pub fn with_compaction(mut self, compaction: Compaction) -> Self {
        self.compaction = compaction;
        self
    }

    /// Set how long to wait between provider attempts.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Set when a run decides its agent has stalled, and how often to tell it.
    ///
    /// `StallPolicy { window: 0, .. }` disables detection, restoring 0.10.0
    /// behaviour exactly.
    ///
    /// Applies to workspace and sub-agent runs. A single-file run
    /// ([`TaskContract::new`]) ignores it: it has one tool and one file, so
    /// "repeated a call without changing anything" describes its only move.
    pub fn with_stall_policy(mut self, stall: StallPolicy) -> Self {
        self.stall = stall;
        self
    }

    /// Override the retry limit for failing provider/tool steps.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Add a constraint the agent must respect.
    pub fn with_constraint(mut self, constraint: impl Into<String>) -> Self {
        self.constraints.push(constraint.into());
        self
    }
}

/// Rules that change which model a run asks, while it is running (0.34.0).
///
/// Roles have been able to name a model since 0.11.0 and
/// [`Fallback`](crate::provider::Fallback) has chained two providers since
/// 0.9.0. What has never existed is a rule that changes either one *during* a
/// run: a role's model is fixed when the roster is written, and a fallback fires
/// on a failure rather than on a judgement about the work.
///
/// Every rule here sets [`CompletionRequest::model`](crate::CompletionRequest) —
/// the per-request knob that has existed since 0.11.0 — so no provider changes
/// and nothing new is constructed. A rule that names a model the provider does
/// not have fails the way any wrong model slug fails: at the vendor, loudly.
///
/// ```
/// use io_harness::Routing;
///
/// // Start on the cheap model, move up if the gate keeps saying no, and refuse
/// // to start at all if the primary provider is not answering.
/// let routing = Routing::new()
///     .escalate_after(3, "big-model")
///     .downshift_under(2_048, "small-model")
///     .require_primary();
///
/// assert_eq!(routing.escalate_after, Some((3, "big-model".into())));
/// assert!(routing.require_primary);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Routing {
    /// After this many *consecutive* failed gate attempts, ask this model
    /// instead.
    ///
    /// Consecutive rather than cumulative: a run that fails, recovers and fails
    /// again much later is not a run that needs a bigger model, it is a run doing
    /// hard work. The escalation happens once and does not come back down —
    /// oscillating between two models mid-run is a behaviour nobody asked for.
    pub escalate_after: Option<(u32, String)>,
    /// While the run has written fewer than this many bytes, ask this model
    /// instead.
    ///
    /// The cheap direction, and deliberately measured on *what was written*
    /// rather than on what was planned: bytes on disk are a fact the run already
    /// has, and an estimate of a change's size before making it is the model's
    /// own guess about its own work.
    pub downshift_under: Option<(u64, String)>,
    /// Ask the provider whether it is reachable before the first step, and refuse
    /// to start if it says no.
    ///
    /// The rule an unattended job needs. Without it, a primary that is down means
    /// a night's work quietly running on whatever
    /// [`Fallback`](crate::provider::Fallback) had underneath — which is correct
    /// for one request and wrong for eight hours of them. See
    /// [`Provider::reachable`](crate::Provider::reachable), which is defaulted to
    /// `Ok(true)`: a provider that says nothing about reachability makes this a
    /// no-op rather than a failure.
    pub require_primary: bool,
}

impl Routing {
    /// No rules — the same behaviour as no routing at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Escalate to `model` after `failures` consecutive failed gate attempts.
    #[must_use]
    pub fn escalate_after(mut self, failures: u32, model: impl Into<String>) -> Self {
        self.escalate_after = Some((failures, model.into()));
        self
    }

    /// Use `model` while the run has written fewer than `bytes` bytes.
    #[must_use]
    pub fn downshift_under(mut self, bytes: u64, model: impl Into<String>) -> Self {
        self.downshift_under = Some((bytes, model.into()));
        self
    }

    /// Refuse to start when the provider reports it is not reachable.
    #[must_use]
    pub fn require_primary(mut self) -> Self {
        self.require_primary = true;
        self
    }

    /// Which model this run should ask now, given what has happened so far, or
    /// `None` to leave the request's model as it is.
    ///
    /// Escalation wins over downshifting: a run whose gate keeps refusing is not
    /// a run to save money on, however few bytes it has written.
    #[must_use]
    pub fn model_for(&self, consecutive_gate_failures: u32, bytes_written: u64) -> Option<&str> {
        if let Some((after, model)) = &self.escalate_after {
            if consecutive_gate_failures >= *after {
                return Some(model);
            }
        }
        if let Some((under, model)) = &self.downshift_under {
            if bytes_written < *under {
                return Some(model);
            }
        }
        None
    }
}
