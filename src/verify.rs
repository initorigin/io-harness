//! The verification layer: checks that confirm the file meets the spec.
//!
//! Two kinds live here. Deterministic content checks ([`Verification::FileContains`],
//! [`Verification::FileEquals`]) are cheap and cannot lie about what the file
//! *says* — but they cannot confirm the file *works*. The 0.1.0 live run proved
//! this: the model passed `FileContains("fn hello")` by writing the literal
//! string `fn hello`, which does not compile (see
//! `.ultraship/.../iterations/US-IO-HARNESS-0.2.0-I01`).
//!
//! v0.2 added execution-based checks that compile — and optionally test — the
//! produced file with `rustc`. A substring stub fails to compile, so it fails
//! the gate. Compilation happens in a throwaway temp dir that is removed
//! afterwards, and `rustc` touches no network. 0.17.0 generalised that idea into
//! [`Verification::Command`], which runs any project's own command; 0.18.0
//! removed the three Rust-specific variants it replaced, leaving
//! [`Verification::EachCompilesRust`] as the one gate that still spawns `rustc`
//! itself.
//!
//! v0.8.1 closes the converse hole. Until then the file under verification and
//! the caller's criterion were compiled as one crate, so the *subject could
//! defeat its own gate* — shadowing a macro the criterion invoked, or deleting
//! the criterion with a crate-level `#![cfg(any())]`. They are still one crate —
//! that is what lets a criterion call a private `fn hello`, and making them two
//! broke exactly that — but the criterion now sits in a module that re-imports
//! the prelude macros explicitly, so a shadowing subject makes the name ambiguous
//! instead of capturing it, and a probe compiled alongside the subject catches
//! one that deleted its own contents. What a passing gate proves — and what it
//! does not — is spelled out on [`Verification`].

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::observe::{EventKind, RunEvent};
use crate::policy::{Act, Effect, Policy};
use crate::run::{refused, Watch};
use crate::sandbox::{self, RunSpec, Sandbox, SandboxConfig};
use crate::state::{PolicyEvent, SandboxEvent, Store};

/// How the harness decides a task is done.
///
/// # What a passing gate proves
///
/// A pass means **the stated criterion was satisfied** under the harness's
/// compile and run. It is not proof that the artifact is correct, and no
/// execution gate can be. A criterion checks what it checks: a gate asserting
/// `hello() == 42` is silent about everything else the file does.
///
/// The execution gates ([`Verification::Command`] and
/// [`Verification::EachCompilesRust`]) are stronger than the content gates
/// ([`Verification::FileContains`], [`Verification::FileEquals`]) because a stub
/// cannot compile and run — that is why 0.2.0 introduced them — but "stronger"
/// is not "sufficient".
///
/// # What 0.8.1 changed
///
/// Until 0.8.1 the file under verification was compiled into the *same crate* as
/// the caller's criterion, so it could change how that criterion resolved. A file
/// defining `#[macro_export] macro_rules! assert` made
/// `assert!(false, "this gate can never pass")` — which no correct
/// implementation can satisfy — report a pass; a file opening with
/// `#![cfg(any())]` deleted the criterion outright and the empty test binary
/// exited 0. An agent found the first of these unprompted during io-cli 0.1.0's
/// live runs (see `iterations/US-IO-HARNESS-0.8.0-I01`).
///
/// The subject and the criterion are still compiled as **one crate** — that is
/// deliberate and is not the fix. Making the subject a separate crate was tried
/// during 0.8.1 development and abandoned: privacy is a wall between crates, so
/// an ordinary non-`pub` implementation began failing a gate it had always
/// passed, and a passing implementation is allowed to be private. What changed
/// is where the criterion sits inside that crate. It is appended in a child
/// module that opens with `use super::*` — so `test_src` still calls the
/// subject's items unqualified, private ones included — and that re-imports the
/// prelude macros a criterion is likely to invoke *explicitly*. A subject
/// defining `macro_rules! assert` now makes the name ambiguous (rustc E0659) and
/// the gate fails to compile, rather than capturing it and passing an impossible
/// criterion. A macro the subject exports under any other name still reaches the
/// criterion through the glob.
///
/// The deletion attack is caught elsewhere, because one crate cannot catch it:
/// the subject is separately compiled to an rlib with a probe item appended, and
/// a second tiny crate is type-checked against that rlib. A subject that strips
/// its own contents strips the probe too, and the reference fails to resolve.
/// That separate subject compile is *not* what the criterion compiles against —
/// its purposes are classifying an ordinary "this file does not compile" failure
/// and hosting the probe.
///
/// This is a boundary against the file under verification, not against a hostile
/// author with other tools. Verification runs the produced code, so it remains
/// governed by the exec [`Policy`] and the 0.6.0 sandbox.
///
/// # Choosing one
///
/// A criterion is a field of the [`TaskContract`](crate::TaskContract), so the
/// run has a definition of done before the model is asked anything:
///
/// ```
/// use io_harness::{TaskContract, Verification};
/// use std::time::Duration;
///
/// // Execution-based, and what a repository task normally wants: the project's
/// // own suite decides, over the whole crate rather than over a list of files
/// // the caller remembered to name. Each file compiling on its own
/// // (`EachCompilesRust`) would not catch a caller updated out of step with the
/// // function it calls; the repository's own tests do.
/// let contract = TaskContract::workspace(
///     "make `parse` reject an empty input instead of panicking",
///     "/path/to/repo",
/// )
/// .with_verification(Verification::Command {
///     argv: vec!["cargo".into(), "test".into()],
///     expect_exit: 0,
/// })
/// .with_time_budget(Duration::from_secs(600));
///
/// // A pass proves this criterion was satisfied under the harness's compile and
/// // run — nothing wider. `parse` is silent about everything else in the repo,
/// // and a criterion is the only thing the gate can check.
/// // https://github.com/initorigin/io-harness/blob/main/docs/guide/verification.md
/// # let _ = contract;
/// ```
///
/// The content variants are the weak tier and exist for outcomes that genuinely
/// *are* about text. They cannot lie about what a file says and cannot confirm
/// it works — a model satisfied `FileContains("fn hello")` in the 0.1.0 live run
/// by writing that literal string, which does not compile:
///
/// ```
/// use io_harness::Verification;
///
/// # async fn demo() -> io_harness::Result<()> {
/// let cheap = Verification::FileContains("fn hello".into());
/// // Both of these pass. Only one of them is a program.
/// assert!(cheap.passes("src/hello.rs".as_ref(), "pub fn hello() -> u32 { 42 }").await?);
/// assert!(cheap.passes("src/hello.rs".as_ref(), "fn hello").await?);
///
/// // An execution gate fails the second: a substring stub does not type-check.
/// // `Verification::Command { argv: vec!["cargo".into(), "build".into()],
/// // expect_exit: 0 }` is what a Rust project reaches for, and the same shape
/// // with a different argv is what every other language reaches for.
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Verification {
    /// The file's contents must contain this text. Cheap, but gameable — a
    /// model can satisfy it without producing working code.
    FileContains(String),
    /// The file's contents must equal this text exactly.
    FileEquals(String),
    /// Run a caller-supplied command in the workspace's execution sandbox and
    /// require this exit status (0.17.0).
    ///
    /// One variant that covers every language the machine has a toolchain for,
    /// and the reason the crate stopped being a Rust-shaped harness: `cargo
    /// test`, `npm test`, `go test ./...`, `pytest`, `dotnet test`, `make check`
    /// are all the same criterion with a different argv.
    ///
    /// `argv` is an array, program first, and there is no shell — `;`, `&&`,
    /// `$( )` and a backtick are bytes inside one argument, not syntax. `argv[0]`
    /// is what the [`Policy`] is asked about, exactly as `rustc` is on the
    /// Rust-specific gates, and verification cannot prompt, so the spawn happens
    /// only when a rule explicitly allows it.
    ///
    /// ```
    /// use io_harness::{TaskContract, Verification};
    ///
    /// // A JavaScript project. Nothing on this path is Rust-aware.
    /// let contract = TaskContract::workspace(
    ///     "make the failing test in test/parse.test.js pass",
    ///     "/path/to/js-repo",
    /// )
    ///     .with_verification(Verification::Command {
    ///         argv: vec!["npm".into(), "test".into()],
    ///         expect_exit: 0,
    ///     });
    /// # let _ = contract;
    /// ```
    ///
    /// `expect_exit` is a status rather than a bool because "the linter found
    /// nothing" and "the command ran" are different claims, and some tools say
    /// so with a number. A command killed by a signal or by a sandbox cap never
    /// satisfies the criterion, whatever `expect_exit` says: it did not exit.
    ///
    /// Workspace mode only — the command runs with the workspace root as its
    /// working directory, and a single-file contract has no root to give it.
    Command {
        /// The command to run, program first. Passed to the OS as an array; no
        /// shell parses it.
        argv: Vec<String>,
        /// The exit status that means the criterion is satisfied. Usually 0.
        expect_exit: i32,
    },
    /// No gate at all (0.17.0). The run ends when the agent stops calling tools.
    ///
    /// This is what makes an open-ended task expressible. Until 0.17.0
    /// [`TaskContract`](crate::TaskContract) took a `Verification` by value and
    /// four of the five variants ran `rustc`, so "debug this production issue" or
    /// "work out why the deploy fails" could not be *stated*, let alone run —
    /// there was no criterion to name and no way to say there was none.
    ///
    /// An assistant turn carrying no tool call ends the run with
    /// [`RunOutcome::Finished`](crate::RunOutcome::Finished), which is a distinct
    /// outcome from a step cap, a stall and a budget stop — so an unattended run
    /// that simply finished is never later mistaken for one that ran out. No
    /// `done` tool is added: an unverified run gains no tool surface over a
    /// verified one, and a model that has nothing left to do says so by saying
    /// something.
    ///
    /// It is also what [`TaskContract::workspace`](crate::TaskContract::workspace)
    /// starts at, so an open-ended task is now the shape a caller falls into rather
    /// than one they have to name; a task that does have a criterion asks for it
    /// with
    /// [`with_verification`](crate::TaskContract::with_verification).
    ///
    /// ```
    /// use io_harness::{RunOutcome, TaskContract, Verification};
    ///
    /// let contract = TaskContract::workspace(
    ///     "work out why the nightly deploy has been failing and write up what you find",
    ///     "/path/to/repo",
    /// );
    /// assert!(matches!(contract.verify, Verification::None));
    ///
    /// // What "it worked" means for a run with no criterion: it finished on its
    /// // own terms rather than hitting a ceiling. Nothing here claims the work is
    /// // *correct* — with no gate, nothing could.
    /// fn done(outcome: &RunOutcome) -> bool {
    ///     matches!(outcome, RunOutcome::Finished { .. })
    /// }
    /// # let _ = (contract, done);
    /// ```
    ///
    /// What you give up is what a gate was ever worth: nothing checked the work.
    /// Reach for [`Verification::Command`] whenever the task *has* a checkable
    /// criterion — this variant is for the tasks that genuinely do not.
    None,
    /// (workspace/multi-file) A named file under the workspace root must contain
    /// this text. Deterministic and language-agnostic — no compilation — so a
    /// task whose success is "a file now holds X" can be verified directly. Like
    /// [`Verification::FileContains`] it is gameable; use it when the outcome is
    /// genuinely about content, or as a composed-tree checkpoint a parent reads.
    WorkspaceFileContains {
        /// File to read, relative to the workspace root.
        file: PathBuf,
        /// Text that must be present in it.
        needle: String,
    },
    /// (workspace/multi-file, 0.14.0) A document under the workspace root must
    /// contain this text **once its text has been extracted** — not in its raw
    /// bytes.
    ///
    /// The distinction is the whole variant. A `.docx`, `.xlsx` and `.pptx` are
    /// zips and a `.pdf` is a compressed object graph, so none of them is UTF-8.
    /// [`Verification::WorkspaceFileContains`] reads with
    /// `read_to_string(..).unwrap_or_default()`, which on a document yields the
    /// empty string — so it reports "does not contain" for every document,
    /// including one whose visible text plainly does contain the needle. It does
    /// not fail loudly; it silently always fails. A criterion that can never pass
    /// is worse than no criterion, because a run that ends `StepCapReached` looks
    /// like an agent that could not do the work rather than a gate that was never
    /// able to say yes.
    ///
    /// The reader is chosen by the file's extension: `.xlsx`, `.docx`, `.pptx`,
    /// `.pdf`. Anything else is an error rather than a fallback to reading the
    /// bytes as text — a criterion that silently degrades into a weaker check is
    /// the failure mode this exists to remove.
    ///
    /// The variant exists in every build; only its implementation is behind the
    /// document features. A build without them returns a typed
    /// [`Error::Config`](crate::Error::Config) saying so, rather than the variant
    /// vanishing and every match arm growing a `cfg`. Loud beats absent, which is
    /// the same call single-file mode makes when handed a policy it cannot
    /// enforce.
    ///
    /// Like the other content criteria this is gameable and does not prove the
    /// document is *correct* — see the
    /// [type-level docs](Verification#what-a-passing-gate-proves).
    DocumentContains {
        /// Document to read, relative to the workspace root.
        file: PathBuf,
        /// Text that must be present in its extracted text.
        needle: String,
    },
    /// (workspace/multi-file) Every listed file — relative to the workspace root
    /// — must compile on its own as a Rust library. The run only succeeds when
    /// all of them do, so one wrong file fails the whole set.
    ///
    /// Hardened in 0.8.1: each file goes through the same probe-backed compile,
    /// so no listed file can pass by deleting its own contents.
    EachCompilesRust(Vec<PathBuf>),
    /// (workspace/multi-file, 0.34.0) A **second model** reads what the run wrote
    /// and decides whether it satisfies `rubric`.
    ///
    /// The first criterion in this crate whose check is a judgement rather than
    /// an exit status or a substring. Every other variant is a fact: the command
    /// exited 0, the file holds the text, the crate compiled. That is the right
    /// default and it cannot catch the change that compiles, passes the suite and
    /// is still the wrong change — which is the failure mode of an agent
    /// optimising against a gate rather than against the goal.
    ///
    /// It is resolved by the run loop, not by [`Verification::passes_in`]: a
    /// review needs a provider, and `passes_in` has none. The run reads
    /// [`TaskContract::reviewer`](crate::TaskContract) and calls it; a contract
    /// carrying this criterion with no reviewer registered fails with
    /// [`Error::Config`](crate::Error::Config) at run start rather than at the
    /// gate, so the mistake costs nothing.
    ///
    /// ```
    /// use io_harness::{TaskContract, Verification};
    ///
    /// let contract = TaskContract::workspace("tidy the parser", "/repo")
    ///     .with_verification(Verification::Review {
    ///         rubric: "every public item changed still has a doc comment".into(),
    ///         allow_self_review: false,
    ///     });
    /// # let _ = contract;
    /// ```
    ///
    /// `allow_self_review` is `false` in every sensible case. With it false, a
    /// [`ModelReviewer`] whose model is the model that produced the change is
    /// refused **before a request is built** — a model grading its own answer
    /// reports what the run already believes. It is a field rather than an
    /// unconditional rule so the exception is visible in the caller's own code:
    /// a smoke test against one scripted provider is the honest use for `true`.
    Review {
        /// What the reviewing model is asked to decide, in the caller's words.
        rubric: String,
        /// Permit the reviewing model to be the model that wrote the change.
        allow_self_review: bool,
    },
}

/// What a [`Reviewer`] is handed: the goal, the rubric, and what the run wrote.
///
/// Deliberately **not** the run's conversation. A reviewer reading the author's
/// own reasoning is a reviewer being led, and the point of the criterion is a
/// judgement formed from the work rather than from the argument for it. The cost
/// is stated in `docs/CONTRACT.md`: a change whose justification lived only in
/// the transcript is judged without it.
///
/// ```
/// use io_harness::ReviewRequest;
///
/// let request = ReviewRequest {
///     goal: "add a parser for the config file".into(),
///     rubric: "errors carry the line number".into(),
///     files: vec![("src/parse.rs".into(), "pub fn parse() {}".into())],
/// };
/// assert_eq!(request.files.len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    /// The contract's goal, so the reviewer knows what was asked for.
    pub goal: String,
    /// The criterion's rubric, verbatim.
    pub rubric: String,
    /// Every file the run wrote, relative to the workspace root, with its
    /// current contents.
    pub files: Vec<(PathBuf, String)>,
}

/// What one file looked like before the run first wrote it, and what it holds now
/// (0.42.0).
///
/// The three cases are the three the store already keeps, and they are kept apart
/// because they read differently: a file that existed has a `before`, a file the
/// run *created* has none, and a file whose previous contents were too large or
/// not text says so in `unkept` rather than pretending to have been empty. A
/// reviewer told a rewritten file was empty would read every line as an addition.
///
/// ```
/// use io_harness::FileChange;
///
/// let edited = FileChange::new("src/parse.rs", "pub fn parse() {}\n")
///     .with_before("/// Parses one line.\npub fn parse() {}\n");
/// // What a "what the run wrote" view cannot show: the line that is gone.
/// assert!(edited.before.as_deref().unwrap().contains("Parses one line"));
/// assert!(!edited.after.contains("Parses one line"));
///
/// // A file the run created carries no before at all.
/// let created = FileChange::new("src/new.rs", "pub fn new() {}\n");
/// assert!(created.before.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChange {
    /// The path, relative to the workspace root.
    pub path: PathBuf,
    /// What the file held before the run first wrote it, or `None` when the run
    /// created it.
    pub before: Option<String>,
    /// What it holds now.
    pub after: String,
    /// Why `before` is absent for a file that did exist — over the store's
    /// snapshot cap, or not text. `None` when `before` is the whole answer.
    pub unkept: Option<String>,
}

/// What a [`Reviewer`] is handed when it wants the change rather than the outcome
/// (0.42.0).
///
/// The same goal and the same rubric a [`ReviewRequest`] carries, and in place of
/// "every file the run wrote, as it stands" the before and after of each one. A
/// rubric about what a change *did* — nothing lost its doc comment, no public
/// item was removed, the new code has a test — is answerable from this and is not
/// answerable from the outcome, because what was deleted is not in the text that
/// remains.
///
/// ```
/// use io_harness::{ChangeReview, FileChange};
///
/// let review = ChangeReview::new(
///     "tidy the parser",
///     "no public item lost its doc comment",
///     vec![FileChange::new("src/parse.rs", "pub fn parse() {}\n")
///         .with_before("/// Parses one line.\npub fn parse() {}\n")],
/// );
///
/// // The same request, seen the way a reviewer written before 0.42.0 sees it.
/// let outcome = review.into_outcome_request();
/// assert_eq!(outcome.files.len(), 1);
/// assert_eq!(outcome.files[0].1, "pub fn parse() {}\n");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangeReview {
    /// The contract's goal, so the reviewer knows what was asked for.
    pub goal: String,
    /// The criterion's rubric, verbatim.
    pub rubric: String,
    /// Every file the run wrote, before and after, in the order it first touched
    /// them.
    pub changes: Vec<FileChange>,
}

impl FileChange {
    /// A file the run created, holding `after`.
    ///
    /// The before is added with [`Self::with_before`] when the file existed, and
    /// [`Self::not_kept`] when it existed and its contents were not kept.
    pub fn new(path: impl Into<PathBuf>, after: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            before: None,
            after: after.into(),
            unkept: None,
        }
    }

    /// What the file held before the run first wrote it.
    pub fn with_before(mut self, before: impl Into<String>) -> Self {
        self.before = Some(before.into());
        self
    }

    /// Why there is no before for a file that did exist.
    pub fn not_kept(mut self, why: impl Into<String>) -> Self {
        self.unkept = Some(why.into());
        self
    }
}

impl ChangeReview {
    /// A review of `changes`, against `rubric`, for a run pursuing `goal`.
    pub fn new(
        goal: impl Into<String>,
        rubric: impl Into<String>,
        changes: Vec<FileChange>,
    ) -> Self {
        Self {
            goal: goal.into(),
            rubric: rubric.into(),
            changes,
        }
    }

    /// The same review as a [`ReviewRequest`] — the files as they stand.
    ///
    /// What the default [`Reviewer::review_change`] forwards, so a reviewer
    /// written before 0.42.0 receives exactly what it received then.
    pub fn into_outcome_request(self) -> ReviewRequest {
        ReviewRequest {
            goal: self.goal,
            rubric: self.rubric,
            files: self
                .changes
                .into_iter()
                .map(|c| (c.path, c.after))
                .collect(),
        }
    }
}

/// One reviewer's verdict, with the reasons it gave for it.
///
/// `reasons` is not decoration: a refusal a human cannot argue with is a gate
/// nobody will trust twice, and the reasons are what reach the trace and the
/// [`Observer`](crate::Observer) through
/// [`EventKind::Reviewed`](crate::EventKind).
///
/// ```
/// use io_harness::Review;
///
/// let verdict = Review::failed(["`parse` returns `()` on a malformed line"]);
/// assert!(!verdict.passed);
/// assert_eq!(verdict.reasons.len(), 1);
/// assert!(Review::passed().reasons.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Review {
    /// Whether the work satisfies the rubric.
    pub passed: bool,
    /// Why, in the reviewer's own words. Empty is permitted for a pass and is
    /// the reason a fail should never be.
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl Review {
    /// A pass with nothing further to say.
    #[must_use]
    pub fn passed() -> Self {
        Self {
            passed: true,
            reasons: Vec::new(),
        }
    }

    /// A refusal, with the reasons for it.
    #[must_use]
    pub fn failed<I, S>(reasons: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            passed: false,
            reasons: reasons.into_iter().map(Into::into).collect(),
        }
    }
}

/// The future a [`Reviewer`] returns.
///
/// Boxed for the same reason [`PlanReview`](crate::PlanReview) is: a reviewer is
/// held behind a `dyn` on the contract, and a trait returning `impl Future` is
/// not dyn-compatible. [`Provider`](crate::Provider) can afford RPITIT because it
/// is always a generic parameter; a reviewer is a field.
///
/// ```
/// use io_harness::{Review, ReviewRequest, Reviewer, Reviewing};
///
/// #[derive(Debug)]
/// struct Human;
///
/// impl Reviewer for Human {
///     // The return type is this alias, which is what makes `dyn Reviewer` work.
///     fn review<'a>(&'a self, _request: ReviewRequest) -> Reviewing<'a> {
///         Box::pin(async { Ok(Review::failed(["I would like a test for it"])) })
///     }
///     fn model(&self) -> Option<&str> { None }
/// }
/// # let _ = Human;
/// ```
pub type Reviewing<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Review>> + Send + 'a>>;

/// Who answers a [`Verification::Review`] criterion.
///
/// A trait rather than a concrete type so the reviewer can be a second model
/// ([`ModelReviewer`]), a human at a terminal, a second harness, or a stub in a
/// test — without this crate growing a second provider abstraction to describe
/// any of them.
///
/// ```
/// use io_harness::{Review, ReviewRequest, Reviewer, Reviewing};
///
/// /// The reviewer a test uses: it says yes, and says so out loud.
/// #[derive(Debug)]
/// struct AlwaysPasses;
///
/// impl Reviewer for AlwaysPasses {
///     fn review<'a>(&'a self, _request: ReviewRequest) -> Reviewing<'a> {
///         Box::pin(async { Ok(Review::passed()) })
///     }
///     fn model(&self) -> Option<&str> { None }
/// }
///
/// let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// let request = ReviewRequest { goal: "g".into(), rubric: "r".into(), files: vec![] };
/// assert!(rt.block_on(AlwaysPasses.review(request)).unwrap().passed);
/// ```
pub trait Reviewer: Send + Sync + std::fmt::Debug {
    /// Judge the work against the rubric.
    ///
    /// An `Err` here is a review that did not *happen* — a transport failure, a
    /// verdict that could not be parsed — and is recorded as
    /// [`GateOutcome::Errored`](crate::GateOutcome), which
    /// [`retry_gate`](crate::retry_gate) can retry. A review that happened and
    /// said no is `Ok(Review { passed: false, .. })` and is not retryable,
    /// because nothing about the work has changed.
    fn review<'a>(&'a self, request: ReviewRequest) -> Reviewing<'a>;

    /// Judge the change rather than what it left behind (0.42.0).
    ///
    /// This is what the run loop calls. The default hands [`Self::review`] the
    /// same [`ReviewRequest`] it has always received, so a reviewer written
    /// before 0.42.0 needs no edit — and, stated plainly because it is the price
    /// of that: such a reviewer sees the outcome and not the change. Overriding
    /// this is how a reviewer sees what was removed.
    ///
    /// ```
    /// use io_harness::{ChangeReview, Review, Reviewer, ReviewRequest, Reviewing};
    ///
    /// #[derive(Debug)]
    /// struct NothingWasDeleted;
    ///
    /// impl Reviewer for NothingWasDeleted {
    ///     fn review<'a>(&'a self, _: ReviewRequest) -> Reviewing<'a> {
    ///         // Unanswerable from the outcome alone, so it says so.
    ///         Box::pin(async { Ok(Review::failed(["this rubric needs the change"])) })
    ///     }
    ///
    ///     fn review_change<'a>(&'a self, request: ChangeReview) -> Reviewing<'a> {
    ///         let shrank = request.changes.iter().any(|c| {
    ///             c.before.as_ref().is_some_and(|b| b.len() > c.after.len())
    ///         });
    ///         Box::pin(async move {
    ///             Ok(if shrank {
    ///                 Review::failed(["a file lost content"])
    ///             } else {
    ///                 Review::passed()
    ///             })
    ///         })
    ///     }
    ///
    ///     fn model(&self) -> Option<&str> {
    ///         None
    ///     }
    /// }
    /// ```
    fn review_change<'a>(&'a self, request: ChangeReview) -> Reviewing<'a> {
        self.review(request.into_outcome_request())
    }

    /// The model this reviewer will ask, when it is a model at all.
    ///
    /// `None` means the question does not apply — a human, a stub, a second
    /// harness — and the self-review refusal has nothing to compare, so it does
    /// not fire. A [`ModelReviewer`] returns the model it was built with, which
    /// is what makes the refusal possible before a request is built.
    fn model(&self) -> Option<&str>;
}

/// A [`Reviewer`] that asks a model.
///
/// It holds its **own** provider and its own model name, which is the whole
/// design: reusing the run's provider is the cheapest implementation available
/// and it is exactly the mistake the criterion exists to prevent.
///
/// ```
/// # use io_harness::{ModelReviewer, Reviewer};
/// # fn demo<P: io_harness::Provider + std::fmt::Debug + Send + Sync>(provider: P) {
/// let reviewer = ModelReviewer::new(provider, "a-different-model");
/// assert_eq!(reviewer.model(), Some("a-different-model"));
/// # }
/// ```
#[derive(Debug)]
pub struct ModelReviewer<P> {
    provider: P,
    model: String,
}

impl<P> ModelReviewer<P> {
    /// Review with `provider`, asking for `model`.
    pub fn new(provider: P, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

/// What the reviewing model is told it is doing, and the shape its answer must
/// take.
///
/// The verdict is JSON because a bool has to be read out of it by a program. The
/// parse is deliberately forgiving about *surroundings* — a model that wraps the
/// object in prose or a fenced block is still answering — and unforgiving about
/// the object itself: a response with no parsable verdict is an
/// [`Error`](crate::Error), which makes the gate `Errored` rather than failing
/// work that was never judged.
const REVIEW_SYSTEM: &str = "\
You are reviewing work another model produced. You did not write it.

Decide one thing: does the work satisfy the rubric? Answer with a single JSON \
object and nothing else:

{\"passed\": true}
{\"passed\": false, \"reasons\": [\"...\", \"...\"]}

Give a reason for every refusal. Judge the work in front of you against the \
rubric, not against what you would have written.";

impl<P: crate::provider::Provider + std::fmt::Debug + Send + Sync> ModelReviewer<P> {
    /// Ask, and read the verdict out of the answer.
    async fn judge(&self, user: String) -> Result<Review> {
        let response = self
            .provider
            .complete(crate::provider::CompletionRequest {
                system: REVIEW_SYSTEM.to_string(),
                user,
                model: Some(self.model.clone()),
                ..Default::default()
            })
            .await?;
        parse_verdict(response.text.as_deref().unwrap_or_default())
    }
}

impl<P: crate::provider::Provider + std::fmt::Debug + Send + Sync> Reviewer for ModelReviewer<P> {
    fn review<'a>(&'a self, request: ReviewRequest) -> Reviewing<'a> {
        Box::pin(async move {
            let mut user = format!(
                "# Goal\n{}\n\n# Rubric\n{}\n\n# What the run wrote\n",
                request.goal, request.rubric
            );
            if request.files.is_empty() {
                user.push_str("(nothing was written)\n");
            }
            for (path, contents) in &request.files {
                user.push_str(&format!(
                    "\n## {}\n```\n{contents}\n```\n",
                    path.to_string_lossy()
                ));
            }
            self.judge(user).await
        })
    }

    /// The change, rendered before-and-after per file.
    ///
    /// Not a unified diff: computing one is 0.51.0's work, where the hunks are
    /// stored and a patch tool needs them. What a model needs to answer "did this
    /// change lose something" is both texts, and both texts is what the store
    /// already holds.
    fn review_change<'a>(&'a self, request: ChangeReview) -> Reviewing<'a> {
        Box::pin(async move {
            let mut user = format!(
                "# Goal\n{}\n\n# Rubric\n{}\n\n# What the run changed\n",
                request.goal, request.rubric
            );
            if request.changes.is_empty() {
                user.push_str("(nothing was changed)\n");
            }
            for change in &request.changes {
                let path = change.path.to_string_lossy();
                match (&change.before, &change.unkept) {
                    (Some(before), _) => user.push_str(&format!(
                        "\n## {path}\n\n### before\n```\n{before}\n```\n\n### after\n```\n{}\n```\n",
                        change.after
                    )),
                    (None, Some(why)) => user.push_str(&format!(
                        "\n## {path}\n\n### before\n(not kept: {why})\n\n### after\n```\n{}\n```\n",
                        change.after
                    )),
                    (None, None) => user.push_str(&format!(
                        "\n## {path}\n\n### before\n(this file did not exist)\n\n### after\n```\n{}\n```\n",
                        change.after
                    )),
                }
            }
            self.judge(user).await
        })
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model)
    }
}

/// The first balanced JSON object in a model's answer, braces included.
///
/// One scanner, used by both things in this crate that read a verdict out of
/// prose — the review gate here and [`ModelApprover`](crate::ModelApprover) — so
/// "a model that wraps its object in a fenced block has still answered" is one
/// rule rather than two that can drift. Strings are tracked so a brace inside one
/// does not close the object, which is exactly what a denial reason containing
/// `}` would otherwise do.
///
/// `None` means there was no balanced object at all. What that *means* is the
/// caller's to decide, and the two callers decide differently: an unreadable
/// review is an error, an unreadable approval is a defer.
pub(crate) fn first_json_object(text: &str) -> Option<&str> {
    json_object_from(text, 0).map(|(start, end)| &text[start..=end])
}

/// The first balanced object at or after `from`, as a byte range.
///
/// Separate from [`first_json_object`] because [`parse_verdict`] must try each
/// candidate in turn — a model that writes `{ "note": "…" } {"passed": true}` has
/// answered in its second object, and stopping at the first would refuse a
/// verdict that is right there.
fn json_object_from(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    for (start, _) in text.char_indices().filter(|&(i, c)| i >= from && c == '{') {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (end, byte) in bytes.iter().enumerate().skip(start) {
            let c = *byte as char;
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((start, end));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Read a verdict out of a model's answer.
///
/// Scans for the first balanced JSON object rather than requiring the whole
/// response to be one: a model that says "Looks good to me. {...}" has answered,
/// and refusing it would turn a judgement into a transport error. A response with
/// no object at all, or one that does not carry `passed`, is an error — a verdict
/// nobody can read is a review that did not happen.
fn parse_verdict(text: &str) -> Result<Review> {
    let mut at = 0;
    while let Some((start, end)) = json_object_from(text, at) {
        if let Ok(review) = serde_json::from_str::<Review>(&text[start..=end]) {
            return Ok(review);
        }
        at = start + 1;
    }
    Err(Error::provider(
        crate::error::ProviderErrorKind::Malformed,
        format!(
            "the reviewing model returned no readable verdict: {}",
            text.chars().take(200).collect::<String>()
        ),
    ))
}

/// What the verification layer is allowed to spawn, and where to record it.
///
/// Verification cannot prompt — there is no approver on this path — so a
/// command is spawned only when the policy explicitly *allows* it. Anything
/// else, including [`Effect::Ask`], is refused.
///
/// A run builds one of these for you. Build it yourself to check a criterion
/// outside a run — a CI step re-verifying what an agent produced, say — under
/// the boundary the agent itself worked under:
///
/// ```
/// use io_harness::{ExecGuard, Policy, Verification, TEST_BINARY};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // Compile-only: `rustc` may run, the produced test binary may not. A gate
/// // that type-checks the criterion against the code and never executes it is
/// // what you want when the code came from a model and this host is not a
/// // sandbox you are willing to lose.
/// let policy = Policy::permissive()
///     .layer("verify")
///     .allow_exec("rustc")
///     .deny_exec(TEST_BINARY);
///
/// // The same boundary refuses a criterion that wants to run something else —
/// // `cargo` is not `rustc`, and this policy allowed one program by name.
/// let criterion = Verification::Command {
///     argv: vec!["cargo".into(), "test".into()],
///     expect_exit: 0,
/// };
/// // An allow the policy does not give is `Error::Refused`, not a silent skip —
/// // a verification that was refused is not one that ran and failed.
/// let outcome = criterion
///     .passes_in_guarded("/path/to/repo".as_ref(), &ExecGuard::new(&policy))
///     .await;
/// assert!(matches!(outcome, Err(io_harness::Error::Refused { .. })));
/// # Ok(()) }
/// ```
///
/// [`ExecGuard::tracing`] additionally writes every spawn's full argv against a
/// run id, and [`ExecGuard::no_sandbox`] opts back to direct host execution —
/// the sandbox is the default.
pub struct ExecGuard<'a> {
    policy: &'a Policy,
    trace: Option<(&'a Store, i64, u32)>,
    /// Where to announce what the trace records, and the depth to announce it
    /// at. Separate from `trace` because a caller may attach a store without an
    /// observer; every event below is written inside the `trace` block anyway,
    /// so an event never reports something no row does.
    watch: Option<(&'a Watch<'a>, u32)>,
    /// How to sandbox the spawn. `Some` (the default) runs the compile inside an
    /// ephemeral sandbox — the 0.6.0 default; `None` opts back to direct host
    /// execution, the exact 0.5.0 behaviour.
    sandbox: Option<SandboxConfig>,
}

impl<'a> ExecGuard<'a> {
    /// Guard spawns with `policy`, recording nothing. Sandboxed by default.
    pub fn new(policy: &'a Policy) -> Self {
        Self {
            policy,
            trace: None,
            watch: None,
            sandbox: Some(SandboxConfig::default()),
        }
    }

    /// Also record every spawn's full argv against `run_id` at `step`, so
    /// argument-level enforcement can be added later against a real baseline.
    pub fn tracing(mut self, store: &'a Store, run_id: i64, step: u32) -> Self {
        self.trace = Some((store, run_id, step));
        self
    }

    /// Also announce what it records to `watch`, at `depth` in the agent tree.
    /// Crate-internal: an observer reaches the gate through the run, not through
    /// a guard an embedder built by hand.
    pub(crate) fn watching(mut self, watch: &'a Watch<'a>, depth: u32) -> Self {
        self.watch = Some((watch, depth));
        self
    }

    /// Run the compile inside `config`'s sandbox instead of the default one.
    pub fn sandboxed(mut self, config: SandboxConfig) -> Self {
        self.sandbox = Some(config);
        self
    }

    /// Opt out of the sandbox: run the compile directly on the host, exactly as
    /// 0.5.0 did. Additive and reversible — the sandbox is the default, not a
    /// forced change.
    pub fn no_sandbox(mut self) -> Self {
        self.sandbox = None;
        self
    }

    /// Allow nothing beyond what a permissive policy permits (the 0.3.0 path).
    /// Sandboxed by default, like [`ExecGuard::new`].
    fn permissive() -> ExecGuard<'static> {
        static PERMISSIVE: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
        ExecGuard {
            policy: PERMISSIVE.get_or_init(Policy::permissive),
            trace: None,
            watch: None,
            sandbox: Some(SandboxConfig::default()),
        }
    }

    /// Check one spawn, recording its argv. Refuses unless explicitly allowed.
    fn check(&self, program: &str, argv: &[String]) -> Result<()> {
        let verdict = self.policy.check(Act::Exec, program);
        let full = format!("{program} {}", argv.join(" "));
        if let Some((store, run_id, step)) = self.trace {
            let mut ev = if verdict.effect == Effect::Allow {
                PolicyEvent::decision(step, "exec", &full, "allow", "policy")
            } else {
                PolicyEvent::refusal(step, "exec", &full)
            };
            ev.rule = verdict.rule.clone();
            ev.layer = verdict.layer.clone();
            let _ = store.record_event(run_id, &ev);
            if verdict.effect != Effect::Allow {
                if let Some((watch, depth)) = self.watch {
                    refused(watch, run_id, depth, &ev);
                }
            }
        }
        if verdict.effect == Effect::Allow {
            Ok(())
        } else {
            Err(Error::Refused {
                act: "exec".into(),
                target: program.to_string(),
                rule: verdict.rule,
                layer: verdict.layer,
            })
        }
    }

    /// Record which phase of an execution gate failed, when a store is attached.
    /// See [`crate::state::SandboxEvent::gate_phase_failed`] — this is what lets
    /// an operator tell a criterion that could not compile against the subject
    /// (the shape a pre-0.8.1 bypass takes) from a test that ran and failed.
    fn record_gate_failure(&self, phase: &str) {
        if let Some((store, run_id, step)) = self.trace {
            self.sandboxed_event(store, &SandboxEvent::gate_phase_failed(run_id, step, phase));
        }
    }

    /// Write one sandbox row and announce it, from the same value, so the event
    /// cannot name a kind or a backend the `sandbox_events` row does not.
    ///
    /// The write stays `let _ =`: a trace failure must not fail the gate, and
    /// telling an observer about a row that failed to land is better than a run
    /// that dies because its audit trail did.
    fn sandboxed_event(&self, store: &Store, e: &SandboxEvent) {
        let _ = store.record_sandbox_event(e);
        if let Some((watch, depth)) = self.watch {
            watch.emit(RunEvent::at_depth(
                e.run_id,
                e.step,
                depth,
                EventKind::Sandbox {
                    kind: e.kind.clone(),
                    backend: e.backend.clone(),
                },
            ));
        }
    }

    /// Record what a failing gate command printed, bounded.
    ///
    /// `Ok(false)` on its own says a criterion did not pass and nothing about
    /// why, and the two causes need opposite responses: the agent's work being
    /// wrong is a run to resume, and the test runner not being installed is a
    /// machine to fix. Bounded because a build log is unbounded and this is a
    /// trace row, and truncated from the tail, which is where a test runner puts
    /// the failure.
    fn record_gate_output(&self, output: &str) {
        if output.trim().is_empty() {
            return;
        }
        if let Some((store, run_id, step)) = self.trace {
            let (bounded, _) =
                crate::tools::exec::head_and_tail(output.trim(), GATE_OUTPUT_TRACE_CHARS);
            self.sandboxed_event(store, &SandboxEvent::gate_output(run_id, step, &bounded));
        }
    }

    /// Execute an already-policy-checked `argv` in `workdir`, returning whether
    /// it succeeded. Routes through the sandbox when one is configured (the
    /// 0.6.0 default) — so model-produced code never runs on the host directly —
    /// and falls back to a direct spawn when the sandbox is opted out (0.5.0).
    async fn exec(&self, argv: &[String], workdir: &Path) -> Result<bool> {
        Ok(self.exec_output(argv, workdir).await?.exit == Some(0))
    }

    /// [`ExecGuard::exec`], keeping the exit status and the output rather than
    /// reducing both to "did it work".
    ///
    /// The one execution path, which is why 0.17.0 put the detail here rather
    /// than beside it: [`Verification::Command`] needs a *specific* exit status
    /// and needs the command's own output for the trace, and the Rust-specific
    /// gates need neither. A second spawn site for the second requirement would
    /// be two places where the sandbox decision, the policy trace and the
    /// lifecycle events could drift apart.
    async fn exec_output(&self, argv: &[String], workdir: &Path) -> Result<GateRun> {
        match &self.sandbox {
            Some(cfg) => {
                let sb = sandbox::select(cfg);
                let backend = sb.backend();
                // Record the sandbox lifecycle so an audit shows where code ran.
                if let Some((store, run_id, step)) = self.trace {
                    self.sandboxed_event(
                        store,
                        &SandboxEvent::create(run_id, step, backend.as_str()),
                    );
                    self.sandboxed_event(
                        store,
                        &SandboxEvent::exec(run_id, step, backend.as_str(), &argv.join(" ")),
                    );
                }
                let outcome = sb
                    .run(RunSpec {
                        argv,
                        workdir,
                        limits: &cfg.limits,
                        allow_network: cfg.allow_network,
                    })
                    .await?;
                if let Some((store, run_id, step)) = self.trace {
                    if let Some(cap) = outcome.cap_hit {
                        self.sandboxed_event(
                            store,
                            &SandboxEvent::cap_hit(run_id, step, cap.as_str()),
                        );
                    }
                    // The workdir is torn down when this call returns (tempdir
                    // drop in the caller); record the destroy now.
                    self.sandboxed_event(store, &SandboxEvent::destroy(run_id, step));
                }
                // A cap hit is a real failure of the gate, not a pass.
                if !outcome.success() {
                    // Do not throw away what the command said about its own
                    // failure. `Ok(false)` on its own reads as "the model's code
                    // is wrong" whatever the real cause was; the compiler's own
                    // diagnostics are what tell the two apart, so keep them
                    // where the next diagnosis can read them.
                    tracing::debug!(
                        backend = backend.as_str(),
                        exit_code = ?outcome.exit_code,
                        cap_hit = ?outcome.cap_hit.map(|c| c.as_str()),
                        stderr = %outcome.stderr.trim(),
                        "sandboxed command failed"
                    );
                }
                Ok(GateRun {
                    // A cap hit is a real failure of the gate, not a pass, and
                    // the exit code a killed process reports is not one it chose
                    // — so it reports as no exit at all, which no `expect_exit`
                    // can match.
                    exit: if outcome.cap_hit.is_some() {
                        None
                    } else {
                        outcome.exit_code
                    },
                    output: joined_streams(&outcome.stdout, &outcome.stderr),
                })
            }
            None => {
                // Direct host execution — the exact 0.5.0 path.
                let out = Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(workdir)
                    .stdin(Stdio::null())
                    .output()
                    .await?;
                if !out.status.success() {
                    tracing::debug!(
                        exit_code = ?out.status.code(),
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "host command failed"
                    );
                }
                Ok(GateRun {
                    exit: out.status.code(),
                    output: joined_streams(
                        &String::from_utf8_lossy(&out.stdout),
                        &String::from_utf8_lossy(&out.stderr),
                    ),
                })
            }
        }
    }
}

/// One gate command's result: how it exited, and what it said.
///
/// `exit` is `None` when the command did not exit on its own terms — a signal,
/// or a sandbox cap. That is deliberately not `Some(some_code)`: a killed
/// process's status is the killer's, and matching it against a caller's
/// `expect_exit` would let a command that was cut short pass a criterion.
struct GateRun {
    exit: Option<i32>,
    output: String,
}

/// Both streams, in the order a reader wants them: what it printed, then what it
/// complained about.
///
/// Shared with the `exec` tool's dispatch arm, so a command's output reads the
/// same way whether it ran as a criterion or as a tool call.
pub(crate) fn joined_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.trim(), stderr.trim()) {
        ("", e) => e.to_string(),
        (o, "") => o.to_string(),
        (o, e) => format!("{o}\n{e}"),
    }
}

/// How much of a failing gate command's output the trace keeps.
///
/// A test runner's useful output is a screenful; a build's is unbounded. This is
/// a trace row rather than the model's context, so it is bounded on its own terms
/// rather than by the run's per-observation cap.
const GATE_OUTPUT_TRACE_CHARS: usize = 4_000;

/// The logical name of the test binary verification builds and runs. Denying it
/// while allowing `rustc` gives compile-only verification: the produced code is
/// type-checked but never executed.
///
/// It is a placeholder, not a path — the real binary lives in a temp dir with a
/// name the caller never sees, so there is nothing else to write a rule against.
/// The one thing to do with it is decide whether model-produced code may run on
/// this host at all:
///
/// ```
/// use io_harness::{Act, Effect, Policy, TEST_BINARY};
///
/// // The tier `Policy::default()` ships: both spawns allowed by name, so the
/// // execution gate works out of the box.
/// assert_eq!(Policy::default().check(Act::Exec, TEST_BINARY).effect, Effect::Allow);
///
/// // Type-check but never execute. A deny beats an allow in any layer, so this
/// // holds however permissive the layers beneath it are — which is what makes it
/// // usable as an operator-level base under an app's own policy.
/// let compile_only = Policy::default().layer("no-execution").deny_exec(TEST_BINARY);
/// assert_eq!(compile_only.check(Act::Exec, "rustc").effect, Effect::Allow);
/// assert_eq!(compile_only.check(Act::Exec, TEST_BINARY).effect, Effect::Deny);
/// ```
///
/// The refusal is [`Error::Refused`], reported to the caller rather than folded
/// into "the gate did not pass" — a criterion that was refused is not one that
/// ran and failed.
pub const TEST_BINARY: &str = "<test-binary>";

impl Verification {
    /// Check a single produced file against the criterion (0.1/0.2 single-file
    /// mode). The multi-file variants belong to [`Verification::passes_in`] and
    /// error here.
    ///
    /// `contents` is the current file text (already read by the caller).
    pub async fn passes(&self, path: &Path, contents: &str) -> Result<bool> {
        self.passes_guarded(path, contents, &ExecGuard::permissive())
            .await
    }

    /// [`Verification::passes`], with every spawn checked against a policy.
    pub async fn passes_guarded(
        &self,
        path: &Path,
        contents: &str,
        guard: &ExecGuard<'_>,
    ) -> Result<bool> {
        match self {
            Verification::FileContains(needle) => Ok(contents.contains(needle)),
            Verification::FileEquals(expected) => Ok(contents == expected),
            // Single-file mode's execution gate since 0.18.0, which removed the
            // Rust-specific variants that used to be it. Without this arm the
            // migration note those variants carried would be false for a
            // single-file caller: it says to use `Command`, and `Command` would
            // have had nowhere to run. It runs in the edited file's own
            // directory, which is the only root a single-file contract has.
            Verification::Command { .. } => {
                let root = path.parent().unwrap_or_else(|| Path::new("."));
                self.run_command(root, guard).await
            }
            // There is no gate, so there is nothing here that can pass. The run
            // ends on an assistant turn that calls no tool — see
            // [`RunOutcome::Finished`](crate::RunOutcome::Finished) — which is a
            // decision the loop makes and not one this function can.
            Verification::None => Ok(false),
            Verification::EachCompilesRust(_)
            | Verification::DocumentContains { .. }
            | Verification::WorkspaceFileContains { .. } => Err(Error::Config(
                "multi-file verification requires a workspace root".into(),
            )),
            // A review needs a provider and this function has none. It is
            // resolved by the run loop, which holds the contract's reviewer —
            // the same shape the multi-file variants use above, and for the same
            // reason: a criterion this entry point cannot honestly evaluate says
            // so rather than returning `false`.
            Verification::Review { .. } => Err(Error::Config(
                "a review criterion is resolved by the run loop, not by `passes`; \
                 register a reviewer with `TaskContract::with_reviewer`"
                    .into(),
            )),
        }
    }

    /// Run a [`Verification::Command`] criterion in `root`. Shared by both
    /// modes since 0.18.0 — single-file mode runs it in the edited file's
    /// directory, workspace mode in the workspace root — so one gate cannot
    /// behave two ways depending on which entry point reached it.
    async fn run_command(&self, root: &Path, guard: &ExecGuard<'_>) -> Result<bool> {
        let Verification::Command { argv, expect_exit } = self else {
            return Err(Error::Config(
                "run_command called with a criterion that is not a Command".into(),
            ));
        };
        let Some(program) = argv.first() else {
            return Err(Error::Config(
                "Verification::Command needs a non-empty argv".into(),
            ));
        };
        guard.check(program, &argv[1..])?;
        let run = guard.exec_output(argv, root).await?;
        let passed = run.exit == Some(*expect_exit);
        if !passed {
            // What the command said about its own failure, where the next
            // diagnosis can read it. Without this a failing gate is an outcome
            // discriminant and nothing else, and "the agent's work is wrong" is
            // indistinguishable from "the test runner is not installed".
            guard.record_gate_failure(&format!(
                "command exited {} (expected {expect_exit})",
                run.exit
                    .map_or_else(|| "on a signal or a cap".to_string(), |c| c.to_string()),
            ));
            guard.record_gate_output(&run.output);
        }
        Ok(passed)
    }

    /// Check the criterion against a workspace `root` (0.3 multi-file mode). The
    /// multi-file variants read their own files relative to `root`.
    pub async fn passes_in(&self, root: &Path) -> Result<bool> {
        self.passes_in_guarded(root, &ExecGuard::permissive()).await
    }

    /// [`Verification::passes_in`], with every spawn checked against a policy.
    pub async fn passes_in_guarded(&self, root: &Path, guard: &ExecGuard<'_>) -> Result<bool> {
        match self {
            // The one criterion that is not about Rust. Everything the gate
            // needs is in the argv, so the same three lines check a Go test, a
            // pytest run, an npm script or a Makefile target.
            Verification::Command { .. } => self.run_command(root, guard).await,
            // No gate: see `passes_guarded`.
            Verification::None => Ok(false),
            Verification::WorkspaceFileContains { file, needle } => {
                let src = tokio::fs::read_to_string(root.join(file))
                    .await
                    .unwrap_or_default();
                Ok(src.contains(needle))
            }
            Verification::DocumentContains { file, needle } => {
                Ok(extract_document_text(root, file)?.contains(needle))
            }
            Verification::EachCompilesRust(files) => {
                for f in files {
                    let src = tokio::fs::read_to_string(root.join(f))
                        .await
                        .unwrap_or_default();
                    if !compile_source(&src, guard).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            // A review needs a provider and this function has none. Resolved by
            // the run loop, which holds the contract's reviewer.
            Verification::Review { .. } => Err(Error::Config(
                "a review criterion is resolved by the run loop, not by `passes_in`; \
                 register a reviewer with `TaskContract::with_reviewer`"
                    .into(),
            )),
            // Single-file variants against a workspace need a target file, which
            // this method does not carry; use them in single-file mode.
            _ => Err(Error::Config(
                "single-file verification used in workspace mode".into(),
            )),
        }
    }

    /// Human-readable description fed to the model as the success criterion.
    pub fn describe(&self) -> String {
        match self {
            Verification::Command { argv, expect_exit } => format!(
                "running `{}` in the workspace root must exit {expect_exit}",
                argv.join(" ")
            ),
            // Said plainly rather than left blank. A model told nothing about
            // how it will be judged infers a criterion and works to that one;
            // told there is none, it works to the goal, which is the whole point
            // of the variant.
            Verification::None => "there is no automated check. Do the work the goal describes, \
                                   then reply without calling a tool to end the run"
                .to_string(),
            Verification::FileContains(needle) => {
                format!("the file must contain exactly this text: {needle:?}")
            }
            Verification::FileEquals(expected) => {
                format!("the file's entire contents must equal exactly: {expected:?}")
            }
            Verification::WorkspaceFileContains { file, needle } => {
                format!("the file {file:?} must contain exactly this text: {needle:?}")
            }
            Verification::DocumentContains { file, needle } => format!(
                "the document {file:?} must contain this text once its text is \
                 extracted: {needle:?}"
            ),
            Verification::EachCompilesRust(files) => {
                format!("each of these files must compile as Rust: {files:?}")
            }
            // The rubric verbatim. A reviewing model reads the same words the
            // working model was told it would be judged by, which is the only
            // honest way to run this criterion: a hidden rubric grades work
            // against a standard nobody could have aimed at.
            Verification::Review { rubric, .. } => format!(
                "a second model will read what you wrote and decide whether it \
                 satisfies this rubric: {rubric:?}"
            ),
        }
    }
}

/// The error for a document this build cannot read because its feature is off.
/// Named rather than absent: a criterion that silently could not run is the
/// failure mode this whole variant exists to remove.
#[allow(dead_code)]
fn missing_feature(ext: &str) -> Error {
    Error::Config(format!(
        "DocumentContains cannot read .{ext}: this build of io-harness does not \
         have the \"{ext}\" feature enabled"
    ))
}

/// A document's extracted text, chosen by extension.
///
/// Reads through a permissive [`Workspace`] rooted at `root`: verification is the
/// *caller's* criterion, not the agent's action, so it is not subject to the
/// policy the agent runs under — the same reason
/// [`Verification::WorkspaceFileContains`] reads the file directly. The
/// `Workspace` is here to reuse the readers, not to gate them.
///
/// An unknown extension is an error rather than a fallback to reading the bytes
/// as text: a criterion that quietly becomes a weaker criterion is exactly what
/// this variant exists to remove.
fn extract_document_text(root: &Path, file: &Path) -> Result<String> {
    #[allow(unused_variables)]
    let rel = file.to_string_lossy().replace('\\', "/");
    #[allow(unused_variables)]
    let ws = crate::tools::Workspace::new(root);
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        #[cfg(feature = "xlsx")]
        "xlsx" => crate::tools::documents::xlsx::read_sheet(&ws, &rel, None),
        #[cfg(feature = "docx")]
        "docx" => crate::tools::documents::docx::read_text(&ws, &rel),
        #[cfg(feature = "pptx")]
        "pptx" => crate::tools::documents::pptx::read_text(&ws, &rel),
        #[cfg(feature = "pdf")]
        "pdf" => crate::tools::documents::pdf::read_text(&ws, &rel),
        // One arm per format, each present only when its feature is absent, so the
        // "you did not build this in" answer is reachable in exactly the builds
        // where it is true.
        #[cfg(not(feature = "xlsx"))]
        "xlsx" => Err(missing_feature("xlsx")),
        #[cfg(not(feature = "docx"))]
        "docx" => Err(missing_feature("docx")),
        #[cfg(not(feature = "pptx"))]
        "pptx" => Err(missing_feature("pptx")),
        #[cfg(not(feature = "pdf"))]
        "pdf" => Err(missing_feature("pdf")),
        other => Err(Error::Config(format!(
            "DocumentContains does not know how to read .{other}; it reads .xlsx, \
             .docx, .pptx and .pdf, and deliberately does not fall back to \
             matching raw bytes"
        ))),
    }
}

/// The argv that compiles the file under verification as its *own* crate, so
/// nothing it declares can reach the crate the criterion lives in.
fn subject_lib_args(subject: &Path, rlib: &Path) -> Vec<String> {
    [
        "--edition",
        "2021",
        "--crate-type",
        "lib",
        "--crate-name",
        SUBJECT_CRATE,
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        subject.display().to_string(),
        "-o".into(),
        rlib.display().to_string(),
    ])
    .collect()
}

/// The argv that type-checks the probe crate against the compiled subject.
///
/// Every element is harness-constructed — no model or caller output reaches it —
/// which is why the command policy gates the binary name and records argv rather
/// than parsing it. See the 0.4.0 contract.
fn probe_args(dir: &Path, probe: &Path, rlib: &Path) -> Vec<String> {
    [
        "--edition",
        "2021",
        "--crate-type",
        "lib",
        "--emit",
        "metadata",
        "--extern",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        format!("{SUBJECT_CRATE}={}", rlib.display()),
        "--out-dir".into(),
        dir.display().to_string(),
        probe.display().to_string(),
    ])
    .collect()
}

/// The crate name the file under verification is compiled under.
const SUBJECT_CRATE: &str = "subject";

/// Appended to the subject on the compile-only path, and referenced by
/// [`PROBE_CRATE`]. A subject that deletes its own items — a crate-level
/// `#![cfg(any())]` — deletes this too, and the reference then fails to resolve.
/// The name is reserved: a subject defining it as well simply fails to compile.
const PROBE_ITEM: &str = "pub fn __io_harness_probe() {}\n";

/// The crate root that proves the subject's items actually exist.
const PROBE_CRATE: &str = "extern crate subject;\n\
    pub fn __io_harness_check() { subject::__io_harness_probe() }\n";

/// Compile `source` with `rustc` in a throwaway temp dir, and report whether it
/// type-checked. `rustc` touches no network and the temp dir is removed on drop.
///
/// Compile-only since 0.18.0. The branch that appended a caller's criterion and
/// ran the produced test binary went with the three Rust-specific variants that
/// were its only callers; a criterion that runs a test is now
/// [`Verification::Command`], where the project's own runner does it.
async fn compile_source(source: &str, guard: &ExecGuard<'_>) -> Result<bool> {
    let dir = tempfile::tempdir()?; // removed on drop — nothing left behind

    // Compile the subject as its own crate, with a probe item appended, then
    // type-check a second crate that *references* the probe.
    //
    // The second compile is what makes the gate honest. Before 0.8.1 the subject
    // was compiled alone, and "it compiled" was taken to mean its contents were
    // type-checked. It does not: a crate-level `#![cfg(any())]` strips every item
    // before rustc examines it, so a body as ill-typed as
    // `pub fn hello() -> u32 { "not a u32" }` compiled clean and passed. A
    // subject that deleted itself now fails, because the probe went with it and
    // the probe crate cannot find it.
    //
    // The probe rather than the more obvious `include!` of the subject from a
    // harness-authored root: that would reject crate-level inner attributes
    // outright, which also fails an honest file opening with
    // `#![allow(dead_code)]` or `#![no_std]`. Legitimate attributes keep working
    // here — only *deleting the crate's contents* is caught.
    let subject = dir.path().join("subject.rs");
    tokio::fs::write(&subject, format!("{source}\n{PROBE_ITEM}")).await?;
    let rlib = dir.path().join("libsubject.rlib");
    let args = subject_lib_args(&subject, &rlib);
    guard.check("rustc", &args)?;
    let argv = std::iter::once("rustc".to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    if !guard.exec(&argv, dir.path()).await? {
        guard.record_gate_failure("subject-compile");
        return Ok(false);
    }

    let probe = dir.path().join("probe.rs");
    tokio::fs::write(&probe, PROBE_CRATE).await?;
    let args = probe_args(dir.path(), &probe, &rlib);
    guard.check("rustc", &args)?;
    let argv = std::iter::once("rustc".to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    let passed = guard.exec(&argv, dir.path()).await?;
    if !passed {
        // The subject compiled but its items are gone.
        guard.record_gate_failure("subject-emptied");
    }
    Ok(passed)
}

// The three Rust-specific variants were removed in 0.18.0 and their tests were
// rewritten over `Verification::Command` rather than deleted: they are the
// specification of what each one proved, and the removal is only safe if the
// same properties still hold through the general gate.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn passes(v: &Verification, contents: &str) -> bool {
        // Content variants ignore the path; use a dummy.
        v.passes(&PathBuf::from("unused"), contents).await.unwrap()
    }

    #[tokio::test]
    async fn contains_passes_and_fails() {
        let v = Verification::FileContains("fn hello".into());
        assert!(passes(&v, "pub fn hello() {}").await);
        assert!(!passes(&v, "pub fn world() {}").await);
    }

    #[tokio::test]
    async fn equals_is_exact() {
        let v = Verification::FileEquals("a".into());
        assert!(passes(&v, "a").await);
        assert!(!passes(&v, "a ").await);
    }

    /// What `CompilesRust` proved, through the gate that replaced it: a
    /// substring stub is not a program, and an execution gate says so where a
    /// content gate cannot. Single-file mode, which is where `CompilesRust`
    /// lived — so this also covers 0.18.0 giving `Command` a root there.
    #[tokio::test]
    async fn a_command_gate_rejects_a_stub_and_accepts_real_code() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.rs");
        let compiles = Verification::Command {
            argv: vec![
                "rustc".into(),
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "hello.rs".into(),
            ],
            expect_exit: 0,
        };

        // The I01 case: the literal substring, which is not valid Rust.
        tokio::fs::write(&file, "fn hello").await.unwrap();
        assert!(!compiles.passes(&file, "fn hello").await.unwrap());

        let good = "pub fn hello() -> u32 { 42 }\n";
        tokio::fs::write(&file, good).await.unwrap();
        assert!(compiles.passes(&file, good).await.unwrap());
    }

    #[tokio::test]
    async fn a_command_absent_from_the_allow_list_is_refused_not_failed() {
        // Denying rustc must refuse, and the refusal must be distinguishable
        // from a verification that ran and returned false.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
        let v = Verification::EachCompilesRust(vec!["a.rs".into()]);

        let policy = Policy::default().layer("locked").deny_exec("rustc");
        let refused = v
            .passes_in_guarded(dir.path(), &ExecGuard::new(&policy))
            .await;
        assert!(
            matches!(refused, Err(Error::Refused { ref target, .. }) if target == "rustc"),
            "expected a typed refusal, got {refused:?}"
        );

        // The same code under the default policy runs and passes — so the
        // refusal above is the policy talking, not a broken compile.
        let allowed = Policy::default();
        assert!(v
            .passes_in_guarded(dir.path(), &ExecGuard::new(&allowed))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn nothing_spawns_the_test_binary_since_the_rust_variants_were_removed() {
        // 0.18.0 removed the only criteria that compiled a caller's test and ran
        // the resulting binary, so `TEST_BINARY` names a spawn the crate no
        // longer makes: denying it now changes nothing. Asserted rather than
        // assumed, because a policy that reads as a restriction and enforces
        // nothing is exactly the kind of thing an operator should not have to
        // discover for themselves. `TEST_BINARY` is kept so that policies
        // written against it still compile.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
        let policy = Policy::default().layer("no-exec").deny_exec(TEST_BINARY);
        assert!(Verification::EachCompilesRust(vec!["a.rs".into()])
            .passes_in_guarded(dir.path(), &ExecGuard::new(&policy))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_verification_with_no_policy_still_spawns_as_0_3_0_did() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn hello() -> u32 { 42 }\n").unwrap();
        assert!(Verification::EachCompilesRust(vec!["a.rs".into()])
            .passes_in(dir.path())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn each_compiles_rust_fails_if_any_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();

        let v = Verification::EachCompilesRust(vec!["a.rs".into(), "b.rs".into()]);
        assert!(v.passes_in(root).await.unwrap());

        // Break one file: the whole set must now fail.
        std::fs::write(root.join("b.rs"), "pub fn b").unwrap();
        assert!(!v.passes_in(root).await.unwrap());
    }

    /// What `WorkspaceTestPasses` proved, through the gate that replaced it: the
    /// edited files have to work *together*, and the project's own runner is
    /// what says so. The migration note tells a caller to write exactly this
    /// criterion, so this is the assertion behind that note.
    #[tokio::test]
    async fn a_command_gate_runs_the_projects_own_suite_and_fails_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() -> u32 { 40 }\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();
        let lib =
            "pub mod a;\npub mod b;\n#[test]\nfn together() { assert_eq!(a::a() + b::b(), 42); }\n";
        std::fs::write(root.join("src/lib.rs"), lib).unwrap();

        let v = Verification::Command {
            argv: vec!["cargo".into(), "test".into(), "--offline".into()],
            expect_exit: 0,
        };
        assert!(v.passes_in(root).await.unwrap());

        // One file wrong → the cross-file test fails, and so does the gate.
        std::fs::write(root.join("src/b.rs"), "pub fn b() -> u32 { 99 }\n").unwrap();
        assert!(!v.passes_in(root).await.unwrap());
    }

    #[tokio::test]
    async fn multi_file_variant_errors_in_single_file_mode() {
        let v = Verification::EachCompilesRust(vec!["a.rs".into()]);
        assert!(v.passes(&PathBuf::from("unused"), "").await.is_err());
    }

    // 0.14.0 — the document criterion. The decisive test is the third one: a
    // criterion that cannot tell a document's text from its container bytes is
    // `WorkspaceFileContains` with extra steps and a longer name.

    #[cfg(feature = "docx")]
    #[tokio::test]
    async fn a_document_criterion_passes_on_text_the_document_actually_shows() {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::tools::Workspace::new(dir.path());
        crate::tools::documents::docx::write_new(
            &ws,
            "report.docx",
            &["Quarterly revenue rose".to_string()],
        )
        .unwrap();

        let v = Verification::DocumentContains {
            file: "report.docx".into(),
            needle: "revenue rose".into(),
        };
        assert!(v.passes_in(dir.path()).await.unwrap());
    }

    #[cfg(feature = "docx")]
    #[tokio::test]
    async fn a_document_criterion_fails_on_text_the_document_does_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::tools::Workspace::new(dir.path());
        crate::tools::documents::docx::write_new(&ws, "report.docx", &["Nothing here".to_string()])
            .unwrap();

        let v = Verification::DocumentContains {
            file: "report.docx".into(),
            needle: "revenue rose".into(),
        };
        assert!(!v.passes_in(dir.path()).await.unwrap());
    }

    /// THE test for this variant, and it found something sharper than expected.
    ///
    /// The first draft assumed `WorkspaceFileContains` would match a needle that
    /// appears in a `.docx`'s container bytes and wrongly pass. It does not — it
    /// reads with `read_to_string(..).unwrap_or_default()`, a document is not
    /// UTF-8, so it reads the empty string and reports "does not contain" for
    /// EVERY document. The wrong answer it gives is not a false pass, it is a
    /// permanent false fail, and silently.
    ///
    /// So both halves are asserted on a document whose text genuinely contains
    /// the needle: the byte criterion says no, the document criterion says yes.
    /// Plus the container-bytes case, so the reader is pinned as reading text
    /// rather than bytes in either direction.
    #[cfg(feature = "docx")]
    #[tokio::test]
    async fn the_byte_criterion_cannot_read_a_document_and_this_one_can() {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::tools::Workspace::new(dir.path());
        crate::tools::documents::docx::write_new(
            &ws,
            "report.docx",
            &["Quarterly revenue rose".to_string()],
        )
        .unwrap();

        let needle = "revenue rose";
        let byte_match = Verification::WorkspaceFileContains {
            file: "report.docx".into(),
            needle: needle.into(),
        };
        assert!(
            !byte_match.passes_in(dir.path()).await.unwrap(),
            "the byte criterion reads a document as empty and can never pass — \
             this is the wrong answer the variant exists to fix"
        );

        let text_match = Verification::DocumentContains {
            file: "report.docx".into(),
            needle: needle.into(),
        };
        assert!(
            text_match.passes_in(dir.path()).await.unwrap(),
            "the document criterion reads what the document shows"
        );

        // And the other direction: an entry name lives in the container's bytes
        // and never in the text, so it must not match either.
        let container = "word/document.xml";
        let raw = std::fs::read(dir.path().join("report.docx")).unwrap();
        assert!(
            String::from_utf8_lossy(&raw).contains(container),
            "the fixture must carry the entry name in its bytes, or the next \
             assertion proves nothing"
        );
        let container_match = Verification::DocumentContains {
            file: "report.docx".into(),
            needle: container.into(),
        };
        assert!(
            !container_match.passes_in(dir.path()).await.unwrap(),
            "a needle only in the container must not match the text"
        );
    }

    #[tokio::test]
    async fn a_document_criterion_refuses_a_format_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "revenue rose").unwrap();

        let v = Verification::DocumentContains {
            file: "notes.txt".into(),
            needle: "revenue rose".into(),
        };
        let err = v.passes_in(dir.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("txt"),
            "the error names the extension it will not guess at, got {err}"
        );
    }

    #[tokio::test]
    async fn a_document_criterion_needs_a_workspace_root() {
        let v = Verification::DocumentContains {
            file: "report.docx".into(),
            needle: "x".into(),
        };
        let err = v
            .passes(std::path::Path::new("f.rs"), "irrelevant")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace root"), "got {err}");
    }
}
