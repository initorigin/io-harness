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

use std::path::Path;
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
        let wrapped = unshare_argv(spec.argv, spec.workdir, spec.allow_network);
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
        // The exact wrapper this backend builds, mount setup included. Probing a
        // bare `unshare` would answer a question this backend stopped asking in
        // 0.40.0: a kernel can permit the namespace and still refuse the
        // remounts, and a probe that missed that would report `LinuxNamespaces`
        // for runs confining nothing.
        let dir = std::env::temp_dir();
        let argv = unshare_argv(&["true".to_string()], &dir, false);
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
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

/// Populate the mount namespace `--mount` created, then run the payload.
///
/// 0.40.0, and it exists because unsharing a mount namespace changes nothing on
/// its own. Before this, the backend created a namespace and remounted nothing
/// into it, so the filesystem view was identical to the host's and a write
/// outside the workdir landed — while `src/lib.rs` told a reader that "Linux
/// does the same through mount and network namespaces" as macOS. Only `--net`
/// was doing real work.
///
/// The script remounts the whole tree read-only and then binds back the two
/// places a command legitimately writes: the run's own workdir, and the system
/// temporary directory. The second is not a convenience — it is the same
/// allowance the macOS profile already makes for `/private/var/folders`, and
/// without it most toolchains fail on their first temporary file. Both are
/// stated in `docs/CONTRACT.md`.
///
/// **`/` is remounted read-only directly, and is deliberately not bound to
/// itself first.** The first version of this script did `mount --bind / /`
/// before the remount, on the assumption that a bind was needed to own the
/// mount. On a GitHub `ubuntu-latest` runner that bind fails with
/// `wrong fs type, bad option, bad superblock on /` and takes the whole setup
/// with it, so the probe reported failure and every contained run on Linux
/// silently took the portable floor — the confinement this module documents was
/// applied nowhere, and the matrix is what caught it. Measured on the runner:
/// with the bind removed, the remount alone leaves the tree genuinely
/// read-only — asserted by *attempting a write and having it refused*, not by
/// the mount's exit status — and the workdir rebinds writable over it.
///
/// **`sh` here is not a shell for the payload.** The command arrives as
/// positional parameters and leaves through `exec "$@"`, so nothing re-parses
/// it and a metacharacter in an argument stays an ordinary byte — the property
/// `src/tools/exec.rs` is built on. The shell runs the mounts, and then it is
/// gone.
///
/// A setup failure exits **125** after writing an `unshare:`-prefixed line, so
/// [`wrapper_failure`] classifies it as the wrapper failing rather than as the
/// payload's own non-zero exit. A wrapper failure reported as a failed command
/// is the exact confusion that made the original Linux breakage need a CI log.
const MOUNT_SETUP: &str = "\
set -e
fail() { echo \"unshare: sandbox mount setup failed: $1\" >&2; exit 125; }
mount --make-rprivate / 2>/dev/null || fail 'could not make / private'
mount -o remount,bind,ro / 2>/dev/null || fail 'could not remount / read-only'
for d in \"$1\" \"${TMPDIR:-/tmp}\"; do
  [ -d \"$d\" ] || continue
  mount --bind \"$d\" \"$d\" 2>/dev/null || fail \"could not bind $d\"
  mount -o remount,bind,rw \"$d\" 2>/dev/null || fail \"could not make $d writable\"
done
cd \"$1\" || fail 'could not enter the workdir'
shift
exec \"$@\"
";

/// The `unshare` argv this backend builds for a run, factored out so it is
/// unit-testable without spawning anything.
pub(crate) fn unshare_argv(inner: &[String], workdir: &Path, allow_network: bool) -> Vec<String> {
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
    // `sh -c <script> sh <workdir> <argv...>`: `$0` is `sh`, `$1` is the workdir
    // the script binds and enters, and `"$@"` after the shift is the payload.
    v.push("sh".into());
    v.push("-c".into());
    v.push(MOUNT_SETUP.into());
    v.push("sh".into());
    v.push(workdir.display().to_string());
    v.extend(inner.iter().cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_network_with_a_new_net_namespace() {
        let argv = unshare_argv(&["echo".into(), "hi".into()], Path::new("/w"), false);
        assert!(
            argv.contains(&"--net".into()),
            "net namespace must isolate network by default"
        );
        // The payload is the tail, after `sh -c <setup> sh <workdir>`. It is no
        // longer adjacent to `--`, because 0.40.0 put the mount setup in
        // between — this assertion was rewritten with that change rather than
        // to accommodate a failure.
        assert_eq!(
            &argv[argv.len() - 2..],
            &["echo".to_string(), "hi".to_string()],
            "the payload is passed through untouched, as the trailing arguments"
        );
    }

    /// The payload must arrive as positional parameters and leave through
    /// `exec "$@"`. If it were ever interpolated into the script, a
    /// metacharacter in an argument would become syntax — which is the one
    /// property `src/tools/exec.rs` is built on.
    #[test]
    fn the_payload_is_never_interpolated_into_the_setup_script() {
        let nasty = "; rm -rf /".to_string();
        let argv = unshare_argv(&["echo".into(), nasty.clone()], Path::new("/w"), false);
        let script = argv.iter().find(|a| a.contains("mount --bind")).unwrap();
        assert!(
            !script.contains("rm -rf"),
            "the argv must not reach the script text"
        );
        assert!(
            script.contains("exec \"$@\""),
            "and must leave through exec"
        );
        assert_eq!(argv.last().unwrap(), &nasty);
    }

    /// The setup binds the workdir back read-write after making the tree
    /// read-only. A script that did the first half only would confine writes by
    /// making every write fail, which is not the same claim at all.
    #[test]
    fn the_setup_makes_the_tree_read_only_and_binds_the_workdir_back() {
        let argv = unshare_argv(&["true".into()], Path::new("/w"), false);
        let script = argv.iter().find(|a| a.contains("mount --bind")).unwrap();
        assert!(script.contains("remount,bind,ro /"));
        assert!(script.contains("remount,bind,rw"));
        assert!(
            argv.contains(&"/w".to_string()),
            "the workdir is passed as a positional parameter, not baked in"
        );
    }

    #[test]
    fn allows_network_when_asked() {
        let argv = unshare_argv(&["echo".into()], Path::new("/w"), true);
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
