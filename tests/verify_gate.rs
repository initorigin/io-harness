//! The execution gate must not be defeatable by the file it is verifying.
//!
//! 0.2.0 introduced execution-based verification because `FileContains` could be
//! satisfied by a stub. 0.8.1 closed the converse hole: the subject file was
//! compiled into the *same crate* as the caller's criterion, so it could redefine
//! what that criterion named — or delete the criterion outright — and still be
//! reported as passing. See `iterations/US-IO-HARNESS-0.8.0-I01`.
//!
//! # What 0.18.0 changed here
//!
//! Half of this suite tested a mechanism that no longer exists. `CompilesRust`,
//! `RustTestPasses` and `WorkspaceTestPasses` were removed, and with them went
//! the only path that compiled a **caller-supplied criterion into the subject's
//! own crate**. A subject can no longer shadow `assert!` to defeat a gate, or
//! delete a criterion with `#![cfg(any())]`, because there is no longer a
//! criterion sitting in its crate to shadow or delete: a criterion is now a
//! command, run by the project's own runner in its own process.
//!
//! Those tests are therefore gone rather than rewritten — a test for an attack
//! that is structurally impossible asserts nothing. What remains is the
//! compile-only gate `EachCompilesRust`, whose subject-deletion defence is still
//! load-bearing and still tested below, and which is the only criterion in the
//! crate that spawns `rustc` itself.

use std::path::PathBuf;

use io_harness::{Error, ExecGuard, Policy, Store, Verification};

/// F9 — the compile-only gate cannot be defeated by a subject that deletes
/// itself. No criterion is involved here at all, which is why the 0.8.1 contract
/// originally predicted these gates were safe: there is nothing to shadow. The
/// attack is on the crate. A crate-level `#![cfg(any())]` strips the item before
/// rustc type-checks it, so a body that is not even well-typed compiles clean.
/// See `iterations/US-IO-HARNESS-0.8.1-I01`.
#[tokio::test]
async fn a_compile_gate_rejects_a_subject_that_deletes_its_own_items() {
    // `-> u32` returning a `&str` cannot type-check. It only compiles if it is
    // never examined.
    let deletes_itself = "#![cfg(any())]\npub fn hello() -> u32 { \"not a u32\" }\n";
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), deletes_itself)
        .await
        .unwrap();
    assert!(
        !Verification::EachCompilesRust(vec![PathBuf::from("a.rs")])
            .passes_in(dir.path())
            .await
            .unwrap(),
        "a crate-level attribute stripped the file's body, so code that does not type-check \
         passed the compile gate"
    );
}

/// F9 — and an honest file still compiles. The guard must not cost a legitimate
/// pass, including for a file that uses inner attributes legitimately.
#[tokio::test]
async fn an_honest_file_still_passes_the_compile_gates() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "pub fn hello() -> u32 { 42 }\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("b.rs"), "pub fn b() -> u32 { 1 }\n")
        .await
        .unwrap();
    let each = Verification::EachCompilesRust(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
    assert!(each.passes_in(dir.path()).await.unwrap());

    // And one broken file still fails the whole set.
    tokio::fs::write(dir.path().join("b.rs"), "pub fn b")
        .await
        .unwrap();
    assert!(!each.passes_in(dir.path()).await.unwrap());

    // A *legitimate* crate-level attribute must keep working. This is why the
    // guard is a probe reference and not a harness-authored root that
    // `include!`s the subject: that construct rejects every inner attribute,
    // which would fail this honest file along with the dishonest one.
    tokio::fs::write(
        dir.path().join("b.rs"),
        "#![allow(dead_code)]\npub fn b() -> u32 { 1 }\n",
    )
    .await
    .unwrap();
    assert!(
        each.passes_in(dir.path()).await.unwrap(),
        "an honest file opening with an inner attribute was failed by the F9 guard"
    );
}

/// NF1 — the hardening added a compiler spawn; it did not add an *unchecked*
/// one. With `rustc` denied, the gate refuses rather than compiling anything.
#[tokio::test]
async fn the_added_subject_compile_is_still_policy_checked() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "pub fn hello() -> u32 { 42 }\n")
        .await
        .unwrap();
    let policy = Policy::default().layer("locked").deny_exec("rustc");
    let out = Verification::EachCompilesRust(vec![PathBuf::from("a.rs")])
        .passes_in_guarded(dir.path(), &ExecGuard::new(&policy))
        .await;
    assert!(
        matches!(out, Err(Error::Refused { ref target, .. }) if target == "rustc"),
        "the subject compile bypassed the exec policy, got {out:?}"
    );
}

/// F9 and NF3 — a compile-only gate defeated by self-deletion is attributable in
/// the trace, and distinguishable from a file that simply does not compile.
///
/// The two phases this can report are now the only two that exist. Until 0.18.0
/// there were four: `criterion-compile` and `test-run` belonged to the removed
/// variants, and a criterion the harness no longer compiles cannot fail to
/// compile.
#[tokio::test]
async fn the_trace_names_a_subject_that_deleted_its_own_items() {
    async fn phase_of(subject: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), subject)
            .await
            .unwrap();
        let store = Store::open(dir.path().join("s.db")).unwrap();
        let run = store.start_run("goal", "a.rs").unwrap();
        let policy = Policy::default();
        let guard = ExecGuard::new(&policy).tracing(&store, run, 1);
        let _ = Verification::EachCompilesRust(vec![PathBuf::from("a.rs")])
            .passes_in_guarded(dir.path(), &guard)
            .await
            .unwrap();
        store
            .sandbox_events(run)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "gate_phase_failed")
            .filter_map(|e| e.detail)
            .collect()
    }

    assert_eq!(
        phase_of("#![cfg(any())]\npub fn hello() -> u32 { \"not a u32\" }\n").await,
        vec!["subject-emptied"],
        "a compile gate defeated by self-deletion is not attributable in the trace"
    );

    // Distinguishable from the ordinary case: the file just does not compile.
    assert_eq!(phase_of("fn hello").await, vec!["subject-compile"]);

    // And an honest file records no failure at all.
    assert!(phase_of("pub fn hello() -> u32 { 42 }\n").await.is_empty());
}

/// The class the removal closed, asserted rather than claimed. The subject from
/// I01 — which shadowed `assert!` so an impossible gate expanded to nothing —
/// has nothing left to attack: a `Command` criterion runs in its own process and
/// never sees the subject's macros. The negative control is the same project
/// with a failing test, which still fails.
#[tokio::test]
async fn a_subject_cannot_shadow_a_macro_a_command_criterion_never_compiles_with() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // The I01 payload, in the subject, where it used to be decisive.
    std::fs::write(
        root.join("src/lib.rs"),
        "#[macro_export] macro_rules! assert { ($c:expr $(, $r:tt)*) => {{ let _ = &$c; }}; }\n\
         pub fn hello() -> u32 { 0 }\n\
         #[test] fn gate() { ::core::assert!(hello() == 42, \"this gate can never pass\"); }\n",
    )
    .unwrap();

    let gate = Verification::Command {
        argv: vec!["cargo".into(), "test".into(), "--offline".into()],
        expect_exit: 0,
    };
    assert!(
        !gate.passes_in(root).await.unwrap(),
        "the shadowing subject defeated a criterion it does not share a crate with"
    );

    // The negative control: the same project, correct, passes — so the failure
    // above is the assertion failing rather than the fixture never building.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> u32 { 42 }\n\
         #[test] fn gate() { ::core::assert!(hello() == 42); }\n",
    )
    .unwrap();
    assert!(gate.passes_in(root).await.unwrap());
}
