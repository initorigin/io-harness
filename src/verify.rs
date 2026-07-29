//! The verification layer: checks that confirm the file meets the spec.
//!
//! Two kinds live here. Deterministic content checks ([`Verification::FileContains`],
//! [`Verification::FileEquals`]) are cheap and cannot lie about what the file
//! *says* — but they cannot confirm the file *works*. The 0.1.0 live run proved
//! this: the model passed `FileContains("fn hello")` by writing the literal
//! string `fn hello`, which does not compile (see
//! `.ultraship/.../iterations/US-IO-HARNESS-0.2.0-I01`).
//!
//! v0.2 adds execution-based checks ([`Verification::CompilesRust`],
//! [`Verification::RustTestPasses`]) that compile — and optionally test — the
//! produced file with `rustc`. A substring stub fails to compile, so it fails
//! the gate. Compilation happens in a throwaway temp dir that is removed
//! afterwards, and `rustc` touches no network.
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
/// The execution gates ([`Verification::CompilesRust`],
/// [`Verification::RustTestPasses`], and the workspace variants) are stronger
/// than the content gates ([`Verification::FileContains`],
/// [`Verification::FileEquals`]) because a stub cannot compile and run — that is
/// why 0.2.0 introduced them — but "stronger" is not "sufficient".
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
/// // Execution-based, and the pair a repository task normally wants: the listed
/// // files are concatenated and compiled together, then `test_src` is compiled
/// // beside them and run. "Together" is the point — each file compiling on its
/// // own (`EachCompilesRust`) would not catch a caller updated out of step with
/// // the function it calls.
/// let contract = TaskContract::workspace(
///     "make `parse` reject an empty input instead of panicking",
///     "/path/to/repo",
///     Verification::WorkspaceTestPasses {
///         files: vec!["src/parse.rs".into(), "src/lib.rs".into()],
///         test_src: r#"
///             #[test]
///             fn empty_input_is_an_error() {
///                 assert!(parse("").is_err());
///             }
///         "#
///         .into(),
///     },
/// )
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
/// // `CompilesRust` fails the second: a substring stub does not type-check, and
/// // since 0.8.1 a file that deletes its own items with `#![cfg(any())]` fails
/// // too rather than compiling clean on nothing.
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
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
    ///     Verification::Command {
    ///         argv: vec!["npm".into(), "test".into()],
    ///         expect_exit: 0,
    ///     },
    /// );
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
    /// ```
    /// use io_harness::{RunOutcome, TaskContract, Verification};
    ///
    /// let contract = TaskContract::workspace(
    ///     "work out why the nightly deploy has been failing and write up what you find",
    ///     "/path/to/repo",
    ///     Verification::None,
    /// );
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
    /// The file must compile as a Rust library (`rustc --crate-type lib`), and
    /// its items must survive to be type-checked. Execution-based: a
    /// non-compiling stub fails.
    ///
    /// Since 0.8.1 the second half of that is enforced. A crate-level attribute
    /// such as `#![cfg(any())]` strips every item *before* rustc examines it, so
    /// a file whose body does not type-check compiled clean and passed. A subject
    /// that deletes its own contents now fails. Legitimate crate-level attributes
    /// — `#![allow(dead_code)]`, `#![no_std]` — are unaffected.
    #[deprecated(
        since = "0.17.0",
        note = "use Verification::Command { argv: vec![\"cargo\".into(), \"build\".into()], \
                expect_exit: 0 } — or any compiler invocation for the language in hand. \
                Removed in 0.18.0."
    )]
    CompilesRust,
    /// The file must compile, and `test_src` must compile against it and pass.
    /// Execution-based.
    ///
    /// Since 0.8.1 the file under verification can no longer shadow the names
    /// `test_src` uses, nor delete it. `test_src` is unchanged: it still refers to
    /// the file's items unqualified, and still reaches *private* items — the two
    /// remain one crate, deliberately, so an implementation does not have to be
    /// `pub` to pass. See the
    /// [type-level docs](Verification#what-a-passing-gate-proves) for what a
    /// pass does and does not prove.
    #[deprecated(
        since = "0.17.0",
        note = "use Verification::Command { argv: vec![\"cargo\".into(), \"test\".into()], \
                expect_exit: 0 } and put the test in the project's own test suite, where the \
                project's own tooling can run it. Removed in 0.18.0."
    )]
    RustTestPasses {
        /// Rust source compiled against the file, e.g. a `#[test] fn`.
        test_src: String,
    },
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
    /// Hardened with [`Verification::CompilesRust`] in 0.8.1: each file goes
    /// through the same check, so no listed file can pass by deleting itself.
    EachCompilesRust(Vec<PathBuf>),
    /// (workspace/multi-file) All listed files, concatenated in order, must
    /// compile, and `test_src` must compile against them and pass. This proves
    /// the edited files work *together*, not merely each in isolation.
    ///
    /// Hardened with [`Verification::RustTestPasses`] in 0.8.1: a shadowing
    /// definition in any one of the files cannot defeat the gate, and a private
    /// item in any of them still reaches `test_src`.
    #[deprecated(
        since = "0.17.0",
        note = "use Verification::Command { argv: vec![\"cargo\".into(), \"test\".into()], \
                expect_exit: 0 }, which runs the repository's own suite over the whole crate \
                rather than a concatenation of the files you listed. Removed in 0.18.0."
    )]
    WorkspaceTestPasses {
        /// Files (relative to the workspace root) concatenated into the subject.
        files: Vec<PathBuf>,
        /// Rust source compiled against the files, e.g. a `#[test] fn`.
        test_src: String,
    },
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
/// // Compile-only: `rustc` may run, the produced test binary may not. The gate
/// // type-checks the criterion against the code and never executes it, which is
/// // what you want when the code came from a model and this host is not a sandbox
/// // you are willing to lose.
/// let policy = Policy::permissive()
///     .layer("verify")
///     .allow_exec("rustc")
///     .deny_exec(TEST_BINARY);
///
/// let criterion = Verification::RustTestPasses {
///     test_src: "#[test] fn it_answers() { assert_eq!(hello(), 42); }".into(),
/// };
/// // An allow the policy does not give is `Error::Refused`, not a silent skip —
/// // a verification that was refused is not one that ran and failed.
/// let outcome = criterion
///     .passes_guarded(
///         "src/hello.rs".as_ref(),
///         "pub fn hello() -> u32 { 42 }",
///         &ExecGuard::new(&policy),
///     )
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
    #[allow(deprecated)] // the variants this release deprecates still work here
    pub async fn passes_guarded(
        &self,
        _path: &Path,
        contents: &str,
        guard: &ExecGuard<'_>,
    ) -> Result<bool> {
        match self {
            Verification::FileContains(needle) => Ok(contents.contains(needle)),
            Verification::FileEquals(expected) => Ok(contents == expected),
            Verification::CompilesRust => compile_source(contents, None, guard).await,
            Verification::RustTestPasses { test_src } => {
                compile_source(contents, Some(test_src), guard).await
            }
            // There is no gate, so there is nothing here that can pass. The run
            // ends on an assistant turn that calls no tool — see
            // [`RunOutcome::Finished`](crate::RunOutcome::Finished) — which is a
            // decision the loop makes and not one this function can.
            Verification::None => Ok(false),
            Verification::Command { .. }
            | Verification::EachCompilesRust(_)
            | Verification::WorkspaceTestPasses { .. }
            | Verification::DocumentContains { .. }
            | Verification::WorkspaceFileContains { .. } => Err(Error::Config(
                "multi-file verification requires a workspace root".into(),
            )),
        }
    }

    /// Check the criterion against a workspace `root` (0.3 multi-file mode). The
    /// multi-file variants read their own files relative to `root`.
    pub async fn passes_in(&self, root: &Path) -> Result<bool> {
        self.passes_in_guarded(root, &ExecGuard::permissive()).await
    }

    /// [`Verification::passes_in`], with every spawn checked against a policy.
    #[allow(deprecated)] // the variants this release deprecates still work here
    pub async fn passes_in_guarded(&self, root: &Path, guard: &ExecGuard<'_>) -> Result<bool> {
        match self {
            // The one criterion that is not about Rust. Everything the gate
            // needs is in the argv, so the same three lines check a Go test, a
            // pytest run, an npm script or a Makefile target.
            Verification::Command { argv, expect_exit } => {
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
                    // diagnosis can read it. Without this a failing gate is an
                    // outcome discriminant and nothing else, and "the agent's work
                    // is wrong" is indistinguishable from "the test runner is not
                    // installed".
                    guard.record_gate_failure(&format!(
                        "command exited {} (expected {expect_exit})",
                        run.exit
                            .map_or_else(|| "on a signal or a cap".to_string(), |c| c.to_string()),
                    ));
                    guard.record_gate_output(&run.output);
                }
                Ok(passed)
            }
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
                    if !compile_source(&src, None, guard).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Verification::WorkspaceTestPasses { files, test_src } => {
                let mut combined = String::new();
                for f in files {
                    let src = tokio::fs::read_to_string(root.join(f))
                        .await
                        .unwrap_or_default();
                    combined.push_str(&src);
                    combined.push('\n');
                }
                compile_source(&combined, Some(test_src), guard).await
            }
            // Single-file variants against a workspace need a target file, which
            // this method does not carry; use them in single-file mode.
            _ => Err(Error::Config(
                "single-file verification used in workspace mode".into(),
            )),
        }
    }

    /// Human-readable description fed to the model as the success criterion.
    #[allow(deprecated)] // the variants this release deprecates still describe themselves
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
            Verification::CompilesRust => {
                "the file must compile as Rust (rustc --crate-type lib)".to_string()
            }
            Verification::RustTestPasses { test_src } => {
                format!("the file must compile and pass this test:\n{test_src}")
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
            Verification::WorkspaceTestPasses { files, test_src } => format!(
                "these files {files:?} must together compile and pass this test:\n{test_src}"
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

/// The argv verification builds to compile and link the test binary from the
/// combined crate — the subject with the criterion module appended.
fn test_build_args(combined: &Path, bin: &Path) -> Vec<String> {
    ["--edition", "2021", "--test"]
        .iter()
        .map(|s| s.to_string())
        .chain([
            combined.display().to_string(),
            "-o".into(),
            bin.display().to_string(),
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

/// Opens the module the harness wraps the caller's criterion in, appended to the
/// subject so the two are one crate. Closed by [`CRITERION_CLOSE`].
///
/// This preamble is why an execution gate cannot be answered by the file it is
/// checking. Two properties, both load-bearing:
///
/// 1. The criterion is a *child module* of the subject's own crate, so
///    `use super::*` reaches the subject's items — including private ones.
///    `test_src` still calls `hello()` exactly as a 0.8.0 caller wrote it, and a
///    subject that writes an idiomatic non-`pub` `fn hello` still passes. The
///    0.8.1 development build made the subject a separate crate instead, and that
///    is precisely what broke: privacy is a wall between crates, so an ordinary
///    private implementation failed a gate it had always passed. The live run for
///    F7 caught it — see `iterations/US-IO-HARNESS-0.8.1-I01`.
/// 2. The prelude macros the criterion is likely to invoke are re-imported
///    *explicitly*. A subject defining `macro_rules! assert` — exported or not —
///    then makes the name ambiguous (rustc E0659) rather than capturing it, so
///    the gate fails to compile instead of passing an impossible criterion. A
///    macro the subject exports under any *other* name still reaches the
///    criterion through the glob, which is what keeps this a fix rather than a
///    restriction.
///
/// The deletion attack — a crate-level `#![cfg(any())]` that strips the criterion
/// along with everything else, leaving a test binary that runs zero tests and
/// exits 0 — is not this preamble's job. Being one crate again, it cannot be. It
/// is caught before this point by the same probe the compile-only path uses.
const CRITERION_OPEN: &str = "\n#[cfg(test)]
mod __io_harness_criterion {
#[allow(unused_imports)]
use ::core::{assert, assert_eq, assert_ne, debug_assert, debug_assert_eq, debug_assert_ne,
    matches, panic, todo, unimplemented, unreachable, write, writeln, format_args};
#[allow(unused_imports)]
use ::std::{dbg, eprint, eprintln, format, print, println, vec};
#[allow(unused_imports)] use super::*;
";

/// Closes [`CRITERION_OPEN`].
const CRITERION_CLOSE: &str = "\n}\n";

/// Compile `source` with `rustc` in a throwaway temp dir. With `test_src`,
/// append it and run the resulting test binary. Returns whether the gate
/// passed. `rustc` touches no network and the temp dir is removed on drop.
async fn compile_source(
    source: &str,
    test_src: Option<&str>,
    guard: &ExecGuard<'_>,
) -> Result<bool> {
    let dir = tempfile::tempdir()?; // removed on drop — nothing left behind

    match test_src {
        None => {
            // Compile the subject as its own crate, with a probe item appended,
            // then type-check a second crate that *references* the probe.
            //
            // The second compile is what makes the gate honest. Before 0.8.1 the
            // subject was compiled alone, and "it compiled" was taken to mean its
            // contents were type-checked. It does not: a crate-level
            // `#![cfg(any())]` strips every item before rustc examines it, so a
            // body as ill-typed as `pub fn hello() -> u32 { "not a u32" }`
            // compiled clean and passed. A subject that deleted itself now fails,
            // because the probe went with it and the probe crate cannot find it.
            //
            // The probe rather than the more obvious `include!` of the subject
            // from a harness-authored root: that would reject crate-level inner
            // attributes outright, which also fails an honest file opening with
            // `#![allow(dead_code)]` or `#![no_std]`. Legitimate attributes keep
            // working here — only *deleting the crate's contents* is caught.
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
        Some(test) => {
            // Three defences, because the two attacks this release closes are
            // different and no single structure stops both without cost.
            //
            // The subject is compiled alone first, with the probe appended. That
            // classifies an ordinary "the file does not compile" failure, and the
            // probe reference then catches a subject that deleted its own items —
            // a crate-level `#![cfg(any())]`, which would otherwise strip the
            // criterion too and leave a test binary that runs zero tests and
            // exits 0. Same mechanism as the compile-only path above.
            //
            // Only then is the criterion appended, as a module of the subject's
            // own crate. It is deliberately NOT a separate crate: that was tried
            // during 0.8.1 and it broke an ordinary private implementation, since
            // privacy is a wall between crates. Shadowing is stopped inside the
            // module by CRITERION_OPEN instead.
            let subject = dir.path().join("subject.rs");
            tokio::fs::write(&subject, format!("{source}\n{PROBE_ITEM}")).await?;
            let rlib = dir.path().join("libsubject.rlib");

            let args = subject_lib_args(&subject, &rlib);
            guard.check("rustc", &args)?;
            let subject_argv = std::iter::once("rustc".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            if !guard.exec(&subject_argv, dir.path()).await? {
                // The subject does not compile: the gate fails exactly as it did
                // in 0.8.0.
                guard.record_gate_failure("subject-compile");
                return Ok(false);
            }

            let probe = dir.path().join("probe.rs");
            tokio::fs::write(&probe, PROBE_CRATE).await?;
            let args = probe_args(dir.path(), &probe, &rlib);
            guard.check("rustc", &args)?;
            let probe_argv = std::iter::once("rustc".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            if !guard.exec(&probe_argv, dir.path()).await? {
                // The subject compiled but its items are gone — so the criterion
                // would be gone too, and the test binary would pass on nothing.
                guard.record_gate_failure("subject-emptied");
                return Ok(false);
            }

            let combined = dir.path().join("combined.rs");
            tokio::fs::write(
                &combined,
                format!("{source}\n{CRITERION_OPEN}{test}{CRITERION_CLOSE}"),
            )
            .await?;
            let bin = dir.path().join("t");

            let args = test_build_args(&combined, &bin);
            guard.check("rustc", &args)?;
            let build_argv = std::iter::once("rustc".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            if !guard.exec(&build_argv, dir.path()).await? {
                // The interesting failure: the subject compiles on its own, but
                // the criterion will not compile beside it. A subject shadowing
                // `assert!` lands here, as an E0659 ambiguity.
                guard.record_gate_failure("criterion-compile");
                return Ok(false);
            }

            // The produced binary is its own spawn: denying TEST_BINARY while
            // allowing rustc type-checks the code without ever running it.
            guard.check(TEST_BINARY, &[bin.display().to_string()])?;
            let passed = guard.exec(&[bin.display().to_string()], dir.path()).await?;
            if !passed {
                guard.record_gate_failure("test-run");
            }
            Ok(passed)
        }
    }
}

// The Rust-specific variants are deprecated in 0.17.0 and removed in 0.18.0.
// Their tests stay, unchanged, until the variants go: they are the specification
// of what each one proves, and F10's claim that a 0.16.2-era contract still works
// is only worth anything if the assertions behind it are still running.
#[allow(deprecated)]
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

    #[tokio::test]
    async fn compiles_rust_rejects_stub_accepts_real() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.rs");

        // The I01 case: the literal substring, which is not valid Rust.
        tokio::fs::write(&file, "fn hello").await.unwrap();
        assert!(!Verification::CompilesRust
            .passes(&file, "fn hello")
            .await
            .unwrap());

        let good = "pub fn hello() -> u32 { 42 }\n";
        tokio::fs::write(&file, good).await.unwrap();
        assert!(Verification::CompilesRust
            .passes(&file, good)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn rust_test_passes_only_when_test_passes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.rs");
        let good = "pub fn hello() -> u32 { 42 }\n";
        tokio::fs::write(&file, good).await.unwrap();

        let ok = Verification::RustTestPasses {
            test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
        };
        assert!(ok.passes(&file, good).await.unwrap());

        let bad = Verification::RustTestPasses {
            test_src: "#[test] fn t() { assert_eq!(hello(), 41); }".into(),
        };
        assert!(!bad.passes(&file, good).await.unwrap());
    }

    #[tokio::test]
    async fn a_command_absent_from_the_allow_list_is_refused_not_failed() {
        // Denying rustc must refuse, and the refusal must be distinguishable
        // from a verification that ran and returned false.
        let policy = Policy::default().layer("locked").deny_exec("rustc");
        let guard = ExecGuard::new(&policy);
        let good = "pub fn hello() -> u32 { 42 }\n";

        let refused = Verification::CompilesRust
            .passes_guarded(Path::new("x.rs"), good, &guard)
            .await;
        assert!(
            matches!(refused, Err(Error::Refused { ref target, .. }) if target == "rustc"),
            "expected a typed refusal, got {refused:?}"
        );

        // The same code under the default policy runs and passes — so the
        // refusal above is the policy talking, not a broken compile.
        let allowed = Policy::default();
        assert!(Verification::CompilesRust
            .passes_guarded(Path::new("x.rs"), good, &ExecGuard::new(&allowed))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn denying_the_test_binary_type_checks_without_running_it() {
        // rustc allowed, the produced binary denied: the code compiles but is
        // never executed, so the gate refuses rather than reporting a result.
        let policy = Policy::default().layer("no-exec").deny_exec(TEST_BINARY);
        let v = Verification::RustTestPasses {
            test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
        };
        let out = v
            .passes_guarded(
                Path::new("x.rs"),
                "pub fn hello() -> u32 { 42 }\n",
                &ExecGuard::new(&policy),
            )
            .await;
        assert!(
            matches!(out, Err(Error::Refused { ref target, .. }) if target == TEST_BINARY),
            "expected the run of the test binary to be refused, got {out:?}"
        );
    }

    #[tokio::test]
    async fn a_verification_with_no_policy_still_spawns_as_0_3_0_did() {
        assert!(Verification::CompilesRust
            .passes(Path::new("x.rs"), "pub fn hello() -> u32 { 42 }\n")
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

    #[tokio::test]
    async fn workspace_test_passes_only_when_files_work_together() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub fn a() -> u32 { 40 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();

        let v = Verification::WorkspaceTestPasses {
            files: vec!["a.rs".into(), "b.rs".into()],
            test_src: "#[test] fn t() { assert_eq!(a() + b(), 42); }".into(),
        };
        assert!(v.passes_in(root).await.unwrap());

        // One file wrong → the cross-file test fails.
        std::fs::write(root.join("b.rs"), "pub fn b() -> u32 { 99 }\n").unwrap();
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
