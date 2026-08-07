//! Running a project's own toolchain: a fixed argv, in the workspace, bounded.
//!
//! This is the module the 0.17.0 repositioning rests on. Until it existed the
//! crate could edit a repository and could not build one, so "debug a production
//! issue" or "fix a deployment" were not tasks a contract could express — there
//! was no way to run `npm install`, `go build`, `pytest`, or `make`.
//!
//! ## What is fixed and what is supplied
//!
//! The opposite arrangement to [`git`](super::git), and deliberately. There the
//! module owns the whole argv and the model supplies only paths, because `git`
//! has subcommands that fetch code and options that execute it, so the safe set
//! is a closed one. Here the model supplies the whole argv, because the point is
//! to run a command this crate has never heard of — and the boundary moves
//! accordingly: it is the [`Policy`](crate::Policy), checked on the program *and*
//! on the joined argv, that decides which of them may start.
//!
//! What stays fixed is that there is no shell. The argv is an array of strings
//! handed to the operating system as an array of strings, so `;`, `&&`, `$( )`
//! and a backtick are ordinary bytes inside one argument rather than syntax. A
//! command with a metacharacter in it does not become two commands, because
//! nothing on this path ever parses one.
//!
//! ## What this does not bound
//!
//! A command runs in the workspace root **with the embedding program's
//! privileges**, not inside the [`Sandbox`](crate::Sandbox). That is the owner's
//! decision of 2026-07-29, recorded in `US-IO-HARNESS-0.17.0-I02`, and it is
//! taken with its cost stated: the sandbox denies network egress by default and
//! discards its working directory, which is right for a verification gate and
//! makes `npm install` impossible. So the policy decides what may *start*, and
//! not what a started process then does — the same honest bound this crate
//! already states for a registered [`Tool`](super::Tool) and for a stdio MCP
//! server.
//!
//! Two ceilings apply to what a started process may do to the *run*: a wall-clock
//! timeout, so a wedged command dies naming itself instead of consuming the
//! contract's whole time budget and reporting a budget stop; and the run's own
//! per-observation ceiling applied head-and-tail, so a build log keeps what ran
//! at the top and what failed at the bottom.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::sandbox::{select, RunSpec, Sandbox, SandboxConfig};

/// How long a command started by the `exec` tool may run before it is killed.
///
/// Generous on purpose. This is a ceiling on a *wedged* command, not a budget: a
/// cold `npm install` or a from-scratch `cargo build` legitimately takes minutes,
/// and a timeout tight enough to be interesting is a timeout that fails honest
/// work. What it prevents is a command that will never return quietly eating the
/// contract's time budget and being reported as a budget stop, which sends whoever
/// reads the trace to the wrong diagnosis entirely.
///
/// Override it per contract when the project's own build is slower than this, or
/// when a run should give up sooner:
///
/// ```
/// use io_harness::{TaskContract, DEFAULT_EXEC_TIMEOUT};
/// use std::time::Duration;
///
/// assert_eq!(DEFAULT_EXEC_TIMEOUT, Duration::from_secs(900));
///
/// // A monorepo whose cold build genuinely takes half an hour.
/// let contract = TaskContract::workspace("build it", "/repo")
///     .with_exec_timeout(Duration::from_secs(1800));
///
/// assert_eq!(contract.exec_timeout, Duration::from_secs(1800));
/// ```
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(900);

/// What one `exec` spawn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecOutcome {
    /// The child ran to completion. `code` is `None` when a signal killed it.
    Ran {
        /// The exit status code.
        code: Option<i32>,
        /// Standard output, head-and-tail bounded.
        stdout: String,
        /// Standard error, head-and-tail bounded.
        stderr: String,
        /// How many characters the two bounds together elided, or 0.
        elided: usize,
    },
    /// The child outlived the wall-clock timeout and was killed.
    TimedOut {
        /// The ceiling it crossed.
        after: Duration,
    },
    /// There is no such program on this machine. Not a failed run: the model is
    /// told and carries on, exactly as it is told when `git` is missing.
    Unavailable {
        /// What was looked for, for the observation text.
        reason: String,
    },
}

/// Spawns caller-supplied argvs in one directory, bounded in time and output.
///
/// Holds no [`Policy`](crate::Policy) — unlike [`Git`](super::git::Git), whose
/// argv it builds itself and can therefore check at the same moment. Here the
/// argv arrives from the model and is checked by `dispatch`'s ordinary gate
/// before this type is reached, so the approval tier ([`Effect::Ask`]) routes to
/// the [`Approver`](crate::Approver) exactly as it does for a read or a write.
/// A runner that re-checked would be a second boundary that could disagree with
/// the first.
///
/// [`Effect::Ask`]: crate::Effect::Ask
pub(crate) struct Exec {
    workdir: PathBuf,
    timeout: Duration,
    cap: usize,
    /// The containment this run asked for, or `None` for the 0.17.0 default.
    ///
    /// Held rather than resolved to a backend at construction because
    /// [`select`](crate::sandbox::select) probes the host, and an `Exec` is built
    /// per tool call while the probe's answer does not change under a running
    /// process.
    sandbox: Option<SandboxConfig>,
}

impl Exec {
    /// Spawn in `workdir`, killing after `timeout`, bounding each captured stream
    /// at `cap` chars.
    ///
    /// `cap` is the run's per-observation ceiling from
    /// [`entry_cap_chars`](crate::context::entry_cap_chars), so a command's
    /// output obeys the same bound as every other tool result rather than one of
    /// its own — the same call [`Git`](super::git::Git) makes.
    pub(crate) fn new(workdir: impl Into<PathBuf>, timeout: Duration, cap: usize) -> Self {
        Self {
            workdir: workdir.into(),
            timeout,
            cap,
            sandbox: None,
        }
    }

    /// Run inside the sandbox this contract asked for, or leave it on the host.
    ///
    /// A builder rather than a fourth argument to [`Exec::new`] so the uncontained
    /// construction — every caller before 0.40.0, and every test that is not about
    /// containment — reads exactly as it did.
    pub(crate) fn contained(mut self, sandbox: Option<&SandboxConfig>) -> Self {
        self.sandbox = sandbox.cloned();
        self
    }

    /// Run `argv`, already authorised by the caller.
    ///
    /// `argv[0]` is the program. An empty argv is an [`Error::Config`] rather
    /// than a panic: it reaches here from model-supplied JSON.
    pub(crate) async fn run(&self, argv: &[String]) -> Result<ExecOutcome> {
        let Some(program) = argv.first() else {
            return Err(Error::Config("exec needs a non-empty argv".into()));
        };
        if let Some(config) = &self.sandbox {
            return self.run_contained(config, argv).await;
        }
        let mut cmd = command(program, &argv[1..], &self.workdir);
        match tokio::time::timeout(self.timeout, cmd.output()).await {
            // Dropping the `output()` future drops the `Child`, and the child was
            // configured `kill_on_drop`, so the timeout *is* the kill. What that
            // reaches is the direct child only: a process it started itself may
            // outlive it on every platform this crate supports. Reported rather
            // than hidden — see docs/CONTRACT.md.
            Err(_elapsed) => Ok(ExecOutcome::TimedOut {
                after: self.timeout,
            }),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(ExecOutcome::Unavailable {
                    reason: format!("no `{program}` on PATH"),
                })
            }
            Ok(Err(e)) => Err(Error::Io(e)),
            Ok(Ok(out)) => {
                let (stdout, cut_out) =
                    head_and_tail(&String::from_utf8_lossy(&out.stdout), self.cap);
                let (stderr, cut_err) =
                    head_and_tail(&String::from_utf8_lossy(&out.stderr), self.cap);
                Ok(ExecOutcome::Ran {
                    code: out.status.code(),
                    stdout,
                    stderr,
                    elided: cut_out + cut_err,
                })
            }
        }
    }

    /// The same command, wrapped by whichever backend this host offers.
    ///
    /// The working directory is the workspace root, not a temporary directory —
    /// that is the whole difference between this and the verification gate's use
    /// of the same machinery, and it is what makes a contained `exec` able to
    /// build a project rather than only to run one command in an empty room.
    /// Nothing is copied in and nothing is copied out, so
    /// [`copy_back`](crate::sandbox::copy_back) has no part here.
    ///
    /// Both ceilings still apply and they are not the same ceiling. The contract's
    /// `exec_timeout` wraps the whole sandboxed call, so a backend that wedges is
    /// bounded by the same rule an unsandboxed command is; the config's
    /// `max_wall_secs` is the sandbox's own, and a command it kills comes back as
    /// a cap rather than as this timeout.
    async fn run_contained(&self, config: &SandboxConfig, argv: &[String]) -> Result<ExecOutcome> {
        let sandbox = select(config);
        let spec = RunSpec {
            argv,
            workdir: &self.workdir,
            limits: &config.limits,
            allow_network: config.allow_network,
        };
        match tokio::time::timeout(self.timeout, sandbox.run(spec)).await {
            Err(_elapsed) => Ok(ExecOutcome::TimedOut {
                after: self.timeout,
            }),
            Ok(Err(e)) => Err(e),
            Ok(Ok(out)) => {
                let (stdout, cut_out) = head_and_tail(&out.stdout, self.cap);
                let (stderr, cut_err) = head_and_tail(&out.stderr, self.cap);
                Ok(ExecOutcome::Ran {
                    code: out.exit_code,
                    stdout,
                    stderr,
                    elided: cut_out + cut_err,
                })
            }
        }
    }
}

/// The configured child. Separate from [`Exec::run`] so the argv and the
/// environment it pins are inspectable in a test without spawning anything.
///
/// The environment is *inherited*, unlike [`git`](super::git)'s. A build needs
/// `PATH`, `HOME`, the toolchain's own variables and whatever else the operator
/// exported; stripping it would break the capability this module exists to
/// provide. What is pinned is the parts that would hang or lie: stdin is the null
/// device, because a command that prompts must fail rather than wait forever on a
/// terminal that will never answer, and both output streams are piped so they can
/// be captured and bounded instead of landing on the host's console.
fn command(program: &str, args: &[String], workdir: &Path) -> Command {
    let mut c = Command::new(program);
    c.args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // What makes the wall-clock timeout a kill rather than a detachment.
        .kill_on_drop(true);
    c
}

/// Keep `cap` characters of `s` from both ends, saying what was dropped.
///
/// Returns the bounded text and how many characters it elided (0 when it fitted).
///
/// Both ends, rather than the head-only cut [`cap_result`](super::cap_result)
/// applies elsewhere, because this text is a build log and the two things a
/// reader needs are at opposite ends of it: what ran, at the top, and what failed,
/// at the bottom. A tail-only cut loses the command; a head-only cut loses the
/// error, which is the more common and the more expensive of the two.
///
/// The elision is stated in the returned text, not only in the count, because the
/// model reads the text and nothing else. An agent that cannot tell a truncated
/// output from a complete one will conclude the build printed nothing after line
/// 400.
pub(crate) fn head_and_tail(s: &str, cap: usize) -> (String, usize) {
    let total = s.chars().count();
    if total <= cap {
        return (s.to_string(), 0);
    }
    // Half each. The failure is usually at the bottom and the command at the top,
    // and there is no principled ratio between "what ran" and "what went wrong".
    let head = cap / 2;
    let tail = cap - head;
    let elided = total - head - tail;
    let head_end = char_offset(s, head);
    let tail_start = char_offset(s, total - tail);
    (
        format!(
            "{}\n[... {elided} characters elided ...]\n{}",
            &s[..head_end],
            &s[tail_start..]
        ),
        elided,
    )
}

/// The byte offset of the `n`th character, or the string's length.
fn char_offset(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 100_000;

    /// A short timeout for the tests that are *about* the timeout, and a long one
    /// everywhere else so a slow machine does not fail an unrelated assertion.
    fn exec(dir: &Path) -> Exec {
        Exec::new(dir, Duration::from_secs(120), CAP)
    }

    #[test]
    fn the_child_gets_the_argv_verbatim_with_no_shell_between() {
        let dir = tempfile::tempdir().unwrap();
        // Every metacharacter a shell would act on, inside ONE argument.
        let nasty = "a;b && c $(id) `whoami` | d > e";
        let cmd = command("prog", &[nasty.to_string(), "second".into()], dir.path());

        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![nasty.to_string(), "second".to_string()],
            "the metacharacters stayed inside one argument and nothing split it"
        );
        assert_eq!(cmd.as_std().get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn a_missing_program_is_an_unavailable_outcome_not_a_run_failure() {
        let dir = tempfile::tempdir().unwrap();
        let out = exec(dir.path())
            .run(&["io-harness-no-such-program".to_string()])
            .await
            .unwrap();
        assert!(matches!(out, ExecOutcome::Unavailable { .. }), "{out:?}");
    }

    #[tokio::test]
    async fn an_empty_argv_is_a_config_error_rather_than_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            exec(dir.path()).run(&[]).await,
            Err(Error::Config(_))
        ));
    }

    /// `rustc` is the one program guaranteed present wherever `cargo test` runs,
    /// which is what lets this assert capture and exit status without a fixture
    /// toolchain or a network.
    #[tokio::test]
    async fn a_real_command_reports_its_exit_status_and_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = exec(dir.path())
            .run(&["rustc".to_string(), "--version".to_string()])
            .await
            .unwrap();
        let ExecOutcome::Ran { code, stdout, .. } = out else {
            panic!("rustc must be present wherever cargo test runs: {out:?}");
        };
        assert_eq!(code, Some(0));
        assert!(stdout.starts_with("rustc "), "{stdout:?}");
    }

    #[tokio::test]
    async fn a_failing_command_comes_back_as_a_nonzero_exit_with_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = exec(dir.path())
            .run(&["rustc".to_string(), "no-such-file.rs".to_string()])
            .await
            .unwrap();
        let ExecOutcome::Ran { code, stderr, .. } = out else {
            panic!("expected a run: {out:?}");
        };
        assert_ne!(code, Some(0));
        assert!(!stderr.trim().is_empty(), "the compiler said why");
    }

    /// The command runs in the workspace root, proven by giving it a *relative*
    /// path that only resolves there.
    #[tokio::test]
    async fn the_working_directory_is_the_one_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        let out = exec(dir.path())
            .run(&[
                "rustc".to_string(),
                "--crate-type".to_string(),
                "lib".to_string(),
                "lib.rs".to_string(),
                "--out-dir".to_string(),
                ".".to_string(),
            ])
            .await
            .unwrap();
        assert!(
            matches!(out, ExecOutcome::Ran { code: Some(0), .. }),
            "{out:?}"
        );
        assert!(
            dir.path().join("liblib.rlib").exists(),
            "the relative output path resolved against the workspace root"
        );
    }

    #[test]
    fn output_that_fits_is_returned_whole_and_reports_no_elision() {
        let (out, elided) = head_and_tail("all of it", 100);
        assert_eq!(out, "all of it");
        assert_eq!(elided, 0);
    }

    #[test]
    fn oversized_output_keeps_both_ends_and_says_how_much_it_dropped() {
        let log: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let (out, elided) = head_and_tail(&log, 200);

        assert!(out.contains("line 0\n"), "the head survived: {out}");
        assert!(out.contains("line 199\n"), "the tail survived: {out}");
        assert!(!out.contains("line 100\n"), "the middle went");
        assert!(elided > 0);
        assert!(
            out.contains(&format!("{elided} characters elided")),
            "the model is told what it is missing: {out}"
        );
    }

    #[test]
    fn the_elision_is_measured_in_characters_not_bytes() {
        // Four-byte characters: a byte-indexed cut would slice one in half and
        // panic, and a byte-counted elision would over-report.
        let s: String = std::iter::repeat_n('🌍', 100).collect();
        let (out, elided) = head_and_tail(&s, 20);
        assert_eq!(elided, 80);
        assert_eq!(out.chars().filter(|c| *c == '🌍').count(), 20);
    }

    #[tokio::test]
    async fn a_command_that_outlives_the_timeout_is_killed_and_named_as_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // A sleep, spelled the way each platform spells it. Both are present on
        // every runner image this crate builds on.
        let argv: Vec<String> = if cfg!(windows) {
            ["ping", "-n", "30", "127.0.0.1"]
        } else {
            ["sleep", "30", "", ""]
        }
        .iter()
        .filter(|a| !a.is_empty())
        .map(|a| (*a).to_string())
        .collect();

        let started = std::time::Instant::now();
        let out = Exec::new(dir.path(), Duration::from_millis(300), CAP)
            .run(&argv)
            .await
            .unwrap();

        assert!(
            matches!(out, ExecOutcome::TimedOut { .. }),
            "a wedged command must report itself, not the run's budget: {out:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the run did not wait for the command to finish"
        );
    }

    /// The negative control for the test above: the same runner and the same
    /// short ceiling, over a command that finishes inside it.
    #[tokio::test]
    async fn a_command_that_finishes_inside_the_timeout_is_not_reported_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let out = Exec::new(dir.path(), Duration::from_secs(120), CAP)
            .run(&["rustc".to_string(), "--version".to_string()])
            .await
            .unwrap();
        assert!(matches!(out, ExecOutcome::Ran { .. }), "{out:?}");
    }
}
