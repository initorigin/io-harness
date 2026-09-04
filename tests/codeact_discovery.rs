//! Interpreter discovery, the configuration boundary, and the structural claims
//! — F8, F14, N5, N6 and N7.
//!
//! Two of these run nothing. A claim like "no CodeAct code path calls a tool
//! implementation directly" cannot be checked by running a program — an
//! implementation that bypassed the gate would pass every behavioural test in
//! this release — so it is checked against the source's own text, the way
//! `tests/one_runtime_path.rs` and `tests/mcp_server_dispatch.rs` already check
//! claims of the same shape.
#![cfg(feature = "codeact")]

use io_harness::{Config, CODEACT_CANDIDATES, CODEACT_MIN_PYTHON};

/// One of this repository's own source files, with line endings normalised.
///
/// The `\r` strip is the whole reason this helper exists rather than a bare
/// `read_to_string`. A Windows checkout has CRLF endings, so a multi-line needle
/// written with `\n` matches on every developer machine here and on both unix CI
/// legs, and fails only on `test (windows-latest, …)` — which is exactly where it
/// did fail, after the local suite, three clippy polarities and two unix
/// platforms were all green.
fn source(relative: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|e| panic!("{relative} is readable: {e}"))
        .replace("\r\n", "\n")
}

// ---------------------------------------------------------------------------
// F8 — every candidate is probed, and a name is never trusted
// ---------------------------------------------------------------------------

/// F8. A `python` that answers 2.7 is rejected by number, and one that answers
/// 3.11 is accepted — the same file, the same name, two different answers.
///
/// This is the case a name alone gets wrong, and it is why discovery runs the
/// candidate rather than reading it off `PATH`. Unix only: it needs an executable
/// script, and the point being made is about the probe rather than about a
/// platform.
#[cfg(unix)]
#[tokio::test]
async fn a_candidate_is_judged_by_what_it_answers_and_not_by_its_name() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let fake = |name: &str, reply: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{reply}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };

    // Named `python` and answering 2.7: rejected, and the run is told why.
    let two = fake("python", "2.7");
    let dir_two = tempfile::tempdir().unwrap();
    let refused = discovery_detail(&dir_two, &two).await;
    assert!(
        refused.contains("2.7"),
        "the probe should reject it by number and say so; it said {refused:?}"
    );

    // The control: the identical mechanism accepting a candidate that answers a
    // supported version. Without it, "rejected" would also be true of a probe
    // that rejects everything.
    let three = fake("python-ish", "3.11");
    let dir_three = tempfile::tempdir().unwrap();
    let accepted = discovery_detail(&dir_three, &three).await;
    assert!(
        accepted.contains("Python 3.11"),
        "a candidate answering 3.11 is accepted; it said {accepted:?}"
    );
}

/// Run one turn with `interpreter` named and return the `Program` event's detail.
#[cfg(unix)]
async fn discovery_detail(ws: &tempfile::TempDir, interpreter: &std::path::Path) -> String {
    use std::sync::Mutex;

    use io_harness::provider::{CompletionRequest, CompletionResponse};
    use io_harness::{
        run_with_observed, ApproveAll, CodeActConfig, EventKind, Flow, Observer, Provider,
        RunEvent, Store, TaskContract,
    };

    struct Silent;
    impl Provider for Silent {
        async fn complete(&self, _: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            Ok(CompletionResponse::default())
        }
    }

    #[derive(Default)]
    struct First(Mutex<Option<String>>);
    impl Observer for First {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Program { detail, .. } = &event.kind {
                let mut slot = self.0.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(detail.clone());
                }
            }
            Flow::Continue
        }
    }

    let store = Store::memory().unwrap();
    let seen = First::default();
    run_with_observed(
        &TaskContract::workspace("probe", ws.path())
            .with_max_steps(1)
            .with_codeact(CodeActConfig::default().with_interpreter(interpreter)),
        &Silent,
        &store,
        &io_harness::policy::Policy::permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();
    let detail = seen.0.lock().unwrap().clone();
    detail.expect("discovery emits an event either way")
}

/// F8's constants, pinned so a later edit to either is a deliberate one. `python`
/// is second because Windows installations and runner images provide it where
/// `python3` may be absent, and it is exactly the name the probe above exists to
/// distrust.
#[test]
fn the_candidate_order_and_the_probed_minimum_are_what_the_documentation_says() {
    assert_eq!(CODEACT_CANDIDATES, ["python3", "python"]);
    assert_eq!(CODEACT_MIN_PYTHON, (3, 8));
}

// ---------------------------------------------------------------------------
// F14 — a cloned repository does not choose the interpreter
// ---------------------------------------------------------------------------

/// F14. `[codeact]` is refused at project scope, naming itself, because
/// `interpreter` is a program on this machine that every model-written program is
/// handed to.
#[test]
fn the_codeact_table_is_refused_at_project_scope() {
    let err = Config::from_toml("[codeact]\ninterpreter = \"/opt/not-python\"\n")
        .expect_err("a project-scoped file may not choose the interpreter");
    let text = err.to_string();
    assert!(
        text.contains("codeact"),
        "the refusal names the table: {text}"
    );

    // Two controls. A file that declares no `[codeact]` is accepted, so the
    // refusal above is the rule firing rather than the parser failing…
    let plain = Config::from_toml("[run]\nmax_steps = 3\n")
        .expect("an ordinary project file is still accepted");
    assert!(plain.codeact().is_none());

    // …and the bounds are refused with the table rather than separately, because
    // a rule that permits half a table is one a reader holds two halves of.
    let bounds = Config::from_toml("[codeact]\nmax_callbacks = 4\n")
        .expect_err("the whole table is refused, not only the interpreter key");
    assert!(bounds.to_string().contains("codeact"));
}

// ---------------------------------------------------------------------------
// F4 / N5 / N6 / N7 — claims no behavioural test can make
// ---------------------------------------------------------------------------

/// F4's structural half. There is exactly one path from a program's callback to a
/// tool, and it is `dispatch` itself.
///
/// The failure this guards is the one the release is most able to ship: a
/// purpose-built dispatcher inside the CodeAct module would compile more easily
/// than re-entering a twenty-nine-parameter `async fn`, would pass every
/// behavioural test in this release, and would bypass the gate. So the module is
/// held to knowing nothing about tools at all.
#[test]
fn no_codeact_code_path_reaches_a_tool_implementation() {
    let module = source("src/codeact.rs");
    for forbidden in [
        "crate::tools::fs",
        "crate::tools::exec",
        "crate::tools::shell",
        "crate::tools::git",
        "Exec::new",
        "FsTool",
        "dispatch(",
    ] {
        assert!(
            !module.contains(forbidden),
            "src/codeact.rs must not reach a tool; it names {forbidden}"
        );
    }

    // The positive half, and the control: the arm that answers a callback really
    // does call `dispatch` again, so the absence above is one path rather than
    // none.
    let arm = source("src/run/dispatch.rs");
    assert!(
        arm.contains("Box::pin(dispatch("),
        "the run_program arm must re-enter dispatch itself"
    );
}

/// N6. Nothing about a program leaves the machine. The interpreter is local and
/// the callbacks are a pipe, so a network client in this module would be a
/// channel nobody asked for.
#[test]
fn a_program_sends_nothing_anywhere() {
    let module = source("src/codeact.rs");
    for forbidden in ["reqwest", "http://", "https://", "TcpStream", "crate::net"] {
        assert!(
            !module.contains(forbidden),
            "src/codeact.rs must not speak to the network; it names {forbidden}"
        );
    }
    // And the program is handed no proxy, which is the other half: it reaches the
    // network only through this crate's own network-governed tools. The call form
    // is what is checked, not the word — an earlier version of this assertion
    // matched the comment that explains the absence and was red on every build,
    // which is the same failure its sibling below already documents.
    assert!(
        !module.contains("proxy_env("),
        "a program is given no proxy environment of its own"
    );
}

/// N7. The feature is gated the way `browser`, `otel` and `mcp-server` are gated,
/// so a reader who knows one knows all four.
#[test]
fn the_feature_follows_the_gate_shape_the_others_already_use() {
    let lib = source("src/lib.rs");
    for pair in [
        "#[cfg(feature = \"codeact\")]\n#[cfg_attr(docsrs, doc(cfg(feature = \"codeact\")))]\npub mod codeact;",
        "#[cfg(feature = \"codeact\")]\n#[cfg_attr(docsrs, doc(cfg(feature = \"codeact\")))]\npub use codeact::{",
    ] {
        assert!(
            lib.contains(pair),
            "src/lib.rs should gate codeact exactly as the other features are gated; missing:\n{pair}"
        );
    }

    // The manifest's half: no dependency, and no feature of one either.
    let manifest = source("Cargo.toml");
    assert!(
        manifest.contains("\ncodeact = []\n"),
        "codeact must enable nothing at all"
    );
}

/// N5. Nothing is downloaded and nothing is installed, ever — which is the whole
/// of why a host interpreter is not a dependency of this crate.
///
/// Checked as what the code does rather than as what it says. An earlier version
/// of this test grepped for the word `pip` and failed on the word `pipe`, which is
/// the failure mode of asserting against prose: the module is mostly comments, so
/// a vocabulary check tests the comments. What actually settles the claim is that
/// every process this module starts is the resolved interpreter itself — there is
/// no `Command::new` with a program this crate chose by name, so there is nothing
/// for a package manager to be.
#[test]
fn every_process_this_module_starts_is_the_resolved_interpreter() {
    let module = source("src/codeact.rs");
    assert!(
        !module.contains("Command::new(\""),
        "src/codeact.rs must not spawn a program named by a literal"
    );
    // The control: it does spawn something, so the absence above is a constrained
    // spawn rather than no spawn at all.
    assert!(
        module.contains("Command::new(path)") && module.contains("Command::new(&argv[0])"),
        "the probe and the program are both spawned from a resolved path"
    );
    // And `ensurepip` — the one form that would install into an interpreter
    // already present, and so would not need a program name of its own.
    assert!(!module.contains("ensurepip"), "nothing is installed");
}
