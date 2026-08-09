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
//! [`Sandbox`], has a native backend per platform — macOS `sandbox-exec`, Linux
//! namespaces, Windows Job Object — over a [portable floor](FloorSandbox) (fresh
//! subprocess, ephemeral tempdir, resource caps, network env stripped) that
//! compiles and runs on all three, so isolation is never *absent* on any OS the
//! crate builds for.
//! [`select`] picks the strongest backend this host can actually deliver — the
//! candidate by cfg, degraded to the floor if its primitive turns out to be
//! unavailable — and the one that ran is recorded.
//!
//! ## Backend isolation strength (documented, not hidden)
//!
//! - **macOS `sandbox-exec`** — a generated profile confines filesystem writes
//!   to the workdir and denies network; `setrlimit` caps CPU time and open file
//!   descriptors; memory is capped by an RSS monitor (macOS does not enforce
//!   `RLIMIT_AS`/`RLIMIT_DATA`). It does **not** cap the process count — see
//!   [`SandboxLimits::max_processes`], which only the Windows backend enforces.
//! - **Linux namespaces** — user + mount + pid + net namespaces give a hard
//!   network boundary and a private tmpfs; rlimits on top. The crate installs
//!   **no seccomp filter of its own**; what syscall filtering there is comes
//!   from whatever the kernel applies by default inside an unprivileged user
//!   namespace. Probed at runtime: a kernel that restricts unprivileged user
//!   namespaces gets the portable floor, reported as such. *(cfg-gated, not
//!   live-run on the macOS build host.)*
//! - **Windows Job Object** — a **resource** boundary, and only that. The job
//!   caps per-process committed memory, per-job user CPU time and the active
//!   process count, and kills the whole tree when its handle closes. It is also
//!   the only backend on any platform that enforces
//!   [`SandboxLimits::max_processes`]. What a Job Object has no facility for is
//!   the filesystem and the network: there is no path rule and no socket rule to
//!   set on one, so on Windows the filesystem scoping is still the floor's
//!   ephemeral workdir and egress denial is still the best-effort proxy-env
//!   strip. A Windows run is resource-contained, not jailed, and the two are not
//!   the same claim. See [`windows`].
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
///
/// The value is a *report*, never a promise: a native backend whose primitive
/// turns out to be unavailable degrades to the floor and says `PortableFloor`
/// here rather than naming an isolation it did not apply. So an application
/// deciding how much to trust a run reads this, instead of inferring isolation
/// from the OS it happens to be running on.
///
/// ```
/// use io_harness::sandbox::{select, Backend, Sandbox, SandboxConfig};
///
/// let backend = select(&SandboxConfig::new()).backend();
/// if backend == Backend::PortableFloor {
///     // Filesystem-scoped and resource-capped, but not a syscall jail, and
///     // network deny is only a proxy-env strip. Refuse genuinely untrusted
///     // work here rather than running it believing it is confined.
///     eprintln!("no kernel isolation on this host: {}", backend.as_str());
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// macOS `sandbox-exec` profile + rlimits + RSS monitor.
    MacosSandboxExec,
    /// Linux Landlock: a filesystem ruleset restricting this process and every
    /// descendant, applied between fork and exec, plus `PR_SET_NO_NEW_PRIVS`, a
    /// seccomp deny-list, and the shared rlimits.
    ///
    /// **No namespace is involved**, which is the entire reason this rung
    /// exists: a stock Ubuntu 24.04 refuses an unprivileged user namespace and
    /// ships Landlock enabled, so this is what a contained run on the commonest
    /// Linux CI image actually gets. It is also the only rung with no wrapper
    /// process — the restriction is installed in the child itself — so the argv
    /// spawned is the argv asked for.
    ///
    /// Egress is denied here only when the kernel's Landlock ABI carries the
    /// network rules (4 and later). A run that denies egress on an older kernel
    /// is given a lower rung instead of this one, so this backend never names a
    /// network boundary it did not apply.
    LinuxLandlock,
    /// Linux `bwrap` (bubblewrap): a mount namespace with the tree bound
    /// read-only and the run's writable roots bound back over it, plus a network
    /// namespace when egress is denied, plus the shared rlimits.
    ///
    /// Beneath [`LinuxLandlock`](Backend::LinuxLandlock) because it needs a
    /// helper the host may not have; above [`LinuxNamespaces`](Backend::LinuxNamespaces)
    /// because a setuid `bwrap` works on the hosts whose kernel refuses this
    /// crate's own `unshare` wrapper.
    LinuxBubblewrap,
    /// Linux user/mount/pid/net namespaces + rlimits. The crate installs no
    /// seccomp filter; only the kernel's own defaults for an unprivileged user
    /// namespace apply on top.
    LinuxNamespaces,
    /// Windows AppContainer **and** Job Object together: a low-box token that
    /// answers *no* to every securable object it was not granted, with an
    /// explicit ACE per granted path and a capability array that is empty unless
    /// the run's policy permits egress — plus the Job Object's memory, CPU and
    /// active-process limits and its kill-on-close.
    ///
    /// This is the only Windows backend that confines **access**. A run
    /// reporting it is both access-confined and resource-contained; a run
    /// reporting [`WindowsJobObject`](Backend::WindowsJobObject) is the second
    /// only. See [`appcontainer`].
    WindowsAppContainer,
    /// Windows Job Object: memory, CPU and active-process limits, and a
    /// tree kill on close. A **resource** boundary and nothing else — a Job
    /// Object has no filesystem facility and no network facility, so a run
    /// reporting this is resource-contained, not jailed. See [`windows`].
    WindowsJobObject,
    /// The portable floor: subprocess + ephemeral workdir + caps + env strip.
    PortableFloor,
}

impl Backend {
    /// A stable label for the trace and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::MacosSandboxExec => "macos-sandbox-exec",
            Backend::LinuxLandlock => "linux-landlock",
            Backend::LinuxBubblewrap => "linux-bubblewrap",
            Backend::LinuxNamespaces => "linux-namespaces",
            Backend::WindowsAppContainer => "windows-appcontainer",
            Backend::WindowsJobObject => "windows-job-object",
            Backend::PortableFloor => "portable-floor",
        }
    }
}

/// A resource cap that was breached, killing the sandboxed process. Returned in
/// [`SandboxOutcome::cap_hit`] so a cap hit is a *typed* result, never a hang.
///
/// Worth matching on rather than folding into "it failed": a process killed by
/// a cap has no exit code at all, so a caller reading only
/// [`exit_code`](SandboxOutcome::exit_code) sees `None` and loses the one fact
/// that says whether to raise a limit or fix the code.
///
/// ```
/// use io_harness::sandbox::{Cap, SandboxOutcome};
///
/// fn why(outcome: &SandboxOutcome) -> String {
///     match outcome.cap_hit {
///         Some(Cap::Wall) => "hung: outlived max_wall_secs".into(),
///         Some(Cap::Cpu) => "spun: burned max_cpu_secs of CPU".into(),
///         Some(Cap::Memory) => "grew past max_memory_bytes".into(),
///         Some(Cap::Processes) => "forked past max_processes".into(),
///         // No cap fired, so the exit code is the whole story.
///         None => format!("exited {:?}", outcome.exit_code),
///         // `Cap` is `#[non_exhaustive]` from 0.24.0, so this arm is required
///         // and is the point of the attribute: a cap added in a later release
///         // reaches here instead of failing your build.
///         Some(other) => format!("stopped by {}", other.as_str()),
///     }
/// }
///
/// // A cap is also what goes in the trace, by this stable label.
/// assert_eq!(Cap::Wall.as_str(), "wall");
/// # let _ = why;
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cap {
    /// CPU time. `RLIMIT_CPU` on unix (the process took SIGXCPU); the Job
    /// Object's per-job user-time limit on Windows (the job terminated it).
    Cpu,
    /// Memory. An RSS monitor killed it on unix; on Windows the job refused the
    /// commit that would have crossed the limit and the process died of that.
    Memory,
    /// Wall-clock time (the run outlived `max_wall_secs`).
    Wall,
    /// The active-process limit. **Windows only** — the Job Object's
    /// `ActiveProcessLimit` is the one mechanism this crate has that bounds the
    /// process count *per sandbox*; unix `RLIMIT_NPROC` is per-real-uid and is
    /// deliberately not used. See [`SandboxLimits::max_processes`].
    ///
    /// Unlike the other three this one is not a kill: the job denies the
    /// `CreateProcess` that would have crossed the limit, and the run fails
    /// because its own spawn failed. The distinction matters to anyone reading
    /// the trace — the payload was *stopped*, not shot.
    Processes,
}

impl Cap {
    /// A stable label for the trace and error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Cap::Cpu => "cpu",
            Cap::Memory => "memory",
            Cap::Wall => "wall",
            Cap::Processes => "processes",
        }
    }
}

/// How much of the machine a run's commands may reach.
///
/// The mode decides **where a command may write**. It does not decide the
/// resource caps ([`SandboxLimits`]) and it does not decide egress, which comes
/// from the run's own [`Policy`](crate::Policy) — one authority per question.
///
/// [`WorkspaceWrite`](ExecMode::WorkspaceWrite) is the default, and that is the
/// 0.46.0 change: every release up to 0.45.0 ran commands at the embedding
/// program's own privileges unless the caller opted into containment, so the
/// widest grant the crate makes was spelled as a field nobody set. It is now
/// spelled as a method call.
///
/// ```
/// use io_harness::{ExecMode, TaskContract};
///
/// // Contained, without having asked: commands may write inside the workspace
/// // root, the system temp directory and the detected toolchain's caches.
/// let default = TaskContract::workspace("run the test suite", "/repo");
/// assert_eq!(default.exec_sandbox.mode, ExecMode::WorkspaceWrite);
///
/// // The run that genuinely needs the machine says so where a reader sees it.
/// let wide = TaskContract::workspace("upgrade the host toolchain", "/repo")
///     .with_full_access();
/// assert_eq!(wide.exec_sandbox.mode, ExecMode::FullAccess);
///
/// // And the label is what reaches the trace and the agent's own prompt.
/// assert_eq!(ExecMode::ReadOnly.as_str(), "read-only");
/// ```
///
/// **What a mode is worth depends on the backend the host could give.** A
/// Windows Job Object has no filesystem facility and the
/// [`PortableFloor`](Backend::PortableFloor) has none either, so on those hosts
/// the mode is routed and reported and enforces nothing for the filesystem —
/// read it off [`Sandbox::backend`] rather than assuming, exactly as with
/// [`Backend`] itself.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecMode {
    /// The workspace is readable and no command may write into it. The system
    /// temp directory stays writable, because a toolchain that cannot open a
    /// temporary file cannot start at all.
    ReadOnly,
    /// Commands may write inside the workspace root, the system temp directory
    /// and the detected toolchain's own cache directories, and nowhere else.
    /// **The default.**
    #[default]
    WorkspaceWrite,
    /// The embedding program's own privileges — no wrapping, no confinement.
    /// What every release up to 0.45.0 did by default, and what
    /// [`TaskContract::with_full_access`](crate::TaskContract::with_full_access)
    /// now asks for explicitly.
    FullAccess,
}

impl ExecMode {
    /// A stable label for the trace, the prompt and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecMode::ReadOnly => "read-only",
            ExecMode::WorkspaceWrite => "workspace-write",
            ExecMode::FullAccess => "full-access",
        }
    }

    /// Whether a command under this mode is wrapped by a backend at all.
    ///
    /// [`FullAccess`](ExecMode::FullAccess) is the one mode that reaches no
    /// backend, so this is the question every dispatch site asks rather than
    /// matching the variant in six places.
    ///
    /// ```
    /// use io_harness::ExecMode;
    ///
    /// assert!(ExecMode::WorkspaceWrite.is_contained());
    /// assert!(ExecMode::ReadOnly.is_contained());
    /// assert!(!ExecMode::FullAccess.is_contained());
    /// ```
    pub fn is_contained(&self) -> bool {
        !matches!(self, ExecMode::FullAccess)
    }
}

/// Resource caps applied to a sandboxed run. Serde-serializable like
/// [`crate::Policy`] and [`crate::Containment`] so io-cli and io-studio load it
/// from config rather than hand-building it.
///
/// Defaults are sized so an ordinary `rustc`/`cargo` verification passes out of
/// the box — a default that failed real compiles would push callers to disable
/// the sandbox entirely. Tighten via the fields for untrusted work.
///
/// These caps **kill**; they do not throttle. A breach terminates the process
/// and comes back as [`SandboxOutcome::cap_hit`], so a runaway is a typed
/// result the gate can report rather than a verification that never returns.
///
/// ```
/// use io_harness::sandbox::{SandboxConfig, SandboxLimits};
///
/// // Tighter than the defaults, for code you did not write. Thirty wall-
/// // seconds is enough for a small `rustc` invocation and not enough for an
/// // infinite loop to be interesting; the CPU cap catches a spin that the
/// // wall clock would let idle-wait past.
/// let config = SandboxConfig {
///     limits: SandboxLimits {
///         max_cpu_secs: Some(5),
///         max_wall_secs: Some(30),
///         max_memory_bytes: Some(256 * 1024 * 1024),
///         max_open_files: Some(64),
///         // Set, but read the field docs before relying on it: only the
///         // Windows Job Object enforces a process count, so on a unix host
///         // this line still buys nothing.
///         max_processes: Some(16),
///         ..SandboxLimits::default()
///     },
///     ..SandboxConfig::new()
/// };
///
/// // Only the wall cap is enforced on every platform under every backend, so
/// // it is the one that must never be left `None` — it is what bounds a run
/// // whose backend degraded to the floor.
/// assert!(config.limits.max_wall_secs.is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxLimits {
    /// Max CPU seconds before the run is killed. `None` = no CPU cap.
    ///
    /// Two different mechanisms, both real: `RLIMIT_CPU` and SIGXCPU on unix,
    /// which counts user *and* system time; the Job Object's per-job time limit
    /// on Windows, which counts **user-mode time only** and terminates every
    /// process in the job at once. A payload that burns its seconds inside the
    /// kernel is therefore capped on unix and not on Windows — the Win32 API
    /// offers no per-job kernel-time limit to set, so this is the ceiling of the
    /// mechanism rather than a choice.
    pub max_cpu_secs: Option<u64>,
    /// Max wall-clock seconds before the run is killed. `None` = no wall cap.
    /// The one cap enforced on **every** platform under **every** backend, and
    /// the only one the portable floor applies on Windows — leaving it `None`
    /// under the floor there means the run is bounded by nothing.
    pub max_wall_secs: Option<u64>,
    /// Max memory before the run is stopped. `None` = no cap.
    ///
    /// Two mechanisms again, and they differ in more than implementation. On
    /// unix an RSS monitor polls the process tree and kills it after the fact,
    /// so a payload can briefly exceed the cap before it dies. On Windows the
    /// Job Object's per-process commit limit is enforced *by the allocator*: the
    /// commit that would cross the line simply fails, the payload never holds
    /// more than this many bytes, and it usually dies of the allocation failure
    /// rather than of a kill. Either way the outcome reports [`Cap::Memory`].
    pub max_memory_bytes: Option<u64>,
    /// Max concurrent processes in the sandbox. **Enforced on Windows only**,
    /// by the Job Object's `ActiveProcessLimit` — see [`windows`]. On every
    /// other platform setting it still changes nothing.
    ///
    /// The portable floor and the unix native backends deliberately do not map
    /// it to `RLIMIT_NPROC`: that limit is per-real-uid, not per-sandbox, so
    /// capping it there would throttle the operator's whole login session
    /// rather than the sandboxed run. Windows is the first platform where the
    /// crate has a mechanism that scopes the count to *this run* and nothing
    /// else. The other one that could — the Linux pid namespace's process limit
    /// — is still not wired up.
    ///
    /// So this is the one limit whose meaning is genuinely OS-dependent, and it
    /// is stated rather than smoothed over: a config that relies on it for
    /// containment is relying on running on Windows. `None` = no cap, and on a
    /// non-Windows host any other value means the same thing.
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

impl SandboxLimits {
    /// Every cap unset — a boundary with no ceiling.
    ///
    /// This is what [`TaskContract`](crate::TaskContract) carries by default, and
    /// the distinction it draws is the whole reason it exists. Defaulting
    /// *containment* on is a claim about where a command may write. Defaulting
    /// [`SandboxLimits::default`]'s ceilings on would be a claim about how long
    /// someone else's build may take — 120 wall seconds was sized in 0.6.0 for a
    /// verification gate compiling one crate, and a run killed at two minutes by a
    /// limit its author never set is indistinguishable from a bug in this crate.
    ///
    /// ```
    /// use io_harness::sandbox::{SandboxConfig, SandboxLimits};
    /// use io_harness::TaskContract;
    ///
    /// // The default: confined, uncapped.
    /// let default = TaskContract::workspace("build the project", "/repo");
    /// assert_eq!(default.exec_sandbox.limits, SandboxLimits::none());
    ///
    /// // The standing caps are one call away, and that call is where they belong.
    /// let capped = TaskContract::workspace("run untrusted code", "/repo")
    ///     .with_contained_exec(SandboxConfig::new());
    /// assert_eq!(capped.exec_sandbox.limits.max_wall_secs, Some(120));
    /// ```
    pub fn none() -> Self {
        Self {
            max_cpu_secs: None,
            max_wall_secs: None,
            max_memory_bytes: None,
            max_processes: None,
            max_open_files: None,
        }
    }
}

/// How the sandbox is configured for a run.
///
/// The *absence* of a `SandboxConfig` on the exec path means opt out: the
/// verification gate runs on the host exactly as it did in 0.5.0. Its presence
/// turns isolation on. This is what makes 0.6.0 additive and reversible.
///
/// ```
/// use io_harness::sandbox::{select, Backend, Sandbox, SandboxConfig};
///
/// // The recommended default: caps that kill, egress denied, and the
/// // strongest backend this host can actually deliver.
/// let config = SandboxConfig::new();
/// assert!(!config.allow_network, "network is denied by default, not allowed");
///
/// // `floor_only` pins every platform to the same weakest backend. Useful for
/// // reproducing a report from a host whose native primitive was unavailable,
/// // and for exercising the floor on a machine that would otherwise never
/// // take it.
/// let floor = SandboxConfig::new().floor_only();
/// assert_eq!(select(&floor).backend(), Backend::PortableFloor);
/// ```
///
/// It derives `Serialize`/`Deserialize` for the same reason [`crate::Policy`]
/// does: io-cli and io-studio load one from a config file rather than
/// hand-building it. Each of its own three fields is `#[serde(default)]`, so a
/// config file may name only what it changes — note that `limits`, if given at
/// all, is a whole [`SandboxLimits`] and every cap in it must be spelled out.
///
/// ```
/// use io_harness::sandbox::{SandboxConfig, SandboxLimits};
///
/// let config: SandboxConfig = serde_json::from_str(r#"{"allow_network": true}"#).unwrap();
/// assert!(config.allow_network);
/// assert_eq!(config.limits, SandboxLimits::default(), "the caps fall back whole");
/// ```
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
    /// Where a command may write (0.46.0). Default
    /// [`WorkspaceWrite`](ExecMode::WorkspaceWrite).
    ///
    /// Sits here rather than beside the caps because it is a run-shaping knob
    /// like the two above it, and because [`Config::sandbox`](crate::Config)
    /// already assembles this type from an `io.toml` `[sandbox]` section — a
    /// mode named there needs no second path.
    #[serde(default)]
    pub mode: ExecMode,
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

    /// The same config under a different [`ExecMode`].
    ///
    /// ```
    /// use io_harness::sandbox::SandboxConfig;
    /// use io_harness::ExecMode;
    ///
    /// let read_only = SandboxConfig::new().with_mode(ExecMode::ReadOnly);
    /// assert_eq!(read_only.mode, ExecMode::ReadOnly);
    /// // The caps are untouched: the mode is a boundary, not a ceiling.
    /// assert_eq!(read_only.limits, SandboxConfig::new().limits);
    /// ```
    pub fn with_mode(mut self, mode: ExecMode) -> Self {
        self.mode = mode;
        self
    }
}

/// One command to run in the sandbox. OS-neutral by construction — no
/// OS-specific type appears here, so the [`Sandbox`] trait signature is portable.
///
/// ```
/// use io_harness::sandbox::{RunSpec, SandboxLimits};
/// use io_harness::ExecMode;
///
/// let argv = vec!["cargo".to_string(), "test".to_string()];
/// let limits = SandboxLimits::default();
/// let roots = vec![std::path::PathBuf::from("/home/u/.cargo")];
///
/// let spec = RunSpec::new(&argv, std::path::Path::new("/repo"), &limits)
///     .with_mode(ExecMode::WorkspaceWrite)
///     .with_writable_roots(&roots);
///
/// assert_eq!(spec.mode, ExecMode::WorkspaceWrite);
/// assert!(!spec.allow_network, "egress is denied unless the caller says otherwise");
/// ```
///
/// `#[non_exhaustive]` since 0.46.0, which added [`RunSpec::mode`] and
/// [`RunSpec::writable_roots`]. It is built with [`RunSpec::new`] and narrowed
/// with the `with_*` methods; a struct literal outside this crate is what stopped
/// compiling, once, so that the fields 0.47.0 and 0.48.0 add cost nobody anything.
#[non_exhaustive]
pub struct RunSpec<'a> {
    /// The command and its arguments. `argv[0]` is the program.
    pub argv: &'a [String],
    /// The isolated working directory the command runs in.
    pub workdir: &'a Path,
    /// Resource caps for this run.
    pub limits: &'a SandboxLimits,
    /// Whether outbound network is permitted (default-deny lives in the caller).
    pub allow_network: bool,
    /// Where this command may write (0.46.0).
    ///
    /// [`ExecMode::FullAccess`] never reaches a backend — a command under it is
    /// not wrapped at all — so a `RunSpec` carrying it is a caller asking a
    /// backend to run something the run itself decided not to confine, and the
    /// backends treat it as [`ExecMode::WorkspaceWrite`] rather than inventing a
    /// fourth behaviour.
    pub mode: ExecMode,
    /// Directories this command may write to besides [`RunSpec::workdir`].
    ///
    /// Two rules, and both are load-bearing. **They are absolute paths that exist
    /// on this host**: the Linux backend binds each one, a bind of a path that is
    /// not there fails its mount setup, and a failed setup degrades the whole
    /// backend to [`Backend::PortableFloor`] — so a root that does not exist would
    /// silently unwind the confinement it was added to preserve. And **the workdir
    /// is not among them**, because a backend grants that separately and a
    /// duplicate would be a second `(allow …)` line saying what the first already
    /// said.
    pub writable_roots: &'a [PathBuf],
}

impl<'a> RunSpec<'a> {
    /// A command in `workdir` under `limits`: egress denied,
    /// [`ExecMode::WorkspaceWrite`], no extra writable roots.
    pub fn new(argv: &'a [String], workdir: &'a Path, limits: &'a SandboxLimits) -> Self {
        Self {
            argv,
            workdir,
            limits,
            allow_network: false,
            mode: ExecMode::WorkspaceWrite,
            writable_roots: &[],
        }
    }

    /// Permit or deny outbound network for this command.
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Run under a different [`ExecMode`].
    pub fn with_mode(mut self, mode: ExecMode) -> Self {
        self.mode = mode;
        self
    }

    /// Grant write access to these directories besides the workdir. They must be
    /// absolute and must exist — see [`RunSpec::writable_roots`].
    pub fn with_writable_roots(mut self, roots: &'a [PathBuf]) -> Self {
        self.writable_roots = roots;
        self
    }
}

/// The result of a sandboxed run — enough to make a verification pass/fail
/// decision identical to the un-sandboxed path, plus the isolation metadata.
///
/// ```
/// use io_harness::sandbox::SandboxOutcome;
///
/// /// Turn one sandboxed command into something a person can act on.
/// fn report(outcome: &SandboxOutcome) -> String {
///     // `success()` is the gate's whole question: a zero exit *and* no cap.
///     // Testing `exit_code == Some(0)` alone reads a capped run — which has
///     // no exit code — as merely "not zero", losing the reason.
///     if outcome.success() {
///         return format!("passed, isolated by {}", outcome.backend.as_str());
///     }
///     match outcome.cap_hit {
///         Some(cap) => format!("killed by the {} cap", cap.as_str()),
///         // Compiler and test failures land in stderr; it is what the model
///         // is shown so it can fix the code on the next step.
///         None => format!("exited {:?}:\n{}", outcome.exit_code, outcome.stderr),
///     }
/// }
/// # let _ = report;
/// ```
///
/// `argv` and `backend` are recorded in the trace, so an audit answers both
/// what ran and how it was confined — including when the backend that answered
/// was weaker than the one this platform advertises.
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
///
/// Reach for it directly when the embedding program wants to run something
/// under the same isolation the verification gate gets — a build, a linter, a
/// script the agent produced — rather than shelling out beside the harness.
///
/// ```
/// use io_harness::sandbox::{select, workdir, RunSpec, Sandbox, SandboxConfig};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let config = SandboxConfig::new();
/// let sandbox = select(&config);
///
/// // An ephemeral workdir whose teardown is its drop: the directory and
/// // everything the command wrote in it are gone when `dir` goes out of
/// // scope, on every exit path including a panic or an early `?`.
/// let dir = workdir()?;
/// std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
///
/// let argv = vec!["rustc".to_string(), "main.rs".to_string()];
/// let outcome = sandbox
///     .run(
///         RunSpec::new(&argv, dir.path(), &config.limits)
///             .with_network(config.allow_network)
///             .with_mode(config.mode),
///     )
///     .await?;
///
/// // Anything worth keeping is copied out deliberately, through
/// // `copy_back`, so the write policy still decides. Nothing leaks by
/// // default.
/// println!("{} under {}", outcome.success(), outcome.backend.as_str());
/// # Ok(())
/// # }
/// ```
///
/// The trait is RPITIT and therefore not object-safe — there is no
/// `Box<dyn Sandbox>`. [`select`] returns the concrete [`Selected`] enum
/// instead, which implements this trait.
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
///
/// Its variants are cfg-gated to the OS whose primitives they use, so a `match`
/// over them does not port. Use it through [`Sandbox`] and ask
/// [`backend`](Sandbox::backend) what it turned out to be — a native variant
/// whose primitive failed its probe still reports [`Backend::PortableFloor`],
/// so the variant you hold and the isolation you got are not the same question.
///
/// ```
/// use io_harness::sandbox::{select, Backend, Sandbox, SandboxConfig};
///
/// let selected = select(&SandboxConfig::new());
/// let confined = selected.backend() != Backend::PortableFloor;
/// println!("kernel-level isolation: {confined}");
/// ```
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

/// Pick the strongest backend this host can actually deliver: the native rung
/// for this target, or the portable floor when `force_floor` skips it (so the
/// floor can be exercised everywhere).
///
/// The *candidate* is chosen at compile time by cfg, but a native backend whose
/// primitive is unavailable degrades to the floor and reports
/// [`Backend::PortableFloor`] rather than naming an isolation it did not apply —
/// [`linux`] probes its `unshare` wrapper (0.9.1: Ubuntu 24.04 restricts
/// unprivileged user namespaces, and every wrapped spawn failed there), and
/// [`windows`] falls back the same way when the job object cannot be created.
/// Since the backend is recorded in the trace, a degraded run is auditable, not
/// silent. Use [`Sandbox::backend`] on the result to see what will really run.
///
/// Which is the point of the example: ask, never assume. Compiling for Linux
/// does not mean you got namespaces. Ubuntu 24.04 ships
/// `kernel.apparmor_restrict_unprivileged_userns=1`, every `unshare`-wrapped
/// spawn fails there, and before 0.9.1 that surfaced to the caller as its code
/// having failed verification. Now the wrapper is probed once per process and
/// this returns a backend that reports the floor.
///
/// ```
/// use io_harness::sandbox::{select, Backend, Sandbox, SandboxConfig};
///
/// let sandbox = select(&SandboxConfig::new());
/// match sandbox.backend() {
///     Backend::PortableFloor => {
///         // Ephemeral workdir and caps that kill, but no syscall jail and no
///         // kernel network boundary — egress denial here is only a proxy-env
///         // strip, which a payload not reading those variables ignores. This
///         // is the branch where an application handling genuinely untrusted
///         // code decides to refuse rather than proceed.
///         eprintln!("degraded to the portable floor on this host");
///     }
///     native => eprintln!("native isolation: {}", native.as_str()),
/// }
/// ```
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
/// - **CPU** via `RLIMIT_CPU` (unix `pre_exec`) → SIGXCPU → [`Cap::Cpu`]. *Unix
///   only here* — on Windows the CPU cap is the Job Object's per-job time limit,
///   applied by [`windows`], not by this function.
/// - **Memory** via an RSS poll-and-kill monitor → [`Cap::Memory`] (macOS does
///   not enforce address-space rlimits, so a monitor is the portable mechanism).
///   *Unix only here* — the monitor reads the process table with `ps`, which does
///   not exist on Windows; the Windows memory bound is the job's commit limit.
/// - **Wall** via a tokio timeout → [`Cap::Wall`]. The one cap that applies on
///   **every** platform and under **every** backend, and therefore the backstop
///   that fires when nothing else can.
///
/// A cap the platform cannot apply is never *claimed*: [`SandboxOutcome::cap_hit`]
/// only ever names a cap that really fired, and a run whose backend has no
/// CPU/memory mechanism warns once rather than letting a caller believe the
/// limits it configured are in force.
/// Apply the unix resource caps a sandboxed spawn gets, to a command the caller
/// owns and will spawn itself.
///
/// Extracted in 0.40.0 because there are now two spawn paths that must cap a
/// child identically: this module's own runner, and the `shell` tool, which
/// pipes stages into one another and therefore cannot hand its argv to
/// [`Sandbox::run`] at all. Two copies of a `pre_exec` block is two places for a
/// cap to lapse, and the lapse would be silent — an uncapped child runs fine.
///
/// A no-op on Windows, where the Job Object bounds the whole job rather than
/// each process.
#[allow(unused_variables)]
pub(crate) fn apply_rlimits(cmd: &mut tokio::process::Command, limits: &SandboxLimits) {
    #[cfg(unix)]
    {
        let cpu = limits.max_cpu_secs;
        let nofile = limits.max_open_files;
        // Note: max_processes is deliberately NOT mapped to RLIMIT_NPROC here —
        // that limit is per-real-uid, so it would throttle the whole login
        // session, not the sandbox. The native backends scope it per-sandbox.
        unsafe {
            cmd.pre_exec(move || {
                // The cast is load-bearing on macOS, where the RLIMIT_* constants
                // are c_int, and a no-op on Linux, where they are already u32 —
                // so clippy's unnecessary_cast fires on Linux only. Keep the cast
                // and silence it rather than cfg-splitting two lines.
                // A cap that could not be applied fails the spawn: running the
                // payload uncapped is worse than not running it.
                #[allow(clippy::unnecessary_cast)]
                {
                    set_rlimit(libc::RLIMIT_CPU as u32, cpu)?;
                    set_rlimit(libc::RLIMIT_NOFILE as u32, nofile)?;
                }
                Ok(())
            });
        }
    }
}

/// The containment a run resolved once, at run start (0.46.0).
///
/// Two things travel together everywhere a command is dispatched — the config the
/// contract asked for, and the writable roots that config's [`ExecMode`] grants on
/// *this* host — and resolving them per call would mean re-reading the environment
/// on a hot path and, worse, letting two call sites disagree about what a mode
/// means. The two loops build one of these and hand it to `dispatch`; the exec
/// tool, the shell tool and the verification gate all read it.
pub(crate) struct ExecContainment {
    /// What the contract asked for, with egress already resolved from the run's
    /// own policy.
    pub(crate) config: SandboxConfig,
    /// Directories besides the workdir this run's commands may write to. Absolute,
    /// existing, deduplicated — see [`RunSpec::writable_roots`] for why each of
    /// those three matters.
    pub(crate) roots: Vec<PathBuf>,
}

impl ExecContainment {
    /// Resolve the roots this config's mode grants on this host.
    ///
    /// The system temporary directory is deliberately **not** in the list: both
    /// native backends already grant it unconditionally (`/private/var/folders` in
    /// the macOS profile, `${TMPDIR:-/tmp}` in the Linux mount setup), and a
    /// second grant saying what the first already said is a line that can drift.
    pub(crate) fn resolve(
        config: &SandboxConfig,
        toolchain: Option<&crate::toolchain::Toolchain>,
    ) -> Self {
        let roots = if config.mode == ExecMode::WorkspaceWrite {
            writable_cache_roots(toolchain)
        } else {
            Vec::new()
        };
        Self {
            config: config.clone(),
            roots,
        }
    }

    /// The same containment with egress decided by the run's own policy.
    ///
    /// Egress is the one part that cannot be resolved once at run start: a plan
    /// gate narrows the effective policy mid-run, so the answer is the policy's at
    /// the moment of the call. The roots and the mode do not move, which is why
    /// they are resolved once and this is not.
    pub(crate) fn with_egress(&self, allow_network: bool) -> Self {
        Self {
            config: SandboxConfig {
                allow_network,
                ..self.config.clone()
            },
            roots: self.roots.clone(),
        }
    }

    /// The backend that will actually run this containment's commands.
    pub(crate) fn backend(&self) -> Backend {
        select(&self.config).backend()
    }

    /// A [`RunSpec`] for one command under this containment.
    pub(crate) fn spec<'a>(&'a self, argv: &'a [String], workdir: &'a Path) -> RunSpec<'a> {
        RunSpec::new(argv, workdir, &self.config.limits)
            .with_network(self.config.allow_network)
            .with_mode(self.config.mode)
            .with_writable_roots(&self.roots)
    }
}

/// The toolchain cache directories this host actually has, as writable roots.
///
/// Absolute, present, and each named once. **The exists-filter is the
/// confinement's own guard, not tidiness**: the Linux mount setup binds every
/// root it is given, a bind of a path that is not there `fail`s the setup, and a
/// failed setup degrades the whole backend to [`Backend::PortableFloor`] — so a
/// granted path that does not exist would silently unwind the confinement it was
/// added to preserve.
///
/// Shared by the run's own containment and by the verification gate, which
/// configure their sandboxes from different places and must not filter
/// differently.
pub(crate) fn writable_cache_roots(
    toolchain: Option<&crate::toolchain::Toolchain>,
) -> Vec<PathBuf> {
    let Some(tc) = toolchain else {
        return Vec::new();
    };
    let mut roots = tc.cache_dirs();
    roots.retain(|p| p.is_absolute() && p.is_dir());
    roots.sort();
    roots.dedup();
    roots
}

/// The argv this host's backend would run, and the backend it chose.
///
/// The native backends confine a command by *wrapping its argv* — macOS prepends
/// `sandbox-exec -p <profile>`, Linux prepends `unshare` — which is what makes
/// containment available to a caller that cannot use [`Sandbox::run`] because it
/// needs the child itself. The `shell` tool is that caller: its stages are piped
/// into one another, so it owns every `Child` and cannot delegate the spawn.
///
/// The returned [`Backend`] is what actually applied, not what was asked for. A
/// host whose primitive failed its probe returns [`Backend::PortableFloor`] and
/// an unwrapped argv — the caller must record that rather than the isolation it
/// wanted, or a run contained less than it claimed becomes indistinguishable
/// from one that was contained.
pub(crate) fn wrap_argv(
    config: &SandboxConfig,
    workdir: &Path,
    allow_network: bool,
    writable_roots: &[PathBuf],
    argv: &[String],
) -> (Backend, Vec<String>) {
    let backend = select(config).backend();
    #[cfg(target_os = "macos")]
    if backend == Backend::MacosSandboxExec {
        let profile = macos::profile_for(workdir, allow_network, config.mode, writable_roots);
        let mut wrapped = vec!["sandbox-exec".to_string(), "-p".to_string(), profile];
        wrapped.extend(argv.iter().cloned());
        return (backend, wrapped);
    }
    #[cfg(target_os = "linux")]
    if backend == Backend::LinuxNamespaces {
        return (
            backend,
            linux::unshare_argv(argv, workdir, allow_network, config.mode, writable_roots),
        );
    }
    let _ = writable_roots;
    (backend, argv.to_vec())
}

async fn run_capped(
    backend: Backend,
    spec: RunSpec<'_>,
    configure: impl FnOnce(&mut tokio::process::Command),
) -> Result<SandboxOutcome> {
    run_capped_hooked(backend, spec, configure, |_child| Ok(())).await
}

/// [`run_capped`] plus a second hook, `started`, which runs on the *spawned*
/// child before anything else touches it.
///
/// The two hooks exist because two genuinely different moments matter, and only
/// one of them is expressible as "mutate the `Command`". `configure` shapes the
/// command; `started` acts on the process that command produced. The Windows Job
/// Object needs the second: a process can only be assigned to a job once it
/// exists, and it must be assigned *before it executes a single instruction*, or
/// it can spawn a descendant that never joins the job and outlives the run. The
/// backend closes that window by spawning `CREATE_SUSPENDED` in `configure` and
/// doing the assignment-then-resume here, where the child is alive and still
/// frozen.
///
/// A `started` that fails is fatal to the run and kills the child on the way
/// out: a process left suspended, or running outside the containment its backend
/// promised, is worse than a spawn that never happened. Every unix backend and
/// the floor pass a no-op, which is why they go through [`run_capped`] and never
/// see this signature.
async fn run_capped_hooked(
    backend: Backend,
    spec: RunSpec<'_>,
    configure: impl FnOnce(&mut tokio::process::Command),
    started: impl FnOnce(&tokio::process::Child) -> Result<()>,
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
    apply_rlimits(&mut cmd, spec.limits);

    configure(&mut cmd);

    let child = cmd.spawn().map_err(|e| crate::error::Error::Sandbox {
        reason: format!("could not spawn {}: {e}", argv[0]),
    })?;
    let pid = child.id();

    // The child exists and, if the backend asked for it, has not run yet. This
    // is the only instant at which a Job Object assignment is both possible and
    // race-free, so it is where the hook goes — before the wall clock starts,
    // before the memory monitor, before anything can read its output.
    if let Err(e) = started(&child) {
        // Whatever the backend was setting up did not take. Kill rather than
        // continue: a child that is still suspended would otherwise sit there
        // until the wall clock, and one that is running uncontained would be
        // running under a backend name that no longer describes it.
        kill_tree(pid);
        return Err(e);
    }

    // Say once, out loud, what this run cannot enforce. The CPU cap is
    // `RLIMIT_CPU` and the memory cap is an RSS monitor over `ps` — both unix
    // mechanisms — so a non-unix run gets the wall clock and nothing else,
    // *unless* it is a Windows Job Object run, where the job supplies both.
    // A cap silently not applied is worse than no cap: the caller thinks it has
    // one. A warning for a cap that *is* applied is just as bad in the other
    // direction, which is why the backend is part of the condition.
    #[cfg(not(unix))]
    if backend != Backend::WindowsJobObject
        && (spec.limits.max_cpu_secs.is_some() || spec.limits.max_memory_bytes.is_some())
    {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::warn!(
                "sandbox: the CPU and memory caps are unix-only mechanisms and are NOT applied \
                 on this platform; only the wall-clock cap is enforced"
            )
        });
    }

    // A flag set by whichever killer fired, so the outcome can name the cap.
    const NONE: u8 = 0;
    const MEM: u8 = 1;
    const WALL: u8 = 2;
    let flag = Arc::new(AtomicU8::new(NONE));

    // Memory monitor: poll the process *tree*'s RSS and kill it on breach.
    // Unix-only (uses `ps`); the build host is macOS where address-space rlimits
    // do not enforce. The tree rather than the pid because a payload that forks
    // — which is what Linux `/bin/sh` does — otherwise evades the cap entirely.
    #[cfg(unix)]
    let mem_monitor = {
        let max = spec.limits.max_memory_bytes;
        let flag = Arc::clone(&flag);
        match (pid, max) {
            (Some(pid), Some(max)) => Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    let Some(tree) = process_tree(pid) else {
                        // The process table could not be read this time. That is
                        // not "the process is gone" — keep polling rather than
                        // switching the cap off for the rest of the run.
                        continue;
                    };
                    if tree.is_empty() {
                        return; // process gone
                    }
                    if tree.iter().map(|(_, rss)| rss).sum::<u64>() > max {
                        flag.store(MEM, Ordering::SeqCst);
                        // Descendants first: killing the root alone would only
                        // reparent the hog and leave it running.
                        for (p, _) in tree.iter().rev() {
                            unsafe { libc::kill(*p as libc::pid_t, libc::SIGKILL) };
                        }
                        return;
                    }
                }
            })),
            _ => None,
        }
    };
    // No monitor where it cannot measure: `process_tree` is unix-only, so on
    // Windows there is no RSS poller at all rather than one that reads nothing
    // and quietly never fires.
    #[cfg(not(unix))]
    let mem_monitor: Option<tokio::task::JoinHandle<()>> = None;

    // Wall-clock cap: the OS-neutral backstop that always kills.
    //
    // The wait runs as its own task so the *timeout does not own the child*.
    // Letting the timeout own it (the shape until 0.9.1) means expiry drops the
    // child first, and the only kill left is `kill_on_drop` — which terminates
    // just the process the harness spawned. Its descendants survive: on unix
    // they reparent, and on Windows they also keep the stdout/stderr pipes open,
    // which strands the blocking pipe reads tokio uses there and hangs the
    // caller's runtime long after the cap "fired". Holding the child alive past
    // expiry lets [`kill_tree`] reach the whole tree by pid instead.
    let waiter = tokio::spawn(async move { child.wait_with_output().await });
    let wall = spec.limits.max_wall_secs;
    let waited = match wall {
        Some(secs) => {
            match tokio::time::timeout(std::time::Duration::from_secs(secs), waiter).await {
                Ok(joined) => joined,
                Err(_elapsed) => {
                    flag.store(WALL, Ordering::SeqCst);
                    // Dropping the JoinHandle detaches the wait, it does not
                    // cancel it — the child is still running and still killable.
                    kill_tree(pid);
                    if let Some(m) = mem_monitor {
                        m.abort();
                    }
                    // Output is lost on a wall kill; the detached wait reaps.
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
        None => waiter.await,
    };
    let output = waited.map_err(|e| crate::error::Error::Sandbox {
        reason: format!("the sandbox wait task did not finish: {e}"),
    })??;

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

/// Set an rlimit's soft value to `value`, keeping the hard limit *above* it; a
/// `None` value leaves the limit alone.
///
/// The soft/hard split is load-bearing on Linux: `check_process_timers` tests the
/// hard limit first and `SIGKILL`s there, so a `RLIMIT_CPU` with soft == hard
/// never sends `SIGXCPU` and [`cpu_capped`] never sees the cap it set. macOS
/// sends `SIGXCPU` either way, which is why this only ever showed up on Linux.
/// The hard limit is clamped to what `getrlimit` reports — lowering it is
/// irreversible for the child and raising it is not permitted to an unprivileged
/// process, so it is only ever lowered, never raised.
///
/// Runs in the forked child before exec, so it must be async-signal-safe: only
/// `getrlimit`/`setrlimit`, no allocation (`last_os_error` just wraps `errno`).
/// A cap that could not be applied is an error, not a shrug — the caller fails
/// the spawn rather than running the payload uncapped.
#[cfg(unix)]
fn set_rlimit(resource: u32, value: Option<u64>) -> std::io::Result<()> {
    let Some(v) = value else { return Ok(()) };
    let v = v as libc::rlim_t;
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        if libc::getrlimit(resource as _, &mut lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Never raise the hard limit, and never ask for a soft limit above it.
        lim.rlim_cur = v.min(lim.rlim_max);
        lim.rlim_max = lim.rlim_max.min(v.saturating_add(1));
        if libc::setrlimit(resource as _, &lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Make the process this command spawns the leader of a process group of its
/// own, so that [`kill_tree_and_group`] can signal the whole group later.
///
/// This is the containment a *process handle* needs and a foreground call does
/// not. A foreground line is awaited, held in a `Vec` with `kill_on_drop(true)`,
/// and gone before the dispatch that started it returns; a handle outlives its
/// call by design, and by the time anything kills it the tree it built has had
/// minutes to rearrange itself. Killing by parent/child links — which is all
/// [`kill_tree`] can do — misses a grandchild whose parent has already exited,
/// because the link it would have walked no longer exists. Group membership has
/// no such gap: it is inherited across `fork`, it survives the parent's death,
/// and a process that never asks to leave never leaves. One `killpg` therefore
/// reaches exactly the processes this handle is responsible for, however deep
/// they are and whoever their parent is by then.
///
/// `setpgid(0, 0)` rather than `setsid()` on purpose. Both would give the child
/// its own group; `setsid` would additionally put it in a new session and drop
/// the controlling terminal, which is a second behaviour change nothing here
/// needs and one that changes how the payload sees its own tty. The narrower
/// call is the one whose effects are all wanted.
///
/// A failure fails the spawn, exactly as an rlimit that could not be applied
/// does: a handle whose processes are not contained is a handle whose kill
/// cannot be relied on, and the crate promises the kill.
#[cfg(unix)]
pub(crate) fn own_process_group(cmd: &mut tokio::process::Command) {
    // SAFETY: `pre_exec` runs in the forked child, after `fork` and before
    // `exec`, where the only calls that are legal are async-signal-safe ones —
    // the child shares the parent's address space locks and must not allocate,
    // take a lock, or call back into arbitrary Rust. The closure below calls
    // `setpgid` and, on failure, `last_os_error`, which only reads `errno`.
    // Nothing here allocates and nothing captures a destructor.
    unsafe {
        cmd.pre_exec(|| {
            // Both zeros mean "this process, its own pid as the group", which is
            // what makes the child a group leader whose group id equals its pid
            // — the equality `kill_tree_and_group` checks before it signals.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Kill the process group `pid` leads, and its tree, and `pid` itself.
///
/// The counterpart to [`own_process_group`] and the only kill that closes the
/// grandchild gap: the group reaches every descendant the spawn ever produced,
/// including the ones whose parents are already gone, which is precisely what
/// walking the process table cannot do.
///
/// The group signal is sent **only** when `pid` really is a group leader — when
/// `getpgid(pid)` answers with `pid` itself. That check is not a formality. For
/// a process this crate did not put in its own group, `getpgid` answers with the
/// group it happens to be *in*, which is the harness's own group, and signalling
/// that would kill the harness and everything it is running. So the check is
/// what makes this function safe to call on any pid at all, including one
/// spawned before this containment existed.
///
/// [`kill_tree`] still runs afterwards, because the two mechanisms fail in
/// different directions: a process that left the group by calling `setpgid` on
/// itself is invisible to the group kill and still reachable by the walk, and a
/// pid whose group could not be read is still reachable directly.
pub(crate) fn kill_tree_and_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        let pid = pid as libc::pid_t;
        // SAFETY: both calls take a pid by value and return a status; neither
        // dereferences anything and neither can touch memory this process owns.
        // A pid that is already gone answers with an error, which is the
        // best-effort contract this shares with `kill_tree`.
        unsafe {
            if libc::getpgid(pid) == pid {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    kill_tree(Some(pid));
}

/// Kill `pid` and everything it spawned, on whatever OS this is.
///
/// Killing only `pid` is not enough anywhere: a payload run through a shell puts
/// the real work in a *child*, so the single kill takes the shell and reparents
/// the work. Unix walks [`process_tree`] and signals descendants first (killing
/// the root first would orphan them before they can be found). Windows has
/// neither signals nor `ps`, so it uses the tree kill the OS itself ships,
/// `taskkill /T` — a system utility, not a new dependency. Best-effort by
/// design: a process that is already gone is a success, not an error.
pub(crate) fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        for (p, _) in process_tree(pid).unwrap_or_default().iter().rev() {
            unsafe { libc::kill(*p as libc::pid_t, libc::SIGKILL) };
        }
        // Always signal the root, even when the process table could not be read.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        use std::process::Stdio;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Every process in `pid`'s tree — the process and its descendants — with each
/// one's RSS in bytes. macOS/BSD and Linux `ps` both report RSS in kibibytes.
///
/// The tree, not the pid, is what the memory cap has to measure: a shell that
/// *forks* its payload (Linux `/bin/sh` does) leaves the monitor watching a
/// 2 MiB shell while its child takes 400 MiB, and the cap silently never fires.
///
/// Two return shapes, deliberately distinct: `Some(empty)` means the pid is no
/// longer in the process table (it is gone, stop polling); `None` means the
/// table could not be read *this time* — a fork failure, an unexpected `ps` —
/// which is not evidence of anything and must not switch the cap off.
///
// ponytail: one `ps` fork per poll and an O(tree × table) scan. Fine for a
// handful of processes at 25 Hz; read /proc directly if a run ever spawns
// hundreds.
#[cfg(unix)]
fn process_tree(pid: u32) -> Option<Vec<(u32, u64)>> {
    let out = std::process::Command::new("ps")
        .args(["-eo", "pid=,ppid=,rss="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(u32, u32, u64)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let p = f.next()?.parse().ok()?;
            let pp = f.next()?.parse().ok()?;
            let kb = f.next()?.parse::<u64>().ok()?;
            Some((p, pp, kb * 1024))
        })
        .collect();
    if rows.is_empty() {
        return None; // the table itself is unreadable — not "the process is gone"
    }
    let mut tree: Vec<(u32, u64)> = rows
        .iter()
        .filter(|(p, _, _)| *p == pid)
        .map(|(p, _, rss)| (*p, *rss))
        .collect();
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i].0;
        for (p, pp, rss) in &rows {
            if *pp == parent && *p != parent && !tree.iter().any(|(t, _)| t == p) {
                tree.push((*p, *rss));
            }
        }
        i += 1;
    }
    Some(tree)
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
///
/// The `allowed` predicate is the whole reason this is not a directory copy.
/// Everything a sandboxed command produces is otherwise dropped with the
/// workdir, so capture is the one path back out — and if it did not consult
/// the policy, it would be the hole in it: a file the agent may not write
/// directly would arrive in the workspace merely by having been produced
/// somewhere the write check does not run.
///
/// ```
/// use std::path::PathBuf;
///
/// use io_harness::sandbox::copy_back;
/// use io_harness::{Act, Effect, Policy};
///
/// # async fn demo(sandbox_dir: &std::path::Path, repo: &std::path::Path)
/// #     -> io_harness::Result<()> {
/// let policy = Policy::default()
///     .layer("app")
///     .allow_write("*")
///     .deny_write("secrets/*");
///
/// let produced = vec![
///     PathBuf::from("src/lib.rs"),
///     PathBuf::from("secrets/leaked.pem"),
/// ];
/// let copied = copy_back(sandbox_dir, repo, &produced, |rel| {
///     policy.check(Act::Write, &rel.to_string_lossy()).effect == Effect::Allow
/// })
/// .await?;
///
/// // The denied path is simply not there. It stays in the workdir and dies
/// // with it; the return value is what actually landed, so a caller can
/// // report the difference rather than guess at it.
/// assert!(!copied.contains(&PathBuf::from("secrets/leaked.pem")));
/// # Ok(())
/// # }
/// ```
///
/// A listed file that the sandbox never produced is skipped rather than an
/// error: a command that failed part way leaves a partial set, and the caller
/// already has the failure from [`SandboxOutcome`].
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

// The AppContainer half is the exception to the paragraph above: it has no
// portable logic to unit-test on the build host, because unlike a Job Object's
// limit mapping there is no pure-data layer between the configuration and the
// Win32 calls. The module is therefore a `cfg(windows)` shell around a
// `cfg(windows)` body rather than a portable type with a gated implementation,
// and it is proven on the Windows runner or nowhere.
pub mod appcontainer;

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(argv: &'a [String], dir: &'a Path, limits: &'a SandboxLimits) -> RunSpec<'a> {
        RunSpec::new(argv, dir, limits)
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

    // The CPU cap is `RLIMIT_CPU`, a unix mechanism with no Windows equivalent;
    // asserting it there would assert a cap the floor deliberately never applies.
    #[cfg(unix)]
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

    // The memory cap is an RSS monitor over `ps`; both the monitor and `ps` are
    // unix-only, so this is a unix mechanism asserted on unix.
    #[cfg(unix)]
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

    // Same unix-only mechanism, exercised through a fork. See above.
    #[cfg(unix)]
    #[tokio::test]
    async fn memory_cap_kills_a_hog_the_shell_forked() {
        let dir = tempfile::tempdir().unwrap();
        // The shell *forks* the hog instead of exec'ing it — which is what
        // Linux /bin/sh (dash) does even without the explicit `&`. The monitor
        // must sum the process tree, not the single pid it spawned, or the cap
        // watches a 2 MiB shell while its child takes 400 MiB.
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "perl -e '$x=\"a\"x(400*1024*1024); sleep 5' & wait".into(),
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
            "a forked hog must still hit the memory cap, got {out:?}"
        );
        assert!(!out.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_wall_clock_kill_reaches_the_children_the_run_forked() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // The payload forks a child that outlives the wall clock and leaves a
        // file behind if it is still alive. Killing only the pid the harness
        // spawned reparents that child and it goes on to write the file.
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "(sleep 4; touch survived) & wait".into(),
        ];
        let limits = SandboxLimits {
            max_wall_secs: Some(1),
            max_cpu_secs: None,
            ..SandboxLimits::default()
        };
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &limits))
            .await
            .unwrap();
        assert_eq!(
            out.cap_hit,
            Some(Cap::Wall),
            "wall must kill it, got {out:?}"
        );
        assert!(!out.success());
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert!(
            !marker.exists(),
            "a forked child must not outlive the wall-clock kill"
        );
    }

    // What is actually true on Windows: no CPU cap and no memory cap are applied
    // there (both are unix mechanisms), so the wall clock is the only thing that
    // can stop an endless run — and the outcome must name the cap that really
    // fired rather than one of the two nobody enforced.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_enforces_the_wall_clock_and_claims_no_cap_it_did_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        // `for /L` with a step of 0 never terminates. Passed as separate argv
        // entries, none containing a space, so nothing depends on how `cmd.exe`
        // re-parses a quoted command line.
        let argv: Vec<String> = [
            "cmd", "/C", "for", "/L", "%i", "in", "(1,0,2)", "do", "@rem",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let limits = SandboxLimits {
            max_cpu_secs: Some(1),
            max_memory_bytes: Some(1024 * 1024),
            max_wall_secs: Some(5),
            ..SandboxLimits::default()
        };
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &limits))
            .await
            .unwrap();
        assert_eq!(
            out.cap_hit,
            Some(Cap::Wall),
            "the wall clock is the only cap Windows applies, got {out:?}"
        );
        assert!(!out.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_capped_run_keeps_its_hard_limit_above_the_soft_one() {
        let dir = tempfile::tempdir().unwrap();
        // Linux's CPU timer tests the HARD limit first and SIGKILLs there, so a
        // cap set with soft == hard never sends SIGXCPU and `cpu_capped` never
        // sees it. The child's own view of its limits is the portable oracle.
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "ulimit -S -n; ulimit -H -n".into(),
        ];
        let limits = SandboxLimits {
            max_open_files: Some(64),
            ..SandboxLimits::default()
        };
        let out = FloorSandbox
            .run(spec(&argv, dir.path(), &limits))
            .await
            .unwrap();
        let seen: Vec<u64> = out
            .stdout
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        assert_eq!(seen.len(), 2, "expected soft and hard, got {out:?}");
        assert_eq!(seen[0], 64, "soft limit must be what was asked for");
        assert!(
            seen[1] > seen[0],
            "hard limit must stay above the soft one, got {out:?}"
        );
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
