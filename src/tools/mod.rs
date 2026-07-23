//! The tool layer — narrow, typed actions the agent may invoke.
//!
//! 0.1/0.2 shipped one tool, [`fs::FsTool`] (`write_file`), scoped to a single
//! file. 0.3 adds a [`workspace::Workspace`] that scopes four tools to a root
//! directory — `grep`, `find`, `read_file`, and a path-taking `write_file` — so
//! the agent can search a repository and edit several files in one run.

pub mod fs;
pub mod workspace;

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
