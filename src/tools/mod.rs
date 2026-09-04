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

#[cfg(feature = "browser")]
pub(crate) mod browser;
pub mod custom;
pub(crate) mod diagnostics;
pub mod documents;
pub mod exec;
pub mod fs;
pub mod git;
/// Long-running processes a run owns beyond the call that started them.
pub(crate) mod handles;
pub mod shell;
pub mod workspace;

pub use custom::{Tool, ToolEffect, ToolFuture, ToolRecovery, Toolbox};
pub use exec::DEFAULT_EXEC_TIMEOUT;
pub use fs::FsTool;
pub use workspace::{Entry, EntryKind, FileContent, Match, TextEncoding, Workspace, Wrote};

/// The tools a turn may not call, withheld by name.
///
/// **Availability, never membership.** A masked run offers the model exactly the
/// catalogue an unmasked one offers — the same [`ToolSpec`](crate::ToolSpec)
/// values, in the same order, serialised to the same bytes. Only the permission
/// to call them changes. That is the whole design and it is not a detail of the
/// implementation: Anthropic orders a request's cacheable prefix tools-then-system,
/// so 0.38.0's breakpoint at the end of `system` covers the tool schemas, and any
/// byte change in the tool array invalidates that entry and everything after it in
/// the same ordering. Removing a tool would save its definition's tokens once and
/// pay a cache **write** on every later turn of the run.
///
/// **A deny set rather than an allow set**, for the reason every other refusal in
/// this crate is deny-first: a set of permitted names silently withholds the next
/// tool anyone adds, and a caller who wrote their list against this version would
/// get a quietly narrower agent on the next one. Withholding is the thing the
/// caller asked for and the thing they are answerable for.
///
/// A mask is a request about **one turn**, not a setting that governs a run — the
/// same distinction [`TaskContract::fold_now`](crate::TaskContract::fold_now)
/// draws. Set it with
/// [`TaskContract::with_tool_mask`](crate::TaskContract::with_tool_mask).
///
/// A name that no tool answers to is kept rather than rejected. The catalogue
/// depends on cargo features and on what the run configured, so a caller
/// withholding `browser_click` in a build without the `browser` feature is asking
/// for something already true, and refusing them would make a portable mask
/// impossible to write.
///
/// ```
/// use io_harness::ToolMask;
///
/// let mask = ToolMask::withholding(["xlsx_write", "pdf_fill_form"]);
/// assert!(mask.withholds("xlsx_write"));
/// assert!(!mask.withholds("grep"));
/// assert_eq!(mask.len(), 2);
///
/// // The default withholds nothing, which is what every run gets unless it asks.
/// assert!(ToolMask::default().is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolMask {
    // A `BTreeSet` rather than a `Vec`: the lookup is per tool call, and the
    // ordering makes the rendered sentence stable without a sort at every use.
    // No `#[non_exhaustive]` — the field is private, so a struct literal is
    // already impossible outside this crate and the attribute would add nothing
    // but a forbidden `..Default::default()`.
    withheld: std::collections::BTreeSet<String>,
}

impl ToolMask {
    /// A mask that withholds nothing. The same value as [`ToolMask::default`].
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Withhold these names for the turn.
    #[must_use]
    pub fn withholding<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            withheld: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether this mask withholds `name`.
    #[must_use]
    pub fn withholds(&self, name: &str) -> bool {
        self.withheld.contains(name)
    }

    /// Whether this mask withholds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.withheld.is_empty()
    }

    /// How many names are withheld.
    #[must_use]
    pub fn len(&self) -> usize {
        self.withheld.len()
    }

    /// The withheld names, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.withheld.iter().map(String::as_str)
    }
}

/// The name the model uses to write a file (single-file 0.1/0.2 form: content only).
pub const WRITE_FILE_TOOL: &str = "write_file";
/// The name the model uses to change part of a file, leaving the rest alone
/// (0.17.0).
///
/// Sits beside [`WRITE_FILE_TOOL`] rather than replacing it: a new file, or one
/// being rewritten wholesale, is still a write. This is for the common case a
/// whole-file write handles badly — an edit costs tokens proportional to the file
/// rather than to the change, and rewrites content the agent never intended to
/// touch. Gated by the same [`Act::Write`](crate::Act::Write) check on the same
/// path, because it is the same act.
pub const EDIT_FILE_TOOL: &str = "edit_file";
/// The name the model uses to apply a unified diff to a file (0.51.0).
///
/// The third write tool, and the one that makes a multi-hunk change one call.
/// [`EDIT_FILE_TOOL`] is a single search-and-replace, so four changes to one
/// file are four calls — four gate evaluations, four checker runs and four round
/// trips, with the file's line numbers moving under the text the model read
/// after the first of them. A patch carries its own context lines, so the four
/// are one call and each lands where it was written to land or not at all.
///
/// Gated by the same [`Act::Write`](crate::Act::Write) check on the same path as
/// the other two, because it is the same act. It cannot create a file: a patch
/// is anchored to text that already exists, and creating is
/// [`WRITE_FILE_TOOL`]'s job.
pub const PATCH_FILE_TOOL: &str = "patch_file";
/// The name the model uses to ask the project's own checker (0.51.0).
///
/// The same ecosystem type-check the crate already runs after every successful
/// write — `cargo check`, `tsc --noEmit`, `go build ./...` — offered as a
/// question rather than only as a reflex. A model that wants to know whether the
/// tree compiles *before* deciding what to write had to reach for
/// [`EXEC_TOOL`] with an argv it guessed, on a project whose ecosystem this
/// crate has already detected and whose check command it already knows.
///
/// Two things it is not. It is not a gate: it reports, and an edit is never
/// failed on what it says. And it is not [`EXEC_TOOL`] with a shorter name — it
/// takes no arguments, so what runs is the detection's answer and not the
/// model's. It is an [`Act::Exec`](crate::Act::Exec) check on that program all
/// the same, because a model-callable path to the project's build command must
/// be refusable by the policy that refuses `exec`.
///
/// Since 0.74.0 the reflex is checked the same way and runs inside the same
/// containment, so what separates the two is only what they do with a refusal:
/// this one reports it, because somebody asked, and the reflex is silently
/// skipped. Only [`Effect::Allow`](crate::Effect::Allow) runs the reflex, and
/// [`Policy::default`](crate::Policy::default) asks about exec — so this tool is
/// where a model under an asking policy gets an answer at all.
pub const CHECK_TOOL: &str = "check";
/// Where the thing at this position is defined (0.52.0).
///
/// The five `lsp_*` names below are offered **only** when the contract or the
/// user-scope `io.toml` configured a language server — since 0.74.0 `[[lsp]]` is
/// refused in both workspace files. A run that configured none is offered
/// exactly the catalogue 0.51.0 offered, byte for byte — which is what makes this
/// feature free for a consumer who does not want it, under 0.38.0's cacheable
/// system prefix where every schema is paid for on every request of every run.
///
/// Positions are 1-based, as `read_file` shows them and as a compiler reports
/// them. The protocol counts from zero and the conversion happens in one place.
pub const LSP_DEFINITION_TOOL: &str = "lsp_definition";
/// Everywhere the thing at this position is used (0.52.0).
///
/// The question `grep` cannot answer: a text search returns the comments, the
/// string literals, the identically-named method on an unrelated type and the
/// definition itself, and a model has to read each hit to tell them apart.
pub const LSP_REFERENCES_TOOL: &str = "lsp_references";
/// What is in this file, or where a symbol is in the workspace (0.52.0).
///
/// One schema with two behaviours — no `query` is this file's symbols, a `query`
/// is the workspace's — because two schemas for one question is prompt bytes on
/// every request of every run.
pub const LSP_SYMBOLS_TOOL: &str = "lsp_symbols";
/// What the thing at this position is (0.52.0).
pub const LSP_HOVER_TOOL: &str = "lsp_hover";
/// Rename the thing at this position everywhere, as a patch (0.52.0).
///
/// **It writes nothing.** The server resolves the rename across the workspace and
/// this returns the change as a patch series in [`PATCH_FILE_TOOL`]'s own format,
/// which the model then applies one file at a time — one
/// [`Act::Write`](crate::Act::Write) check per path, all-or-nothing per file.
/// A tool that wrote N files on a server's say-so would be the multi-file write
/// 0.51.0 excluded, with the additional property that this crate did not compute
/// the change.
///
/// **It reads only what the policy allows outright** (0.74.0). Rendering a file's
/// patch reads that file and puts every removed line in the model's context, and
/// the paths are the *server's* choice rather than the model's, so each is
/// resolved under the workspace root and must be
/// [`Effect::Allow`](crate::Effect::Allow) for [`Act::Read`](crate::Act::Read).
/// An [`Effect::Ask`](crate::Effect::Ask) is not
/// permission here: there is no approver on this path, and an unanswered question
/// is not a yes. A file that does not clear the bar is omitted with a count and
/// the reason, so a run under an asking read policy is told why its patch is
/// short rather than handed a quietly incomplete one.
pub const LSP_RENAME_TOOL: &str = "lsp_rename";

/// The names the model uses to drive a browser (0.53.0).
///
/// Offered only to a run that configured one. A run that did not sees none of
/// these in its catalogue, which is what keeps its composed prompt — and the
/// cacheable prefix that prompt is billed against — byte-identical to a build
/// made before this release.
///
/// These six names exist in **every** build, though the tools behind them do not
/// — the `browser` feature is what compiles the dispatch arm and the CDP client,
/// not what makes the name the harness's. Until 0.61.0 the constants themselves
/// were `#[cfg(feature = "browser")]`, which is why the reserved-name set
/// `Toolbox::validate` enforces could not name them: an unconditional list cannot
/// hold a constant that does not exist in a default build. That is the same wall 0.17.0 met with the image and document names and
/// answered the same way — see [`VIEW_IMAGE_TOOL`].
pub const BROWSER_NAVIGATE_TOOL: &str = "browser_navigate";
/// The rendered text of the page, after its scripts have run.
pub const BROWSER_READ_TOOL: &str = "browser_read";
/// A picture of the page, which the model is shown.
pub const BROWSER_SCREENSHOT_TOOL: &str = "browser_screenshot";
/// A trusted click at a resolved element.
pub const BROWSER_CLICK_TOOL: &str = "browser_click";
/// Typing into a focused element.
pub const BROWSER_TYPE_TOOL: &str = "browser_type";
/// Scrolling the page.
pub const BROWSER_SCROLL_TOOL: &str = "browser_scroll";
/// The name the model uses to run a command (0.17.0).
///
/// The widest capability the crate grants, and the one that made a task in any
/// language expressible. Every call is an [`Act::Exec`](crate::Act::Exec) check
/// on the program *and* on the joined argv, so an operator can allow `cargo *`
/// and then take `cargo publish *` back out of it with the rule syntax the policy
/// already has. See [`exec`] for what it does and does not bound.
///
/// **An argv deny is sound inside an allowlist and nowhere else.** It narrows
/// something already allowed — `defaults.exec` at
/// [`Effect::Deny`](crate::Effect::Deny) or [`Effect::Ask`](crate::Effect::Ask),
/// with explicit `allow_exec` rules saying what
/// may run — and it is not a blocklist over a permissive default. A joined argv
/// has more spellings than a pattern can enumerate: `["git", "-c", "x", "push"]`
/// puts a flag between the program and the sub-command, and `["env", "rm"]` and
/// `["busybox", "rm"]` reach the program under a name the rule never sees. No
/// pattern completes that set, because the set is not finite. Documented here
/// rather than fixed: the check is what it is, and a rule writer has to know
/// which of the two shapes they are writing.
pub const EXEC_TOOL: &str = "exec";
/// The name the model uses to run a command *line* (0.24.0).
///
/// [`EXEC_TOOL`] takes an argv array, which is what makes its check meaningful
/// and what puts pipelines, redirects and sequences out of reach: `;` and `&&`
/// are ordinary bytes inside one argument because nothing on that path parses
/// them. This tool parses the line itself and checks every sub-command it finds
/// against [`Act::Exec`](crate::Act::Exec) and every redirect target against
/// [`Act::Write`](crate::Act::Write) or [`Act::Read`](crate::Act::Read) —
/// **all of them before anything is spawned**, so a line whose second stage is
/// denied does not run its first.
///
/// There is no host shell after the parse. Each sub-command is spawned as argv
/// the way [`EXEC_TOOL`] spawns one, and this crate wires the pipes. The grammar
/// admitted is a conservative subset of POSIX; command substitution, parameter
/// expansion, subshells, heredocs, background and control flow are refused by
/// name. See [`shell`] for the whole set and why it is drawn where it is.
pub const SHELL_TOOL: &str = "shell";
/// The name the model uses to start a command line and keep it running (0.25.0).
///
/// The line is parsed and checked by exactly the machinery [`SHELL_TOOL`] uses —
/// same lexer, same refusal set, same per-stage [`Act::Exec`](crate::Act::Exec)
/// and per-redirect path check, all before the first spawn. A handle is a
/// different *lifetime* for a command line, not a second way to run one.
///
/// What it changes is what happens after the check passes: the processes are
/// registered and an id comes back instead of a result. That id is polled with
/// [`SHELL_POLL_TOOL`] and ended with [`SHELL_KILL_TOOL`].
///
/// A handle does not survive the process that started it. On resume it is
/// reported orphaned and is never re-attached, polled or signalled — a recorded
/// pid may since have been reused, and signalling it is the one way this crate
/// could damage something outside its workspace.
///
/// **On Windows, quote absolute paths.** The grammar treats `\` as an escape,
/// as POSIX does, so an unquoted `C:\repo\server.exe` lexes to
/// `C:reposerver.exe` and the spawn fails naming a program nobody wrote. This is
/// the same answer a real shell gives and applies equally to [`SHELL_TOOL`];
/// single quotes are the simplest form.
pub const SHELL_START_TOOL: &str = "shell_start";
/// The name the model uses to read what a started line has produced (0.25.0).
///
/// Returns what was written since the previous poll, not the whole history: a
/// log tail polled ten times must not return its output ten times. The full
/// stream stays in the run's trace, so nothing is lost by the window.
pub const SHELL_POLL_TOOL: &str = "shell_poll";
/// The name the model uses to end a started line and everything it spawned
/// (0.25.0).
pub const SHELL_KILL_TOOL: &str = "shell_kill";
/// The name the model uses to run one Python program instead of a chain of tool
/// calls (0.79.0).
///
/// The argument is source, not a command line: nothing about this tool spawns
/// what the model typed. The interpreter is a host binary resolved before the
/// run and the program is written to the contained workdir, so
/// [`Act::Exec`](crate::Act::Exec) never sees model-authored argv here — which is
/// the opposite of [`EXEC_TOOL`] and [`SHELL_TOOL`], where the argv *is* the
/// model's text and the whole check.
///
/// **A program is not trusted more than a tool call.** Each act it takes comes
/// back over a pipe, becomes a [`ToolCall`](crate::ToolCall), and re-enters the
/// same dispatch a model's own call takes — same policy, same gate, same
/// `policy_events` row, same journal attempt, same observer event. What collapses
/// is the number of provider round trips, not the number of boundaries.
///
/// The tool is advertised only when the feature is compiled *and* a usable
/// interpreter was found. A host without one is a supported host: the name is
/// still reserved, the model is simply never offered it, and the turn proceeds as
/// it would have with the feature off.
pub const RUN_PROGRAM_TOOL: &str = "run_program";
/// The name the model uses to search file contents by regex/substring.
pub const GREP_TOOL: &str = "grep";
/// The name the model uses to list files by name/path glob.
pub const FIND_TOOL: &str = "find";
/// The name the model uses to list one directory, one level deep (0.24.0).
///
/// Beside [`FIND_TOOL`] rather than replacing it, because they answer different
/// questions. `find` globs the entire tree and needs a name to glob for; this
/// needs only a directory, and returns what is immediately in it with each
/// entry's kind and each file's size. It is the first thing anyone does in an
/// unfamiliar repository, and until 0.24.0 the only way to do it was to glob `*`
/// and read the shape of the tree out of the paths that came back.
///
/// Gated as the read it is: an [`Act::Read`](crate::Act::Read) check on the
/// directory's own path, through the same `check_path` that decides a
/// [`READ_FILE_TOOL`] call, so a directory an operator denied reading cannot be
/// enumerated by naming it to a different tool.
pub const LIST_DIR_TOOL: &str = "list_dir";
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
/// The name the model uses to create a branch and move onto it (0.36.0).
///
/// `git switch --create=<name>`, so a run lands its commits somewhere a human
/// can review or delete on its own. It is the only shape of a checkout this
/// crate builds, and the only one that cannot discard a working-tree change:
/// the new ref starts at `HEAD`, an existing name is refused by git, and the
/// tree is carried across rather than replaced. See [`GIT_LOG_TOOL`] for why
/// each of these is its own tool rather than an argument.
pub const GIT_BRANCH_TOOL: &str = "git_branch";
/// The name the model uses to make a second working tree (0.36.0).
///
/// `git worktree add -b <name> -- <path>`: another checkout of the same
/// repository, at its own new branch, so two agents stop overwriting each
/// other's files without either leaving the workspace. Nothing here removes
/// one — see [`GIT_LOG_TOOL`].
pub const GIT_WORKTREE_TOOL: &str = "git_worktree";

/// The name the model uses to look at an image in the workspace (0.15.0,
/// `media` feature).
///
/// A built-in rather than a registered [`Tool`], for the reason the document
/// tools are: this one decides which of the user's files is sent to a third
/// party, so it is gated per call on the real path the model names rather than
/// authorised once by name.
///
/// This name and the document names below exist in **every** build, though the
/// tools behind them do not. Until 0.17.0 they were `#[cfg]`-gated, which made
/// the reserved-name set [`Toolbox::validate`] enforces depend on the feature
/// set: a caller could register a `Tool` called `xlsx_read` in a default build,
/// pass validation, and then have that tool silently stop being reachable the
/// day they turned the `xlsx` feature on. A name the harness owns is owned in
/// all builds, so enabling a feature can never take a working tool away.
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
pub const XLSX_READ_TOOL: &str = "xlsx_read";
/// The name the model uses to list a workbook's sheets. See [`XLSX_READ_TOOL`].
pub const XLSX_SHEETS_TOOL: &str = "xlsx_sheets";
/// The name the model uses to create a new workbook. See [`XLSX_READ_TOOL`].
pub const XLSX_WRITE_TOOL: &str = "xlsx_write";
/// The name the model uses to change one cell of an existing workbook, keeping
/// the rest of it. See [`XLSX_READ_TOOL`].
pub const XLSX_SET_CELL_TOOL: &str = "xlsx_set_cell";

/// The names the model uses for the other document formats (0.14.0). Built-ins
/// for the same reason the spreadsheet tools are — see [`XLSX_READ_TOOL`].
pub const DOCX_READ_TOOL: &str = "docx_read";
/// The name the model uses to create a Word document. See [`DOCX_READ_TOOL`].
pub const DOCX_WRITE_TOOL: &str = "docx_write";
/// The name the model uses to read a slide deck's text. Read-only: there is no
/// `pptx_write`, because writing one is not a capability this crate has.
pub const PPTX_READ_TOOL: &str = "pptx_read";
/// The name the model uses to read a PDF's text. See [`XLSX_READ_TOOL`].
pub const PDF_READ_TOOL: &str = "pdf_read";
/// The name the model uses to create a PDF. See [`XLSX_READ_TOOL`].
pub const PDF_WRITE_TOOL: &str = "pdf_write";
/// The name the model uses to stamp a watermark across every page of a PDF.
pub const PDF_WATERMARK_TOOL: &str = "pdf_watermark";
/// The name the model uses to fill a PDF's form fields.
pub const PDF_FILL_FORM_TOOL: &str = "pdf_fill_form";
/// The name the model uses to decode barcodes and QR codes out of an image.
pub const BARCODE_DECODE_TOOL: &str = "barcode_decode";

/// The name the model uses to record a fact for later runs over this workspace.
///
/// Deliberately narrow: it writes one keyed note into the harness's own store, not
/// into the workspace, so it is not a path act. What it writes is bounded, attributed
/// to the run and step that wrote it, and readable and clearable by the embedding
/// program through [`Store`](crate::state::Store).
pub const REMEMBER_TOOL: &str = "remember";

/// The name the model uses to withdraw a note it wrote (0.56.0).
///
/// The counterpart to [`REMEMBER_TOOL`], and narrow in the same way: it removes
/// one keyed note from the harness's own store, not from the workspace, so it is
/// not a path act. It exists because writing a key again only *replaces* it — an
/// agent that learned the same wrong thing under two names could correct neither
/// without recalling the exact key it used, and both notes would go on
/// disagreeing.
///
/// What an operator pinned is refused, for the same reason a write to it is, and
/// the removal is reversible: the run's restore point is taken before the entry
/// goes, so [`rewind_run`](crate::rewind_run) puts it back.
pub const FORGET_TOOL: &str = "forget";
/// The name the model uses to write down its plan (0.21.0).
///
/// Narrow for the same reason [`REMEMBER_TOOL`] is: it writes into the harness's own
/// store, not into the workspace, the network, or a binary, so it is not an
/// [`Act`](crate::Act) and it is not gated. The whole list is replaced on every call,
/// which is why there is no item id and no partial-update semantics for a model to
/// get wrong.
///
/// What it is for is a long run that can be recognised as going the wrong way before
/// it ends. What it is explicitly *not* is a commitment: nothing verifies a plan and
/// no outcome depends on one. See the plan section of `docs/CONTRACT.md`.
///
/// ```
/// use io_harness::{Toolbox, Tool, TODO_WRITE_TOOL};
///
/// assert_eq!(TODO_WRITE_TOOL, "todo_write");
///
/// // Reserved, so a caller's own tool cannot shadow the built-in and silently
/// // replace the operator's view of the plan.
/// # #[derive(Debug)]
/// # struct Mine;
/// # impl Tool for Mine {
/// #     fn spec(&self) -> io_harness::provider::ToolSpec {
/// #         io_harness::provider::ToolSpec {
/// #             name: TODO_WRITE_TOOL.to_string(),
/// #             description: "mine".into(),
/// #             parameters: serde_json::json!({"type": "object"}),
/// #         }
/// #     }
/// #     fn invoke(&self, _args: &serde_json::Value) -> io_harness::ToolFuture {
/// #         Box::pin(async { Ok(String::new()) })
/// #     }
/// # }
/// assert!(Toolbox::new().with(Mine).validate().is_err());
/// ```
pub const TODO_WRITE_TOOL: &str = "todo_write";
/// The name the model uses to ask the operator what they actually wanted (0.21.0).
///
/// The distinction this tool exists to draw: the approval path
/// ([`Approver`](crate::Approver)) asks whether an action is *permitted*; this asks
/// what the operator *meant*. An answer is text the model reads, delivered as an
/// observation, and it authorizes nothing — every tool call that follows it is checked
/// against the same [`Policy`](crate::Policy) by the same code.
///
/// ```
/// use io_harness::ASK_QUESTION_TOOL;
///
/// assert_eq!(ASK_QUESTION_TOOL, "ask_question");
/// ```
pub const ASK_QUESTION_TOOL: &str = "ask_question";
/// The name the model uses to ask several **independent** questions at once (0.72.0).
///
/// [`ASK_QUESTION_TOOL`] is unchanged and is still the right tool for one question.
/// This one exists because `Responder::answer` takes one question and blocks, so a
/// model that needed five facts spent five round trips and an interface downstream
/// could only render what reached it — one question at a time. It cannot batch what
/// arrives serially.
///
/// The batch is for facts that do not depend on each other. Questions whose answers
/// depend on one another belong in separate calls: an operator cannot answer question
/// two before question one, and a batch that asks them together gets a guess.
///
/// ```
/// use io_harness::ASK_QUESTIONS_TOOL;
///
/// assert_eq!(ASK_QUESTIONS_TOOL, "ask_questions");
/// ```
pub const ASK_QUESTIONS_TOOL: &str = "ask_questions";
/// The most questions one [`ASK_QUESTIONS_TOOL`] call may carry.
///
/// Capped and *told*, following `todo_write` rather than truncating silently — the
/// number is in the tool's own description so the model reads it before it sends
/// rather than only in the error afterwards. Ten is what one terminal overlay can
/// plausibly present with its context lines still readable.
pub(crate) const QUESTIONS_MAX: usize = 10;
/// The most lines a [`Choice`](crate::Choice)'s preview may carry.
///
/// A preview is a snippet, not a document. The bound exists because this value is
/// rendered inside a terminal viewport by every consumer and is written by a model, so
/// a harness that accepted an unbounded block would have moved the problem to each of
/// them.
pub(crate) const PREVIEW_MAX_LINES: usize = 12;
/// The most bytes a [`Choice`](crate::Choice)'s preview may carry, whichever bound is
/// reached first. Cut at a line boundary, never mid-word.
pub(crate) const PREVIEW_MAX_BYTES: usize = 800;
/// The tool the agent proposes a plan with, offered only while a
/// [`PlanGate`](crate::PlanGate) is registered and unsatisfied (0.31.0).
///
/// The distinction from [`TODO_WRITE_TOOL`] is the reason both exist: that one
/// records a plan the agent is already executing, this one proposes a plan the run
/// has not started. While it is on the table every [`Act::Write`](crate::Act::Write)
/// and [`Act::Exec`](crate::Act::Exec) is denied, so calling it is the only way the
/// agent gets to do anything at all.
///
/// ```
/// use io_harness::PROPOSE_PLAN_TOOL;
///
/// assert_eq!(PROPOSE_PLAN_TOOL, "propose_plan");
/// ```
pub const PROPOSE_PLAN_TOOL: &str = "propose_plan";
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
