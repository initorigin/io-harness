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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::{run_capped, Backend, ExecMode, RunSpec, Sandbox, SandboxOutcome};
use crate::error::{Error, Result};

/// The first Landlock ABI carrying `LANDLOCK_ACCESS_NET_CONNECT_TCP`, and
/// therefore the first that can deny egress. Linux 6.7.
///
/// Below it Landlock confines the filesystem and says nothing about the network,
/// which is why [`rung`] refuses to hand it a run that denies egress.
pub(crate) const LANDLOCK_NET_ABI: u32 = 4;

/// What each rung's probe answered on this host.
///
/// Plain data, deliberately: which rung a host takes is then a *function* of
/// four answers rather than a nest of conditions at the call site, and the whole
/// chain can be decided in a table test without a Linux kernel anywhere near it.
/// Every field is filled by attempting the restriction, never by reading a
/// sysctl, a `/sys/kernel/security/lsm` line or a package's presence — 0.40.0's
/// Linux breakage survived three matrix runs behind exactly that shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rungs {
    /// The kernel's Landlock ABI as it reported it, or `None` when this host has
    /// no usable Landlock at all. A version rather than a boolean because the
    /// network rules arrive at a known version and the chain has to know.
    pub(crate) landlock_abi: Option<u32>,
    /// A `bwrap` on `PATH` whose probe spawn of the exact wrapper this backend
    /// builds succeeded.
    pub(crate) bubblewrap: bool,
    /// The `unshare` wrapper this crate has built since 0.9.1, spawned and
    /// successful — [`unshare_works`].
    pub(crate) unshare: bool,
}

/// Which rung this host takes for a run with this egress requirement.
///
/// Strength order, and it is the roadmap's: Landlock, bubblewrap, namespaces,
/// floor. The floor is not a rung anyone probes for — it is what is left.
///
/// **The one rule that can send a host below its strongest available primitive**
/// is the egress requirement. A run that denies egress may not be given a rung
/// that cannot deny egress, so a kernel with Landlock below
/// [`LANDLOCK_NET_ABI`] falls through to bubblewrap or to the namespace rung —
/// both of which have a network namespace — rather than taking a filesystem-only
/// rung and leaving the run's own policy unenforced. Without that rule the chain
/// would be ordered but dishonest, which is the failure mode this crate has
/// already shipped once.
///
/// The [`ExecMode`] is deliberately not a parameter: every rung renders all three
/// modes, so the mode decides what goes in a rung's rule set and never which rung
/// is chosen. The table test asserts that invariance rather than leaving it as a
/// claim in this sentence.
pub(crate) fn rung(probes: Rungs, deny_egress: bool) -> Backend {
    if let Some(abi) = probes.landlock_abi {
        if !deny_egress || abi >= LANDLOCK_NET_ABI {
            return Backend::LinuxLandlock;
        }
    }
    if probes.bubblewrap {
        return Backend::LinuxBubblewrap;
    }
    if probes.unshare {
        return Backend::LinuxNamespaces;
    }
    Backend::PortableFloor
}

/// The Linux namespaces backend.
pub struct LinuxSandbox;

impl Sandbox for LinuxSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        // The chain, in strength order. `rung` decides; this matches on what it
        // decided. A rung that cannot build its own apparatus for *this* run —
        // a rule set the kernel refuses, a helper that is not there — falls to
        // the next one rather than failing the run, and every one of them
        // reports the backend that was actually applied.
        if rung(probes(), !spec.allow_network) == Backend::LinuxLandlock {
            if let Some(outcome) = landlock_run(&spec).await {
                return outcome;
            }
        }
        if !unshare_works() {
            // No usable namespaces on this host: take the floor rather than
            // failing every run, and report the floor rather than naming an
            // isolation that was never applied.
            return run_capped(Backend::PortableFloor, spec, |_cmd| {}).await;
        }
        // Wrap in `unshare`: new user (map root), mount, pid, and — when network
        // is denied — a new empty network namespace with no route out.
        let wrapped = unshare_argv(
            spec.argv,
            spec.workdir,
            spec.allow_network,
            spec.mode,
            spec.writable_roots,
        );
        let wspec = RunSpec::new(&wrapped, spec.workdir, spec.limits)
            .with_network(spec.allow_network)
            .with_mode(spec.mode)
            .with_writable_roots(spec.writable_roots);
        let outcome = run_capped(Backend::LinuxNamespaces, wspec, |_cmd| {}).await?;
        match wrapper_failure(&outcome) {
            Some(reason) => Err(Error::Sandbox { reason }),
            None => Ok(outcome),
        }
    }

    /// The rung this host takes, reported **conservatively**.
    ///
    /// The trait method has no run to read, and the chain's one run-dependent
    /// input is the egress requirement — so this answers for the stricter of the
    /// two: what a run that *denies* egress would get. A host whose Landlock
    /// predates the network rules therefore reports the namespace rung here
    /// while a run permitting egress would really take Landlock.
    ///
    /// Under-reporting rather than over-reporting, on purpose. The rule this
    /// crate keeps is that a backend never names an isolation it did not apply;
    /// naming a weaker one than was applied costs a reader precision, naming a
    /// stronger one costs them the boundary. The exact per-command answer is in
    /// [`SandboxOutcome::backend`] and in the `SandboxEvent` rows either way.
    fn backend(&self) -> Backend {
        rung(probes(), true)
    }
}

/// Ask every rung, once per process.
///
/// Cached inside each probe rather than here, because the probes are three
/// independent questions and a host can gain none of these answers while a
/// process is running.
fn probes() -> Rungs {
    Rungs {
        landlock_abi: landlock_abi(),
        // The bubblewrap rung is not built yet, so the chain must not be able to
        // select it: a rung `rung` names and `run` cannot deliver would report
        // an isolation that was never applied, which is the one thing no part of
        // this module may do.
        bubblewrap: false,
        unshare: unshare_works(),
    }
}

#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<u32> {
    super::landlock::abi()
}

#[cfg(not(target_os = "linux"))]
fn landlock_abi() -> Option<u32> {
    None
}

/// Run `spec` under a Landlock rule set, or return `None` to fall to the next
/// rung.
///
/// `None` is not a failure of the run — it is this rung declining. A kernel that
/// answers the version query and then refuses a rule set, or a granted path that
/// cannot be opened, means the confinement this rung would report was not
/// installed; the honest response is the next rung down, not a run that reports
/// Landlock and enforces nothing.
#[cfg(target_os = "linux")]
async fn landlock_run(spec: &RunSpec<'_>) -> Option<Result<SandboxOutcome>> {
    use std::os::fd::RawFd;

    let abi = super::landlock::abi()?;
    let tmp = std::env::temp_dir();
    let plan = super::landlock::plan(
        abi,
        spec.mode,
        !spec.allow_network,
        spec.workdir,
        spec.writable_roots,
        &tmp,
    );
    let ruleset = match super::landlock::Ruleset::build(&plan) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "sandbox: the Landlock rule set could not be built ({e}); taking the next rung"
            );
            return None;
        }
    };
    let fd: RawFd = ruleset.raw();

    let wspec = RunSpec::new(spec.argv, spec.workdir, spec.limits)
        .with_network(spec.allow_network)
        .with_mode(spec.mode)
        .with_writable_roots(spec.writable_roots);

    // The argv is the caller's own, untouched: this rung wraps the payload in
    // nothing. What runs between fork and exec is two syscalls with no
    // allocation, which is why the rule set was built above rather than here.
    let outcome = run_capped(Backend::LinuxLandlock, wspec, move |cmd| {
        // SAFETY: the closure runs in the forked child before `exec`. It
        // allocates nothing, takes no lock and calls only `prctl` and one
        // `landlock_restrict_self`, both async-signal-safe. `fd` is owned by
        // `ruleset`, which outlives the spawn below.
        unsafe {
            cmd.pre_exec(move || unsafe { super::landlock::restrict_self(fd) });
        }
    })
    .await;
    drop(ruleset);
    Some(outcome)
}

#[cfg(not(target_os = "linux"))]
async fn landlock_run(_spec: &RunSpec<'_>) -> Option<Result<SandboxOutcome>> {
    None
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
        let argv = unshare_argv(
            &["true".to_string()],
            &dir,
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
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
/// The script remounts the whole tree read-only and then binds back the places a
/// command legitimately writes: the run's own workdir (unless the
/// [`ExecMode`](crate::ExecMode) withholds it), the writable roots the run
/// resolved — a toolchain's own caches, since 0.46.0 — and the system temporary
/// directory. The last is not a convenience: it is the same allowance the macOS
/// profile already makes for `/private/var/folders`, and without it most
/// toolchains fail on their first temporary file. All three are stated in
/// `docs/CONTRACT.md`.
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
/// **The writable set is a counted argument list, not a fixed pair (0.46.0).**
/// `$1` is the workdir to enter, `$2` is how many writable roots follow, and the
/// payload begins after them — so the same script serves a run with no extra
/// grants and one whose toolchain writes to three caches, without the count ever
/// being inferred from the argv's shape. The workdir is in that list only when
/// the [`ExecMode`](crate::ExecMode) grants it, which is what makes `read-only`
/// a mode here rather than a label: the process still `cd`s into the workspace
/// and still cannot write to it.
const MOUNT_SETUP: &str = "\
set -e
fail() { echo \"unshare: sandbox mount setup failed: $1\" >&2; exit 125; }
rw() {
  [ -d \"$1\" ] || return 0
  mount --bind \"$1\" \"$1\" 2>/dev/null || fail \"could not bind $1\"
  mount -o remount,bind,rw \"$1\" 2>/dev/null || fail \"could not make $1 writable\"
}
mount --make-rprivate / 2>/dev/null || fail 'could not make / private'
mount -o remount,bind,ro / 2>/dev/null || fail 'could not remount / read-only'
wd=\"$1\"; shift
n=\"$1\"; shift
while [ \"$n\" -gt 0 ]; do rw \"$1\"; shift; n=$((n-1)); done
rw \"${TMPDIR:-/tmp}\"
cd \"$wd\" || fail 'could not enter the workdir'
exec \"$@\"
";

/// The `unshare` argv this backend builds for a run, factored out so it is
/// unit-testable without spawning anything.
pub(crate) fn unshare_argv(
    inner: &[String],
    workdir: &Path,
    allow_network: bool,
    mode: ExecMode,
    writable_roots: &[PathBuf],
) -> Vec<String> {
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
    // `sh -c <script> sh <workdir> <n> <root>... <argv...>`: `$0` is `sh`, `$1` is
    // the workdir the script enters, `$2` is how many writable roots follow, and
    // the payload is what remains after them.
    v.push("sh".into());
    v.push("-c".into());
    v.push(MOUNT_SETUP.into());
    v.push("sh".into());
    v.push(workdir.display().to_string());

    let mut writable: Vec<String> = Vec::new();
    if mode != ExecMode::ReadOnly {
        writable.push(workdir.display().to_string());
    }
    writable.extend(writable_roots.iter().map(|p| p.display().to_string()));
    v.push(writable.len().to_string());
    v.extend(writable);

    v.extend(inner.iter().cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F3 — the chain's order, decided without a host.
    ///
    /// Every combination of the four probe answers against both egress answers.
    /// The strongest serviceable rung is returned, and `PortableFloor` only when
    /// nothing above it can serve.
    #[test]
    fn the_chain_takes_the_strongest_rung_that_can_serve_the_run() {
        // (landlock_abi, bubblewrap, unshare, deny_egress) -> expected
        let table = [
            // Landlock with the network rules serves either kind of run.
            ((Some(4), true, true, true), Backend::LinuxLandlock),
            ((Some(4), true, true, false), Backend::LinuxLandlock),
            ((Some(6), false, false, true), Backend::LinuxLandlock),
            ((Some(4), false, false, true), Backend::LinuxLandlock),
            // Landlock below the network ABI still serves a run that permits
            // egress — the filesystem half is all such a run needs.
            ((Some(1), true, true, false), Backend::LinuxLandlock),
            ((Some(3), false, false, false), Backend::LinuxLandlock),
            // ...and must NOT serve one that denies it. This is the honesty
            // rule, and it is the only reason a host takes a rung weaker than
            // its strongest available primitive.
            ((Some(3), true, true, true), Backend::LinuxBubblewrap),
            ((Some(3), false, true, true), Backend::LinuxNamespaces),
            ((Some(1), false, false, true), Backend::PortableFloor),
            // No Landlock at all: the rest of the chain in order.
            ((None, true, true, true), Backend::LinuxBubblewrap),
            ((None, true, false, false), Backend::LinuxBubblewrap),
            ((None, false, true, true), Backend::LinuxNamespaces),
            ((None, false, true, false), Backend::LinuxNamespaces),
            // Nothing above the floor.
            ((None, false, false, true), Backend::PortableFloor),
            ((None, false, false, false), Backend::PortableFloor),
        ];

        for ((landlock_abi, bubblewrap, unshare, deny_egress), expected) in table {
            let probes = Rungs {
                landlock_abi,
                bubblewrap,
                unshare,
            };
            assert_eq!(
                rung(probes, deny_egress),
                expected,
                "probes {probes:?}, deny_egress {deny_egress}"
            );
        }
    }

    /// F3, second half — the mode decides what goes *in* a rung's rule set and
    /// never *which* rung is chosen. Asserted rather than left as a claim in
    /// `rung`'s doc comment, because a mode that leaked into the decision would
    /// make a `ReadOnly` run silently take a different backend from the
    /// `WorkspaceWrite` run beside it.
    #[test]
    fn the_mode_does_not_decide_which_rung_a_host_takes() {
        // `rung` does not take an `ExecMode` at all, which is the strongest
        // available form of this assertion; what remains to check is that the
        // three modes reach it through one call site each producing the same
        // answer for one host.
        // Only one rung available, so this test cannot pass or fail on the
        // chain's *order* — that is F3's first half, and a criterion that
        // asserts two things is a criterion whose failure does not say which.
        let probes = Rungs {
            landlock_abi: Some(4),
            bubblewrap: false,
            unshare: false,
        };
        let under = |_mode: ExecMode| rung(probes, true);
        assert_eq!(under(ExecMode::ReadOnly), Backend::LinuxLandlock);
        assert_eq!(under(ExecMode::WorkspaceWrite), Backend::LinuxLandlock);
        assert_eq!(under(ExecMode::FullAccess), Backend::LinuxLandlock);
    }

    /// The rung a host takes must never be a backend that belongs to another
    /// platform or to a rung the chain does not contain. Cheap, and it is what
    /// catches a variant added to `Backend` and wired into the chain by
    /// accident.
    #[test]
    fn the_chain_only_ever_returns_a_linux_rung_or_the_floor() {
        for abi in [None, Some(1), Some(3), Some(4), Some(6)] {
            for bubblewrap in [false, true] {
                for unshare in [false, true] {
                    for deny in [false, true] {
                        let got = rung(
                            Rungs {
                                landlock_abi: abi,
                                bubblewrap,
                                unshare,
                            },
                            deny,
                        );
                        assert!(
                            matches!(
                                got,
                                Backend::LinuxLandlock
                                    | Backend::LinuxBubblewrap
                                    | Backend::LinuxNamespaces
                                    | Backend::PortableFloor
                            ),
                            "the Linux chain returned {got:?}"
                        );
                    }
                }
            }
        }
    }

    /// The Landlock rung's enforcement arms.
    ///
    /// These live here rather than in `tests/` for one reason and it is a
    /// deliberate one: a criterion that pins a rung has to be able to *reach*
    /// that rung, and the alternative — an environment variable the production
    /// selection path reads — would be an ambient, attacker-reachable way to
    /// downgrade containment. A crate-internal test needs no such seam.
    ///
    /// Every one of them returns early on a host with no usable Landlock, which
    /// is every developer machine that is not Linux and every kernel before
    /// 5.13. That is a skip, and a skip states its reason rather than passing
    /// quietly — 0.40.0's egress tests reported success for three matrix runs
    /// while stepping over the thing they existed to assert.
    #[cfg(target_os = "linux")]
    mod landlock_rung {
        use super::*;
        use crate::sandbox::SandboxLimits;

        /// Run `argv` under the Landlock rung specifically, or `None` when this
        /// host has no Landlock to pin.
        async fn pinned(
            argv: &[String],
            workdir: &Path,
            mode: ExecMode,
            allow_network: bool,
            writable: &[PathBuf],
        ) -> Option<SandboxOutcome> {
            if landlock_abi().is_none() {
                eprintln!("skipped: this host has no usable Landlock");
                return None;
            }
            let limits = SandboxLimits::none();
            let spec = RunSpec::new(argv, workdir, &limits)
                .with_network(allow_network)
                .with_mode(mode)
                .with_writable_roots(writable);
            let outcome = landlock_run(&spec).await?.expect("the rung must run");
            assert_eq!(
                outcome.backend,
                Backend::LinuxLandlock,
                "the rung under test must be the rung that ran"
            );
            Some(outcome)
        }

        fn sh(script: &str) -> Vec<String> {
            vec!["/bin/sh".into(), "-c".into(), script.into()]
        }

        /// F4 — a write inside the granted roots lands and a write outside them
        /// is refused. Both arms, because a rule set that refused everything
        /// would pass the second alone.
        #[tokio::test]
        async fn the_rung_confines_writes_to_what_it_granted() {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("escaped");

            let inside = pinned(
                &sh("echo in > ./inside"),
                dir.path(),
                ExecMode::WorkspaceWrite,
                true,
                &[],
            )
            .await;
            let Some(inside) = inside else { return };
            assert!(inside.success(), "a granted write must land: {inside:?}");
            assert!(dir.path().join("inside").exists());

            let out = pinned(
                &sh(&format!("echo out > {}", target.display())),
                dir.path(),
                ExecMode::WorkspaceWrite,
                true,
                &[],
            )
            .await
            .unwrap();
            assert!(!out.success(), "a write outside the roots must be refused");
            assert!(
                !target.exists(),
                "and must not have landed: the rung reported success while enforcing nothing"
            );
        }

        /// F4's second half — a root the run resolved is writable, which is what
        /// makes a real toolchain able to run at all under this rung.
        #[tokio::test]
        async fn a_resolved_writable_root_is_granted() {
            let dir = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let target = cache.path().join("artifact");

            let out = pinned(
                &sh(&format!("echo x > {}", target.display())),
                dir.path(),
                ExecMode::WorkspaceWrite,
                true,
                &[cache.path().to_path_buf()],
            )
            .await;
            let Some(out) = out else { return };
            assert!(
                out.success(),
                "a granted cache root must be writable: {out:?}"
            );
            assert!(target.exists());
        }

        /// F5 — `ReadOnly` refuses a write into the workspace itself and still
        /// permits the read. The mode's entire difference is that one directory,
        /// so both halves are asserted against the same file.
        #[tokio::test]
        async fn read_only_refuses_the_workspace_and_still_reads_it() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("seed"), "hello").unwrap();

            let read = pinned(&sh("cat ./seed"), dir.path(), ExecMode::ReadOnly, true, &[]).await;
            let Some(read) = read else { return };
            assert!(read.success(), "a read-only run must still read: {read:?}");
            assert!(read.stdout.contains("hello"));

            let write = pinned(
                &sh("echo no > ./seed"),
                dir.path(),
                ExecMode::ReadOnly,
                true,
                &[],
            )
            .await
            .unwrap();
            assert!(!write.success(), "read-only must refuse the workspace");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("seed")).unwrap(),
                "hello",
                "and must not have changed it"
            );

            // The temporary directory stays writable in every mode: a toolchain
            // that cannot open a temporary file cannot run at all.
            let tmp = pinned(
                &sh("echo t > \"${TMPDIR:-/tmp}/io-harness-ro-probe\""),
                dir.path(),
                ExecMode::ReadOnly,
                true,
                &[],
            )
            .await
            .unwrap();
            assert!(
                tmp.success(),
                "the temp directory is writable under every mode"
            );
        }

        /// F6 — this rung wraps the payload in nothing, so the argv recorded is
        /// the argv asked for, and `current_dir` means what it says.
        #[tokio::test]
        async fn the_rung_spawns_the_callers_own_argv_and_honours_the_workdir() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("sub")).unwrap();

            let argv = sh("pwd");
            let out = pinned(
                &argv,
                &dir.path().join("sub"),
                ExecMode::WorkspaceWrite,
                true,
                &[dir.path().to_path_buf()],
            )
            .await;
            let Some(out) = out else { return };
            assert_eq!(
                out.argv, argv,
                "no wrapper: the recorded argv is the caller's own"
            );
            assert!(
                !out.argv.iter().any(|a| a == "unshare" || a == "bwrap"),
                "and names no helper program"
            );
            // 0.46.0's defect at its root cause: the wrapper entered the
            // directory it was handed and beat `Command::current_dir`. There is
            // no wrapper here, so the working directory is the one named.
            assert!(
                out.stdout.trim().ends_with("sub"),
                "the payload ran in the directory it was given, got {:?}",
                out.stdout
            );
        }

        /// F2's rung-level arm — an egress-denying run cannot dial out, and the
        /// same run with egress permitted can.
        ///
        /// On a kernel below the network ABI the rung is not given an
        /// egress-denying run at all, so the assertion there is the honesty rule
        /// itself rather than a skipped connection.
        #[tokio::test]
        async fn egress_is_denied_only_where_the_kernel_can_enforce_it() {
            let Some(abi) = landlock_abi() else {
                eprintln!("skipped: this host has no usable Landlock");
                return;
            };
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let dir = tempfile::tempdir().unwrap();
            // `/dev/tcp` is a bash builtin; the probe uses it because it needs no
            // network tool to be installed on the runner.
            let dial = vec![
                "/bin/bash".into(),
                "-c".into(),
                format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
            ];

            if abi < LANDLOCK_NET_ABI {
                assert_ne!(
                    rung(probes(), true),
                    Backend::LinuxLandlock,
                    "a kernel that cannot deny egress must not be handed a run that denies it"
                );
                return;
            }

            let allowed = pinned(&dial, dir.path(), ExecMode::WorkspaceWrite, true, &[])
                .await
                .unwrap();
            assert!(
                allowed.success(),
                "a run permitting egress must still connect: {allowed:?}"
            );

            let denied = pinned(&dial, dir.path(), ExecMode::WorkspaceWrite, false, &[])
                .await
                .unwrap();
            assert!(
                !denied.success(),
                "an egress-denying run must be refused the connection"
            );
        }
    }

    #[test]
    fn denies_network_with_a_new_net_namespace() {
        let argv = unshare_argv(
            &["echo".into(), "hi".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
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
        let argv = unshare_argv(
            &["echo".into(), nasty.clone()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
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
        let argv = unshare_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
        let script = argv.iter().find(|a| a.contains("mount --bind")).unwrap();
        assert!(script.contains("remount,bind,ro /"));
        assert!(script.contains("remount,bind,rw"));
        assert!(
            argv.contains(&"/w".to_string()),
            "the workdir is passed as a positional parameter, not baked in"
        );
    }

    /// F7 — the writable roots arrive as a counted list, ahead of the payload,
    /// and the workdir leads it. The count is what lets the script find the
    /// payload again, so it is asserted rather than assumed.
    #[test]
    fn the_writable_roots_are_a_counted_list_before_the_payload() {
        let roots = vec![
            PathBuf::from("/home/u/.cargo"),
            PathBuf::from("/home/u/.npm"),
        ];
        let argv = unshare_argv(
            &["echo".into(), "hi".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &roots,
        );

        // `sh -c <script> sh <workdir> <n> <root>... <payload...>`
        let wd_at = argv.iter().position(|a| a == "/w").unwrap();
        assert_eq!(argv[wd_at + 1], "3", "workdir plus two roots");
        assert_eq!(argv[wd_at + 2], "/w", "the workdir leads the writable list");
        assert_eq!(argv[wd_at + 3], "/home/u/.cargo");
        assert_eq!(argv[wd_at + 4], "/home/u/.npm");
        assert_eq!(&argv[wd_at + 5..], &["echo".to_string(), "hi".to_string()]);
    }

    /// F3 — under `ReadOnly` the workdir is entered and not bound writable, so
    /// the count is the roots alone.
    #[test]
    fn read_only_does_not_put_the_workdir_in_the_writable_list() {
        let argv = unshare_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::ReadOnly,
            &[],
        );
        let wd_at = argv.iter().position(|a| a == "/w").unwrap();
        assert_eq!(argv[wd_at + 1], "0", "nothing is writable but the temp dir");
        assert_eq!(&argv[wd_at + 2..], &["true".to_string()]);
    }

    #[test]
    fn allows_network_when_asked() {
        let argv = unshare_argv(
            &["echo".into()],
            Path::new("/w"),
            true,
            ExecMode::WorkspaceWrite,
            &[],
        );
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
            .run(RunSpec::new(
                &argv,
                dir.path(),
                &crate::sandbox::SandboxLimits::default(),
            ))
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
