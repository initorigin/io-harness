//! The tool layer — narrow, typed actions the agent may invoke.
//!
//! 0.1/0.2 shipped one tool, [`fs::FsTool`] (`write_file`), scoped to a single
//! file. 0.3 adds a [`workspace::Workspace`] that scopes four tools to a root
//! directory — `grep`, `find`, `read_file`, and a path-taking `write_file` — so
//! the agent can search a repository and edit several files in one run.
//!
//! 0.9 opens the layer to the embedding program: [`custom::Tool`] is the public
//! trait a caller implements to add an action of their own in-process, collected
//! in a [`custom::Toolbox`] and offered to the model beside the built-ins under
//! the same policy and the same trace. Out-of-process extension stays where
//! 0.8.0 put it — the MCP client in [`crate::mcp`].

pub mod custom;
pub mod fs;
pub mod workspace;

pub use custom::{Tool, ToolFuture, Toolbox};
pub use fs::FsTool;
pub use workspace::{Match, Workspace};

/// The name the model uses to write a file (single-file 0.1/0.2 form: content only).
pub const WRITE_FILE_TOOL: &str = "write_file";
/// The name the model uses to search file contents by regex/substring.
pub const GREP_TOOL: &str = "grep";
/// The name the model uses to list files by name/path glob.
pub const FIND_TOOL: &str = "find";
/// The name the model uses to read a file into context.
pub const READ_FILE_TOOL: &str = "read_file";
/// Keep a tool result within `cap` chars, reporting whether it was cut.
///
/// A tool that returns a megabyte would otherwise spend the rest of the run's
/// token budget on a single observation, every turn. The bound is the same for
/// every non-built-in tool — an MCP server's and a caller's registered [`Tool`]
/// alike — because the model cannot tell them apart and neither should the
/// ceiling.
///
/// 0.10.0 takes the cap as an argument instead of holding its own constant: it is
/// derived per turn from the run's [`ContextBudget`](crate::context::ContextBudget)
/// by [`entry_cap_chars`](crate::context::entry_cap_chars), so the per-result
/// ceiling and the whole prompt's ceiling are one unit from one source and cannot
/// drift apart.
///
/// Truncation is visible in the returned text rather than silent: a model that
/// cannot see it was cut off will treat a partial answer as the whole one.
pub(crate) fn cap_result(s: String, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n[truncated at {cap} chars]", &s[..end]), true)
}

/// The name the model uses to load one skill's body into its observations.
///
/// Offered only when the contract configures skills — a tool that could do
/// nothing but fail would cost a slot in every request of every other run.
pub const READ_SKILL_TOOL: &str = "read_skill";
