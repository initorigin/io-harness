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
    /// Directory of skill files to offer the agent, or `None` (the default) for
    /// no skills.
    ///
    /// The *path* is held rather than the discovered set, because reading a
    /// directory is fallible and a builder method is not: discovery happens at
    /// run start, so a directory that does not exist fails the run with
    /// [`Error::Config`](crate::Error::Config) naming the path — the same point
    /// and the same way [`TaskContract::tools`] is arbitrated.
    pub skills: Option<PathBuf>,
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
            #[cfg(feature = "media")]
            images: Vec::new(),
            tools: crate::tools::Toolbox::new(),
            context: ContextBudget::default(),
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            skills: None,
        }
    }

    /// A workspace task: the agent may grep, find, read, and write several files
    /// under `root`. `verify` should be a multi-file variant
    /// ([`Verification::EachCompilesRust`] / [`Verification::WorkspaceTestPasses`]).
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
            #[cfg(feature = "media")]
            images: Vec::new(),
            tools: crate::tools::Toolbox::new(),
            context: ContextBudget::default(),
            retry: RetryPolicy::default(),
            stall: StallPolicy::default(),
            skills: None,
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
