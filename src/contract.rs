//! The task contract: what the agent is asked to do and how success is judged.

use std::path::PathBuf;
use std::time::Duration;

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
    /// The one file the agent may read and write.
    pub file: PathBuf,
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
}

impl TaskContract {
    /// Minimal contract: goal, target file, and a success criterion.
    /// Defaults to 8 steps, no time/token budget, 2 retries, no constraints.
    pub fn new(goal: impl Into<String>, file: impl Into<PathBuf>, verify: Verification) -> Self {
        Self {
            goal: goal.into(),
            file: file.into(),
            constraints: Vec::new(),
            verify,
            max_steps: 8,
            max_duration: None,
            max_tokens: None,
            max_retries: 2,
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
