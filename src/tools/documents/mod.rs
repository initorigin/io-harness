//! Documents — formats the agent reads and writes that are not source text.
//!
//! Every format here is optional and behind its own feature, and every one of
//! them obeys the same rule: the bytes come from and go back through
//! [`Workspace::read_bytes`](crate::tools::Workspace::read_bytes) and
//! [`write_bytes`](crate::tools::Workspace::write_bytes). No parser is handed a
//! path. That is not a style preference — a document capability that opened
//! files itself would sit outside the permission policy, and a
//! `deny_write("secrets/*")` that stops `write_file` but not `set_cell` is not a
//! policy, it is a suggestion.
//!
//! 0.14.0 ships spreadsheets ([`xlsx`]).

pub mod xlsx;

pub mod barcode;

pub mod docx;
pub mod pptx;
