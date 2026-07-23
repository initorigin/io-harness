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

impl Verification {
    /// Check a single produced file against the criterion (0.1/0.2 single-file
    /// mode). The multi-file variants belong to [`Verification::passes_in`] and
    /// error here.
    ///
    /// `contents` is the current file text (already read by the caller).
    pub async fn passes(&self, _path: &Path, contents: &str) -> Result<bool> {
        match self {
            Verification::FileContains(needle) => Ok(contents.contains(needle)),
            Verification::FileEquals(expected) => Ok(contents == expected),
            Verification::CompilesRust => compile_source(contents, None).await,
            Verification::RustTestPasses { test_src } => {
                compile_source(contents, Some(test_src)).await
            }
            Verification::EachCompilesRust(_) | Verification::WorkspaceTestPasses { .. } => Err(
                Error::Config("multi-file verification requires a workspace root".into()),
            ),
        }
    }

    /// Check the criterion against a workspace `root` (0.3 multi-file mode). The
    /// multi-file variants read their own files relative to `root`.
    pub async fn passes_in(&self, root: &Path) -> Result<bool> {
        match self {
            Verification::EachCompilesRust(files) => {
                for f in files {
                    let src = tokio::fs::read_to_string(root.join(f)).await.unwrap_or_default();
                    if !compile_source(&src, None).await? {
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
                compile_source(&combined, Some(test_src)).await
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
            Verification::EachCompilesRust(files) => {
                format!("each of these files must compile as Rust: {files:?}")
            }
            Verification::WorkspaceTestPasses { files, test_src } => format!(
                "these files {files:?} must together compile and pass this test:\n{test_src}"
            ),
        }
    }
}

/// Compile `source` with `rustc` in a throwaway temp dir. With `test_src`,
/// append it and run the resulting test binary. Returns whether the gate
/// passed. `rustc` touches no network and the temp dir is removed on drop.
async fn compile_source(source: &str, test_src: Option<&str>) -> Result<bool> {
    let dir = tempfile::tempdir()?; // removed on drop — nothing left behind

    match test_src {
        None => {
            // Metadata-only compile: type-checks without linking a binary.
            let lib = dir.path().join("lib.rs");
            tokio::fs::write(&lib, source).await?;
            let status = Command::new("rustc")
                .args(["--edition", "2021", "--crate-type", "lib", "--emit", "metadata"])
                .arg("--out-dir")
                .arg(dir.path())
                .arg(&lib)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            Ok(status.success())
        }
        Some(test) => {
            let combined = dir.path().join("combined.rs");
            tokio::fs::write(&combined, format!("{source}\n{test}\n")).await?;
            let bin = dir.path().join("t");

            let built = Command::new("rustc")
                .args(["--edition", "2021", "--test"])
                .arg(&combined)
                .arg("-o")
                .arg(&bin)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            if !built.success() {
                return Ok(false);
            }

            let run = Command::new(&bin)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            Ok(run.success())
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
