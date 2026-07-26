//! The execution sandbox: model-produced code runs isolated, per run.
//!
//! Since 0.2.0 the verification gate has compiled and run model-produced code
//! directly on the host — the "compiles locally, no isolation" limitation, made
//! sharper by 0.5.0's many concurrent agents. 0.6.0 routes every such execution
//! through a [`Sandbox`]: an ephemeral working directory, resource caps that
//! *kill* rather than throttle, network denied by default, and guaranteed
//! teardown so nothing the run wrote or spawned outlives it.
//!
//! The sandbox is both **OS-native** and **OS-neutral**. One trait,
//! [`Sandbox`], has a native backend per platform — macOS `sandbox-exec` and
//! Linux namespaces + seccomp; Windows is still the floor (its Job Object is
//! unimplemented) — over a [portable floor](FloorSandbox) (fresh subprocess,
//! ephemeral tempdir, resource caps, network env stripped) that compiles and runs
//! on all three, so isolation is never *absent* on any OS the crate builds for.
//! [`select`] picks the backend for this OS — at compile time, not by probing —
//! and the one that ran is recorded.
//!
//! ## Backend isolation strength (documented, not hidden)
//!
//! - **macOS `sandbox-exec`** — a generated profile confines filesystem writes
//!   to the workdir and denies network; `setrlimit` caps CPU/procs/fds; memory is
//!   capped by an RSS monitor (macOS does not enforce `RLIMIT_AS`/`RLIMIT_DATA`).
//! - **Linux namespaces** — user + mount + pid + net namespaces give a hard
//!   network boundary and a private tmpfs; seccomp + rlimits on top. *(cfg-gated,
//!   not live-run on the macOS build host.)*
//! - **Windows** — *no native backend yet.* The Job Object was designed but
//!   never implemented (no Win32 call is made), so a Windows run gets the
//!   portable floor and reports it as such. See [`windows`].
//! - **Portable floor** — the weakest backend: filesystem-scoped (a fresh
//!   ephemeral workdir) and resource-capped, **not a full syscall jail**. Network
//!   deny is best-effort (proxy env stripped), *not* a kernel boundary. It exists
//!   so no OS ever runs code with no sandbox at all.
//!
//! A configurable network egress *allow-list* is out of scope for 0.6.0 (network
//! is deny-by-default only); it lands in 0.8.0 with MCP/plugins.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Which backend actually ran a sandboxed command. Recorded in the trace so an
/// operator can audit not just *what* ran but *how* it was isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// macOS `sandbox-exec` profile + rlimits + RSS monitor.
    MacosSandboxExec,
    /// Linux user/mount/pid/net namespaces + seccomp + rlimits.
    LinuxNamespaces,
    /// Windows Job Object + restricted token. **Reserved, never reported** —
    /// the Job Object is not implemented, so Windows runs report
    /// [`Backend::PortableFloor`]. Kept so the variant is here when it is.
    WindowsJobObject,
    /// The portable floor: subprocess + ephemeral workdir + caps + env strip.
    PortableFloor,
}

impl Backend {
    /// A stable label for the trace and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::MacosSandboxExec => "macos-sandbox-exec",
            Backend::LinuxNamespaces => "linux-namespaces",
            Backend::WindowsJobObject => "windows-job-object",
            Backend::PortableFloor => "portable-floor",
        }
    }
}

/// A resource cap that was breached, killing the sandboxed process. Returned in
/// [`SandboxOutcome::cap_hit`] so a cap hit is a *typed* result, never a hang.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cap {
    /// CPU time (`RLIMIT_CPU` on unix; the process took SIGXCPU).
    Cpu,
    /// Resident memory (an RSS monitor killed it).
    Memory,
    /// Wall-clock time (the run outlived `max_wall_secs`).
    Wall,
}

impl Cap {
    /// A stable label for the trace and error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Cap::Cpu => "cpu",
            Cap::Memory => "memory",
            Cap::Wall => "wall",
        }
    }
}

/// Resource caps applied to a sandboxed run. Serde-serializable like
/// [`crate::Policy`] and [`crate::Containment`] so io-cli and io-studio load it
/// from config rather than hand-building it.
///
/// Defaults are sized so an ordinary `rustc`/`cargo` verification passes out of
/// the box — a default that failed real compiles would push callers to disable
/// the sandbox entirely. Tighten via the fields for untrusted work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxLimits {
    /// Max CPU seconds before SIGXCPU. `None` = no CPU cap.
    pub max_cpu_secs: Option<u64>,
    /// Max wall-clock seconds before the run is killed. `None` = no wall cap.
    pub max_wall_secs: Option<u64>,
    /// Max resident bytes before the RSS monitor kills the run. `None` = no cap.
    pub max_memory_bytes: Option<u64>,
    /// Max concurrent processes in the sandbox. Enforced only by the native
    /// backends that can scope it to the sandbox (Linux pid namespace, Windows
    /// Job Object active-process limit); the portable floor does **not** enforce
    /// it, because unix `RLIMIT_NPROC` is per-real-uid, not per-sandbox — capping
    /// it there would throttle the operator's whole login session. `None` = no
    /// cap.
    pub max_processes: Option<u64>,
    /// Max open file descriptors (`RLIMIT_NOFILE`, unix). `None` = no cap.
    pub max_open_files: Option<u64>,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_cpu_secs: Some(60),
            max_wall_secs: Some(120),
            max_memory_bytes: Some(2 * 1024 * 1024 * 1024), // 2 GiB
            // Not enforced by the floor (RLIMIT_NPROC is per-uid, not per-sandbox);
            // the native pid-namespace / Job-Object backends scope it properly.
            max_processes: None,
            max_open_files: Some(512),
        }
    }
}

/// How the sandbox is configured for a run.
///
/// The *absence* of a `SandboxConfig` on the exec path means opt out: the
/// verification gate runs on the host exactly as it did in 0.5.0. Its presence
/// turns isolation on. This is what makes 0.6.0 additive and reversible.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Resource caps for the run.
    #[serde(default)]
    pub limits: SandboxLimits,
    /// Allow outbound network. Default `false` — network is denied by default.
    #[serde(default)]
    pub allow_network: bool,
    /// Disable the native backend and force the portable floor. Off by default;
    /// used to prove the selection ladder and to run the floor everywhere.
    #[serde(default)]
    pub force_floor: bool,
}

impl SandboxConfig {
    /// A config with default caps and network denied — the recommended default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Force the portable floor backend (disable the native one).
    pub fn floor_only(mut self) -> Self {
        self.force_floor = true;
        self
    }
}

/// One command to run in the sandbox. OS-neutral by construction — no
/// OS-specific type appears here, so the [`Sandbox`] trait signature is portable.
pub struct RunSpec<'a> {
    /// The command and its arguments. `argv[0]` is the program.
    pub argv: &'a [String],
    /// The isolated working directory the command runs in.
    pub workdir: &'a Path,
    /// Resource caps for this run.
    pub limits: &'a SandboxLimits,
    /// Whether outbound network is permitted (default-deny lives in the caller).
    pub allow_network: bool,
}

/// The result of a sandboxed run — enough to make a verification pass/fail
/// decision identical to the un-sandboxed path, plus the isolation metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutcome {
    /// Which backend ran the command.
    pub backend: Backend,
    /// The exact argv that ran (recorded in the trace).
    pub argv: Vec<String>,
    /// The process exit code, or `None` when killed by a signal or a cap.
    pub exit_code: Option<i32>,
    /// The cap that killed the run, if any.
    pub cap_hit: Option<Cap>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

impl SandboxOutcome {
    /// The command ran to completion with a zero exit code and hit no cap.
    pub fn success(&self) -> bool {
        self.cap_hit.is_none() && self.exit_code == Some(0)
    }
}

/// The one execution abstraction. Every external command the harness runs —
/// the execution-based verification gate, and any command an agent runs as a
/// tool — goes through a `Sandbox`, so there is exactly one place model-produced
/// code leaves the harness.
///
/// The signature is OS-neutral: no OS-specific type appears, so the same trait
/// is the public surface on mac, linux, and windows. Implemented by
/// [`FloorSandbox`] and each native backend. Mirrors [`crate::Provider`]'s
/// async style (RPITIT, no `async-trait` dependency).
pub trait Sandbox {
    /// Run one command under isolation, returning its captured outcome.
    fn run(
        &self,
        spec: RunSpec<'_>,
    ) -> impl std::future::Future<Output = Result<SandboxOutcome>> + Send;

    /// Which backend this is — recorded so an audit shows how a run was isolated.
    fn backend(&self) -> Backend;
}

/// The selected backend for a run. An internal enum so the crate can dispatch to
/// one concrete backend without `dyn` (the trait is RPITIT and not
/// object-safe). Callers see the [`Sandbox`] trait; [`select`] returns this.
pub enum Selected {
    /// The portable floor, always available.
    Floor(FloorSandbox),
    /// The macOS native backend.
    #[cfg(target_os = "macos")]
    Macos(macos::MacosSandbox),
    /// The Linux native backend.
    #[cfg(target_os = "linux")]
    Linux(linux::LinuxSandbox),
    /// The Windows native backend.
    #[cfg(target_os = "windows")]
    Windows(windows::WindowsSandbox),
}

impl Sandbox for Selected {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        match self {
            Selected::Floor(s) => s.run(spec).await,
            #[cfg(target_os = "macos")]
            Selected::Macos(s) => s.run(spec).await,
            #[cfg(target_os = "linux")]
            Selected::Linux(s) => s.run(spec).await,
            #[cfg(target_os = "windows")]
            Selected::Windows(s) => s.run(spec).await,
        }
    }

    fn backend(&self) -> Backend {
        match self {
            Selected::Floor(s) => s.backend(),
            #[cfg(target_os = "macos")]
            Selected::Macos(s) => s.backend(),
            #[cfg(target_os = "linux")]
            Selected::Linux(s) => s.backend(),
            #[cfg(target_os = "windows")]
            Selected::Windows(s) => s.backend(),
        }
    }
}

/// Pick the backend for this OS. The choice is made at **compile time** by cfg,
/// not by probing the host: the native rung for this target, or the portable
/// floor when `force_floor` skips it (so the floor can be exercised everywhere).
/// There is no runtime capability check and so no runtime degradation — a native
/// backend whose primitive is unavailable fails at spawn rather than falling back.
pub fn select(config: &SandboxConfig) -> Selected {
    if !config.force_floor {
        #[cfg(target_os = "macos")]
        return Selected::Macos(macos::MacosSandbox);
        #[cfg(target_os = "linux")]
        return Selected::Linux(linux::LinuxSandbox);
        #[cfg(target_os = "windows")]
        return Selected::Windows(windows::WindowsSandbox);
    }
    Selected::Floor(FloorSandbox)
}

/// The portable floor backend: a fresh subprocess in an ephemeral working
/// directory, with resource caps and network env stripped. The guaranteed-present
/// isolation floor on every OS. Deliberately the weakest backend — filesystem-
/// scoped and resource-capped, not a syscall jail.
pub struct FloorSandbox;

impl Sandbox for FloorSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        run_capped(Backend::PortableFloor, spec, |_cmd| {}).await
    }

    fn backend(&self) -> Backend {
        Backend::PortableFloor
    }
}

/// Run `argv` in `workdir` under `limits`, capturing output and enforcing caps
/// that *kill*. `configure` is a backend hook to further restrict the command
/// (e.g. wrap it in `sandbox-exec`) before it is spawned; the floor passes a
/// no-op. Shared by the floor and the native unix backends so caps and teardown
/// live in one place.
///
/// Caps:
/// - **CPU** via `RLIMIT_CPU` (unix `pre_exec`) → SIGXCPU → [`Cap::Cpu`].
/// - **Memory** via an RSS poll-and-kill monitor → [`Cap::Memory`] (macOS does
///   not enforce address-space rlimits, so a monitor is the portable mechanism).
/// - **Wall** via a tokio timeout → [`Cap::Wall`].
async fn run_capped(
    backend: Backend,
    spec: RunSpec<'_>,
    configure: impl FnOnce(&mut tokio::process::Command),
) -> Result<SandboxOutcome> {
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

    let argv: Vec<String> = spec.argv.to_vec();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(spec.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Deny network on the floor best-effort by stripping proxy configuration.
    // A real kernel boundary comes from the native backends; documented as such.
    if !spec.allow_network {
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            cmd.env_remove(k);
        }
    }

    // Unix: apply rlimits in the child before exec. CPU is the reliable kill.
    #[cfg(unix)]
    {
        let cpu = spec.limits.max_cpu_secs;
        let nofile = spec.limits.max_open_files;
        // Note: max_processes is deliberately NOT mapped to RLIMIT_NPROC here —
        // that limit is per-real-uid, so it would throttle the whole login
        // session, not the sandbox. The native backends scope it per-sandbox.
        unsafe {
            cmd.pre_exec(move || {
                // The cast is load-bearing on macOS, where the RLIMIT_* constants
                // are c_int, and a no-op on Linux, where they are already u32 —
                // so clippy's unnecessary_cast fires on Linux only. Keep the cast
                // and silence it rather than cfg-splitting two lines.
                #[allow(clippy::unnecessary_cast)]
                {
                    set_rlimit(libc::RLIMIT_CPU as u32, cpu);
                    set_rlimit(libc::RLIMIT_NOFILE as u32, nofile);
                }
                Ok(())
            });
        }
    }

    configure(&mut cmd);

    let child = cmd.spawn().map_err(|e| crate::error::Error::Sandbox {
        reason: format!("could not spawn {}: {e}", argv[0]),
    })?;
    let pid = child.id();
    #[cfg(not(unix))]
    let _ = pid; // only the unix caps use the raw pid; kill_on_drop covers the rest

    // A flag set by whichever killer fired, so the outcome can name the cap.
    const NONE: u8 = 0;
    const MEM: u8 = 1;
    const WALL: u8 = 2;
    let flag = Arc::new(AtomicU8::new(NONE));

    // Memory monitor: poll RSS and kill on breach. Unix-only (uses `ps`); the
    // build host is macOS where address-space rlimits do not enforce.
    #[cfg(unix)]
    let mem_monitor = {
        let max = spec.limits.max_memory_bytes;
        let flag = Arc::clone(&flag);
        match (pid, max) {
            (Some(pid), Some(max)) => Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    match rss_bytes(pid) {
                        Some(rss) if rss > max => {
                            flag.store(MEM, Ordering::SeqCst);
                            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                            return;
                        }
                        Some(_) => {}
                        None => return, // process gone
                    }
                }
            })),
            _ => None,
        }
    };
    #[cfg(not(unix))]
    let mem_monitor: Option<tokio::task::JoinHandle<()>> = None;

    // Wall-clock cap: the OS-neutral backstop that always kills.
    let wall = spec.limits.max_wall_secs;
    let output = match wall {
        Some(secs) => {
            match tokio::time::timeout(
                std::time::Duration::from_secs(secs),
                child.wait_with_output(),
            )
            .await
            {
                Ok(res) => res?,
                Err(_elapsed) => {
                    flag.store(WALL, Ordering::SeqCst);
                    #[cfg(unix)]
                    if let Some(pid) = pid {
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    }
                    // Reap so nothing is orphaned; output is lost on a wall kill.
                    return Ok(SandboxOutcome {
                        backend,
                        argv,
                        exit_code: None,
                        cap_hit: Some(Cap::Wall),
                        stdout: String::new(),
                        stderr: String::new(),
                    });
                }
            }
        }
        None => child.wait_with_output().await?,
    };

    if let Some(m) = mem_monitor {
        m.abort();
    }

    let cap_hit = match flag.load(Ordering::SeqCst) {
        MEM => Some(Cap::Memory),
        WALL => Some(Cap::Wall),
        _ => cpu_capped(&output.status).then_some(Cap::Cpu),
    };

    Ok(SandboxOutcome {
        backend,
        argv,
        exit_code: output.status.code(),
        cap_hit,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Did the process die of SIGXCPU (the `RLIMIT_CPU` kill)?
#[cfg(unix)]
fn cpu_capped(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGXCPU)
}
#[cfg(not(unix))]
fn cpu_capped(_status: &std::process::ExitStatus) -> bool {
    false
}

/// Set a soft+hard rlimit to `value`; a `None` value leaves the limit alone.
/// Runs in the forked child before exec, so it must be async-signal-safe: only
/// `setrlimit`, no allocation.
#[cfg(unix)]
fn set_rlimit(resource: u32, value: Option<u64>) {
    if let Some(v) = value {
        let lim = libc::rlimit {
            rlim_cur: v as libc::rlim_t,
            rlim_max: v as libc::rlim_t,
        };
        unsafe {
            libc::setrlimit(resource as _, &lim);
        }
    }
}

/// Resident set size of `pid` in bytes, via `ps`. `None` if the process is gone.
/// macOS/BSD and Linux `ps` both report RSS in kibibytes.
#[cfg(unix)]
fn rss_bytes(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kb * 1024)
}

/// Create an ephemeral working directory for one sandboxed run, seeding it with
/// the files the command needs. Returned as a [`tempfile::TempDir`] so teardown
/// is a guaranteed drop — the directory is removed when it goes out of scope, on
/// every exit path including a panic or an early return.
pub fn workdir() -> Result<tempfile::TempDir> {
    Ok(tempfile::tempdir()?)
}

/// Copy files produced in the sandbox `workdir` back to `dest_root`, keeping only
/// those `allowed` accepts (the 0.4.0 write policy). Returns the relative paths
/// copied. So sandbox capture composes with the permission layer rather than
/// bypassing it: a file the policy would deny writing is not copied back.
pub async fn copy_back(
    workdir: &Path,
    dest_root: &Path,
    files: &[PathBuf],
    allowed: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut copied = Vec::new();
    for rel in files {
        if !allowed(rel) {
            continue;
        }
        let src = workdir.join(rel);
        if !src.exists() {
            continue;
        }
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::copy(&src, &dest).await?;
        copied.push(rel.clone());
    }
    Ok(copied)
}

// All three backend modules are always compiled — their logic (profile/argv
// construction, Job-Object limit mapping) is portable and unit-tested on the
// build host. Only the wiring into `select`/`Selected` is `cfg`-gated to the OS
// whose native primitives actually run. This is how the Linux and Windows
// backends "compile under their cfg and pass their backend unit tests" on a
// macOS host without a cross toolchain (rusqlite's bundled C blocks a full
// cross-check, which is an environment limit, not a limit of this code).
pub mod linux;
pub mod macos;
pub mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(argv: &'a [String], dir: &'a Path, limits: &'a SandboxLimits) -> RunSpec<'a> {
        RunSpec {
            argv,
            workdir: dir,
            limits,
            allow_network: false,
        }
    }

    #[tokio::test]
    async fn floor_runs_a_command_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["sh".into(), "-c".into(), "echo hello; exit 0".into()];
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &SandboxLimits::default()))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.backend, Backend::PortableFloor);
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn floor_reports_a_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["sh".into(), "-c".into(), "exit 3".into()];
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &SandboxLimits::default()))
            .await
            .unwrap();
        assert!(!out.success());
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.cap_hit, None);
    }

    #[tokio::test]
    async fn force_floor_selects_the_portable_backend() {
        let sb = select(&SandboxConfig::new().floor_only());
        assert_eq!(sb.backend(), Backend::PortableFloor);
    }

    #[test]
    fn config_and_limits_round_trip_through_serde() {
        let cfg = SandboxConfig::new();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: SandboxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[tokio::test]
    async fn cpu_cap_kills_a_busy_loop_and_names_the_cpu_cap() {
        let dir = tempfile::tempdir().unwrap();
        // A pure busy loop that would never finish; RLIMIT_CPU must kill it.
        let argv = vec!["sh".into(), "-c".into(), "while :; do :; done".into()];
        let limits = SandboxLimits {
            max_cpu_secs: Some(1),
            max_wall_secs: Some(30), // wall is the backstop; CPU should fire first
            ..SandboxLimits::default()
        };
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &limits))
            .await
            .unwrap();
        assert_eq!(out.cap_hit, Some(Cap::Cpu), "expected CPU cap, got {out:?}");
        assert!(!out.success());
    }

    #[tokio::test]
    async fn memory_cap_kills_a_heap_hog_and_names_the_memory_cap() {
        let dir = tempfile::tempdir().unwrap();
        // Grow RSS well past the cap; the monitor must kill it.
        let argv = vec![
            "sh".into(),
            "-c".into(),
            // perl builds a large string in RSS; portable enough on macOS.
            "perl -e '$x=\"a\"x(400*1024*1024); sleep 5'".into(),
        ];
        let limits = SandboxLimits {
            max_memory_bytes: Some(64 * 1024 * 1024), // 64 MiB
            max_wall_secs: Some(30),
            ..SandboxLimits::default()
        };
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &limits))
            .await
            .unwrap();
        assert_eq!(
            out.cap_hit,
            Some(Cap::Memory),
            "expected memory cap, got {out:?}"
        );
        assert!(!out.success());
    }

    #[tokio::test]
    async fn workdir_is_removed_on_drop() {
        let path = {
            let wd = workdir().unwrap();
            let p = wd.path().to_path_buf();
            assert!(p.exists());
            p
            // wd dropped here
        };
        assert!(!path.exists(), "sandbox workdir must be gone after drop");
    }

    #[tokio::test]
    async fn copy_back_honours_the_write_policy() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        tokio::fs::write(src.path().join("keep.txt"), "y")
            .await
            .unwrap();
        tokio::fs::write(src.path().join("secret.txt"), "n")
            .await
            .unwrap();

        let files = vec![PathBuf::from("keep.txt"), PathBuf::from("secret.txt")];
        let copied = copy_back(src.path(), dst.path(), &files, |p| {
            p != Path::new("secret.txt")
        })
        .await
        .unwrap();

        assert_eq!(copied, vec![PathBuf::from("keep.txt")]);
        assert!(dst.path().join("keep.txt").exists());
        assert!(
            !dst.path().join("secret.txt").exists(),
            "denied file must not be copied back"
        );
    }
}
