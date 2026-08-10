//! Governed `git` spawns: a fixed argv, built here, run under the policy.
//!
//! This is the foundation the git built-ins sit on. It deliberately does *not*
//! repeat the weakness of [`ExecGuard`](crate::verify::ExecGuard), which checks
//! only the program name and formats the rest of the argv into a trace string:
//! for `rustc` that is survivable, because the argv is the gate's own; for
//! `git` it is not, because the argv would carry model-supplied text and `git`
//! has subcommands that fetch code and options that execute it.
//!
//! ## Why a caller cannot pass a free-form argument
//!
//! There is no `&str`, no `Vec<String>`, and no `impl IntoIterator<Item = ...>`
//! anywhere on the way in that reaches the argv as an *option*. A caller names a
//! `GitCmd` variant, and every field of every variant is one of three things:
//!
//! * a typed non-string value (`bool`, `u32`) that this module renders into a
//!   flag itself — a `bool` cannot spell `--upload-pack=…`; or
//! * a `paths` vector, which is model-supplied data and is therefore emitted
//!   *only* after the `--` separator, and only after each element passes
//!   `check_path`; or
//! * a string this module fuses into an argv element it owns — a commit message
//!   as the operand of `-m`, a branch name inside `--create=<name>` — which is
//!   never a separate element `git` could read as an option, and which passes
//!   `check_branch_name` first where the fusion is not available.
//!
//! `git worktree add` is the one shape where the fusion is not available:
//! `--branch=<name>` does not exist, only `-b <name>`, so the name is a separate
//! argv element and `check_branch_name` is what stands between it and `git`'s
//! own parser. Measured on git 2.41.0, not assumed.
//!
//! Adding a git capability means adding a variant here and getting it reviewed.
//! That is the whole security property: the set of argvs this module can emit is
//! finite, enumerable, and enumerated by the tests below.
//!
//! No shell is ever involved — no `sh -c`, no joining. Each path is one argv
//! element, so a path containing spaces, quotes, newlines or shell
//! metacharacters reaches `git` as that exact path and nothing interprets it.

// The git built-ins that call this land in a follow-up task; until they do, the
// default lib build sees the module as unused. Remove this once they exist.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use super::cap_result;
use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy};

/// The binary. A constant rather than a parameter so a policy rule has a stable
/// `Act::Exec` target to name.
const GIT: &str = "git";

/// The platform's bit bucket, used both as the "global config" file and as the
/// hooks directory.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The child's environment, fixed on every spawn:
///
/// * `GIT_PAGER`/`PAGER` — a pager would block forever on a pipe that never
///   becomes a tty, and its output is terminal escapes, not data.
/// * `LC_ALL`/`LANG` — porcelain and error text must not change with the
///   operator's locale, or parsing and tests drift per machine.
/// * `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL` — `/etc/gitconfig` and
///   `~/.gitconfig` can define aliases and `core.*` settings that change what a
///   subcommand does. Nothing in this crate's permission model covers them.
const FIXED_ENV: [(&str, &str); 6] = [
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("LC_ALL", "C"),
    ("LANG", "C"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", NULL_DEVICE),
];

/// Repository hooks, disabled by pointing `core.hooksPath` at a path that cannot
/// be a directory.
///
/// This matters more than the rest of the environment put together. `.git/hooks/*`
/// is arbitrary executable code carried by the repository the agent was pointed
/// at — cloned, or supplied by whoever opened the pull request — and `git`
/// runs it on ordinary commands. Nothing in this crate's permission model covers
/// it: the policy authorises spawning `git`, not spawning whatever a checked-out
/// tree left in its hooks directory. `-c` is the reliable way to suppress it,
/// because it wins over every config file, including the repository's own.
const NO_HOOKS: &str = "core.hooksPath";

/// One git invocation, as a closed set of shapes.
///
/// Every variant renders to a subcommand that reads the repository, writes an
/// object, or creates a ref — and none that reaches the network or rewrites
/// history. See the module docs for why the fields are typed the way they are,
/// and [`no_shape_can_write_or_reach_the_network`] for the test that holds the
/// line.
//
// ponytail: three shapes, the ones the first built-ins need. More git
// capability = one more variant plus its case in `render`, not a new
// mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitCmd {
    /// `git status --porcelain=v1 --untracked-files=normal -- <paths>`
    Status {
        /// Model-supplied paths to limit the report to. Data, never options.
        paths: Vec<String>,
    },
    /// `git diff --no-color --no-ext-diff [--cached] -- <paths>`
    Diff {
        /// Diff the index rather than the working tree (`--cached`).
        staged: bool,
        /// Model-supplied paths to limit the diff to.
        paths: Vec<String>,
    },
    /// `git log --no-color --oneline --no-decorate --max-count=N -- <paths>`
    Log {
        /// How many commits at most. A number, so it cannot spell a flag.
        max_count: u32,
        /// Model-supplied paths to limit the history to.
        paths: Vec<String>,
    },
    /// `git add -- <paths>`
    ///
    /// No `-f`: staging honours `.gitignore`, and the flag that overrides it is
    /// not reachable from here. An ignored path comes back as git's own message,
    /// which is what lets the model tell "ignored" from "no such path".
    Add {
        /// Model-supplied paths to stage.
        paths: Vec<String>,
    },
    /// `git -c user.name=… -c user.email=… commit --no-verify -m <message>`
    ///
    /// Commits what is staged, on the branch that is checked out. No paths: a
    /// commit is of the index, and `git_add` is what decides the index.
    Commit {
        /// The model's commit message. Safe as data because it is the operand of
        /// `-m`: git takes the next argv element literally, so a message
        /// beginning with `-` is a message and not an option.
        message: String,
        /// Who the commit is attributed to.
        identity: Identity,
    },
    /// `git switch --create=<name> --` (0.36.0)
    ///
    /// Creates a branch at the current commit and moves onto it. This is the one
    /// shape of a checkout that cannot discard anything: `--create` refuses a
    /// name that already exists, the new ref starts at `HEAD`, and the working
    /// tree is carried across rather than replaced — which is why `switch` is
    /// reachable here while `checkout` stays on the forbidden list.
    ///
    /// The name is fused into one argv element (`--create=<name>`), so `git`
    /// cannot read it as an option however it is spelled, and it still passes
    /// [`check_branch_name`] because a name `git` rejects is better refused here
    /// with a reason the agent can act on.
    Branch {
        /// The branch to create and move onto.
        name: String,
    },
    /// `git worktree add -b <name> -- <path>` (0.36.0)
    ///
    /// A second working tree at its own new branch, checked out from the current
    /// commit. Two agents in one repository stop overwriting each other's files
    /// without either of them leaving the repository.
    ///
    /// Unlike [`GitCmd::Branch`] the name cannot be fused — `git worktree add`
    /// has no `--branch=<name>` form — so it is a separate argv element and
    /// [`check_branch_name`] is the whole of what keeps it from being read as an
    /// option.
    Worktree {
        /// The branch the new working tree is created on.
        name: String,
        /// Model-supplied path for the new working tree. Data, after `--`.
        path: String,
    },
}

/// The longest branch name this crate will build an argv from.
///
/// Not git's limit — git's is the filesystem's — but a bound, for the reason
/// every other model-supplied string here has one: an unbounded name reaches a
/// ref file, a reflog and every subsequent `git log` line.
const MAX_BRANCH_NAME: usize = 100;

/// Accept one model-supplied branch name, or refuse it.
///
/// An allowlist rather than a denylist, and deliberately narrower than git's own
/// `check-ref-format`. Git's rules are a set of exclusions over an otherwise open
/// byte string, and reproducing them here would mean tracking a grammar this
/// crate does not own; the set of names an agent has any reason to ask for is
/// small, so the safe subset is the one that is enumerable. A name git would
/// accept and this refuses costs an observation the agent can act on; a name git
/// would treat as an option costs the property the whole module exists for.
fn check_branch_name(name: &str) -> Result<()> {
    let bad = if name.is_empty() {
        Some("<branch name may not be empty>")
    } else if name.len() > MAX_BRANCH_NAME {
        Some("<branch name may not be over 100 characters>")
    } else if name.starts_with('-') {
        Some("<branch name may not begin with `-`>")
    } else if name.contains("..") {
        Some("<branch name may not contain `..`>")
    } else if name
        .split('/')
        .any(|part| part.is_empty() || part.ends_with(".lock") || part == ".")
    {
        // Covers a leading `/`, a trailing `/`, an empty component and the
        // `.lock` suffix git reserves for its own lock files — per component,
        // because `refs/heads/a.lock/b` is as invalid as `a.lock`.
        Some(
            "<each `/`-separated part of a branch name must be non-empty and not end with `.lock`>",
        )
    } else if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    {
        Some("<branch name may hold only letters, digits, `.`, `_`, `/` and `-`>")
    } else {
        None
    };
    match bad {
        None => Ok(()),
        Some(rule) => Err(Error::Refused {
            act: "exec".into(),
            target: name.to_string(),
            rule: Some(rule.into()),
            layer: None,
        }),
    }
}

/// Who a commit is attributed to.
///
/// `git commit` fails outright with no `user.email` configured, so this cannot
/// be left to the machine. Inheriting the repository's identity would attribute
/// the agent's commit to whichever human configured that checkout, which is the
/// wrong default; requiring configuration would fail on a fresh machine, which
/// is the wrong other one. So the harness supplies one and the caller may
/// replace it.
///
/// This is what `git log` shows for every commit a run makes, and it is passed
/// as `-c user.name=… -c user.email=…` on the commit itself rather than written
/// into the repository's config, so a run leaves the checkout's own identity
/// untouched.
///
/// ```
/// use io_harness::{Identity, TaskContract, Verification};
///
/// // The default: a name at a domain that can never exist. RFC 2606 reserves
/// // `.invalid` precisely so a synthetic address cannot accidentally be
/// // someone's real one.
/// assert_eq!(Identity::default().email, "agent@io-harness.invalid");
///
/// // Replace it when the commits should be attributable to your service, so
/// // a human reading `git log` a month later can tell which system made
/// // them and where to ask about it.
/// let contract = TaskContract::workspace(
///     "fix the failing test and commit the fix",
///     "/path/to/repo",
/// )
/// .with_verification(Verification::WorkspaceFileContains {
///     file: "src/lib.rs".into(),
///     needle: "fn".into(),
/// })
/// .with_commit_identity("nightly-agent", "nightly-agent@example.com");
/// # let _ = contract;
/// ```
///
/// Neither field may be empty or hold a control character: both reach the
/// commit object and the reflog, and a newline in a name has nowhere useful to
/// go. A bad one is an [`Error::Config`].
/// `Serialize`/`Deserialize` since 0.19.0, so a team's commit identity can live
/// in the project's config file rather than in each program that embeds the
/// crate. Both fields are `#[serde(default)]`, and the validation below still
/// runs when the identity is used — a deserialized identity is not a checked one.
///
/// ```
/// use io_harness::Identity;
///
/// let team: Identity = serde_json::from_str(r#"{"name": "release bot"}"#).unwrap();
/// assert_eq!(team.name, "release bot");
/// assert_eq!(team.email, Identity::default().email);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    /// The committer name.
    pub name: String,
    /// The committer email.
    pub email: String,
}

impl Default for Identity {
    /// An agent identity at a domain that can never exist. `.invalid` is
    /// reserved by RFC 2606 precisely so that a synthetic address cannot
    /// accidentally be someone's.
    fn default() -> Self {
        Self {
            name: "io-harness agent".into(),
            email: "agent@io-harness.invalid".into(),
        }
    }
}

impl Identity {
    /// Refuse an identity that could split into more than one `-c` setting.
    ///
    /// `-c key=value` is a single argv element, so a value cannot introduce a
    /// second setting — but a newline in a name reaches the commit object and
    /// the reflog, and there is no reason to carry one.
    fn check(&self) -> Result<()> {
        for (what, v) in [("name", &self.name), ("email", &self.email)] {
            if v.is_empty() || v.chars().any(char::is_control) {
                return Err(Error::Config(format!(
                    "commit identity {what} must be non-empty and free of control characters"
                )));
            }
        }
        Ok(())
    }
}

impl GitCmd {
    /// Whether this command only reads the repository (0.48.0).
    ///
    /// The same three-and-four split `dispatch` already makes when it decides a
    /// git call's `Act` on `.git`, and the reason it is a method here is that this
    /// module builds the argv: a reader is what `--no-optional-locks` is for, and
    /// a mode is what it is declared under.
    pub(crate) fn reads_only(&self) -> bool {
        matches!(self, Self::Status { .. } | Self::Diff { .. } | Self::Log { .. })
    }

    /// The subcommand and its fixed options — everything this module chose, with
    /// nothing model-supplied in it.
    fn options(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        match self {
            Self::Status { .. } => {
                v.push("status".into());
                v.push("--porcelain=v1".into());
                v.push("--untracked-files=normal".into());
            }
            Self::Diff { staged, .. } => {
                v.push("diff".into());
                v.push("--no-color".into());
                v.push("--no-ext-diff".into());
                if *staged {
                    v.push("--cached".into());
                }
            }
            Self::Log { max_count, .. } => {
                v.push("log".into());
                v.push("--no-color".into());
                v.push("--oneline".into());
                v.push("--no-decorate".into());
                v.push(format!("--max-count={max_count}"));
            }
            Self::Add { .. } => v.push("add".into()),
            Self::Commit { message, .. } => {
                v.push("commit".into());
                // Belt and braces with `core.hooksPath`: that stops the hook
                // being found, this stops it being asked for.
                v.push("--no-verify".into());
                v.push("-m".into());
                v.push(message.clone());
            }
            Self::Branch { name } => {
                v.push("switch".into());
                // Fused, not `--create <name>`: one argv element cannot be split
                // into an option and its operand by any spelling of the name.
                v.push(format!("--create={name}"));
            }
            Self::Worktree { name, .. } => {
                v.push("worktree".into());
                v.push("add".into());
                // `--branch=<name>` does not exist on `worktree add`, so this is
                // the one name that reaches git as its own element. It is
                // validated by `check` before `argv` builds anything.
                v.push("-b".into());
                v.push(name.clone());
            }
        }
        v
    }

    /// The model-supplied half.
    fn paths(&self) -> &[String] {
        match self {
            Self::Status { paths }
            | Self::Diff { paths, .. }
            | Self::Log { paths, .. }
            | Self::Add { paths } => paths,
            // A commit takes the index, not a pathspec.
            Self::Commit { .. } => &[],
            // A branch is created where HEAD already is; there is no path.
            Self::Branch { .. } => &[],
            // The worktree's location is model-supplied, so it goes through the
            // separator and `check_path` with every other path in this module.
            Self::Worktree { path, .. } => std::slice::from_ref(path),
        }
    }

    /// Refuse a shape whose model-supplied non-path text this crate will not
    /// build an argv from (0.36.0).
    ///
    /// Runs before [`Git::argv`] assembles anything, for the same reason
    /// `check_path` runs before a path is pushed: the answer to a name `git`
    /// could read as an option is that it is not present.
    fn check(&self) -> Result<()> {
        match self {
            Self::Branch { name } | Self::Worktree { name, .. } => check_branch_name(name),
            _ => Ok(()),
        }
    }

    /// The `-c` settings this command needs before its subcommand.
    fn config(&self) -> Result<Vec<String>> {
        match self {
            Self::Commit { identity, .. } => {
                identity.check()?;
                Ok(vec![
                    "-c".into(),
                    format!("user.name={}", identity.name),
                    "-c".into(),
                    format!("user.email={}", identity.email),
                ])
            }
            _ => Ok(Vec::new()),
        }
    }
}

/// What a spawn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitOutcome {
    /// The child ran. `code` is `None` when a signal killed it.
    Ran {
        /// The exit status code.
        code: Option<i32>,
        /// Standard output, bounded by the run's per-observation cap.
        stdout: String,
        /// Standard error, bounded by the same cap.
        stderr: String,
    },
    /// There is no `git` on this machine. A capability the host does not have is
    /// not a failed run: the caller turns this into an observation and the agent
    /// carries on without it, exactly as it would with a tool it was never given.
    Unavailable {
        /// What was looked for, for the observation text.
        reason: String,
    },
}

impl GitOutcome {
    /// Whether git ran and reported success.
    pub(crate) fn ok(&self) -> bool {
        matches!(self, Self::Ran { code: Some(0), .. })
    }
}

/// Spawns `GitCmd`s in one directory under one policy.
pub(crate) struct Git<'a> {
    policy: &'a Policy,
    workdir: PathBuf,
    program: String,
    cap: usize,
    /// The containment this call resolved (0.48.0), or `None` for a run that
    /// granted [`ExecMode::FullAccess`](crate::ExecMode::FullAccess).
    ///
    /// Before 0.48.0 this spawn was the one the boundary never reached: a run
    /// whose `exec` could not write outside the workspace could still reach `git`,
    /// and no surface said so. The mode is the *call's*, already narrowed — the
    /// three readers declare `ReadOnly` — so it is handed in rather than derived
    /// here, which keeps one answer per call instead of two that can disagree.
    sandbox: Option<std::sync::Arc<crate::sandbox::ExecContainment>>,
}

impl<'a> Git<'a> {
    /// Spawn in `workdir` under `policy`, bounding captured output at `cap`
    /// chars — the run's per-observation cap from
    /// [`entry_cap_chars`](crate::context::entry_cap_chars), so a git command
    /// obeys the same ceiling as every other tool result rather than one of its
    /// own.
    pub(crate) fn new(policy: &'a Policy, workdir: impl Into<PathBuf>, cap: usize) -> Self {
        Self {
            policy,
            workdir: workdir.into(),
            program: GIT.into(),
            cap,
            sandbox: None,
        }
    }

    /// Run this call's `git` inside the containment the run resolved (0.48.0).
    pub(crate) fn contained(
        mut self,
        sandbox: Option<std::sync::Arc<crate::sandbox::ExecContainment>>,
    ) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Use a `git` that is not on `PATH` under that name.
    ///
    /// The policy check targets this exact string; `Act::Exec` patterns match by
    /// full text or by basename, so a rule naming `git` still covers
    /// `/usr/bin/git`.
    // Only the tests need this today: production always runs the `git` on PATH,
    // and a caller-configurable git binary is a capability nobody has asked for.
    #[cfg(test)]
    pub(crate) fn program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// The complete argv, program at index 0, or a refusal if any path is not
    /// one.
    ///
    /// Order is load-bearing: the `-c` overrides and `--no-pager` must precede
    /// the subcommand (git only accepts them there), and the `--` separator must
    /// precede every model-supplied byte.
    pub(crate) fn argv(&self, cmd: &GitCmd) -> Result<Vec<String>> {
        cmd.check()?;
        let mut argv = vec![self.program.clone(), "--no-pager".into()];
        // 0.48.0 — a reader declares `ReadOnly`, and `git status` refreshing a
        // stale index would take `.git/index.lock` under exactly that mode. This
        // is git's own way of saying "do not take a lock you did not have to", so
        // the declaration is correct for these three rather than merely survivable
        // (`US-IO-HARNESS-0.48.0-I01`). It is a top-level option and must precede
        // the subcommand.
        if cmd.reads_only() {
            argv.push("--no-optional-locks".into());
        }
        argv.push("-c".into());
        argv.push(format!("{NO_HOOKS}={NULL_DEVICE}"));
        argv.extend(cmd.config()?);
        argv.extend(cmd.options());
        argv.push("--".into());
        for p in cmd.paths() {
            check_path(p)?;
            argv.push(p.clone());
        }
        Ok(argv)
    }

    /// Check the policy and run `cmd`, capturing a bounded result.
    pub(crate) async fn run(&self, cmd: &GitCmd) -> Result<GitOutcome> {
        // Build first: a refusable argument is refused before the policy is even
        // consulted, so a permissive policy is not a way to smuggle one through.
        let argv = self.argv(cmd)?;
        let verdict = self.policy.check(Act::Exec, &self.program);
        if verdict.effect != Effect::Allow {
            return Err(Error::Refused {
                act: "exec".into(),
                target: self.program.clone(),
                rule: verdict.rule,
                layer: verdict.layer,
            });
        }
        // 0.48.0 — the same two halves every other spawn site in this crate uses:
        // what the argv becomes under a wrapping backend, and the restriction that
        // is installed in the child instead of expressed as an argv. Both, or a
        // Landlock rung would report itself while this one command ran unconfined
        // — which is the defect the matrix caught on a `shell` stage in 0.46.0.
        let roots = self
            .sandbox
            .as_ref()
            .map(|c| c.roots_for(&self.workdir))
            .unwrap_or_default();
        let spawn_argv = match &self.sandbox {
            Some(c) => {
                crate::sandbox::wrap_argv(
                    &c.config,
                    &self.workdir,
                    c.config.allow_network,
                    &roots,
                    &argv,
                )
                .1
            }
            None => argv.clone(),
        };
        let mut child = self.command(&spawn_argv);
        // Held across the spawn: the guard owns the rule set the child restricts
        // itself with.
        let _contained = match &self.sandbox {
            Some(c) => {
                crate::sandbox::apply_rlimits(&mut child, &c.config.limits);
                crate::sandbox::contain_command(
                    &mut child,
                    &c.config,
                    &self.workdir,
                    c.config.allow_network,
                    &roots,
                )
            }
            None => None,
        };
        match child.output().await {
            Ok(out) => Ok(GitOutcome::Ran {
                code: out.status.code(),
                stdout: cap_result(String::from_utf8_lossy(&out.stdout).into_owned(), self.cap).0,
                stderr: cap_result(String::from_utf8_lossy(&out.stderr).into_owned(), self.cap).0,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GitOutcome::Unavailable {
                reason: format!("no `{}` on PATH", self.program),
            }),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// The configured child. Separate from [`Git::run`] so the environment it
    /// pins is testable without a `git` on the machine.
    fn command(&self, argv: &[String]) -> Command {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..])
            .current_dir(&self.workdir)
            // No inherited stdin: git must never be able to sit waiting on a
            // terminal that will never answer.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in FIXED_ENV {
            c.env(k, v);
        }
        c
    }
}

/// Accept one model-supplied path, or refuse it.
///
/// A leading `-` is refused outright rather than escaped or quoted. There is no
/// escaping that helps: `git` parses its own argv, so the only two states are
/// "this is an option" and "this is not present". The `--` separator already
/// covers the well-behaved case; this covers the case where a future variant
/// forgets it, and it is cheap. A path genuinely named `-foo` is still reachable
/// as `./-foo`.
fn check_path(p: &str) -> Result<()> {
    let bad = if p.is_empty() {
        Some("<empty path>")
    } else if p.starts_with('-') {
        Some("<path may not begin with `-`>")
    } else {
        None
    };
    match bad {
        None => Ok(()),
        Some(rule) => Err(Error::Refused {
            act: "exec".into(),
            target: p.to_string(),
            rule: Some(rule.into()),
            layer: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generous cap; the bound itself is [`cap_result`]'s and tested there.
    const CAP: usize = 100_000;

    fn git<'a>(policy: &'a Policy, dir: &std::path::Path) -> Git<'a> {
        Git::new(policy, dir, CAP)
    }

    /// Every shape, with one benign path each, for the sweeps below.
    fn every_shape() -> Vec<GitCmd> {
        vec![
            GitCmd::Status {
                paths: vec!["src".into()],
            },
            GitCmd::Diff {
                staged: false,
                paths: vec!["src".into()],
            },
            GitCmd::Diff {
                staged: true,
                paths: vec![],
            },
            GitCmd::Log {
                max_count: 20,
                paths: vec!["src/main.rs".into()],
            },
            GitCmd::Add {
                paths: vec!["src/main.rs".into()],
            },
            GitCmd::Commit {
                message: "a message".into(),
                identity: Identity::default(),
            },
            GitCmd::Branch {
                name: "agent/fix-the-flake".into(),
            },
            GitCmd::Worktree {
                name: "agent/child-1".into(),
                path: ".worktrees/child-1".into(),
            },
        ]
    }

    /// Fails to compile if a variant is added without adding it to
    /// [`every_shape`]. Without this the surface tests below would silently stop
    /// covering the new capability while continuing to pass, which is the exact
    /// way a closed surface quietly opens.
    #[test]
    fn every_shape_covers_every_variant() {
        for cmd in every_shape() {
            match cmd {
                GitCmd::Status { .. }
                | GitCmd::Diff { .. }
                | GitCmd::Log { .. }
                | GitCmd::Add { .. }
                | GitCmd::Commit { .. }
                | GitCmd::Branch { .. }
                | GitCmd::Worktree { .. } => {}
            }
        }
        let mut kinds: Vec<_> = every_shape().iter().map(std::mem::discriminant).collect();
        let before = kinds.len();
        kinds.dedup_by(|a, b| a == b);
        assert_eq!(before, 8, "every_shape lists eight commands");
        assert_eq!(
            kinds.len(),
            7,
            "every_shape must contain one of each variant; add the new one"
        );
    }

    /// The subcommand is the invariant, and it is checked directly rather than
    /// by scanning the whole argv — the whole argv contains model-supplied data,
    /// and a path named `push` is a path.
    #[test]
    fn the_subcommand_is_always_one_of_the_seven_this_crate_ships() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());
        for cmd in every_shape() {
            let argv = g.argv(&cmd).unwrap();
            // The fixed prefix is program, --no-pager, -c, hooksPath, then any
            // -c config pairs, then the subcommand.
            let sub = argv
                .iter()
                .skip(1)
                .find(|a| !a.starts_with('-') && !a.contains('='))
                .expect("every argv has a subcommand");
            assert!(
                ["status", "diff", "log", "add", "commit", "switch", "worktree"]
                    .contains(&sub.as_str()),
                "{cmd:?} produced subcommand {sub:?}"
            );
        }
    }

    /// F1's control. Each of these is refused *before* an argv exists, and the
    /// same name with the offending part removed is accepted — which is what
    /// makes the refusal a rule about the name rather than a rule about
    /// everything.
    #[test]
    fn a_branch_name_git_could_read_as_an_option_is_refused_and_its_repaired_form_is_not() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());

        let long = "a".repeat(MAX_BRANCH_NAME + 1);
        let refused: &[(&str, &str)] = &[
            ("-x", "x"),
            ("a..b", "a.b"),
            ("/lead", "lead"),
            ("trail/", "trail"),
            ("x.lock", "x.locked"),
            (&long, "a"),
            ("", "a"),
            // Not in the criterion's list, and refused by the same allowlist:
            // the byte a shell user would expect to be quoted, and the one git
            // itself reserves.
            ("has space", "has-space"),
            ("caret^1", "caret1"),
            ("at@{0}", "at0"),
        ];
        for (bad, repaired) in refused {
            for cmd in [
                GitCmd::Branch {
                    name: (*bad).into(),
                },
                GitCmd::Worktree {
                    name: (*bad).into(),
                    path: "wt".into(),
                },
            ] {
                let out = g.argv(&cmd);
                assert!(
                    matches!(out, Err(Error::Refused { .. })),
                    "{bad:?} was not refused: {out:?}"
                );
            }
            // The control: the identical name with the offending part removed
            // builds an argv, so the rule is discriminating rather than a
            // blanket refusal.
            let ok = g
                .argv(&GitCmd::Branch {
                    name: (*repaired).into(),
                })
                .unwrap_or_else(|e| panic!("{repaired:?} should be accepted: {e:?}"));
            assert!(
                ok.contains(&format!("--create={repaired}")),
                "{repaired:?} produced {ok:?}"
            );
        }
    }

    /// The name is fused into `--create=<name>` and is therefore not an argv
    /// element of its own, and the worktree's path lands after the separator
    /// while its branch does not.
    #[test]
    fn a_branch_name_is_fused_and_a_worktree_path_is_data() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());

        let argv = g
            .argv(&GitCmd::Branch {
                name: "agent/x".into(),
            })
            .unwrap();
        assert!(argv.contains(&"switch".to_string()));
        assert!(argv.contains(&"--create=agent/x".to_string()));
        assert!(
            !argv.iter().any(|a| a == "agent/x"),
            "the name is fused, never its own element: {argv:?}"
        );
        // Nothing follows the separator: a branch is created where HEAD is.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv.len(), sep + 1, "{argv:?}");

        let argv = g
            .argv(&GitCmd::Worktree {
                name: "agent/x".into(),
                path: ".worktrees/x".into(),
            })
            .unwrap();
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], [".worktrees/x".to_string()]);
        // The branch is on the crate's side of the separator, next to the `-b`
        // this module wrote.
        assert_eq!(argv[sep - 2], "-b");
        assert_eq!(argv[sep - 1], "agent/x");
    }

    #[test]
    fn a_path_beginning_with_a_dash_is_refused_and_the_same_path_without_it_is_not() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());

        let refused = g.argv(&GitCmd::Status {
            paths: vec!["--exec=rm -rf /".into()],
        });
        assert!(matches!(refused, Err(Error::Refused { .. })), "{refused:?}");

        // Negative control: the identical path with the leading `-` removed.
        let argv = g
            .argv(&GitCmd::Status {
                paths: vec!["exec=rm -rf /".into()],
            })
            .unwrap();
        assert_eq!(argv.last().unwrap(), "exec=rm -rf /");
    }

    #[test]
    fn a_path_with_a_space_a_quote_and_a_newline_survives_as_one_argv_element() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let nasty = "a dir/it's \"quoted\"\nand $(whoami) `id` ;rm -rf *";

        let argv = git(&p, dir.path())
            .argv(&GitCmd::Diff {
                staged: false,
                paths: vec![nasty.into()],
            })
            .unwrap();

        // One element, byte-identical, and the last one — nothing split it.
        assert_eq!(argv.iter().filter(|a| a.contains("whoami")).count(), 1);
        assert_eq!(argv.last().unwrap(), nasty);
    }

    #[test]
    fn a_path_named_like_a_flag_is_refused_and_its_relative_form_stays_data() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());

        // The classic git argument injection. Refused, not escaped.
        let refused = g.argv(&GitCmd::Log {
            max_count: 1,
            paths: vec!["--upload-pack=x".into()],
        });
        assert!(matches!(refused, Err(Error::Refused { .. })), "{refused:?}");

        // Negative control: a file genuinely called that, named the way a shell
        // user names it. It is a path, it lands after `--`, and it is one element.
        let argv = g
            .argv(&GitCmd::Log {
                max_count: 1,
                paths: vec!["./--upload-pack=x".into()],
            })
            .unwrap();
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1], "./--upload-pack=x");
        assert_eq!(argv.len(), sep + 2);
    }

    #[test]
    fn every_model_supplied_path_lands_after_the_separator() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());
        for cmd in every_shape() {
            let argv = g.argv(&cmd).unwrap();
            let sep = argv.iter().position(|a| a == "--").unwrap();
            assert_eq!(&argv[sep + 1..], cmd.paths(), "{cmd:?}");
        }
    }

    #[test]
    fn no_shape_can_write_or_reach_the_network() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());
        const FORBIDDEN: &[&str] = &[
            "push",
            "fetch",
            "clone",
            "remote",
            "reset",
            "checkout",
            "rebase",
            "stash",
            "filter-branch",
            "--force",
        ];
        for cmd in every_shape() {
            let argv = g.argv(&cmd).unwrap();
            for word in FORBIDDEN {
                assert!(
                    !argv.iter().any(|a| a.contains(word)),
                    "{cmd:?} produced {argv:?} containing {word}"
                );
            }
        }
    }

    #[test]
    fn the_child_pins_the_pager_the_locale_the_config_and_the_hooks() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let g = git(&p, dir.path());
        let argv = g.argv(&GitCmd::Status { paths: vec![] }).unwrap();

        // Hooks are suppressed in the argv, because `-c` is the only place that
        // beats the repository's own config.
        assert_eq!(argv[1], "--no-pager");
        assert_eq!(argv[2], "-c");
        assert_eq!(argv[3], format!("core.hooksPath={NULL_DEVICE}"));

        let cmd = g.command(&argv);
        let env: Vec<(String, Option<String>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for (k, v) in FIXED_ENV {
            assert!(
                env.contains(&(k.to_string(), Some(v.to_string()))),
                "{k}={v} missing from {env:?}"
            );
        }
        assert_eq!(cmd.as_std().get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn a_missing_git_binary_is_an_unavailable_outcome_not_a_run_failure() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let out = git(&p, dir.path())
            .program("io-harness-no-such-git")
            .run(&GitCmd::Status { paths: vec![] })
            .await
            .unwrap();
        assert!(matches!(out, GitOutcome::Unavailable { .. }), "{out:?}");
        assert!(!out.ok());
    }

    #[tokio::test]
    async fn an_exec_deny_refuses_the_spawn_and_a_permissive_policy_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = GitCmd::Status { paths: vec![] };

        let denied = Policy::default()
            .layer("l")
            .deny_exec("io-harness-fake-git");
        let err = git(&denied, dir.path())
            .program("io-harness-fake-git")
            .run(&cmd)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Refused { act, target, .. }
                if act == "exec" && target == "io-harness-fake-git"),
            "{err:?}"
        );

        // Negative control: the same spawn under a policy that allows exec gets
        // past the gate — it reaches the spawn and reports the binary missing.
        let allowed = Policy::permissive();
        let out = git(&allowed, dir.path())
            .program("io-harness-fake-git")
            .run(&cmd)
            .await
            .unwrap();
        assert!(matches!(out, GitOutcome::Unavailable { .. }), "{out:?}");
    }

    /// End to end, when the machine has a git. Deliberately outside a repository:
    /// it proves capture and exit status without needing one, and needs no
    /// network. A machine without git skips, and the test above covers that path.
    #[tokio::test]
    async fn a_real_git_reports_its_own_failure_rather_than_erroring_the_run() {
        let p = Policy::permissive();
        let dir = tempfile::tempdir().unwrap();
        let out = git(&p, dir.path())
            .run(&GitCmd::Status { paths: vec![] })
            .await
            .unwrap();
        let GitOutcome::Ran { code, stderr, .. } = out else {
            return; // no git on this machine
        };
        // 128 outside a repository; 0 on the rare box whose TMPDIR sits inside
        // one. Either way git ran, and its failure came back as a result.
        match code {
            Some(128) => assert!(stderr.contains("not a git repository"), "{stderr}"),
            other => assert_eq!(other, Some(0), "{stderr}"),
        }
    }
}
