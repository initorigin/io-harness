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

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::error::Result;

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
}

impl Verification {
    /// Check the produced file against the criterion.
    ///
    /// `contents` is the current file text (already read by the caller);
    /// `path` is where it lives, needed by the execution-based variants.
    pub async fn passes(&self, path: &Path, contents: &str) -> Result<bool> {
        match self {
            Verification::FileContains(needle) => Ok(contents.contains(needle)),
            Verification::FileEquals(expected) => Ok(contents == expected),
            Verification::CompilesRust => compile_rust(path, None).await,
            Verification::RustTestPasses { test_src } => compile_rust(path, Some(test_src)).await,
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
        }
    }
}

/// Compile `path` with `rustc` in a throwaway temp dir. With `test_src`, append
/// it and run the resulting test binary. Returns whether the gate passed.
async fn compile_rust(path: &Path, test_src: Option<&str>) -> Result<bool> {
    let dir = tempfile::tempdir()?; // removed on drop — nothing left behind

    match test_src {
        None => {
            // Metadata-only compile: type-checks without linking a binary.
            let status = Command::new("rustc")
                .args(["--edition", "2021", "--crate-type", "lib", "--emit", "metadata"])
                .arg("--out-dir")
                .arg(dir.path())
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?;
            Ok(status.success())
        }
        Some(test) => {
            let base = tokio::fs::read_to_string(path).await.unwrap_or_default();
            let combined = dir.path().join("combined.rs");
            tokio::fs::write(&combined, format!("{base}\n{test}\n")).await?;
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
}
