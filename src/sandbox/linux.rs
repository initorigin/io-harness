//! Linux native backend: user/mount/pid/**net** namespaces via `unshare`, plus
//! the shared rlimit + RSS caps. The new network namespace is a *hard* boundary
//! — a process in an empty net namespace has no route out — which is stronger
//! than the floor's best-effort env strip.
//!
//! cfg-gated to `target_os = "linux"`; the argv construction is unit-tested on
//! the macOS build host under its cfg. Seccomp tightening is layered by the
//! kernel default under the unprivileged user namespace.
//!
//! **The wrapper is probed before it is promised.** Until 0.9.1 this backend
//! never checked for `unshare`, and [`super::select`] returned it unconditionally
//! on Linux; on a kernel with unprivileged user namespaces restricted — Ubuntu
//! 24.04 ships `kernel.apparmor_restrict_unprivileged_userns=1`, and the CI
//! runner is one — every wrapped spawn failed, and the caller was told its code
//! had failed verification. Now the wrapper is spawned once per process and, if
//! it does not work, this backend degrades to the [portable
//! floor](super::FloorSandbox) and *reports* [`Backend::PortableFloor`], the same
//! honesty [`super::windows`] already practises. The backend that ran is in the
//! trace, so a degraded run is auditable rather than silent — and a wrapper that
//! fails anyway is [`crate::Error::Sandbox`], never a failed verification.

use std::process::Stdio;

use super::{run_capped, Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::{Error, Result};

/// The Linux namespaces backend.
pub struct LinuxSandbox;

impl Sandbox for LinuxSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        if !unshare_works() {
            // No usable namespaces on this host: take the floor rather than
            // failing every run, and report the floor rather than naming an
            // isolation that was never applied.
            return run_capped(Backend::PortableFloor, spec, |_cmd| {}).await;
        }
        // Wrap in `unshare`: new user (map root), mount, pid, and — when network
        // is denied — a new empty network namespace with no route out.
        let wrapped = unshare_argv(spec.argv, spec.allow_network);
        let wspec = RunSpec {
            argv: &wrapped,
            workdir: spec.workdir,
            limits: spec.limits,
            allow_network: spec.allow_network,
        };
        let outcome = run_capped(Backend::LinuxNamespaces, wspec, |_cmd| {}).await?;
        match wrapper_failure(&outcome) {
            Some(reason) => Err(Error::Sandbox { reason }),
            None => Ok(outcome),
        }
    }

    fn backend(&self) -> Backend {
        if unshare_works() {
            Backend::LinuxNamespaces
        } else {
            Backend::PortableFloor
        }
    }
}

/// Does the exact wrapper this backend builds actually work on this host?
///
/// A real spawn, not a sysctl read: the restriction shows up as `unshare`
/// failing to write `/proc/self/uid_map`, and only an attempt sees that. Probed
/// with `--net`, the strictest form — if that works, the network-allowed subset
/// does too.
///
/// One spawn per process, not per run: the kernel's answer does not change under
/// a running process.
fn unshare_works() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        std::process::Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--pid",
                "--fork",
                "--net",
                "--",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Did the *wrapper* fail, rather than the payload it wrapped? `unshare` reports
/// its own setup failures on stderr with an `unshare:` prefix and never reaches
/// the payload, so nothing the caller asked for ran.
///
/// This distinction is the whole point: a wrapper failure reported as
/// `Ok(false)` is indistinguishable from "the model's code does not compile",
/// which is exactly why the Linux breakage needed a CI log to diagnose.
fn wrapper_failure(outcome: &SandboxOutcome) -> Option<String> {
    let stderr = outcome.stderr.trim();
    (!outcome.success() && stderr.starts_with("unshare:"))
        .then(|| format!("the namespace wrapper failed, the command never ran: {stderr}"))
}

/// The `unshare` argv this backend builds for a run, factored out so it is
/// unit-testable without spawning anything.
pub(crate) fn unshare_argv(inner: &[String], allow_network: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "unshare".into(),
        "--user".into(),
        "--map-root-user".into(),
        "--mount".into(),
        "--pid".into(),
        "--fork".into(),
    ];
    if !allow_network {
        v.push("--net".into());
    }
    v.push("--".into());
    v.extend(inner.iter().cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_network_with_a_new_net_namespace() {
        let argv = unshare_argv(&["echo".into(), "hi".into()], false);
        assert!(
            argv.contains(&"--net".into()),
            "net namespace must isolate network by default"
        );
        assert!(argv
            .windows(2)
            .any(|w| w == ["--".to_string(), "echo".to_string()]));
    }

    #[test]
    fn allows_network_when_asked() {
        let argv = unshare_argv(&["echo".into()], true);
        assert!(
            !argv.contains(&"--net".into()),
            "no net namespace when network is allowed"
        );
    }

    #[test]
    fn the_reported_backend_is_the_one_the_host_can_actually_run() {
        // Never name an isolation that was not applied: `LinuxNamespaces` only
        // when the wrapper works, the floor otherwise.
        let expected = if unshare_works() {
            Backend::LinuxNamespaces
        } else {
            Backend::PortableFloor
        };
        assert_eq!(LinuxSandbox.backend(), expected);
    }

    // The degrade path itself, which runs on any host without a working
    // `unshare` — including the macOS build host, where the binary does not
    // exist at all, and the restricted-userns CI runner this release is about.
    #[tokio::test]
    async fn degrades_to_the_floor_when_the_wrapper_does_not_work() {
        if unshare_works() {
            return; // this host has real namespaces; nothing to degrade to
        }
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["sh".into(), "-c".into(), "echo hi".into()];
        let out = LinuxSandbox
            .run(RunSpec {
                argv: &argv,
                workdir: dir.path(),
                limits: &crate::sandbox::SandboxLimits::default(),
                allow_network: false,
            })
            .await
            .unwrap();
        assert!(out.success(), "a degraded run must still run, got {out:?}");
        assert_eq!(out.backend, Backend::PortableFloor);
        assert!(out.stdout.contains("hi"));
    }

    #[test]
    fn a_wrapper_failure_is_not_a_verification_failure() {
        let fail = |stderr: &str, code: Option<i32>| SandboxOutcome {
            backend: Backend::LinuxNamespaces,
            argv: vec!["unshare".into()],
            exit_code: code,
            cap_hit: None,
            stdout: String::new(),
            stderr: stderr.into(),
        };
        // The wrapper never reached the payload — that is a sandbox error.
        assert!(wrapper_failure(&fail(
            "unshare: write failed /proc/self/uid_map: Operation not permitted\n",
            Some(1)
        ))
        .is_some());
        // The payload ran and failed to compile — that is a verdict, not an error.
        assert!(wrapper_failure(&fail("error[E0308]: mismatched types", Some(1))).is_none());
        // A payload that succeeded is never a wrapper failure.
        assert!(wrapper_failure(&fail("", Some(0))).is_none());
    }
}
