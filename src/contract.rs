//! The task contract: what the agent is asked to do and how success is judged.

use std::path::PathBuf;

use crate::verify::Verification;

/// A single unit of work handed to the harness.
///
/// v0.1 scope: the agent edits exactly one file to meet [`Verification`].
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
    /// Hard cap on loop iterations. The run stops when it is reached.
    pub max_steps: u32,
}

impl TaskContract {
    /// Minimal contract: goal, target file, and a success criterion.
    /// Defaults to 8 steps and no extra constraints.
    pub fn new(goal: impl Into<String>, file: impl Into<PathBuf>, verify: Verification) -> Self {
        Self {
            goal: goal.into(),
            file: file.into(),
            constraints: Vec::new(),
            verify,
            max_steps: 8,
        }
    }

    /// Override the step cap.
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Add a constraint the agent must respect.
    pub fn with_constraint(mut self, constraint: impl Into<String>) -> Self {
        self.constraints.push(constraint.into());
        self
    }
}
