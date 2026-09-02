//! 0.74.0 — the Landlock rung's two syscall-level holes, at the level where they
//! are observable.
//!
//! The rung's own decisions are unit-tested beside them, on any host: the plan,
//! the ABI floor and the assembled BPF program are plain values. What cannot be
//! decided there is whether a real kernel, given the real filter, refuses a real
//! datagram — so that is what lives here, and it is `cfg`-gated to Linux for the
//! same reason `tests/exec_contained.rs` gates its namespace tests.
//!
//! The wiring check below is not gated. The seccomp filter's network rule is
//! only installed if the two spawn paths hand it the rule set's own answer, and
//! that is a line of glue in another module which a refactor can silently turn
//! back into "no rule" — a shape no Linux-only test would catch on the build
//! host.

/// H9 — a run that denied egress cannot complete a UDP `sendto` to an external
/// address.
///
/// Landlock's network vocabulary is `BIND_TCP` and `CONNECT_TCP`, so up to
/// 0.73.0 the rung a modern Ubuntu CI host takes confined the filesystem, denied
/// outbound TCP, and let a datagram socket carry anything anywhere. DNS is the
/// obvious channel and needs nothing installed; this test needs nothing
/// installed either, because `bash` opens the socket itself.
///
/// On 0.73.0 the command exits 0 and this fails. After the fix `socket` returns
/// `EPERM`, the redirection fails, and a non-interactive shell exits non-zero.
///
/// The control is the same command under a run that *permits* egress, in the
/// same test so it cannot be dropped: the rule is installed for the run whose
/// network the rung restricts and for no other, and a filter that refused both
/// would pass the first assertion while breaking every networked run.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn h9_an_egress_denying_run_cannot_send_a_udp_datagram_to_an_external_address() {
    use io_harness::sandbox::{select, RunSpec, Sandbox};
    use io_harness::{SandboxConfig, SandboxLimits};

    let dir = tempfile::tempdir().unwrap();
    // `bash`'s own `/dev/udp` is `socket(AF_INET, SOCK_DGRAM)`, `connect` and
    // `write` — no resolver, no helper binary, nothing a payload would have to
    // bring with it. `1.1.1.1:53` is off-host by construction.
    let argv: Vec<String> = ["bash", "-c", "exec 3<>/dev/udp/1.1.1.1/53; printf x >&3"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let limits = SandboxLimits::default();

    let denied = select(&SandboxConfig::new())
        .run(RunSpec::new(&argv, dir.path(), &limits).with_network(false))
        .await
        .unwrap();

    // The run's own report, not the selection's: a rung that probes as available
    // can still decline a particular run, and asserting against the probe demands
    // a boundary from a run that honestly reported it did not get one.
    if !denied.backend.denies_egress() {
        assert!(
            std::env::var("CI").is_err(),
            "this runner reported {:?}, which claims no network boundary. On CI that is a \
             failure and not a skip: the assertion below would pass without denying anything.",
            denied.backend
        );
        eprintln!(
            "skipped: this host reports {:?}, which claims no network boundary",
            denied.backend
        );
        return;
    }
    assert!(
        !denied.success(),
        "a run that denied egress sent a UDP datagram off-host under {:?}: {denied:?}",
        denied.backend
    );

    // The control. A run that may reach the network keeps every socket it ever
    // had, and the same command must still work.
    let permitted = select(&SandboxConfig::new())
        .run(RunSpec::new(&argv, dir.path(), &limits).with_network(true))
        .await
        .unwrap();
    assert!(
        permitted.success(),
        "a run that permits egress must keep its datagram sockets — refusing both is not a \
         boundary, it is a broken toolchain: {permitted:?}"
    );
}

/// H9's wiring, on every host: both spawn paths install the filter with the rule
/// set's own network answer.
///
/// The rule is conditional by design — a run that may reach the network keeps
/// its sockets — so the whole fix is one argument at two call sites, and passing
/// a constant there is the fail-open mistake that looks correct in review. The
/// filter itself is asserted in `sandbox::seccomp`'s own tests, which only a
/// Linux leg compiles.
#[test]
fn h9_both_spawn_paths_install_the_filter_with_the_plans_own_answer() {
    let sources = [
        ("src/sandbox.rs", include_str!("../src/sandbox.rs")),
        (
            "src/sandbox/linux.rs",
            include_str!("../src/sandbox/linux.rs"),
        ),
    ];
    let mut calls = 0;
    for (path, text) in sources {
        for line in text.lines().filter(|l| l.contains("seccomp::install(")) {
            calls += 1;
            let line = line.trim();
            assert!(
                !line.contains("seccomp::install()"),
                "{path}: `{line}` installs the filter with no network answer, so the run's \
                 datagram sockets are never refused"
            );
            assert!(
                !line.contains("seccomp::install(false)")
                    && !line.contains("seccomp::install(true)"),
                "{path}: `{line}` hands the filter a constant instead of the rule set's own \
                 answer — read it from the plan, as `Plan::restricts_network`"
            );
        }
    }
    // The negative control: a rename that moved the call out of these two files
    // would otherwise leave this test asserting nothing at all.
    assert_eq!(
        calls, 2,
        "the filter is installed from exactly two places — the contained-command path in \
         sandbox.rs and the Landlock rung in sandbox/linux.rs"
    );
}
