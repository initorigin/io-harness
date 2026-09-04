//! Running one contained Python program instead of a chain of tool calls
//! (0.79.0).
//!
//! A step that would have been six round trips — grep, read, read, read, edit,
//! exec — is one program the model writes once. The program runs as a child of
//! this process, contained the way a backgrounded command line is contained, and
//! every act it takes comes back over a pipe and re-enters the same dispatch a
//! model's own tool call takes. What collapses is the number of provider round
//! trips, not the number of boundaries.
//!
//! # Why the child is not a [`Sandbox`](crate::Sandbox)
//!
//! [`Sandbox::run`](crate::Sandbox::run) sets `stdin` to null and consumes its
//! `RunSpec` to completion, which is right for every other contained execution in
//! this crate and is exactly wrong for a program that has to ask questions while
//! it runs. So the containment is composed from the pieces underneath that trait
//! — [`wrap_argv`](crate::sandbox::wrap_argv),
//! [`contain_command`](crate::sandbox::contain_command),
//! [`apply_rlimits`](crate::sandbox::apply_rlimits) and
//! [`own_process_group`](crate::sandbox::own_process_group) — which is what
//! `shell_start` already does for a line that outlives the call that made it. The
//! backend selected is the same, chosen by the same rules, and the caps are the
//! same `SandboxLimits`.
//!
//! **Which by default are none, and that is why the two bounds here are not
//! belt-and-braces.** [`TaskContract`](crate::TaskContract)'s `exec_sandbox`
//! carries [`SandboxLimits::none()`](crate::SandboxLimits) unless a caller sets
//! limits, so on a default contract nothing underneath a program bounds its CPU,
//! its memory or its wall clock — and a run that never asked for contained exec
//! has no rlimits at all. That is the bound `exec` already has and this module
//! does not change it; it is the reason [`CodeActConfig::timeout`] is applied to
//! the *wait for a frame* rather than to the loop around it. A program that spins
//! without ever calling back produces no frame to check a deadline between, so
//! that bound is the only thing that stops it.
//!
//! **That seam applies nothing at all on Windows, so a program that asked to be
//! contained is refused there rather than degraded.** `wrap_argv` has only macOS
//! and Linux branches, `apply_rlimits` is unix-only, `contain_command` answers
//! `None` off Linux, and the Job Object is created by the `Sandbox` runner and by
//! `shell_start`'s own suspended-spawn path — neither of which this is. A program
//! started here on a Windows host would therefore have had the full filesystem
//! and the full network while the run reported a backend granting neither, which
//! is 0.74.0's rule exactly: a boundary named in the trace and not applied to the
//! process is worse than no boundary at all. See [`containment_refusal`].
//!
//! # What the program can and cannot reach
//!
//! It runs in an ephemeral workdir of its own, not in the workspace. It needs no
//! workspace access, because every effect it has on the workspace is a callback
//! that goes through the policy. Under a backend that confines writes, a program
//! that tries to edit a workspace file directly cannot; under the portable floor
//! which has no path rule, the honest claim is only the ephemeral workdir; and
//! under a contract whose [`ExecMode`](crate::ExecMode) is not a contained one
//! there is no backend at all, so a program runs on the host with this process's
//! privileges, exactly as an `exec` does on such a run.
//!
//! Egress is denied and no proxy is named whatever the run itself was granted,
//! because a program that could open its own socket would be a second route out
//! of a run whose first one is gated. **How much that denial is worth is the
//! backend's answer, not this module's**: `Backend::denies_egress` is false for
//! the portable floor, so there a program can still open a socket, and on a run
//! with no containment at all there is nothing to deny it. The claim is exactly
//! what the backend delivers and never more.
//!
//! Starting the interpreter is itself an [`Act::Exec`](crate::Act::Exec) check on
//! the program and on the whole argv, taken before anything is spawned — so a run
//! that denies execution denies programs too, and this tool is not a second path
//! around that gate.
//!
//! The interpreter is the host's, resolved the way a browser is resolved.
//! **Nothing is downloaded, ever.** A host with no usable interpreter is a
//! supported host: [`RUN_PROGRAM_TOOL`](crate::tools::RUN_PROGRAM_TOOL) is not
//! advertised and the turn runs as it would have with the feature off.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{Error, Result};

/// The interpreters looked for, in order, when no `[codeact]` block names one.
///
/// `python` is second and is never trusted by name: a `python` that answers 2.7
/// is rejected by the probe, which is the case a name alone gets wrong. Windows
/// runner images and most Windows installations provide `python` rather than
/// `python3`, which is why the second entry exists at all.
///
/// ```
/// assert_eq!(io_harness::CODEACT_CANDIDATES, ["python3", "python"]);
/// ```
pub const CODEACT_CANDIDATES: &[&str] = &["python3", "python"];

/// The lowest interpreter version a program is handed to, as `(major, minor)`.
///
/// 3.8 rather than something newer: the shim is written without syntax a
/// long-lived host might not have, and every current runner image and supported
/// distribution exceeds it. A candidate reporting less is rejected rather than
/// run, so the failure is "no usable interpreter" at discovery instead of a
/// `SyntaxError` inside a program the model wrote.
///
/// ```
/// assert_eq!(io_harness::CODEACT_MIN_PYTHON, (3, 8));
/// ```
pub const CODEACT_MIN_PYTHON: (u32, u32) = (3, 8);

/// The tools a program may not call, written out by name.
///
/// Three groups, and the reason differs by group:
///
/// - `remember`, `forget` and `todo_write` are the writes the policy deliberately
///   does not see, because they land in the harness's own store rather than in
///   the workspace and there is no `Act::Write` for a gate to check. Their only
///   boundary is the plan gate, which is a property of the turn and not of the
///   program, so a program calling them would have no boundary at all.
/// - `ask_question`, `ask_questions` and `propose_plan` need a conversation, and
///   `spawn_agent`, `send_message` and `read_messages` need a tree. A program is
///   inside one step of one run; there is nobody to answer it and no sibling to
///   address.
/// - `read_skill` hands a server-side document to the caller, and `run_program`
///   would let one program start another — which turns the callback bound into a
///   bound per level rather than a bound.
///
/// It is a literal rather than a derivation. Deriving it as the catalogue minus
/// the exclusions is the same set today and makes every built-in added later
/// callable silently; a literal fails a test until somebody classifies the new
/// name, which is the outcome worth having.
///
/// ```
/// use io_harness::CODEACT_UNCALLABLE;
///
/// // The three writes the policy does not see are refused to a program.
/// assert!(CODEACT_UNCALLABLE.contains(&"remember"));
/// // Reading a file is not — that is the whole point.
/// assert!(!CODEACT_UNCALLABLE.contains(&"read_file"));
/// ```
pub const CODEACT_UNCALLABLE: &[&str] = &[
    crate::tools::REMEMBER_TOOL,
    crate::tools::FORGET_TOOL,
    crate::tools::TODO_WRITE_TOOL,
    crate::tools::ASK_QUESTION_TOOL,
    crate::tools::ASK_QUESTIONS_TOOL,
    crate::tools::PROPOSE_PLAN_TOOL,
    crate::tools::READ_SKILL_TOOL,
    crate::tools::RUN_PROGRAM_TOOL,
    crate::run::SPAWN_TOOL,
    crate::run::SEND_MESSAGE_TOOL,
    crate::run::READ_MESSAGES_TOOL,
];

/// Words a generated binding may not take, because Python 3 parses them.
///
/// A name is not a `SyntaxError` risk any more — the shim carries its names as
/// data and injects them into the program's namespace, so a keyword lands in a
/// dictionary and breaks nothing. The reason is narrower and still real: the
/// program calls a tool by writing `name(...)`, and a name that is a keyword or
/// is not an identifier cannot be written that way at all. Such a name is left
/// out of the surface rather than advertised as something the model can call and
/// then cannot, and it stays reachable the ordinary way in the same turn.
///
/// **`exec` and `print` are not on this list and must not be added.** Both are
/// keywords in Python 2 and ordinary builtins in Python 3, and this crate probes
/// for 3.8 or better, so `def exec(**kwargs):` is legal — it shadows a builtin
/// inside the module, which is the intent. They were on an earlier version of
/// this list and the effect was that `exec`, the widest capability this crate
/// grants, was silently missing from every program's surface while the module
/// still compiled and every other tool still worked.
///
/// Capitalised keywords (`None`, `True`, `False`) are absent for the same reason
/// the comparison below is exact rather than case-insensitive: `none` is a legal
/// function name and only `None` is the keyword. Soft keywords (`match`, `case`,
/// `type`) are legal names and are likewise not here.
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

/// Whether a tool's name can be a Python function this crate generates.
///
/// Every built-in passes by construction — the reserved-name test already holds
/// them to `[a-z0-9_]+`. This filter is about the two catalogues that are not
/// this crate's: a registered [`Tool`](crate::tools::Tool) and an MCP server's,
/// whose names are whoever wrote them. A name that cannot be a binding is left
/// out of the program's surface rather than allowed to break the shim, and it
/// stays callable the ordinary way in the same turn.
pub(crate) fn is_callable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !PYTHON_KEYWORDS.contains(&name)
}

/// How many callbacks one program may make before it is stopped.
const DEFAULT_MAX_CALLBACKS: usize = 64;

/// How long one program may run, wall clock, independent of the sandbox's own
/// caps. The sandbox bound covers the child's CPU and memory; this one covers a
/// program that spends the harness's time instead of its own.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// How long a version probe may take. A candidate on `PATH` that does not answer
/// promptly is not a usable interpreter, and discovery must not be able to hang a
/// run before its first completion.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the probe asks. Written with `%`-formatting and a parenthesised `print`
/// so that a Python 2 candidate answers rather than raising — a candidate that
/// cannot answer is indistinguishable from one that is absent, and this crate
/// would rather reject `2.7` by number than by silence.
const PROBE_SOURCE: &str = "import sys;print('%d.%d' % sys.version_info[:2])";

/// Which interpreter runs a program, and how far one program may reach (0.79.0).
///
/// Every field has a default, so a caller who wants the capability and no opinion
/// about it constructs one with [`CodeActConfig::default`]. Naming an interpreter
/// skips discovery for that path only — it is still version-probed, because a
/// path an operator wrote once is not evidence about the binary that is there
/// today.
///
/// ```
/// use io_harness::CodeActConfig;
/// use std::time::Duration;
///
/// let config = CodeActConfig::default()
///     .with_max_callbacks(16)
///     .with_timeout(Duration::from_secs(30));
///
/// assert_eq!(config.max_callbacks(), 16);
/// assert_eq!(config.timeout(), Duration::from_secs(30));
/// // Nothing is named, so the candidate list decides.
/// assert!(config.interpreter().is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActConfig {
    interpreter: Option<PathBuf>,
    max_callbacks: usize,
    timeout: Duration,
}

impl Default for CodeActConfig {
    fn default() -> Self {
        Self {
            interpreter: None,
            max_callbacks: DEFAULT_MAX_CALLBACKS,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

impl CodeActConfig {
    /// Use this interpreter rather than searching [`CODEACT_CANDIDATES`].
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    ///
    /// let config = CodeActConfig::default().with_interpreter("/usr/bin/python3");
    /// assert_eq!(config.interpreter().unwrap().to_str(), Some("/usr/bin/python3"));
    /// ```
    #[must_use]
    pub fn with_interpreter(mut self, path: impl Into<PathBuf>) -> Self {
        self.interpreter = Some(path.into());
        self
    }

    /// Stop a program after this many callbacks.
    ///
    /// The sandbox's caps bound what the child spends. This bounds what the
    /// program makes *this* process spend, which is a different resource and is
    /// the one a tight callback loop exhausts.
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    ///
    /// assert_eq!(CodeActConfig::default().with_max_callbacks(4).max_callbacks(), 4);
    /// ```
    #[must_use]
    pub fn with_max_callbacks(mut self, calls: usize) -> Self {
        self.max_callbacks = calls;
        self
    }

    /// Stop a program after this much wall-clock time.
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    /// use std::time::Duration;
    ///
    /// let config = CodeActConfig::default().with_timeout(Duration::from_secs(5));
    /// assert_eq!(config.timeout(), Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The interpreter this configuration names, if any.
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    ///
    /// assert!(CodeActConfig::default().interpreter().is_none());
    /// ```
    #[must_use]
    pub fn interpreter(&self) -> Option<&Path> {
        self.interpreter.as_deref()
    }

    /// How many callbacks one program may make.
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    ///
    /// assert_eq!(CodeActConfig::default().max_callbacks(), 64);
    /// ```
    #[must_use]
    pub fn max_callbacks(&self) -> usize {
        self.max_callbacks
    }

    /// How long one program may run.
    ///
    /// ```
    /// use io_harness::CodeActConfig;
    /// use std::time::Duration;
    ///
    /// assert_eq!(CodeActConfig::default().timeout(), Duration::from_secs(120));
    /// ```
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// The file the program's own source is written to inside the workdir.
///
/// Named rather than spelled twice, because it is half of the argv the
/// `Act::Exec` check on starting an interpreter is made against.
pub(crate) const PROGRAM_FILE: &str = "program.py";

/// The largest single frame this crate will read from a program.
///
/// A program's captured output is unbounded on its own side — `SandboxLimits` is
/// `none()` on a default contract, so there is no `RLIMIT_AS` under it — and a
/// `print("x" * 10**9)` would otherwise arrive as one line this process buffers
/// whole, three times over, before the result cap ever saw it. The shim truncates
/// at [`SHIM_OUTPUT_CAP`] and this is the second half of the same bound: a shim
/// that did not truncate, because somebody edited it or because the program
/// reached past it, still cannot make the harness allocate without limit.
const MAX_FRAME_BYTES: u64 = 4 * 1024 * 1024;

/// What the shim truncates a program's captured output to before sending it.
///
/// Larger than any observation the result cap will keep, so the truncation a
/// reader sees is the crate's ordinary one rather than this.
const SHIM_OUTPUT_CAP: usize = 512 * 1024;

/// An approver that answers for a program, and never defers on its behalf.
///
/// [`Decision::Defer`](crate::Decision) records a pending action and pauses the
/// run so a human can answer after the process has exited. That is a coherent
/// answer to a model's own tool call and an incoherent one to an act inside a
/// program: the program is mid-flight with a pipe open, the acts it has already
/// taken have already happened, and a resumed run re-writes the program from
/// scratch — so it would re-execute them. Deferring inside a program also loses
/// the run's own accounting of what the program changed, because the pause leaves
/// the arm before the `changed` and `remember` it has accumulated are reported.
///
/// So a deferral becomes a denial *for the program only*, in this crate's own
/// words, and the program branches on it like any other refusal. The caller's
/// approver is untouched everywhere else, and an act the model makes itself can
/// still be deferred exactly as before.
pub(crate) struct NoDefer<'a>(pub(crate) &'a dyn crate::Approver);

impl crate::Approver for NoDefer<'_> {
    fn decide<'a>(&'a self, request: &'a crate::Request) -> crate::approve::DecisionFuture<'a> {
        Box::pin(async move { undefer(self.0.decide(request).await) })
    }

    /// Overridden as well as [`Approver::decide`], because this is the one the run
    /// loop actually calls — wrapping only the other would have let a deferral
    /// through on every real path.
    fn decide_in_context<'a>(
        &'a self,
        request: &'a crate::Request,
        context: &'a crate::ApprovalContext,
    ) -> crate::approve::DecisionFuture<'a> {
        Box::pin(async move { undefer(self.0.decide_in_context(request, context).await) })
    }

    /// Both forwarded, so wrapping an approver does not quietly turn a model
    /// approver into something the self-approval refusal cannot recognise.
    fn model(&self) -> Option<&str> {
        self.0.model()
    }

    fn self_approval_allowed(&self) -> bool {
        self.0.self_approval_allowed()
    }
}

fn undefer(decision: crate::Decision) -> crate::Decision {
    match decision {
        crate::Decision::Defer => crate::Decision::deny(
            "this act was deferred for a decision later, and a program cannot wait for one. \
             Finish the program and take this act directly, where a deferral can park the run \
             until somebody answers it.",
        ),
        other => other,
    }
}

/// Why this host cannot contain a program, when it cannot.
///
/// The living-child seam applies a backend on unix and applies **nothing** on
/// Windows: `wrap_argv` has no Windows branch, `apply_rlimits` is unix-only,
/// `contain_command` answers `None` off Linux, and the Job Object is created by
/// the `Sandbox` runner and by `shell_start`'s own suspended-spawn path, neither
/// of which this is. A program started here on a Windows host that asked to be
/// contained would therefore run with the full filesystem and the full network
/// while the run reported a backend that grants neither.
///
/// `shell_start` already refuses rather than degrades for the narrower case of
/// the AppContainer, on 0.74.0's reasoning that a boundary asked for by name and
/// not applied is worse than no boundary at all. This is that rule, applied to
/// every Windows backend, because this seam applies none of them.
///
/// A run that asked for no containment is not refused: it is uncontained by the
/// caller's own choice, exactly as `exec` is on such a run.
pub(crate) fn containment_refusal(
    containment: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> Option<String> {
    let containment = containment?;
    let _ = containment;
    #[cfg(windows)]
    {
        return Some(format!(
            "this run asked for `{}` containment and a program cannot be given it on this host. \
             Nothing was started. The interpreter would have run with the full filesystem and the \
             full network while the run reported a boundary it did not have, so it is refused \
             rather than degraded. Use the individual tools, which are contained here, or run \
             this on a host where a program can be confined.",
            containment.backend().as_str()
        ));
    }
    #[cfg(not(windows))]
    None
}

/// What discovery found, kept whole so the run can say what it looked for.
///
/// The `Missing` arm carries every candidate and what it answered, because "no
/// interpreter" and "a `python` that reported 2.7" are different facts about a
/// host and an operator reading the trace needs the second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discovery {
    Found { path: PathBuf, version: (u32, u32) },
    Missing { tried: Vec<(String, String)> },
}

impl Discovery {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Found { path, .. } => Some(path),
            Self::Missing { .. } => None,
        }
    }

    /// One line an observer and the trace can both carry.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Found { path, version } => {
                format!("{} (Python {}.{})", path.display(), version.0, version.1)
            }
            Self::Missing { tried } if tried.is_empty() => {
                "no interpreter named and no candidate on PATH".to_string()
            }
            Self::Missing { tried } => {
                let parts: Vec<String> =
                    tried.iter().map(|(c, why)| format!("{c}: {why}")).collect();
                parts.join("; ")
            }
        }
    }
}

/// Find a usable interpreter, or say what was tried and why none was.
///
/// A named interpreter is probed like any other candidate. An operator writes a
/// path once and the machine changes underneath it, so the file being there is
/// not evidence that it is an interpreter this crate can hand a program to.
pub(crate) async fn discover(config: &CodeActConfig) -> Discovery {
    let mut tried = Vec::new();

    if let Some(named) = &config.interpreter {
        match probe(named).await {
            Ok(version) if usable(version) => {
                return Discovery::Found {
                    path: named.clone(),
                    version,
                }
            }
            Ok(version) => tried.push((
                named.display().to_string(),
                format!("reported Python {}.{}", version.0, version.1),
            )),
            Err(why) => tried.push((named.display().to_string(), why)),
        }
        // A named interpreter that does not answer is not silently replaced by a
        // candidate off `PATH`. The operator said which one; falling through to
        // another would run a program on a binary nobody chose.
        return Discovery::Missing { tried };
    }

    for candidate in CODEACT_CANDIDATES {
        let Some(path) = crate::sandbox::resolve_program(candidate) else {
            tried.push(((*candidate).to_string(), "not on PATH".to_string()));
            continue;
        };
        match probe(&path).await {
            Ok(version) if usable(version) => return Discovery::Found { path, version },
            Ok(version) => tried.push((
                (*candidate).to_string(),
                format!("reported Python {}.{}", version.0, version.1),
            )),
            Err(why) => tried.push(((*candidate).to_string(), why)),
        }
    }

    Discovery::Missing { tried }
}

fn usable(version: (u32, u32)) -> bool {
    version >= CODEACT_MIN_PYTHON
}

/// Ask one candidate what it is. Uncontained on purpose: this runs before the run
/// does, asks a fixed argv this crate wrote, and reads two integers back.
async fn probe(path: &Path) -> std::result::Result<(u32, u32), String> {
    let mut cmd = Command::new(path);
    cmd.arg("-c")
        .arg(PROBE_SOURCE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(format!("could not run it ({err})")),
        Err(_) => return Err("did not answer the version probe in time".to_string()),
    };
    if !output.status.success() {
        return Err("the version probe exited non-zero".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_version(text.trim()).ok_or_else(|| format!("answered {text:?}, which is not a version"))
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.split_once('.')?;
    Some((major.trim().parse().ok()?, minor.trim().parse().ok()?))
}

/// One frame from the program.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Frame {
    /// The program is asking for a tool.
    Call {
        name: String,
        args: serde_json::Value,
    },
    /// The program finished. `output` is everything it printed.
    Done { output: String },
    /// The program raised. `output` is everything it printed before it did.
    Failed { message: String, output: String },
}

/// The wire shape the shim writes. Deserialized rather than pattern-matched on
/// `Value` so a malformed frame is one typed failure instead of four `unwrap`s.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum WireFrame {
    Call {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Done {
        #[serde(default)]
        output: String,
    },
    Failed {
        #[serde(default)]
        message: String,
        #[serde(default)]
        output: String,
    },
}

/// A living, contained interpreter and the pipe the protocol runs over.
///
/// The caller drives it: [`Session::next`] until a terminal frame, answering each
/// [`Frame::Call`] with [`Session::reply`]. The loop lives in the dispatch arm
/// rather than here so that the thing a callback re-enters is `dispatch` itself,
/// with the arguments that arm already holds, rather than a second dispatcher
/// this module would have to be given.
pub(crate) struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    calls: usize,
    max_callbacks: usize,
    /// Held for the child's lifetime: dropping it removes the program, the shim
    /// and anything the program wrote beside them.
    _workdir: tempfile::TempDir,
    /// The Landlock rung installs its rule set in the child and the guard owns
    /// it, so it must outlive the spawn. Named with a leading underscore for the
    /// same reason `shell.rs` names its own.
    _contained: Option<crate::sandbox::Contained>,
}

impl Session {
    /// Write the program beside the shim in a fresh workdir and start it.
    pub(crate) async fn start(
        interpreter: &Path,
        source: &str,
        catalogue: &[String],
        max_callbacks: usize,
        containment: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
    ) -> Result<Self> {
        let workdir = crate::sandbox::workdir()?;
        let dir = workdir.path().to_path_buf();
        tokio::fs::write(dir.join(PROGRAM_FILE), source)
            .await
            .map_err(Error::Io)?;
        tokio::fs::write(dir.join("_io_shim.py"), shim(catalogue))
            .await
            .map_err(Error::Io)?;

        // The workdir is the only writable root. The program has no business in
        // the workspace: everything it does to the workspace is a callback that
        // the policy sees, and a program that could edit a file directly would be
        // an act with no `policy_events` row.
        let roots = vec![dir.clone()];
        let argv = vec![
            interpreter.display().to_string(),
            dir.join("_io_shim.py").display().to_string(),
        ];
        // Egress is denied and no proxy is named, whatever the run itself was
        // granted. A program reaches the network only the way anything else does
        // — through this crate's own network-governed tools, as callbacks checked
        // under `Act::Net` — so a child that could open its own socket would be a
        // second route out of a run whose first one is gated. Inheriting
        // `c.config.allow_network` here would have given exactly that to any run
        // whose policy names a host.
        let argv = match containment {
            Some(c) => crate::sandbox::wrap_argv(&c.config, &dir, false, &roots, &argv, None).1,
            None => argv,
        };

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut contained = None;
        if let Some(c) = containment {
            crate::sandbox::apply_rlimits(&mut cmd, &c.config.limits);
            // Same two arguments as `wrap_argv` above, and they have to agree:
            // this installs the rungs an argv wrapper cannot express, so a
            // permissive value here would reopen what the wrapper closed.
            contained =
                crate::sandbox::contain_command(&mut cmd, &c.config, &dir, false, &roots, None);
            // And no `proxy_env` is set, which is the other half of denying
            // egress: a program is given no route out and no address to try.
        }
        // A program that spawns is a tree, and killing the leader has to reach
        // it. `kill_on_drop` covers the child; the group covers what the child
        // started, which is what a callback bound or a timeout has to end.
        #[cfg(unix)]
        crate::sandbox::own_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(Error::Io)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Config("the interpreter gave no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("the interpreter gave no stdout".into()))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            calls: 0,
            max_callbacks,
            _workdir: workdir,
            _contained: contained,
        })
    }

    /// How many callbacks the program has made.
    pub(crate) fn calls(&self) -> usize {
        self.calls
    }

    /// Whether serving one more call would exceed the bound.
    ///
    /// Asked **after** a frame has been read and only when that frame is a call,
    /// never before reading. Asking first meant a program that made exactly its
    /// allowance and then finished was reported as having hit the bound: the
    /// terminal frame it had already written was never read, its output was
    /// thrown away, and the model was told to do less by a run that had done
    /// exactly what it was allowed.
    pub(crate) fn at_bound(&self) -> bool {
        self.calls >= self.max_callbacks
    }

    /// Count a call this crate is about to serve.
    ///
    /// Counted here rather than when the frame is read, so [`Session::calls`] is
    /// the number of acts that actually reached dispatch. A call the bound
    /// refused is not one the program got.
    pub(crate) fn count_call(&mut self) {
        self.calls += 1;
    }

    /// Read the next frame the program wrote.
    ///
    /// A closed pipe with no terminal frame is a program the interpreter killed —
    /// a segfault, an `os._exit`, a cap. It is reported as a failure rather than
    /// as a quiet success, because a program that produced nothing and said
    /// nothing must not read as one that finished.
    pub(crate) async fn next(&mut self) -> Result<Frame> {
        let mut line = String::new();
        // Bounded, because the writer is the untrusted end. A frame longer than
        // this is a program that got past the shim's own truncation, and it is
        // reported rather than buffered: an unbounded `read_line` would have this
        // process hold the whole of it, and then hold it twice more while the
        // report was built, before any result cap saw a byte.
        let read = {
            use tokio::io::AsyncReadExt;
            let mut bounded = (&mut self.stdout).take(MAX_FRAME_BYTES);
            bounded.read_line(&mut line).await.map_err(Error::Io)?
        };
        if read == 0 {
            return Ok(Frame::Failed {
                message: "the interpreter exited without finishing the program".to_string(),
                output: String::new(),
            });
        }
        if !line.ends_with('\n') {
            return Ok(Frame::Failed {
                message: format!(
                    "the program wrote more than {MAX_FRAME_BYTES} bytes in one frame and was \
                     stopped. Print less, or write what you need to a file and read it back."
                ),
                output: String::new(),
            });
        }
        let frame: WireFrame = serde_json::from_str(line.trim()).map_err(|err| {
            Error::Config(format!(
                "the program's shim wrote a frame this crate could not read ({err})"
            ))
        })?;
        Ok(match frame {
            WireFrame::Call { name, args } => Frame::Call { name, args },
            WireFrame::Done { output } => Frame::Done { output },
            WireFrame::Failed { message, output } => Frame::Failed { message, output },
        })
    }

    /// Answer the call the program is waiting on.
    ///
    /// `ok` is the program's own branch: a refusal is a result it can read and
    /// act on, not an exception that hides which act was denied.
    pub(crate) async fn reply(&mut self, ok: bool, text: &str) -> Result<()> {
        let body = serde_json::json!({ "t": "result", "ok": ok, "text": text });
        let mut line = serde_json::to_string(&body)
            .map_err(|err| Error::Config(format!("could not encode a tool result ({err})")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(Error::Io)?;
        self.stdin.flush().await.map_err(Error::Io)
    }

    /// End the program and everything it started.
    ///
    /// The tree is walked **before** the leader is signalled, which is the order
    /// every other kill site in this crate uses and is not cosmetic: killing the
    /// interpreter first orphans its children, and on Windows — where there is no
    /// process group and the walk is a `taskkill /T` on a pid — the walk then has
    /// nothing left to follow and the descendants survive.
    pub(crate) async fn stop(mut self) {
        crate::sandbox::kill_tree_and_group(self.child.id());
        let _ = self.child.start_kill();
        // Awaited before the workdir drops with `self`, so the program is gone
        // before the directory it was writing into is removed.
        let _ = self.child.wait().await;
    }
}

/// The shim the interpreter actually runs.
///
/// It owns the protocol descriptors before the program has run one instruction:
/// the program is handed a captured `stdout`, a devnull file descriptor 1, and an
/// empty `stdin`, so nothing it **prints** — not a plain line, not raw bytes
/// through `os.write`, not a line that is itself a well-formed frame — can reach
/// or forge the pipe this crate is reading.
///
/// That is a claim about printing, and it is deliberately not a claim about
/// isolation. The descriptors are closure variables rather than module globals,
/// so the one-lookup route through `_act.__globals__` is closed, but a program
/// that walks `__closure__` can still find them. It gains nothing: a forged
/// `call` frame is dispatched under the same policy as an honest one, and a
/// forged `done` only ends the program early. A program is untrusted code running
/// under a boundary, not code this crate is trying to sandbox from itself.
fn shim(catalogue: &[String]) -> String {
    // The names are DATA, not generated `def`s. An earlier version wrote one
    // module-level `def {name}` per tool, and `exec` — the widest capability this
    // crate grants — then shadowed the builtin the shim's own epilogue calls to
    // run the program, so no program ran at all. Injecting the bindings into the
    // program's namespace instead means a tool can be called anything without
    // reaching a single name the shim depends on.
    let names = serde_json::to_string(catalogue).unwrap_or_else(|_| "[]".to_string());
    format!(
        "{SHIM_PRELUDE}\n_TOOL_NAMES = {names}\n_OUTPUT_CAP = {SHIM_OUTPUT_CAP}\n\
         _UNSET = object()\n{SHIM_EPILOGUE}"
    )
}

const SHIM_PRELUDE: &str = r#"
import io
import json
import os
import sys
import traceback

# Take the protocol descriptors before the program exists. `sys.stdout` is
# replaced so `print` is captured, and file descriptors 1 and 2 are pointed at
# devnull so that a program reaching past `sys.stdout` with `os.write` cannot
# reach the pipe either. Descriptor 0 goes the same way: a program calling
# `input()` must not be able to eat a tool result.
_proto_out = os.fdopen(os.dup(1), "w", 1)
_proto_in = os.fdopen(os.dup(0), "r")
_null = os.open(os.devnull, os.O_RDWR)
os.dup2(_null, 0)
os.dup2(_null, 1)
os.dup2(_null, 2)

_captured = io.StringIO()
sys.stdout = _captured
sys.stderr = _captured
sys.stdin = io.StringIO()


class Obs(object):
    """One tool result. Truthy when the act was allowed and ran."""

    def __init__(self, ok, text):
        self.ok = ok
        self.text = text

    def __str__(self):
        return self.text

    def __repr__(self):
        return "Obs(ok=%r, text=%r)" % (self.ok, self.text)

    def __bool__(self):
        return self.ok

    __nonzero__ = __bool__


def _protocol(out, inp):
    """Close the two descriptors into `_send` and `_act` and hand them back.

    They are closure variables rather than module globals so that the names are
    gone from `globals()` once this returns. A program is handed `_act`, and a
    module-global `_proto_out` would have been one `_act.__globals__` lookup
    away — the `dup2` of file descriptors 0, 1 and 2 does not protect a
    descriptor the shim itself is holding a live Python object for.

    This closes the reachable route, not every route: `__closure__` still exists
    for a program that goes looking. It gains nothing by it — a forged `call`
    frame is dispatched under the same policy as an honest one, and a forged
    `done` only ends the program early — and the documentation says so rather
    than claiming an isolation this cannot give.
    """

    def send(obj):
        out.write(json.dumps(obj) + "\n")
        out.flush()

    def act(name, kwargs):
        send({"t": "call", "name": name, "args": kwargs})
        line = inp.readline()
        if not line:
            # The harness closed the pipe: a bound was hit or the run was
            # cancelled. Leaving through SystemExit lets the program's own
            # `finally` blocks run and cannot be caught by `except Exception`.
            raise SystemExit("the harness stopped this program")
        reply = json.loads(line)
        return Obs(bool(reply.get("ok")), reply.get("text") or "")

    return send, act


_send, _act = _protocol(_proto_out, _proto_in)
del _proto_out, _proto_in


def _binding(name):
    def call(**kwargs):
        return _act(name, kwargs)

    call.__name__ = name if name.isidentifier() else "tool"
    return call

"#;

const SHIM_EPILOGUE: &str = r#"
def _main():
    here = os.path.dirname(os.path.abspath(__file__))
    with open(os.path.join(here, "program.py")) as handle:
        source = handle.read()
    # The program gets its own namespace holding exactly the tools, `Obs`, and
    # `_act` for a call built by hand. Nothing else this module defines is in
    # there, and — the half that matters — nothing the program is given is in
    # this module. A tool named `exec`, `open` or `compile` is then a name in the
    # program's scope and not a name the shim's own machinery resolves through.
    scope = {"__name__": "__main__", "__builtins__": __builtins__}
    scope["Obs"] = Obs
    scope["_act"] = _act
    for _name in _TOOL_NAMES:
        scope[_name] = _binding(_name)
    exec(compile(source, "program.py", "exec"), scope)
    # A sentinel rather than `is not None`, so a program that deliberately sets
    # `result = None` gets that reported instead of silently nothing.
    return scope["result"] if "result" in scope else _UNSET


def _output(extra=None):
    text = _captured.getvalue()
    if extra is not None:
        if text and not text.endswith("\n"):
            text += "\n"
        text += extra
    # Truncated here, at the source, so the harness never has to hold a frame
    # this crate did not bound. The parent bounds it a second time.
    if len(text) > _OUTPUT_CAP:
        half = _OUTPUT_CAP // 2
        text = text[:half] + "\n… output truncated …\n" + text[-half:]
    return text


try:
    _value = _main()
    _send({"t": "done", "output": _output(None if _value is _UNSET else repr(_value))})
except SystemExit as _exit:
    # An exit code is an outcome, not decoration. `sys.exit(1)` reported as a
    # finish told the model its program had succeeded, and `sys.exit("boom")`
    # swallowed the message entirely — stderr is captured, so nothing printed it.
    _code = _exit.code
    if _code is None or _code == 0:
        _send({"t": "done", "output": _output()})
    else:
        _send({
            "t": "failed",
            "message": "the program exited with %r" % (_code,),
            "output": _output(),
        })
except BaseException:
    _send({
        "t": "failed",
        "message": traceback.format_exc(),
        "output": _output(),
    })
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_two_integers_or_nothing() {
        assert_eq!(parse_version("3.11"), Some((3, 11)));
        assert_eq!(parse_version(" 3.8 "), Some((3, 8)));
        assert_eq!(parse_version("2.7"), Some((2, 7)));
        assert_eq!(parse_version("Python 3.11"), None);
        assert_eq!(parse_version("3"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn the_minimum_rejects_python_2_by_number() {
        assert!(!usable((2, 7)));
        assert!(!usable((3, 7)));
        assert!(usable((3, 8)));
        assert!(usable((3, 13)));
    }

    #[test]
    fn a_missing_interpreter_says_what_it_tried() {
        let missing = Discovery::Missing {
            tried: vec![
                ("python3".into(), "not on PATH".into()),
                ("python".into(), "reported Python 2.7".into()),
            ],
        };
        let text = missing.describe();
        assert!(text.contains("python3: not on PATH"), "{text}");
        assert!(text.contains("python: reported Python 2.7"), "{text}");
        assert!(missing.path().is_none());
    }

    #[test]
    fn the_shim_carries_its_names_as_data_rather_than_as_definitions() {
        let source = shim(&["read_file".to_string(), "exec".to_string()]);
        assert!(
            source.contains(r#"_TOOL_NAMES = ["read_file","exec"]"#),
            "{source}"
        );
        // And nothing else: a name the catalogue did not carry is not callable.
        assert!(!source.contains("write_file"), "{source}");
        // The load-bearing property, asserted rather than trusted: no tool name
        // becomes a definition in the shim's own module, so a tool called `exec`
        // cannot shadow the builtin the epilogue runs the program with.
        assert!(!source.contains("def exec("), "{source}");
        assert!(!source.contains("def read_file("), "{source}");
        assert!(source.contains("exec(compile(source"), "{source}");
    }

    /// The interpreter this host has, or `None` — every round-trip test below
    /// skips rather than fails without one, because a machine with no Python is a
    /// supported machine and a red suite there would be this crate asserting a
    /// property of the host.
    async fn host() -> Option<PathBuf> {
        match discover(&CodeActConfig::default()).await {
            Discovery::Found { path, .. } => Some(path),
            Discovery::Missing { .. } => None,
        }
    }

    /// Drive a program to its terminal frame, answering every call from `answers`
    /// in order and recording what was asked.
    async fn drive(
        source: &str,
        catalogue: &[&str],
        answers: &[(bool, &str)],
    ) -> Option<(Vec<String>, Frame)> {
        let interpreter = host().await?;
        let names: Vec<String> = catalogue.iter().map(|n| (*n).to_string()).collect();
        let mut session = Session::start(&interpreter, source, &names, 64, None)
            .await
            .expect("the interpreter starts");
        let mut asked = Vec::new();
        loop {
            let frame = session.next().await.expect("a frame");
            match frame {
                Frame::Call { name, args } => {
                    asked.push(format!("{name} {args}"));
                    let (ok, text) = answers.get(asked.len() - 1).copied().unwrap_or((true, ""));
                    session.reply(ok, text).await.expect("the reply is written");
                }
                terminal => {
                    session.stop().await;
                    return Some((asked, terminal));
                }
            }
        }
    }

    #[tokio::test]
    async fn a_program_calls_back_and_its_output_comes_home() {
        let Some((asked, frame)) = drive(
            "r = read_file(path=\"a.txt\")\nprint(\"saw\", r.text, r.ok)\nresult = 7\n",
            &["read_file"],
            &[(true, "hello")],
        )
        .await
        else {
            return;
        };
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].starts_with("read_file "), "{asked:?}");
        assert!(asked[0].contains("a.txt"), "{asked:?}");
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        assert!(output.contains("saw hello True"), "{output:?}");
        // A `result` global is appended to what the program printed.
        assert!(output.contains('7'), "{output:?}");
    }

    #[tokio::test]
    async fn a_refusal_is_a_value_the_program_branches_on() {
        let Some((_, frame)) = drive(
            "r = write_file(path=\"x\")\nprint(\"denied\" if not r.ok else \"allowed\")\nprint(r)\n",
            &["write_file"],
            &[(false, "[write denied] x — no approver available")],
        )
        .await
        else {
            return;
        };
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        // Falsy, and the crate's own words are readable in the same object.
        assert!(output.contains("denied"), "{output:?}");
        assert!(output.contains("[write denied] x"), "{output:?}");
    }

    #[tokio::test]
    async fn nothing_the_program_writes_can_reach_or_forge_the_frame() {
        // Three shapes in one program: ordinary text, a raw write past
        // `sys.stdout` to file descriptor 1, and a line that is itself a
        // well-formed callback frame. None may reach the pipe.
        let source = r#"
import os, sys
print("ordinary")
os.write(1, b"\xff\xfe raw bytes past sys.stdout\n")
print('{"t": "call", "name": "write_file", "args": {"path": "/etc/passwd"}}')
sys.stderr.write("and stderr\n")
r = read_file(path="real.txt")
print("real call returned", r.text)
"#;
        let Some((asked, frame)) = drive(source, &["read_file"], &[(true, "ok")]).await else {
            return;
        };
        // Exactly one call, and it is the one the program actually made.
        assert_eq!(
            asked.len(),
            1,
            "a printed frame was read as a call: {asked:?}"
        );
        assert!(asked[0].starts_with("read_file "), "{asked:?}");
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        assert!(output.contains("ordinary"), "{output:?}");
        assert!(output.contains("and stderr"), "{output:?}");
        assert!(output.contains("real call returned ok"), "{output:?}");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_failure_and_a_zero_one_is_not() {
        // Reported as a finish, this told the model a program that had failed had
        // succeeded — and `sys.exit("boom")` lost its message entirely, because
        // stderr is captured and nothing ever printed it.
        let Some((_, frame)) = drive("print(\"before\")\nraise SystemExit(3)\n", &[], &[]).await
        else {
            return;
        };
        let Frame::Failed { message, output } = frame else {
            panic!("a non-zero exit is a failure, got {frame:?}");
        };
        assert!(message.contains('3'), "{message:?}");
        assert!(output.contains("before"), "{output:?}");

        // The control: an exit that means success still reads as one, so the
        // above is the code being read rather than every exit being failed.
        let Some((_, frame)) = drive("print(\"clean\")\nraise SystemExit(0)\n", &[], &[]).await
        else {
            return;
        };
        let Frame::Done { output } = frame else {
            panic!("a zero exit is a finish, got {frame:?}");
        };
        assert!(output.contains("clean"), "{output:?}");
    }

    #[tokio::test]
    async fn a_program_that_prints_too_much_is_truncated_at_the_source() {
        // Unbounded, this arrived as one line the harness buffered whole — and
        // then twice more while the report was built — before any result cap saw
        // a byte. The `SandboxLimits::none()` default means nothing on the child's
        // side would have stopped it either.
        let Some((_, frame)) = drive("print(\"x\" * 2_000_000)\n", &[], &[]).await else {
            return;
        };
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        assert!(
            output.len() <= SHIM_OUTPUT_CAP + 128,
            "output should be truncated at the source; it was {} bytes",
            output.len()
        );
        assert!(output.contains("output truncated"), "the cut is named");
    }

    #[tokio::test]
    async fn a_result_of_none_is_reported_rather_than_swallowed() {
        let Some((_, frame)) = drive("result = None\n", &[], &[]).await else {
            return;
        };
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        assert!(
            output.contains("None"),
            "a deliberate `result = None` is an answer, not an absence: {output:?}"
        );
    }

    #[tokio::test]
    async fn a_program_that_raises_comes_back_with_its_traceback() {
        let Some((_, frame)) =
            drive("print(\"before\")\nraise ValueError(\"nope\")\n", &[], &[]).await
        else {
            return;
        };
        let Frame::Failed { message, output } = frame else {
            panic!("expected a failed program, got {frame:?}");
        };
        assert!(message.contains("ValueError"), "{message:?}");
        assert!(message.contains("nope"), "{message:?}");
        assert!(output.contains("before"), "{output:?}");
    }

    #[tokio::test]
    async fn a_program_cannot_read_the_pipe_through_input() {
        // `input()` on an empty stdin raises rather than eating a tool result.
        let Some((asked, frame)) = drive(
            "try:\n    input()\n    print(\"read something\")\nexcept EOFError:\n    print(\"nothing to read\")\nr = read_file(path=\"a\")\nprint(r.text)\n",
            &["read_file"],
            &[(true, "intact")],
        )
        .await
        else {
            return;
        };
        assert_eq!(asked.len(), 1, "{asked:?}");
        let Frame::Done { output } = frame else {
            panic!("expected a finished program, got {frame:?}");
        };
        assert!(output.contains("nothing to read"), "{output:?}");
        assert!(output.contains("intact"), "{output:?}");
    }

    #[test]
    fn the_shim_takes_the_descriptors_before_the_program_runs() {
        let source = shim(&[]);
        let dup = source.find("os.dup(1)").expect("protocol fd is duplicated");
        let exec = source
            .find("exec(compile(")
            .expect("the program is executed");
        assert!(
            dup < exec,
            "the shim must own stdout before the program runs"
        );
        assert!(source.contains("os.dup2(_null, 1)"), "{source}");
        assert!(source.contains("sys.stdin = io.StringIO()"), "{source}");
    }
}
