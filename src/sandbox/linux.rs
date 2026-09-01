//! Linux native backend: user/mount/pid/**net** namespaces via `unshare`, plus
//! the shared rlimit + RSS caps. The new network namespace is a *hard* boundary
//! — a process in an empty net namespace has no route out — which is stronger
//! than the floor's best-effort env strip.
//!
//! **Read-only means every mount, and `/proc` is the namespace's own (0.74.0).**
//! A mount namespace is a set of mounts, not one tree: `remount,bind,ro /`
//! changes the `/` mount and leaves every separately-mounted filesystem — `/run`
//! on every systemd host, `/dev/shm`, a separate `/home` or `/var` — exactly as
//! writable as it was, while the child is uid 0 in its user namespace mapped to
//! the caller's own uid and writes them with the caller's rights. [`MOUNT_SETUP`]
//! now walks `/proc/self/mountinfo` and remounts *each* mount read-only, and
//! mounts a fresh `procfs` belonging to the run's own pid namespace, which is
//! what stops a payload reading `/proc/<harness-pid>/environ` and taking every
//! provider key the harness process holds. Both are the effect [`bwrap_argv`]'s
//! `--ro-bind / /` and `--proc /proc` already had, said in the one place the
//! namespace rung says anything.
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
/// **A rung placed above another must confine at least as much as it.** That is
/// what "strength order" has to mean for the order to be worth anything, and
/// until 0.74.0 the bubblewrap rung broke it: it sat above the namespace rung
/// while [`bwrap_argv`] created no pid namespace at all, so a payload on the
/// *stronger* rung could see and signal the harness and every sibling agent's
/// processes, which the *weaker* one's `--pid --fork` already prevented. The
/// flags in [`bwrap_argv`] are what make this ordering honest rather than
/// nominal; a namespace added to [`unshare_argv`] and not to `bwrap_argv` puts
/// it back into the state this comment exists to record.
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
pub(crate) fn rung(probes: Rungs, deny_egress: bool, proxied: bool) -> Backend {
    if let Some(abi) = probes.landlock_abi {
        // 0.48.0 — a proxied run needs the network rules for the same reason an
        // egress-denying one does: without them this rung cannot scope outbound
        // TCP at all, and a run whose policy names hosts would silently get every
        // host. It is 0.47.0's rule with the second question added.
        if (!deny_egress && !proxied) || abi >= LANDLOCK_NET_ABI {
            return Backend::LinuxLandlock;
        }
    }
    // 0.48.0 — **the namespace rungs cannot serve a proxied run at all.** Both put
    // the child in an empty network namespace, where the host's loopback is not
    // reachable, so the proxy the run owns would be unreachable and the command
    // would get no network rather than the hosts its policy names. A run that
    // names hosts and finds no rung above them takes the boolean and reports the
    // backend that applied — the weaker guarantee, said plainly, which is the same
    // answer this chain has given since 0.47.0.
    if !proxied {
        if probes.bubblewrap {
            return Backend::LinuxBubblewrap;
        }
        if probes.unshare {
            return Backend::LinuxNamespaces;
        }
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
        match rung(probes(), !spec.allow_network, spec.proxy.is_some()) {
            Backend::LinuxLandlock => {
                if let Some(outcome) = landlock_run(&spec).await {
                    return outcome;
                }
            }
            Backend::LinuxBubblewrap => return bwrap_run(&spec).await,
            _ => {}
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
            .with_writable_roots(spec.writable_roots)
            .with_proxy(spec.proxy);
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
        rung(probes(), true, false)
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
        bubblewrap: bwrap_works(),
        unshare: unshare_works(),
    }
}

/// Does the `bwrap` on this host work, running the exact wrapper this rung
/// builds?
///
/// The same shape as [`unshare_works`] and for the same reason: `bwrap` being
/// on `PATH` is not the question. A `bwrap` without the setuid bit on a kernel
/// that refuses unprivileged user namespaces is present and useless, which is
/// precisely the host this rung exists for, so the probe has to be a spawn.
fn bwrap_works() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let dir = std::env::temp_dir();
        // Probed with `--unshare-net`, the strictest form: if that works the
        // network-allowed subset does too. Same argument as the `unshare` probe.
        let argv = bwrap_argv(
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

/// The one directory a contained command may use for temporary files, and the
/// value its `TMPDIR` is set to.
///
/// **Not the system temporary directory (0.74.0).** Both mount rungs used to
/// bind the whole of it writable, and [`crate::sandbox::workdir`] puts every
/// run's ephemeral workspace *inside* it — so each run could read and rewrite
/// every concurrent run's workspace from inside its own sandbox, and a workspace
/// located there was confined by nothing at all.
///
/// The replacement is [`super::macos`]'s shape, which has pointed `TMPDIR` at the
/// workdir since 0.6.0: under a mode that grants the workdir, the workdir *is*
/// the writable temporary directory, so there is no second directory to create,
/// to grant or to clean up. [`ExecMode::ReadOnly`] is the case macOS leaves to
/// the system directory and this does not — there the workdir is precisely what
/// may not be written — so it gets a directory of its own beneath the system
/// temporary directory. A toolchain that cannot open a temporary file cannot run
/// at all, which is why the grant is narrowed rather than withdrawn.
///
/// The directory is created here rather than by the mount script or by `bwrap`,
/// because both need it to exist on the host already: `bwrap --bind` fails on a
/// source that is not there, and the script's `rw` runs after every mount has
/// been made read-only.
fn tmp_target(workdir: &Path, mode: ExecMode) -> PathBuf {
    if mode != ExecMode::ReadOnly {
        return workdir.to_path_buf();
    }
    // Named for the process, not for the run: this rung is handed a workdir and
    // a mode and no run identity, and a per-command directory would be a
    // per-command leak — nothing here outlives the process to remove it. Two
    // concurrent `ReadOnly` runs of one embedder therefore share a scratch
    // directory, which is a far smaller surface than the system temporary
    // directory and, under `ReadOnly`, holds nothing either run produced on
    // purpose.
    let dir = std::env::temp_dir().join(format!("io-harness-tmp-{}", std::process::id()));
    // A directory this process cannot create is one the payload cannot write
    // either: the mount setup's `[ -d ]` guard leaves it ungranted and the
    // command fails on its first temporary file, which is the honest outcome
    // rather than a silent grant of somewhere else.
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The `bwrap` argv this rung builds, factored out so it is unit-testable
/// without spawning anything — the same treatment [`unshare_argv`] gets.
///
/// The tree is bound read-only, then each writable root is bound back over it,
/// which is the identical statement the mount setup makes with its
/// `remount,bind,ro` walk and its `rw` loop. `--ro-bind` is recursive, so this
/// rung has always covered every mount rather than only `/`. `/proc` and `/dev`
/// are populated because a mount namespace with neither is a namespace most
/// toolchains cannot start in.
///
/// **The namespaces are what make this rung's place in [`rung`] honest
/// (0.74.0).** It is ranked above the namespace rung, so it has to confine at
/// least as much, and it did not: `--unshare-pid`, `--unshare-ipc`,
/// `--unshare-uts` and `--new-session` were all absent while `unshare_argv`'s
/// `--pid --fork` was not. The pid namespace is also the half that gives
/// `--proc /proc` its meaning — a `procfs` instance belongs to the pid namespace
/// of whoever mounted it, so without the unshare the "new" `/proc` still listed
/// every process on the host and `/proc/<harness-pid>/environ` still handed a
/// payload every provider key the harness holds.
///
/// **The payload is the trailing arguments after `--`**, never interpolated, so
/// a metacharacter in an argument stays an ordinary byte — the property
/// `src/tools/exec.rs` is built on and the one `unshare_argv` is also careful
/// about.
pub(crate) fn bwrap_argv(
    inner: &[String],
    workdir: &Path,
    allow_network: bool,
    mode: ExecMode,
    writable_roots: &[PathBuf],
) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "bwrap".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        // Every namespace the rung below this one takes, and the two it does not.
        // A pid namespace is the difference between a payload that can read and
        // signal the harness and one that cannot see it at all; `--proc /proc`
        // below is only a fresh view of the *host's* process table without it.
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        // bubblewrap's documented terminal-injection guard: a payload sharing the
        // caller's controlling terminal can push characters into it with
        // `TIOCSTI` and have the caller's shell run them. Every contained command
        // is spawned with a null stdin and piped output, so nothing here has a
        // terminal to keep — the flag closes the case where a future caller
        // passes one through.
        "--new-session".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        // A child of a run that ends must not outlive it. The shared runner
        // kills the tree, and this is the kernel saying the same thing —
        // `--die-with-parent` is a `PDEATHSIG` on `bwrap` alone, and it reaches
        // the payload's own descendants only because `--unshare-pid` above makes
        // them a pid namespace the kernel tears down with its init.
        "--die-with-parent".into(),
    ];

    // The workdir is bound writable only when the mode grants it, which is what
    // makes `ReadOnly` a mode rather than a label here too.
    let mut writable: Vec<&Path> = Vec::new();
    if mode != ExecMode::ReadOnly {
        writable.push(workdir);
    }
    writable.extend(writable_roots.iter().map(|p| p.as_path()));
    // One temporary directory that belongs to this run, never the whole system
    // temporary directory — see [`tmp_target`]. Under a mode that grants the
    // workdir it *is* the workdir, already bound just above.
    let tmp = tmp_target(workdir, mode);
    if tmp.as_path() != workdir {
        writable.push(&tmp);
    }
    for root in writable {
        v.push("--bind".into());
        v.push(root.display().to_string());
        v.push(root.display().to_string());
    }
    // And the payload is told where it is, so a toolchain reaching for a
    // temporary file reaches the one place this run may write rather than a
    // read-only `/tmp`.
    v.push("--setenv".into());
    v.push("TMPDIR".into());
    v.push(tmp.display().to_string());

    if !allow_network {
        v.push("--unshare-net".into());
    }
    // `--chdir` rather than letting the shared runner's `current_dir` decide.
    // Both are set — the runner sets its own — and they name the same directory;
    // stating it here is what makes the wrapper's view and the spawn's view the
    // same one, which is exactly what 0.46.0 found they were not.
    v.push("--chdir".into());
    v.push(workdir.display().to_string());
    v.push("--".into());
    v.extend(inner.iter().cloned());
    v
}

/// Run `spec` under `bwrap`.
async fn bwrap_run(spec: &RunSpec<'_>) -> Result<SandboxOutcome> {
    let wrapped = bwrap_argv(
        spec.argv,
        spec.workdir,
        spec.allow_network,
        spec.mode,
        spec.writable_roots,
    );
    let wspec = RunSpec::new(&wrapped, spec.workdir, spec.limits)
        .with_network(spec.allow_network)
        .with_mode(spec.mode)
        .with_writable_roots(spec.writable_roots)
        .with_proxy(spec.proxy);
    let outcome = run_capped(Backend::LinuxBubblewrap, wspec, |_cmd| {}).await?;
    match wrapper_failure(&outcome) {
        Some(reason) => Err(Error::Sandbox { reason }),
        None => Ok(outcome),
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
        spec.proxy.map(|a| a.port()),
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
    // The rule set's own answer, and the seccomp filter's. Landlock's network
    // rights are TCP only, so the run whose outbound TCP the rule set takes
    // control of is the run whose datagram sockets the filter beside it has to
    // refuse — one hole with two halves, and a constant here would install the
    // half that does nothing. Read from the plan, exactly as
    // `crate::sandbox::contain_command` reads it on the other spawn path.
    let net_restricted = plan.restricts_network();

    let wspec = RunSpec::new(spec.argv, spec.workdir, spec.limits)
        .with_network(spec.allow_network)
        .with_mode(spec.mode)
        .with_writable_roots(spec.writable_roots)
        .with_proxy(spec.proxy);

    // The argv is the caller's own, untouched: this rung wraps the payload in
    // nothing. What runs between fork and exec is two syscalls with no
    // allocation, which is why the rule set was built above rather than here.
    let outcome = run_capped(Backend::LinuxLandlock, wspec, move |cmd| {
        // SAFETY: the closure runs in the forked child before `exec`. It
        // allocates nothing, takes no lock and calls only `prctl` and one
        // `landlock_restrict_self`, both async-signal-safe. `fd` is owned by
        // `ruleset`, which outlives the spawn below.
        unsafe {
            cmd.pre_exec(move || {
                // Order is not arbitrary: `restrict_self` sets
                // `PR_SET_NO_NEW_PRIVS`, which installing a seccomp filter also
                // requires, so the rule set goes on first and the deny-list
                // second. Neither allocates.
                super::landlock::restrict_self(fd)?;
                super::seccomp::install(net_restricted)
            });
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
    // Both wrapping rungs announce their own setup failures with their program
    // name and neither reaches the payload when they do. `bwrap` was added to
    // this list rather than given a second copy of the function: the rule is
    // about wrappers in general, and two copies is two places for it to drift.
    let wrapper = ["unshare:", "bwrap:"].iter().any(|p| stderr.starts_with(p));
    (!outcome.success() && wrapper)
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
/// The script remounts every mount read-only and then binds back the places a
/// command legitimately writes: the run's own workdir (unless the
/// [`ExecMode`](crate::ExecMode) withholds it), the writable roots the run
/// resolved — a toolchain's own caches, since 0.46.0 — and [`tmp_target`], which
/// the payload's `TMPDIR` is pointed at. The last is not a convenience: without
/// somewhere to open a temporary file most toolchains fail immediately.
///
/// **"Read-only" is a walk over `/proc/self/mountinfo`, not one remount
/// (0.74.0).** A mount namespace is a set of mounts and `remount,bind,ro /`
/// changes exactly one of them. Until 0.74.0 that was the whole of this rung's
/// filesystem confinement, so every separately-mounted filesystem stayed
/// writable — `/run`, which is a tmpfs on every systemd host, `/dev/shm`, and a
/// separate `/home` or `/var` — and the child, uid 0 in its user namespace
/// mapped to the caller's own uid, wrote them with the caller's rights while
/// [`Backend::confines_writes`](crate::Backend::confines_writes) answered
/// `true`. Each mount is now remounted read-only in turn.
///
/// Two mounts are passed over and neither is a hole. A mount the sixth
/// `mountinfo` field already reports as `ro` — a `squashfs` snap, a
/// `/run/credentials` drop — is read-only before the walk reaches it, and
/// remounting it would be one more kernel call to be refused by a host for no
/// gain. `/dev/pts` is skipped because a `devpts` instance holds pty nodes the
/// kernel creates and nothing a payload can put there, while a read-only one
/// stops a payload opening a terminal at all.
///
/// **A mount that cannot be confined fails the setup, and nothing then claims it
/// was confined.** The walk has no partial success: a remount the kernel refuses
/// exits 125 with the `unshare:` prefix, so [`wrapper_failure`] turns it into
/// [`crate::Error::Sandbox`] and the payload never runs, and the same script run
/// by [`unshare_works`] answers `false`, which takes this host to a lower rung
/// that reports itself. There is no path on which the recursion fails and
/// `LinuxNamespaces` is still the backend recorded.
///
/// **`/proc` is remounted, and it is the run's own.** `--pid --fork` puts the
/// payload in a new pid namespace but leaves the host's `procfs` mounted over
/// it, so `/proc/<harness-pid>/environ` handed a payload every provider key the
/// harness process holds. A `procfs` instance belongs to the pid namespace of
/// whoever mounted it, and this one is mounted by the script running as init of
/// the run's namespace, so the harness is not in it to be read.
///
/// **No mount is bound to itself before its remount.** The first version of this
/// script did `mount --bind / /` before the remount, on the assumption that a
/// bind was needed to own the mount. On a GitHub `ubuntu-latest` runner that
/// bind fails with `wrong fs type, bad option, bad superblock on /` and takes
/// the whole setup with it, so the probe reported failure and every contained
/// run on Linux silently took the portable floor — the confinement this module
/// documents was applied nowhere, and the matrix is what caught it. Measured on
/// the runner: with the bind removed, the remount alone leaves the tree
/// genuinely read-only — asserted by *attempting a write and having it refused*,
/// not by the mount's exit status — and the workdir rebinds writable over it.
/// The mount points are read out of `mountinfo` in one pass before any of them
/// is touched, because the file is a live view of the very set being changed.
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
/// `$1` is the workdir to enter, `$2` is the temporary directory the run owns,
/// `$3` is how many writable roots follow, and the payload begins after them —
/// so the same script serves a run with no extra grants and one whose toolchain
/// writes to three caches, without the count ever being inferred from the argv's
/// shape. The workdir is in that list only when the
/// [`ExecMode`](crate::ExecMode) grants it, which is what makes `read-only` a
/// mode here rather than a label: the process still `cd`s into the workspace and
/// still cannot write to it.
///
/// `set -f` matters: the mount points read out of `mountinfo` are re-split by
/// the shell, and a directory named with a `*` in it would otherwise be expanded
/// into whatever happened to be beside it. The kernel escapes space, tab,
/// newline and backslash in that file as `\0ddd`, which is exactly the form
/// `printf %b` decodes, so a mount point containing any of them survives the
/// round trip instead of being silently skipped — and a mount silently skipped
/// is a mount left writable.
const MOUNT_SETUP: &str = "\
set -ef
fail() { echo \"unshare: sandbox mount setup failed: $1\" >&2; exit 125; }
rw() {
  [ -d \"$1\" ] || return 0
  mount --bind \"$1\" \"$1\" 2>/dev/null || fail \"could not bind $1\"
  mount -o remount,bind,rw \"$1\" 2>/dev/null || fail \"could not make $1 writable\"
}
mount --make-rprivate / 2>/dev/null || fail 'could not make / private'
[ -r /proc/self/mountinfo ] || fail 'could not read /proc/self/mountinfo'
points=
while read -r _ _ _ _ point opts _; do
  case \"$opts\" in ro|ro,*) continue ;; esac
  points=\"$points $point\"
done < /proc/self/mountinfo
for esc in $points; do
  point=$(printf '%b' \"$esc\")
  case \"$point\" in /dev/pts) continue ;; esac
  mount -o remount,bind,ro \"$point\" 2>/dev/null || fail \"could not remount $point read-only\"
done
mount -t proc -o nosuid,nodev,noexec proc /proc 2>/dev/null || fail 'could not mount /proc'
wd=\"$1\"; shift
TMPDIR=\"$1\"; export TMPDIR; shift
n=\"$1\"; shift
while [ \"$n\" -gt 0 ]; do rw \"$1\"; shift; n=$((n-1)); done
[ \"$TMPDIR\" = \"$wd\" ] || rw \"$TMPDIR\"
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
    // `sh -c <script> sh <workdir> <tmpdir> <n> <root>... <argv...>`: `$0` is `sh`,
    // `$1` is the workdir the script enters, `$2` is the one temporary directory
    // this run may write to and the value it exports as `TMPDIR`, `$3` is how
    // many writable roots follow, and the payload is what remains after them.
    v.push("sh".into());
    v.push("-c".into());
    v.push(MOUNT_SETUP.into());
    v.push("sh".into());
    v.push(workdir.display().to_string());
    v.push(tmp_target(workdir, mode).display().to_string());

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
                rung(probes, deny_egress, false),
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
        let under = |_mode: ExecMode| rung(probes, true, false);
        assert_eq!(under(ExecMode::ReadOnly), Backend::LinuxLandlock);
        assert_eq!(under(ExecMode::WorkspaceWrite), Backend::LinuxLandlock);
        assert_eq!(under(ExecMode::FullAccess), Backend::LinuxLandlock);
    }

    /// The rung a host takes must never be a backend that belongs to another
    /// platform or to a rung the chain does not contain. Cheap, and it is what
    /// catches a variant added to `Backend` and wired into the chain by
    /// accident.
    /// F8 — the rung a proxied run takes, as a table, decided without a host
    /// (0.48.0).
    ///
    /// Two rules meet here. A proxied run needs Landlock's **network** rules, for
    /// the same reason an egress-denying run does: without them the rung cannot
    /// scope outbound TCP and the run would silently get every host. And the
    /// namespace rungs cannot serve a proxied run **at all** — both put the child
    /// in an empty network namespace where the host's loopback is unreachable, so
    /// the proxy the run owns could not be dialled and the command would get no
    /// network instead of the hosts its policy names.
    ///
    /// The negative control is the whole of 0.47.0's table: with `proxied` false,
    /// every row must return exactly what it returned before this release.
    #[test]
    fn a_proxied_run_takes_a_rung_that_can_reach_its_proxy() {
        let all = |abi, bubblewrap, unshare| Rungs {
            landlock_abi: abi,
            bubblewrap,
            unshare,
        };

        // Landlock with the network rules serves a proxied run.
        assert_eq!(
            rung(all(Some(LANDLOCK_NET_ABI), true, true), false, true),
            Backend::LinuxLandlock
        );
        // Below them it cannot, and neither can the namespace rungs — so a host
        // with every other primitive still takes the floor and reports it.
        assert_eq!(
            rung(all(Some(3), true, true), false, true),
            Backend::PortableFloor,
            "a proxied run is never given a rung that cannot reach its proxy"
        );
        assert_eq!(
            rung(all(None, true, true), false, true),
            Backend::PortableFloor
        );

        // The negative control: with no proxy, every row is 0.47.0's answer.
        for abi in [None, Some(1), Some(3), Some(4), Some(6)] {
            for bubblewrap in [false, true] {
                for unshare in [false, true] {
                    for deny in [false, true] {
                        let probes = all(abi, bubblewrap, unshare);
                        let before = match abi {
                            Some(a) if !deny || a >= LANDLOCK_NET_ABI => Backend::LinuxLandlock,
                            _ if bubblewrap => Backend::LinuxBubblewrap,
                            _ if unshare => Backend::LinuxNamespaces,
                            _ => Backend::PortableFloor,
                        };
                        assert_eq!(
                            rung(probes, deny, false),
                            before,
                            "abi {abi:?} bwrap {bubblewrap} unshare {unshare} deny {deny}"
                        );
                    }
                }
            }
        }
    }

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
                            false,
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

        /// A scratch directory that is **not** under the system temporary
        /// directory, plus its cleanup.
        ///
        /// This exists because of a defect the matrix found and the development
        /// host could not. Every rung grants the system temporary directory
        /// writable — the mount setup binds `${TMPDIR:-/tmp}`, the macOS profile
        /// allows `/private/var/folders`, and this rung grants it too — and
        /// `tempfile::tempdir()` creates its directories *inside* it. So a test
        /// whose workspace and whose "outside" target were both `tempdir()`s was
        /// asserting about two paths that had **both** been granted, and it
        /// failed on the one arm that mattered: a write outside the roots landed,
        /// because the roots included the whole of `/tmp`.
        ///
        /// The consequence is not confined to tests, and it is stated in
        /// `docs/CONTRACT.md` rather than left here: **a workspace located inside
        /// the system temporary directory is not confined on any unix backend**,
        /// because the temporary directory is writable by design. That is the
        /// price of a default under which a toolchain can open a temporary file
        /// at all.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Self {
                // Under the crate's own `target/`, which is inside the checkout
                // and therefore outside `/tmp` on every host the matrix runs.
                let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("landlock-scratch")
                    .join(format!("{}-{}", tag, std::process::id()));
                std::fs::create_dir_all(&root).expect("create the scratch root");
                Scratch(root)
            }
            fn dir(&self, name: &str) -> PathBuf {
                let p = self.0.join(name);
                std::fs::create_dir_all(&p).expect("create a scratch directory");
                p
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

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
            let scratch = Scratch::new("confines");
            let dir = scratch.dir("ws");
            // Deliberately NOT a `tempfile::tempdir()`: that would sit inside the
            // system temporary directory, which every rung grants, and the
            // assertion below would be about a path that had been granted.
            let outside = scratch.dir("outside");
            let target = outside.join("escaped");

            let inside = pinned(
                &sh("echo in > ./inside"),
                &dir,
                ExecMode::WorkspaceWrite,
                true,
                &[],
            )
            .await;
            let Some(inside) = inside else { return };
            assert!(inside.success(), "a granted write must land: {inside:?}");
            assert!(dir.join("inside").exists());

            let out = pinned(
                &sh(&format!("echo out > {}", target.display())),
                &dir,
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
            let scratch = Scratch::new("root");
            let dir = scratch.dir("ws");
            let cache = scratch.dir("cache");
            let target = cache.join("artifact");

            let out = pinned(
                &sh(&format!("echo x > {}", target.display())),
                &dir,
                ExecMode::WorkspaceWrite,
                true,
                std::slice::from_ref(&cache),
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
            // Outside the system temporary directory, or the workspace would be
            // writable through the temp grant whatever the mode says.
            let scratch = Scratch::new("readonly");
            let dir = scratch.dir("ws");
            std::fs::write(dir.join("seed"), "hello").unwrap();

            let read = pinned(&sh("cat ./seed"), &dir, ExecMode::ReadOnly, true, &[]).await;
            let Some(read) = read else { return };
            assert!(read.success(), "a read-only run must still read: {read:?}");
            assert!(read.stdout.contains("hello"));

            let write = pinned(&sh("echo no > ./seed"), &dir, ExecMode::ReadOnly, true, &[])
                .await
                .unwrap();
            assert!(!write.success(), "read-only must refuse the workspace");
            assert_eq!(
                std::fs::read_to_string(dir.join("seed")).unwrap(),
                "hello",
                "and must not have changed it"
            );

            // The temporary directory stays writable in every mode: a toolchain
            // that cannot open a temporary file cannot run at all.
            let tmp = pinned(
                &sh("echo t > \"${TMPDIR:-/tmp}/io-harness-ro-probe\""),
                &dir,
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
            let scratch = Scratch::new("argv");
            let dir = scratch.dir("ws");
            let sub = scratch.dir("ws/sub");

            let argv = sh("pwd");
            let out = pinned(
                &argv,
                &sub,
                ExecMode::WorkspaceWrite,
                true,
                std::slice::from_ref(&dir),
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
            let scratch = Scratch::new("egress");
            let dir = scratch.dir("ws");
            // `/dev/tcp` is a bash builtin; the probe uses it because it needs no
            // network tool to be installed on the runner.
            let dial = vec![
                "/bin/bash".into(),
                "-c".into(),
                format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
            ];

            if abi < LANDLOCK_NET_ABI {
                assert_ne!(
                    rung(probes(), true, false),
                    Backend::LinuxLandlock,
                    "a kernel that cannot deny egress must not be handed a run that denies it"
                );
                return;
            }

            let allowed = pinned(&dial, &dir, ExecMode::WorkspaceWrite, true, &[])
                .await
                .unwrap();
            assert!(
                allowed.success(),
                "a run permitting egress must still connect: {allowed:?}"
            );

            let denied = pinned(&dial, &dir, ExecMode::WorkspaceWrite, false, &[])
                .await
                .unwrap();
            assert!(
                !denied.success(),
                "an egress-denying run must be refused the connection"
            );
        }
    }

    /// The bubblewrap rung's argv, asserted the way `unshare_argv`'s already is:
    /// the tree read-only, the writable roots bound back over it, the payload
    /// trailing and untouched.
    #[test]
    fn the_bwrap_argv_binds_the_tree_read_only_and_the_roots_back() {
        let roots = vec![PathBuf::from("/home/u/.cargo")];
        let argv = bwrap_argv(
            &["echo".into(), "hi".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &roots,
        );

        let pos = |s: &str| argv.iter().position(|a| a == s).unwrap();
        assert_eq!(argv[0], "bwrap");
        assert!(argv.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(argv.windows(3).any(|w| w == ["--bind", "/w", "/w"]));
        assert!(argv
            .windows(3)
            .any(|w| w == ["--bind", "/home/u/.cargo", "/home/u/.cargo"]));
        assert!(argv.contains(&"--unshare-net".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--chdir", "/w"]));
        // The read-only bind of the whole tree must come before the writable
        // binds, or the roots are covered by it instead of overriding it.
        assert!(pos("--ro-bind") < pos("--bind"));

        // The payload is the tail, after `--`, and is never interpolated.
        let sep = pos("--");
        assert_eq!(&argv[sep + 1..], &["echo".to_string(), "hi".to_string()]);
    }

    /// F5's bubblewrap half, host-free: `ReadOnly` does not bind the workspace
    /// writable, and the temporary directory is bound in every mode.
    #[test]
    fn the_bwrap_argv_withholds_the_workdir_under_read_only() {
        let argv = bwrap_argv(
            &["true".into()],
            Path::new("/w"),
            true,
            ExecMode::ReadOnly,
            &[],
        );
        assert!(
            !argv.windows(3).any(|w| w == ["--bind", "/w", "/w"]),
            "read-only must not bind the workspace writable"
        );
        // A temporary directory is writable under every mode — but it is this
        // run's own, not the system one. 0.74.0 replaced the second with the
        // first; see `tmp_target`.
        let tmp = tmp_target(Path::new("/w"), ExecMode::ReadOnly)
            .display()
            .to_string();
        assert!(
            argv.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == tmp && w[2] == tmp),
            "a temp directory is writable under every mode"
        );
        assert!(
            argv.windows(3)
                .any(|w| w == ["--setenv", "TMPDIR", tmp.as_str()]),
            "and the payload is told where it is"
        );
        assert!(
            !argv.contains(&"--unshare-net".to_string()),
            "no network namespace when the run permits egress"
        );
    }

    /// A `bwrap` setup failure must be classified as the wrapper failing, not as
    /// the payload's own non-zero exit — the same distinction `unshare` gets,
    /// and the reason both prefixes live in one function.
    #[test]
    fn a_bwrap_setup_failure_is_a_wrapper_failure() {
        let fail = SandboxOutcome {
            backend: Backend::LinuxBubblewrap,
            argv: vec!["bwrap".into()],
            exit_code: Some(1),
            cap_hit: None,
            stdout: String::new(),
            stderr: "bwrap: Creating new namespace failed: Operation not permitted\n".into(),
        };
        assert!(wrapper_failure(&fail).is_some());
    }

    // --- 0.74.0, the audited holes in the two mount rungs --------------------
    //
    // Every one of these reads the argv or the setup script the rung *builds*,
    // and none of them needs a namespace to exist. That is deliberate: the build
    // host is macOS, the CI runner's kernel refuses unprivileged user namespaces
    // (see this module's header), so a live assertion about either mount rung
    // runs on no machine anyone watches — while a missing flag is exactly the
    // defect these findings are. `tests/security_linux.rs` holds the live arms,
    // `cfg(target_os = "linux")` and skipping when the host's chain selects a
    // rung other than these two; it is the second line, not the first.

    /// The setup script this rung builds.
    fn setup_script(mode: ExecMode) -> String {
        unshare_argv(&["true".into()], Path::new("/w"), false, mode, &[])
            .into_iter()
            .find(|a| a.contains("mount --bind"))
            .expect("the mount setup is one of the arguments")
    }

    /// H8 — a mount namespace is a *set* of mounts, and `remount,bind,ro /`
    /// changes one of them.
    ///
    /// Fails on 0.73.0's script, whose entire filesystem confinement was that one
    /// line: `/run` (a tmpfs on every systemd host), `/dev/shm` and a separately
    /// mounted `/home` or `/var` stayed writable, and the child — uid 0 in its
    /// user namespace, mapped to the caller's own uid — wrote them with the
    /// caller's rights while the rung reported that it confined writes.
    #[test]
    fn h8_the_setup_remounts_every_mount_read_only_and_not_only_the_root_one() {
        let script = setup_script(ExecMode::WorkspaceWrite);
        assert!(
            script.starts_with("set -ef"),
            "the mount points are re-split by the shell, so globbing is off or a \
             directory named with a `*` in it expands into its neighbours"
        );
        assert!(
            script.contains("/proc/self/mountinfo"),
            "the set of mounts has to be enumerated before any of it can be confined"
        );
        assert!(
            script.contains("mount -o remount,bind,ro \"$point\""),
            "and every mount in it remounted read-only in turn"
        );
        assert!(
            !script.contains("mount -o remount,bind,ro / "),
            "0.73.0's single remount of `/` is not the confinement any more — if it \
             is still here, it is still all that runs"
        );
    }

    /// H8's fail-closed half: the walk has no partial success.
    ///
    /// A rung that cannot apply the confinement it promises must not report that
    /// it did. There is no branch on which a refused remount leaves the payload
    /// running and `LinuxNamespaces` recorded: the script exits 125 with the
    /// `unshare:` prefix, [`wrapper_failure`] turns that into
    /// [`crate::Error::Sandbox`], and the same script run by [`unshare_works`]
    /// answers `false`, which takes the host to a rung that reports itself.
    #[test]
    fn h8_a_mount_that_cannot_be_confined_fails_the_setup_closed() {
        let script = setup_script(ExecMode::WorkspaceWrite);
        assert!(
            script.contains("|| fail \"could not remount $point read-only\""),
            "a refused remount must fail the setup, not be skipped"
        );
        assert!(
            script.contains("[ -r /proc/self/mountinfo ] || fail"),
            "and a mountinfo that cannot be read is a failure too, or the walk \
             would confine nothing and say nothing"
        );
        assert!(script.contains("exit 125"));

        let refused = SandboxOutcome {
            backend: Backend::LinuxNamespaces,
            argv: vec!["unshare".into()],
            exit_code: Some(125),
            cap_hit: None,
            stdout: String::new(),
            stderr: "unshare: sandbox mount setup failed: could not remount /run read-only\n"
                .into(),
        };
        assert!(
            wrapper_failure(&refused).is_some(),
            "the run must end as a sandbox error and not as the payload's own verdict"
        );
    }

    /// H10 — `/proc` belongs to the run's pid namespace, so the harness is not in
    /// it to be read.
    ///
    /// Fails on 0.73.0's script, which mounted no `procfs` at all: `--pid --fork`
    /// made a pid namespace and left the *host's* `/proc` mounted over it, so
    /// `/proc/<harness-pid>/environ` handed a payload every provider key the
    /// harness process holds.
    #[test]
    fn h10_the_setup_replaces_proc_with_the_pid_namespaces_own() {
        let argv = unshare_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
        assert!(
            argv.contains(&"--pid".to_string()) && argv.contains(&"--fork".to_string()),
            "the pid namespace the procfs instance will belong to"
        );
        let script = setup_script(ExecMode::WorkspaceWrite);
        assert!(
            script.contains("mount -t proc"),
            "a procfs instance belongs to the pid namespace of whoever mounted it, \
             so the namespace is worth nothing until one is mounted inside it"
        );
        assert!(
            script.contains("|| fail 'could not mount /proc'"),
            "and a /proc that could not be replaced fails the setup rather than \
             running the payload against the host's"
        );
    }

    /// H10's bubblewrap half. Fails on 0.73.0's argv, which had `--proc /proc`
    /// and no pid namespace for it to belong to — a fresh mount of the same host
    /// process table.
    #[test]
    fn h10_the_bwrap_argv_unshares_the_pid_namespace_its_proc_belongs_to() {
        let argv = bwrap_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
        assert!(argv.contains(&"--unshare-pid".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--proc", "/proc"]));
    }

    /// H12 — a rung ranked above another must confine at least as much as it.
    ///
    /// The premise is asserted first, because the requirement only exists while
    /// the ranking does. Fails on 0.73.0, where bubblewrap sat above the
    /// namespace rung with no pid namespace at all: a payload on the *stronger*
    /// rung could see and signal the harness and every sibling agent's processes,
    /// which the weaker rung's `--pid --fork` already prevented — and
    /// `--die-with-parent`, a `PDEATHSIG` on `bwrap` alone, reached none of the
    /// payload's own descendants.
    #[test]
    fn h12_the_bwrap_rung_confines_every_namespace_the_rung_below_it_confines() {
        assert_eq!(
            rung(
                Rungs {
                    landlock_abi: None,
                    bubblewrap: true,
                    unshare: true,
                },
                true,
                false
            ),
            Backend::LinuxBubblewrap,
            "the premise: bubblewrap is ranked above the namespace rung"
        );

        let bwrap = bwrap_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );
        let unshare = unshare_argv(
            &["true".into()],
            Path::new("/w"),
            false,
            ExecMode::WorkspaceWrite,
            &[],
        );

        // `unshare`'s spelling on the left, bubblewrap's on the right. The user
        // and mount namespaces are not in the table because `bwrap` takes both
        // implicitly — it cannot build its root without them — and a row that
        // named a flag never passed would assert nothing.
        for (below, above) in [("--pid", "--unshare-pid"), ("--net", "--unshare-net")] {
            assert!(
                unshare.contains(&below.to_string()),
                "the premise: the namespace rung takes {below}"
            );
            assert!(
                bwrap.contains(&above.to_string()),
                "the bubblewrap rung is ranked above the namespace rung and takes \
                 {below}'s equivalent {above} nowhere, so it confines less than the \
                 rung it outranks"
            );
        }
        for extra in ["--unshare-ipc", "--unshare-uts", "--new-session"] {
            assert!(
                bwrap.contains(&extra.to_string()),
                "{extra} is this rung's alone, and it is part of what its place in \
                 the order is claiming"
            );
        }
    }

    /// L11 — no rung grants the whole system temporary directory.
    ///
    /// Fails on 0.73.0, where both mount rungs bound it writable and
    /// [`crate::sandbox::workdir`] puts every run's ephemeral workspace inside
    /// it: each run could read and rewrite every concurrent run's workspace from
    /// inside its own sandbox, and a workspace located there was confined by
    /// nothing at all.
    #[test]
    fn l11_no_mount_rung_grants_the_whole_system_temporary_directory() {
        let system = std::env::temp_dir().display().to_string();
        for mode in [
            ExecMode::ReadOnly,
            ExecMode::WorkspaceWrite,
            ExecMode::FullAccess,
        ] {
            let bwrap = bwrap_argv(&["true".into()], Path::new("/w"), true, mode, &[]);
            assert!(
                !bwrap
                    .windows(3)
                    .any(|w| w[0] == "--bind" && w[1] == system && w[2] == system),
                "{mode:?}: bubblewrap bound the whole of {system} writable"
            );
            let unshare = unshare_argv(&["true".into()], Path::new("/w"), true, mode, &[]);
            assert!(
                !unshare.contains(&system),
                "{mode:?}: the namespace rung passed {system} to its setup script"
            );
        }
        assert!(
            !MOUNT_SETUP.contains("TMPDIR:-/tmp"),
            "and the script no longer reaches for `/tmp` on its own behalf"
        );
    }

    /// L11's other half — the one directory that *is* granted is the one the
    /// payload's `TMPDIR` names, so a toolchain reaching for a temporary file
    /// reaches somewhere it may write.
    #[test]
    fn l11_the_temporary_grant_is_exactly_what_tmpdir_points_at() {
        for mode in [ExecMode::ReadOnly, ExecMode::WorkspaceWrite] {
            let target = tmp_target(Path::new("/w"), mode).display().to_string();

            let bwrap = bwrap_argv(&["true".into()], Path::new("/w"), true, mode, &[]);
            assert!(
                bwrap
                    .windows(3)
                    .any(|w| w == ["--setenv", "TMPDIR", target.as_str()]),
                "{mode:?}: TMPDIR must name the directory this run owns"
            );
            assert!(
                bwrap
                    .windows(3)
                    .any(|w| w[0] == "--bind" && w[1] == target && w[2] == target),
                "{mode:?}: and that directory must be bound writable"
            );

            let unshare = unshare_argv(&["true".into()], Path::new("/w"), true, mode, &[]);
            let head = unshare
                .iter()
                .position(|a| a.contains("mount --bind"))
                .unwrap();
            assert_eq!(
                unshare[head + 3],
                target,
                "{mode:?}: and reaches the script as its second positional"
            );
        }
        assert!(
            MOUNT_SETUP.contains("TMPDIR=\"$1\"; export TMPDIR"),
            "which the script exports, or the payload never learns where it is"
        );
        assert_eq!(
            tmp_target(Path::new("/w"), ExecMode::WorkspaceWrite),
            Path::new("/w"),
            "under a mode that grants the workdir there is no second directory to \
             create or clean up — the shape `super::macos` has used since 0.6.0"
        );
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
        assert!(script.contains("remount,bind,ro"));
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

        // `sh -c <script> sh <workdir> <tmpdir> <n> <root>... <payload...>`.
        // Anchored on the script rather than on the first `/w`, because the
        // workdir now appears three times over — as the directory to enter, as
        // this run's `TMPDIR`, and as the head of the writable list.
        let head = argv
            .iter()
            .position(|a| a.contains("mount --bind"))
            .unwrap();
        assert_eq!(argv[head + 1], "sh", "$0");
        assert_eq!(argv[head + 2], "/w", "the workdir to enter");
        assert_eq!(argv[head + 3], "/w", "and the temporary directory it owns");
        assert_eq!(argv[head + 4], "3", "workdir plus two roots");
        assert_eq!(argv[head + 5], "/w", "the workdir leads the writable list");
        assert_eq!(argv[head + 6], "/home/u/.cargo");
        assert_eq!(argv[head + 7], "/home/u/.npm");
        assert_eq!(&argv[head + 8..], &["echo".to_string(), "hi".to_string()]);
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
        let head = argv
            .iter()
            .position(|a| a.contains("mount --bind"))
            .unwrap();
        assert_eq!(argv[head + 2], "/w", "the workdir is still entered");
        assert_eq!(
            argv[head + 3],
            tmp_target(Path::new("/w"), ExecMode::ReadOnly)
                .display()
                .to_string(),
            "and the temporary directory is not the workdir under this mode"
        );
        assert_eq!(argv[head + 4], "0", "nothing is writable but the temp dir");
        assert_eq!(&argv[head + 5..], &["true".to_string()]);
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

    /// Never name an isolation that was not applied.
    ///
    /// Up to 0.46.0 there were two possible answers and this pinned them against
    /// `unshare_works()` directly. 0.47.0 made it a chain, so the assertion is
    /// now the *property* rather than the enumeration: whatever rung is reported,
    /// this host must actually be able to deliver it. `backend()` answers for a
    /// run that denies egress, which is why the Landlock arm requires the
    /// network ABI and not merely the presence of Landlock.
    #[test]
    fn the_reported_backend_is_the_one_the_host_can_actually_run() {
        let p = probes();
        match LinuxSandbox.backend() {
            Backend::LinuxLandlock => assert!(
                p.landlock_abi.is_some_and(|abi| abi >= LANDLOCK_NET_ABI),
                "reported Landlock for an egress-denying run on a host that \
                 cannot deny egress with it: {p:?}"
            ),
            Backend::LinuxBubblewrap => assert!(p.bubblewrap, "reported a bwrap this host lacks"),
            Backend::LinuxNamespaces => assert!(
                p.unshare && !p.bubblewrap,
                "reported namespaces where a stronger rung was available or none works: {p:?}"
            ),
            Backend::PortableFloor => assert!(
                !p.unshare && !p.bubblewrap,
                "reported the floor while a rung above it works: {p:?}"
            ),
            other => panic!("the Linux chain reported {other:?}"),
        }
    }

    /// The degrade path, and 0.47.0 changed what it degrades *to*.
    ///
    /// Written in 0.9.1, when a host without a working `unshare` had exactly one
    /// place left to fall: the portable floor. That premise is the hole this
    /// release closes. On a stock Ubuntu 24.04 — the very host the assertion was
    /// written for — the chain now hands the run to Landlock, and the CI leg that
    /// leaves the restriction in place is where this first failed, reporting
    /// `LinuxLandlock` where the test demanded `PortableFloor`.
    ///
    /// So the assertion is the property rather than the destination: a host with
    /// no rung it can serve still *runs*, and reports whatever rung actually
    /// applied rather than failing. The floor arm is asserted where the floor is
    /// genuinely what is left.
    #[tokio::test]
    async fn a_host_with_no_working_rung_still_runs_and_reports_what_applied() {
        let expected = rung(probes(), false, false);
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["sh".into(), "-c".into(), "echo hi".into()];
        let out = LinuxSandbox
            .run(
                RunSpec::new(&argv, dir.path(), &crate::sandbox::SandboxLimits::default())
                    .with_network(true),
            )
            .await
            .unwrap();
        assert!(out.success(), "a degraded run must still run, got {out:?}");
        assert!(out.stdout.contains("hi"));
        assert_eq!(
            out.backend, expected,
            "the backend reported must be the rung the chain chose for this run"
        );
        if expected == Backend::PortableFloor {
            let p = probes();
            assert!(
                p.landlock_abi.is_none() && !p.bubblewrap && !p.unshare,
                "the floor is only reached when nothing above it works: {p:?}"
            );
        }
    }

    /// N5 — what each rung costs per command, measured rather than argued.
    ///
    /// The Landlock rung's claim is that it installs its restriction between fork
    /// and exec and spawns **no wrapper**, unlike the namespace rung which prepends
    /// `unshare`. That is a claim about a number, so it is timed: the same trivial
    /// command, the same iteration count 0.46.0 used for `sandbox-exec`, run
    /// unconfined and then under each rung this host actually has.
    ///
    /// `#[ignore]`d because it is a measurement and not an assertion — it has no
    /// threshold to fail, and a wall-clock number on a shared runner is not
    /// something to gate a merge on ([[never gate CI on a clock]]). CI runs it in
    /// a step of its own that shows the output; the figures go into the release
    /// record from that log.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "a measurement, not an assertion — run by the overhead CI step"]
    async fn n5_per_command_overhead_by_rung() {
        use crate::sandbox::SandboxLimits;
        use std::time::Instant;

        const ITERATIONS: u32 = 30;
        let dir = tempfile::tempdir().unwrap();
        let argv: Vec<String> = vec!["/bin/true".into()];
        let limits = SandboxLimits::none();

        let spec = || {
            RunSpec::new(&argv, dir.path(), &limits)
                .with_network(true)
                .with_mode(ExecMode::WorkspaceWrite)
        };

        // The baseline: the shared runner with no wrapper and no rule set, which
        // is what a `FullAccess` command pays. Every figure below is a cost *over*
        // this one, so it is measured with the same machinery rather than assumed
        // to be zero.
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            run_capped(Backend::PortableFloor, spec(), |_cmd| {})
                .await
                .expect("the unconfined baseline must run");
        }
        let baseline = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
        println!("N5 unconfined (full-access): {baseline:.2} ms/command over {ITERATIONS}");

        if landlock_abi().is_some() {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                landlock_run(&spec())
                    .await
                    .expect("the rung is available on this host")
                    .expect("the rung must run");
            }
            let per = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
            println!(
                "N5 linux-landlock: {per:.2} ms/command over {ITERATIONS} \
                 (over baseline: {:+.2} ms)",
                per - baseline
            );
        } else {
            println!("N5 linux-landlock: not measured — this host has no usable Landlock");
        }

        if unshare_works() {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                let wrapped = unshare_argv(&argv, dir.path(), true, ExecMode::WorkspaceWrite, &[]);
                let wspec = RunSpec::new(&wrapped, dir.path(), &limits)
                    .with_network(true)
                    .with_mode(ExecMode::WorkspaceWrite);
                run_capped(Backend::LinuxNamespaces, wspec, |_cmd| {})
                    .await
                    .expect("the namespace rung must run");
            }
            let per = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
            println!(
                "N5 linux-namespaces: {per:.2} ms/command over {ITERATIONS} \
                 (over baseline: {:+.2} ms)",
                per - baseline
            );
        } else {
            println!("N5 linux-namespaces: not measured — no usable user namespace on this host");
        }

        if bwrap_works() {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                bwrap_run(&spec()).await.expect("the bwrap rung must run");
            }
            let per = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
            println!(
                "N5 linux-bubblewrap: {per:.2} ms/command over {ITERATIONS} \
                 (over baseline: {:+.2} ms)",
                per - baseline
            );
        } else {
            println!("N5 linux-bubblewrap: not measured — no working bwrap on this host");
        }
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
