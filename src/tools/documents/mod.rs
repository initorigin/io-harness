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
//! The formats are spreadsheets (`xlsx`), Word (`docx`), PowerPoint text
//! (`pptx`, read-only), PDF (`pdf`) and barcode decoding (`barcode`). Each is
//! behind its own feature; the `documents` umbrella turns on all five.
//!
//! The names are written plainly rather than as intra-doc links: each is both a
//! feature and a module, and the module only exists when the feature is on, so
//! a link resolves in an `--all-features` build and dangles in the default one.

pub mod barcode;
pub mod docx;
pub mod pdf;
pub mod pptx;
pub mod xlsx;
