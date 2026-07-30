//! The task contract: what the agent is asked to do and how success is judged.

use std::path::PathBuf;
use std::time::Duration;

use crate::context::ContextBudget;
use crate::resilience::{RetryPolicy, StallPolicy};
use crate::verify::Verification;

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
///     // The criterion is checked by running the project's own suite, so a
///     // plausible-looking stub cannot satisfy it. This is the half of the
///     // contract that decides whether the run *succeeded*, as opposed to merely
///     // stopping.
///     Verification::Command { argv: vec!["cargo".into(), "test".into()], expect_exit: 0 },
/// )
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
#[derive(Debug, Clone)]
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
    /// The checkable success criterion. The run succeeds when this passes.
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
    /// Directory of skill files to offer the agent, or `None` (the default) for
    /// no skills.
    ///
    /// The *path* is held rather than the discovered set, because reading a
    /// directory is fallible and a builder method is not: discovery happens at
    /// run start, so a directory that does not exist fails the run with
    /// [`Error::Config`](crate::Error::Config) naming the path — the same point
    /// and the same way [`TaskContract::tools`] is arbitrated.
    pub skills: Option<PathBuf>,
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
            verify,
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
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            exec_timeout: crate::tools::DEFAULT_EXEC_TIMEOUT,
            skills: None,
            agents: crate::agent::Agents::new(),
            responder: None,
            web: None,
        }
    }

    /// A workspace task: the agent may grep, find, read, and write several files
    /// under `root`. `verify` should be a multi-file variant
    /// ([`Verification::EachCompilesRust`], or a [`Verification::Command`] that runs
    /// the project's own suite).
    /// Defaults match [`TaskContract::new`] (12 steps here, since repo tasks take
    /// more turns), no time/token budget, 2 retries.
    pub fn workspace(
        goal: impl Into<String>,
        root: impl Into<PathBuf>,
        verify: Verification,
    ) -> Self {
        let root = root.into();
        Self {
            goal: goal.into(),
            file: root.clone(),
            root: Some(root),
            constraints: Vec::new(),
            verify,
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
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            exec_timeout: crate::tools::DEFAULT_EXEC_TIMEOUT,
            skills: None,
            agents: crate::agent::Agents::new(),
            responder: None,
            web: None,
        }
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
    ///     Verification::WorkspaceFileContains { file: "README.md".into(), needle: "install".into() },
    /// )
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
    /// use io_harness::{AgentDef, Agents, TaskContract, Verification};
    ///
    /// let contract = TaskContract::workspace(
    ///     "find the bug, then fix it",
    ///     "/repo",
    ///     Verification::None,
    /// )
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
    /// use io_harness::{FixedResponder, TaskContract, Verification};
    /// use std::sync::Arc;
    ///
    /// let contract = TaskContract::workspace("port the parser", "/repo", Verification::None)
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

    /// Discover the configured skills. Called at run start by every entry point,
    /// alongside [`Toolbox::validate`](crate::tools::Toolbox::validate).
    pub(crate) fn discover_skills(&self) -> crate::Result<crate::skills::Skills> {
        match &self.skills {
            Some(dir) => crate::skills::Skills::discover(dir),
            None => Ok(crate::skills::Skills::none()),
        }
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
    /// use io_harness::{TaskContract, Verification, DEFAULT_EXEC_TIMEOUT};
    /// use std::time::Duration;
    ///
    /// // A repository whose cold build is slower than the default ceiling. Raise
    /// // it rather than watching honest work be killed as if it had hung.
    /// let patient = TaskContract::workspace("build and test it", "/monorepo", Verification::None)
    ///     .with_exec_timeout(Duration::from_secs(2400));
    ///
    /// // And the other direction: an unattended fleet job that would rather give
    /// // up on a command than sit behind it.
    /// let impatient = TaskContract::workspace("lint it", "/repo", Verification::None)
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

    /// Set how much of each request the observation log may occupy.
    ///
    /// Sits beside [`TaskContract::with_token_budget`] because they are the two
    /// halves of one thing: that bounds what the run may spend, this bounds what
    /// any one request carries of what it has already observed.
    pub fn with_context_budget(mut self, context: ContextBudget) -> Self {
        self.context = context;
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
