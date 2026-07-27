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
pub mod documents;
pub mod fs;
pub mod git;
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
/// The names the model uses for git work (0.15.0).
///
/// Built-ins for the reason the document tools are, and one sharper: the exec
/// policy enforces a program *name* and records argv without checking it
/// (src/verify.rs:248), so `Act::Exec("git")` cannot tell `git log` from
/// `git push --force`. Each of these constructs its own complete argv instead —
/// the model supplies paths and a message and never an argument — so the
/// networked and destructive surface is unreachable by construction rather than
/// excluded by a rule someone has to maintain.
pub const GIT_LOG_TOOL: &str = "git_log";
/// See [`GIT_LOG_TOOL`].
pub const GIT_STATUS_TOOL: &str = "git_status";
/// See [`GIT_LOG_TOOL`].
pub const GIT_DIFF_TOOL: &str = "git_diff";
/// See [`GIT_LOG_TOOL`].
pub const GIT_ADD_TOOL: &str = "git_add";
/// See [`GIT_LOG_TOOL`].
pub const GIT_COMMIT_TOOL: &str = "git_commit";

/// The name the model uses to look at an image in the workspace (0.15.0,
/// `media` feature).
///
/// A built-in rather than a registered [`Tool`], for the reason the document
/// tools are: this one decides which of the user's files is sent to a third
/// party, so it is gated per call on the real path the model names rather than
/// authorised once by name.
#[cfg(feature = "media")]
pub const VIEW_IMAGE_TOOL: &str = "view_image";
/// The names the model uses for spreadsheet work (0.14.0, `xlsx` feature).
///
/// These are built-ins rather than registered [`Tool`]s on purpose. A registered
/// tool is authorised once, by an exec check on its name, and the crate is
/// explicit that the policy governs whether a tool is *called* and not what it
/// does once running. A spreadsheet tool's whole job is reading and writing files
/// in the user's workspace, so it is dispatched here instead, gated per call on
/// the real path it names — `deny_write("secrets/*")` refuses
/// `xlsx_set_cell("secrets/book.xlsx", ...)` for exactly the reason it refuses
/// `write_file` to the same path.
#[cfg(feature = "xlsx")]
pub const XLSX_READ_TOOL: &str = "xlsx_read";
/// The name the model uses to list a workbook's sheets. See [`XLSX_READ_TOOL`].
#[cfg(feature = "xlsx")]
pub const XLSX_SHEETS_TOOL: &str = "xlsx_sheets";
/// The name the model uses to create a new workbook. See [`XLSX_READ_TOOL`].
#[cfg(feature = "xlsx")]
pub const XLSX_WRITE_TOOL: &str = "xlsx_write";
/// The name the model uses to change one cell of an existing workbook, keeping
/// the rest of it. See [`XLSX_READ_TOOL`].
#[cfg(feature = "xlsx")]
pub const XLSX_SET_CELL_TOOL: &str = "xlsx_set_cell";

/// The names the model uses for the other document formats (0.14.0). Built-ins
/// for the same reason the spreadsheet tools are — see [`XLSX_READ_TOOL`].
#[cfg(feature = "docx")]
pub const DOCX_READ_TOOL: &str = "docx_read";
/// The name the model uses to create a Word document. See [`DOCX_READ_TOOL`].
#[cfg(feature = "docx")]
pub const DOCX_WRITE_TOOL: &str = "docx_write";
/// The name the model uses to read a slide deck's text. Read-only: there is no
/// `pptx_write`, because writing one is not a capability this crate has.
#[cfg(feature = "pptx")]
pub const PPTX_READ_TOOL: &str = "pptx_read";
/// The name the model uses to read a PDF's text. See [`XLSX_READ_TOOL`].
#[cfg(feature = "pdf")]
pub const PDF_READ_TOOL: &str = "pdf_read";
/// The name the model uses to create a PDF. See [`XLSX_READ_TOOL`].
#[cfg(feature = "pdf")]
pub const PDF_WRITE_TOOL: &str = "pdf_write";
/// The name the model uses to stamp a watermark across every page of a PDF.
#[cfg(feature = "pdf")]
pub const PDF_WATERMARK_TOOL: &str = "pdf_watermark";
/// The name the model uses to fill a PDF's form fields.
#[cfg(feature = "pdf")]
pub const PDF_FILL_FORM_TOOL: &str = "pdf_fill_form";
/// The name the model uses to decode barcodes and QR codes out of an image.
#[cfg(feature = "barcode")]
pub const BARCODE_DECODE_TOOL: &str = "barcode_decode";

/// The name the model uses to record a fact for later runs over this workspace.
///
/// Deliberately narrow: it writes one keyed note into the harness's own store, not
/// into the workspace, so it is not a path act. What it writes is bounded, attributed
/// to the run and step that wrote it, and readable and clearable by the embedding
/// program through [`Store`](crate::state::Store).
pub const REMEMBER_TOOL: &str = "remember";
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
