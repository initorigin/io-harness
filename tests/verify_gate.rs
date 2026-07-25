//! The execution gate must not be defeatable by the file it is verifying.
//!
//! 0.2.0 introduced execution-based verification because `FileContains` could be
//! satisfied by a stub. 0.8.1 closes the converse hole: the subject file was
//! compiled into the *same crate* as the caller's criterion, so it could redefine
//! what that criterion named — or delete the criterion outright — and still be
//! reported as passing. See `iterations/US-IO-HARNESS-0.8.0-I01`.
//!
//! Every gate here is written so that **no correct implementation can satisfy
//! it**. A pass is therefore never ambiguous: it is proof of a bypass.

use std::path::PathBuf;

use io_harness::{Error, ExecGuard, Policy, Store, Verification};

/// A criterion no honest file can meet.
const IMPOSSIBLE: &str = r#"#[test] fn gate() { assert!(false, "this gate can never pass"); }"#;

/// The subject from I01: shadows `assert!` so the impossible gate expands to nothing.
const SHADOWS_ASSERT: &str = r#"
#[macro_export] macro_rules! assert { ($c:expr $(, $r:tt)*) => {{ let _ = &$c; }}; }
pub fn hello() -> u32 { 0 }
"#;

async fn single(test_src: &str, contents: &str) -> bool {
    Verification::RustTestPasses {
        test_src: test_src.into(),
    }
    // The single-file variants ignore the path; contents is what is compiled.
    .passes(&PathBuf::from("unused.rs"), contents)
    .await
    .unwrap()
}

/// Write `files` under a fresh root and check a `WorkspaceTestPasses` gate.
async fn workspace(files: &[(&str, &str)], test_src: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    for (name, src) in files {
        tokio::fs::write(dir.path().join(name), src).await.unwrap();
    }
    Verification::WorkspaceTestPasses {
        files: files.iter().map(|(n, _)| PathBuf::from(n)).collect(),
        test_src: test_src.into(),
    }
    .passes_in(dir.path())
    .await
    .unwrap()
}

/// F1 — the recorded bypass. Reproduced verbatim from I01, no model involved.
#[tokio::test]
async fn a_subject_shadowing_assert_cannot_pass_an_impossible_gate() {
    assert!(
        !single(IMPOSSIBLE, SHADOWS_ASSERT).await,
        "US-IO-HARNESS-0.8.0-I01: a `#[macro_export] macro_rules! assert` in the file under \
         verification made a gate no correct implementation can satisfy report a pass"
    );
}

/// F2 — the same attack through the multi-file gate. I01 expected this to share
/// the weakness by inspection but never demonstrated it; this pins it.
#[tokio::test]
async fn a_workspace_subject_shadowing_assert_cannot_pass_an_impossible_gate() {
    let passed = workspace(
        &[("shadow.rs", SHADOWS_ASSERT), ("other.rs", "pub fn other() {}\n")],
        IMPOSSIBLE,
    )
    .await;
    assert!(
        !passed,
        "the shadowing definition in one of several concatenated files made an impossible \
         WorkspaceTestPasses gate report a pass"
    );
}

/// F3 — resistance is not specific to `assert`. A fix that blocklists one macro
/// name passes F1 and F2 and still leaves the class open.
#[tokio::test]
async fn a_subject_shadowing_a_second_prelude_macro_cannot_pass_either() {
    let subject = r#"
#[macro_export] macro_rules! assert_eq { ($a:expr, $b:expr $(, $r:tt)*) => {{ let _ = (&$a, &$b); }}; }
pub fn hello() -> u32 { 0 }
"#;
    let gate = r#"#[test] fn gate() { assert_eq!(1, 2, "this gate can never pass"); }"#;
    assert!(
        !single(gate, subject).await,
        "shadowing `assert_eq!` defeated the gate — the fix is name-specific, not structural"
    );
}

/// The second vector, found while reproducing the first: the subject does not
/// need to know anything about macros. A crate-level `#![cfg(any())]` deletes the
/// whole crate — the appended criterion with it — and a test binary with zero
/// tests exits 0, which the gate reads as a pass.
///
/// Not named in the 0.8.1 contract (see `execution/active.yaml`); the two-crate
/// fix closes it because the subject's inner attributes stay in the subject's crate.
#[tokio::test]
async fn a_subject_cannot_delete_the_criterion_with_a_crate_level_cfg() {
    let subject = "#![cfg(any())]\npub fn hello() -> u32 { 0 }\n";
    assert!(
        !single(IMPOSSIBLE, subject).await,
        "`#![cfg(any())]` in the subject removed the criterion and the empty test binary \
         exited 0, so the gate reported a pass"
    );
    let passed = workspace(&[("cfg.rs", subject)], IMPOSSIBLE).await;
    assert!(!passed, "the same crate-level cfg defeated the workspace gate");
}

/// F9 — the compile-only gates cannot be defeated by a subject that deletes
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
    assert!(
        !Verification::CompilesRust
            .passes(&PathBuf::from("unused.rs"), deletes_itself)
            .await
            .unwrap(),
        "a crate-level attribute stripped the file's body, so code that does not type-check \
         passed CompilesRust"
    );

    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), deletes_itself).await.unwrap();
    assert!(
        !Verification::EachCompilesRust(vec![PathBuf::from("a.rs")])
            .passes_in(dir.path())
            .await
            .unwrap(),
        "EachCompilesRust shares the weakness — it compiles each file the same way"
    );
}

/// F9 — and an honest file still compiles. The guard must not cost a legitimate
/// pass, including for a file that uses inner attributes legitimately.
#[tokio::test]
async fn an_honest_file_still_passes_the_compile_gates() {
    let good = "pub fn hello() -> u32 { 42 }\n";
    assert!(Verification::CompilesRust
        .passes(&PathBuf::from("unused.rs"), good)
        .await
        .unwrap());

    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), good).await.unwrap();
    tokio::fs::write(dir.path().join("b.rs"), "pub fn b() -> u32 { 1 }\n").await.unwrap();
    let each = Verification::EachCompilesRust(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
    assert!(each.passes_in(dir.path()).await.unwrap());

    // And one broken file still fails the whole set.
    tokio::fs::write(dir.path().join("b.rs"), "pub fn b").await.unwrap();
    assert!(!each.passes_in(dir.path()).await.unwrap());

    // A *legitimate* crate-level attribute must keep working. This is why the
    // guard is a probe reference and not a harness-authored root that
    // `include!`s the subject: that construct rejects every inner attribute,
    // which would fail this honest file along with the dishonest one.
    assert!(
        Verification::CompilesRust
            .passes(
                &PathBuf::from("unused.rs"),
                "#![allow(dead_code)]\npub fn hello() -> u32 { 42 }\n"
            )
            .await
            .unwrap(),
        "an honest file opening with an inner attribute was failed by the F9 guard"
    );
}

/// F7 — a private implementation still passes. This is the regression the live
/// run caught: the 0.8.1 development build compiled the criterion as a separate
/// crate, and privacy is a wall between crates, so an agent writing an idiomatic
/// non-`pub` `fn hello` failed a gate 0.8.0 passed. It hit the step cap rewriting
/// a correct answer four times. The criterion is a module of the subject's own
/// crate for this reason. See `iterations/US-IO-HARNESS-0.8.1-I01`.
#[tokio::test]
async fn a_private_implementation_still_passes_the_gate() {
    let criterion = "#[test] fn t() { assert_eq!(hello(), 42); }";
    assert!(
        single(criterion, "fn hello() -> u32 { 42 }\n").await,
        "a private `fn hello` failed the gate — the criterion cannot see the subject's private items"
    );
    // Still wrong when it is wrong, private or not.
    assert!(!single(criterion, "fn hello() -> u32 { 41 }\n").await);

    // Private items reach a workspace criterion the same way.
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "fn a() -> u32 { 42 }\n").await.unwrap();
    assert!(
        Verification::WorkspaceTestPasses {
            files: vec![PathBuf::from("a.rs")],
            test_src: "#[test] fn t() { assert_eq!(a(), 42); }".into(),
        }
        .passes_in(dir.path())
        .await
        .unwrap(),
        "a private item failed the workspace gate"
    );
}

/// F4 — the boundary tightened without costing an honest pass. The criterion
/// calls `hello()` unqualified, exactly as a 0.8.0 caller wrote it.
#[tokio::test]
async fn an_honest_implementation_still_passes_and_a_wrong_one_still_fails() {
    let good = "pub fn hello() -> u32 { 42 }\n";
    assert!(
        single("#[test] fn t() { assert_eq!(hello(), 42); }", good).await,
        "a correct implementation failed the hardened gate"
    );
    assert!(
        !single("#[test] fn t() { assert_eq!(hello(), 41); }", good).await,
        "an incorrect implementation passed the hardened gate"
    );
}

/// F4, multi-file — the workspace gate still proves the files work *together*.
#[tokio::test]
async fn an_honest_workspace_still_passes_across_files() {
    let passed = workspace(
        &[
            ("a.rs", "pub fn a() -> u32 { 20 }\n"),
            ("b.rs", "pub fn b() -> u32 { 22 }\n"),
        ],
        "#[test] fn t() { assert_eq!(a() + b(), 42); }",
    )
    .await;
    assert!(passed, "a correct multi-file implementation failed the hardened gate");
}

/// NF1 — the hardening added a compiler spawn; it did not add an *unchecked*
/// one. With `rustc` denied, the gate refuses rather than compiling anything.
#[tokio::test]
async fn the_added_subject_compile_is_still_policy_checked() {
    let policy = Policy::default().layer("locked").deny_exec("rustc");
    let out = Verification::RustTestPasses {
        test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
    }
    .passes_guarded(
        &PathBuf::from("unused.rs"),
        "pub fn hello() -> u32 { 42 }\n",
        &ExecGuard::new(&policy),
    )
    .await;
    assert!(
        matches!(out, Err(Error::Refused { ref target, .. }) if target == "rustc"),
        "the subject compile bypassed the exec policy, got {out:?}"
    );
}

/// A subject that does not compile fails the gate — it does not raise an error.
/// The two-crate split moved where that failure happens, so it is pinned here.
#[tokio::test]
async fn a_subject_that_does_not_compile_fails_the_gate() {
    assert!(
        !single("#[test] fn t() { assert_eq!(hello(), 42); }", "fn hello").await,
        "a non-compiling subject should fail the gate, not pass or error"
    );
}

/// O3 — the trace says *which* phase failed: `subject-compile`,
/// `criterion-compile`, or `test-run`.
///
/// One honest bound, recorded here rather than in prose only. A *neutralised
/// macro shadow* reports `test-run`, not `criterion-compile` — because once
/// `::core::assert!` wins, `assert!(false, ...)` is simply a false assertion, and
/// the run has become an ordinary failure rather than merely looking like one.
/// The phase marker separates the three places a gate can die; it does not
/// reconstruct which 0.8.0 bypass a run used to rely on.
#[tokio::test]
async fn the_trace_distinguishes_a_closed_bypass_from_an_ordinary_failure() {
    async fn phase_of(subject: &str, test_src: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s.db")).unwrap();
        let run = store.start_run("goal", "x.rs").unwrap();
        let policy = Policy::default();
        let guard = ExecGuard::new(&policy).tracing(&store, run, 1);
        let _ = Verification::RustTestPasses {
            test_src: test_src.into(),
        }
        .passes_guarded(&PathBuf::from("unused.rs"), subject, &guard)
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

    // A subject that compiles to nothing: `#![cfg(any())]` deletes its own crate.
    // Caught by the probe before the criterion is ever appended, so the trace
    // names the actual cause rather than a downstream symptom.
    assert_eq!(
        phase_of(
            "#![cfg(any())]\npub fn hello() -> u32 { 42 }\n",
            "#[test] fn t() { assert_eq!(hello(), 42); }"
        )
        .await,
        vec!["subject-emptied"],
        "a subject that deleted itself is not attributable in the trace"
    );

    // The blocked shadow: re-importing the prelude macros explicitly makes the
    // subject's `assert` ambiguous (E0659) rather than authoritative, so the
    // criterion does not compile beside it.
    assert_eq!(phase_of(SHADOWS_ASSERT, IMPOSSIBLE).await, vec!["criterion-compile"]);

    // An ordinary failure: everything compiled, the test ran and failed.
    assert_eq!(
        phase_of("pub fn hello() -> u32 { 41 }\n", "#[test] fn t() { assert_eq!(hello(), 42); }")
            .await,
        vec!["test-run"],
    );

    // The subject itself does not compile.
    assert_eq!(
        phase_of("fn hello", "#[test] fn t() { assert_eq!(hello(), 42); }").await,
        vec!["subject-compile"],
    );
}

/// F9 and NF3 — a compile-only gate defeated by self-deletion is attributable in
/// the trace too, and distinguishable from a file that simply does not compile.
#[tokio::test]
async fn the_trace_names_a_subject_that_deleted_its_own_items() {
    async fn phase_of(subject: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s.db")).unwrap();
        let run = store.start_run("goal", "x.rs").unwrap();
        let policy = Policy::default();
        let guard = ExecGuard::new(&policy).tracing(&store, run, 1);
        let _ = Verification::CompilesRust
            .passes_guarded(&PathBuf::from("unused.rs"), subject, &guard)
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

/// F5 — the caller-facing shape of `test_src` did not move. A macro the subject
/// legitimately exports must still reach the criterion; only the prelude names
/// the criterion depends on are protected.
#[tokio::test]
async fn a_macro_the_subject_legitimately_exports_still_reaches_the_criterion() {
    let subject = r#"
#[macro_export] macro_rules! answer { () => { 42 }; }
pub fn hello() -> u32 { 42 }
"#;
    assert!(
        single("#[test] fn t() { assert_eq!(answer!(), hello()); }", subject).await,
        "the subject's own exported macro was not visible to the criterion — the fix over-reached \
         and changed what a caller may write in test_src"
    );
}
