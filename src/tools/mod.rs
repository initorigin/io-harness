//! The tool layer — narrow, typed actions the agent may invoke.
//!
//! v0.1 ships one tool: [`fs::FsTool`]. Its model-facing name is `write_file`.

pub mod fs;

pub use fs::FsTool;

/// The name the model uses to call the filesystem write tool.
pub const WRITE_FILE_TOOL: &str = "write_file";
