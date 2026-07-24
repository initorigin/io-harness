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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy};
use crate::sandbox::{self, RunSpec, Sandbox, SandboxConfig};
use crate::state::{PolicyEvent, Store};

/// How the harness decides a task is done.
#[derive(Debug, Clone)]
pub enum Verification {
    /// The file's contents must contain this text. Cheap, but gameable — a
    /// model can satisfy it without producing working code.
    FileContains(String),
    /// The file's contents must equal this text exactly.
    FileEquals(String),
    /// The file must compile as a Rust library (`rustc --crate-type lib`).
    /// Execution-based: a non-compiling stub fails.
    CompilesRust,
    /// The file must compile together with `test_src` (appended after the file)
    /// and the resulting test binary must pass. Execution-based.
    RustTestPasses {
        /// Rust source appended after the file's contents, e.g. a `#[test] fn`.
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
    /// (workspace/multi-file) Every listed file — relative to the workspace root
    /// — must compile on its own as a Rust library. The run only succeeds when
    /// all of them do, so one wrong file fails the whole set.
    EachCompilesRust(Vec<PathBuf>),
    /// (workspace/multi-file) All listed files, concatenated in order and
    /// followed by `test_src`, must compile and the test must pass. This proves
    /// the edited files work *together*, not merely each in isolation.
    WorkspaceTestPasses {
        /// Files (relative to the workspace root) concatenated before the test.
        files: Vec<PathBuf>,
        /// Rust source appended after the files, e.g. a `#[test] fn`.
        test_src: String,
    },
}

/// What the verification layer is allowed to spawn, and where to record it.
///
/// Verification cannot prompt — there is no approver on this path — so a
/// command is spawned only when the policy explicitly *allows* it. Anything
/// else, including [`Effect::Ask`], is refused.
pub struct ExecGuard<'a> {
    policy: &'a Policy,
    trace: Option<(&'a Store, i64, u32)>,
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
            sandbox: Some(SandboxConfig::default()),
        }
    }

    /// Also record every spawn's full argv against `run_id` at `step`, so
    /// argument-level enforcement can be added later against a real baseline.
    pub fn tracing(mut self, store: &'a Store, run_id: i64, step: u32) -> Self {
        self.trace = Some((store, run_id, step));
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

    /// Execute an already-policy-checked `argv` in `workdir`, returning whether
    /// it succeeded. Routes through the sandbox when one is configured (the
    /// 0.6.0 default) — so model-produced code never runs on the host directly —
    /// and falls back to a direct spawn when the sandbox is opted out (0.5.0).
    async fn exec(&self, argv: &[String], workdir: &Path) -> Result<bool> {
        match &self.sandbox {
            Some(cfg) => {
                let sb = sandbox::select(cfg);
                let backend = sb.backend();
                // Record the sandbox lifecycle so an audit shows where code ran.
                if let Some((store, run_id, step)) = self.trace {
                    let _ = store.record_sandbox_event(&crate::state::SandboxEvent::create(
                        run_id,
                        step,
                        backend.as_str(),
                    ));
                    let _ = store.record_sandbox_event(&crate::state::SandboxEvent::exec(
                        run_id,
                        step,
                        backend.as_str(),
                        &argv.join(" "),
                    ));
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
                        let _ = store.record_sandbox_event(&crate::state::SandboxEvent::cap_hit(
                            run_id,
                            step,
                            cap.as_str(),
                        ));
                    }
                    // The workdir is torn down when this call returns (tempdir
                    // drop in the caller); record the destroy now.
                    let _ = store.record_sandbox_event(&crate::state::SandboxEvent::destroy(
                        run_id, step,
                    ));
                }
                // A cap hit is a real failure of the gate, not a pass.
                Ok(outcome.success())
            }
            None => {
                // Direct host execution — the exact 0.5.0 path.
                let status = Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(workdir)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await?;
                Ok(status.success())
            }
        }
    }
}

/// The logical name of the test binary verification builds and runs. Denying it
/// while allowing `rustc` gives compile-only verification: the produced code is
/// type-checked but never executed.
pub const TEST_BINARY: &str = "<test-binary>";

impl Verification {
    /// Check a single produced file against the criterion (0.1/0.2 single-file
    /// mode). The multi-file variants belong to [`Verification::passes_in`] and
    /// error here.
    ///
    /// `contents` is the current file text (already read by the caller).
    pub async fn passes(&self, path: &Path, contents: &str) -> Result<bool> {
        self.passes_guarded(path, contents, &ExecGuard::permissive()).await
    }

    /// [`Verification::passes`], with every spawn checked against a policy.
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
            Verification::EachCompilesRust(_)
            | Verification::WorkspaceTestPasses { .. }
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
    pub async fn passes_in_guarded(&self, root: &Path, guard: &ExecGuard<'_>) -> Result<bool> {
        match self {
            Verification::WorkspaceFileContains { file, needle } => {
                let src = tokio::fs::read_to_string(root.join(file)).await.unwrap_or_default();
                Ok(src.contains(needle))
            }
            Verification::EachCompilesRust(files) => {
                for f in files {
                    let src = tokio::fs::read_to_string(root.join(f)).await.unwrap_or_default();
                    if !compile_source(&src, None, guard).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Verification::WorkspaceTestPasses { files, test_src } => {
                let mut combined = String::new();
                for f in files {
                    let src = tokio::fs::read_to_string(root.join(f)).await.unwrap_or_default();
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
    pub fn describe(&self) -> String {
        match self {
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
            Verification::EachCompilesRust(files) => {
                format!("each of these files must compile as Rust: {files:?}")
            }
            Verification::WorkspaceTestPasses { files, test_src } => format!(
                "these files {files:?} must together compile and pass this test:\n{test_src}"
            ),
        }
    }
}

/// The argv verification builds for a metadata-only compile. Every element is
/// harness-constructed — no model or caller output reaches it — which is why
/// the command policy gates the binary name and records argv rather than
/// parsing it. See the 0.4.0 contract; argument gating becomes required in
/// 0.6.0 when plugins can supply argv.
fn metadata_args(dir: &Path, lib: &Path) -> Vec<String> {
    [
        "--edition", "2021", "--crate-type", "lib", "--emit", "metadata", "--out-dir",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([dir.display().to_string(), lib.display().to_string()])
    .collect()
}

/// The argv verification builds to compile and link the test binary.
fn test_build_args(combined: &Path, bin: &Path) -> Vec<String> {
    ["--edition", "2021", "--test"]
        .iter()
        .map(|s| s.to_string())
        .chain([combined.display().to_string(), "-o".into(), bin.display().to_string()])
        .collect()
}

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
            // Metadata-only compile: type-checks without linking a binary.
            let lib = dir.path().join("lib.rs");
            tokio::fs::write(&lib, source).await?;
            let args = metadata_args(dir.path(), &lib);
            guard.check("rustc", &args)?;
            let argv = std::iter::once("rustc".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            guard.exec(&argv, dir.path()).await
        }
        Some(test) => {
            let combined = dir.path().join("combined.rs");
            tokio::fs::write(&combined, format!("{source}\n{test}\n")).await?;
            let bin = dir.path().join("t");

            let args = test_build_args(&combined, &bin);
            guard.check("rustc", &args)?;
            let build_argv = std::iter::once("rustc".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            if !guard.exec(&build_argv, dir.path()).await? {
                return Ok(false);
            }

            // The produced binary is its own spawn: denying TEST_BINARY while
            // allowing rustc type-checks the code without ever running it.
            guard.check(TEST_BINARY, &[bin.display().to_string()])?;
            guard.exec(&[bin.display().to_string()], dir.path()).await
        }
    }
}

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
        assert!(!Verification::CompilesRust.passes(&file, "fn hello").await.unwrap());

        let good = "pub fn hello() -> u32 { 42 }\n";
        tokio::fs::write(&file, good).await.unwrap();
        assert!(Verification::CompilesRust.passes(&file, good).await.unwrap());
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
}
