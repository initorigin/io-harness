//! Windows backend — a **Job Object**, and only a Job Object.
//!
//! A job object is a kernel container for *resources*. Processes assigned to one
//! share its accounting and its limits, and the kernel enforces them: committed
//! memory per process, user-mode CPU time per job, how many processes may be
//! alive at once, and — the reason this backend is worth its dependency — a
//! guaranteed teardown of the whole tree when the job handle closes. That last
//! one is not best-effort. `taskkill /T`, which is what the shared path uses and
//! what Windows runs got before this, walks parent/child links in the process
//! table; a grandchild whose parent has already exited is no longer reachable
//! that way and survives the kill. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` does not
//! walk anything. Membership in a job is inherited and cannot be renounced, so
//! closing the handle terminates every descendant however it was spawned and
//! whoever its parent is by then.
//!
//! ## What this is not
//!
//! **A job object has no filesystem facility and no network facility.** There is
//! no path rule to set on one and no socket rule to set on one; those are not
//! options this backend declines to use, they do not exist in the API. So on
//! Windows:
//!
//! - **Filesystem** — still only the floor's ephemeral working directory. The
//!   payload can read and write anywhere its user token can. Scoping, not
//!   confinement.
//! - **Network** — still only the floor's best-effort proxy-env strip, which a
//!   payload that does not read those variables ignores completely. There is no
//!   kernel egress boundary on Windows and this release does not add one; that
//!   is AppContainer's job and it is not this.
//! - **Syscalls** — unfiltered. A job is not a jail.
//!
//! A run that reports [`Backend::WindowsJobObject`] is **resource-contained**,
//! not isolated, and the two are different claims. Read it that way in an audit.
//!
//! ## Ordering, which is the whole correctness argument
//!
//! A process must be inside the job before it executes its first instruction. A
//! process that runs even briefly outside can spawn a descendant that is never a
//! member, and that descendant then outlives the run, ignores every limit, and —
//! the part that makes this a real bug rather than a small one — *nothing
//! reports a failure*. The job is created, the limits are set, every call
//! returns success, and the containment silently has a hole in it.
//!
//! So the child is spawned `CREATE_SUSPENDED`: created, but with its initial
//! thread frozen before it runs a single instruction. It is assigned to the job
//! in that state and only then resumed. There is no window, because there is no
//! moment at which the process is both running and unassigned.
//!
//! The awkward part is the resume. `CreateProcess` hands back a handle to the
//! initial thread, but `std`'s `Command` closes it before returning a `Child`,
//! so by the time this code has a process there is no thread handle left to call
//! `ResumeThread` on. The documented way back to it is a ToolHelp thread
//! snapshot filtered by owning process id — an unlovely detour, and the reason
//! this file needs `Win32_System_Diagnostics_ToolHelp` at all. It is safe here
//! for a specific reason: the harness holds the child's process handle open for
//! the whole call, so Windows cannot recycle that process id, so every thread the
//! snapshot attributes to it really is a thread of *this* child. A suspended
//! process has exactly one thread, so resuming everything the snapshot returns
//! resumes exactly the initial thread.
//!
//! ## How a cap is attributed
//!
//! Three of the four limits behave differently from their unix counterparts, and
//! the trace should not pretend otherwise:
//!
//! - **CPU** — the per-job user-time limit; the OS terminates every process in
//!   the job when it is exceeded. Kernel time is not counted, because the API
//!   offers no per-job kernel-time limit to set.
//! - **Memory** — the per-process commit limit is enforced by the *allocator*:
//!   the allocation that would cross it fails. The payload is never allowed to
//!   hold more than the cap, and typically dies of its own failed allocation
//!   rather than of a kill.
//! - **Processes** — `CreateProcess` past the limit fails. The payload is
//!   stopped, not shot.
//!
//! None of the three announces itself, so after the run the job is asked what it
//! accounted for and the answer is turned into a [`super::Cap`]. This is
//! evidence, not a
//! notification, and it is deliberately only consulted when the run actually
//! failed. The wall-clock cap never reaches this code at all — that path returns
//! from the shared runner before the job is queried — so a timeout cannot be
//! reported as a CPU or memory breach.

use std::path::{Path, PathBuf};

use super::{Backend, ExecMode, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The Windows backend: every command runs inside a fresh Job Object whose
/// handle is held for exactly the length of the run, so teardown is the drop.
pub struct WindowsSandbox;

impl Sandbox for WindowsSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        #[cfg(windows)]
        {
            // **The Job Object, and only the Job Object.** The AppContainer half
            // of 0.47.0 was specified here and taken out of the release whole —
            // see `US-IO-HARNESS-0.47.0-I01` and the 0.59.0 roadmap entry. The
            // module below is built and unit-tested and reached by nothing on
            // this path, which is where it has been since 0.26.0.
            job::run(spec).await
        }
        // The type is compiled everywhere so its limit mapping can be unit-tested
        // on the build host (see the note above `pub mod windows` in the parent).
        // Off Windows there is no job to create, so it is the floor, and it says
        // so — a backend name is a report, never a promise.
        #[cfg(not(windows))]
        {
            super::run_capped(Backend::PortableFloor, spec, |_cmd| {}).await
        }
    }

    /// The Job Object, which is what a contained Windows run gets.
    ///
    /// A **resource** boundary: memory, CPU, active processes, and a tree kill
    /// when the handle closes. Not an access boundary, and this method saying so
    /// plainly is the point — `ExecMode` is routed and reported on this platform
    /// and enforces nothing for the filesystem, which `docs/CONTRACT.md` states
    /// in the same words. The access half is 0.59.0.
    fn backend(&self) -> Backend {
        #[cfg(windows)]
        {
            Backend::WindowsJobObject
        }
        #[cfg(not(windows))]
        {
            Backend::PortableFloor
        }
    }
}

/// The Windows **access** backend: an AppContainer inside a Job Object.
///
/// Selected only when the caller set
/// [`access_confinement`](super::SandboxConfig::access_confinement), which is
/// the whole of the difference between this and [`WindowsSandbox`]. What it adds
/// is the two columns a job object has no facility for: a filesystem the
/// container's token is default-denied on and reaches only by explicit ACE, and
/// a network the token holds no capability for unless the run's policy permits
/// egress.
///
/// **A decline is an error here, and that is the point.** Every other backend in
/// this crate degrades to a weaker rung and reports it, because the caller asked
/// for *a* run and would rather have a weaker boundary than none. This one was
/// asked for by name. A run that quietly took the Job Object instead is a run
/// with no access boundary at all whose every assertion still passes — 0.47.0
/// read exactly that as proof the container had run `cargo`, twice — so the
/// grant that could not be applied is returned to the caller with its reason.
pub struct WindowsAppContainerSandbox;

impl Sandbox for WindowsAppContainerSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        #[cfg(windows)]
        {
            match job::run_contained(&spec).await {
                Some(outcome) => outcome,
                None => Err(crate::error::Error::Sandbox {
                    reason: format!(
                        "this run asked for access confinement and it could not be applied on \
                         this host: {}. Nothing was started — a command run under the job \
                         object instead would have had no filesystem and no network boundary, \
                         which is the failure this refusal exists to prevent",
                        job::last_decline().unwrap_or_else(|| "no reason was recorded".to_string())
                    ),
                }),
            }
        }
        // Off Windows this type is compiled so the selection logic beside it can
        // be read and tested on the build host, and it is never selected there.
        #[cfg(not(windows))]
        {
            super::run_capped(Backend::PortableFloor, spec, |_cmd| {}).await
        }
    }

    fn backend(&self) -> Backend {
        #[cfg(windows)]
        {
            Backend::WindowsAppContainer
        }
        #[cfg(not(windows))]
        {
            Backend::PortableFloor
        }
    }
}

/// The Job Object limits derived from [`super::SandboxLimits`], as the flags and
/// values a `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` carries. Factored out as pure
/// data so the mapping — the part with the unit conversion in it, which is the
/// part that would be wrong silently — is testable on any host, without Win32.
//
// Off Windows nothing but the tests below constructs one, and a plain
// `cargo check` on the library sees a type no code touches. The allowance is
// deliberate and narrow: the mapping lives out here rather than behind
// `cfg(windows)` exactly so a macOS or Linux build host can still test it, which
// is worth one silenced lint on the platforms that cannot run it.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobLimits {
    /// Process-memory cap in bytes, if any.
    pub process_memory: Option<u64>,
    /// Active-process cap, if any.
    pub active_processes: Option<u64>,
    /// Per-job CPU-time cap in 100-ns ticks, if any.
    pub cpu_ticks: Option<u64>,
    /// Kill the whole job (process tree) when the job handle closes.
    pub kill_on_close: bool,
}

impl From<&super::SandboxLimits> for JobLimits {
    fn from(l: &super::SandboxLimits) -> Self {
        Self {
            process_memory: l.max_memory_bytes,
            active_processes: l.max_processes,
            // Windows measures job CPU time in 100-nanosecond ticks.
            cpu_ticks: l.max_cpu_secs.map(|s| s.saturating_mul(10_000_000)),
            // Never configurable. It is the only teardown on this platform that
            // reaches a re-parented grandchild, so a run that opted out of it
            // would be a run whose containment cannot be relied on.
            kill_on_close: true,
        }
    }
}

/// What an AppContainer may do with one path.
///
/// Mirrors `appcontainer::Access`, out here where it can be decided and tested
/// on any host. The two are kept as separate types on purpose: this one is the
/// *decision* and that one is the ACE mask, and a build host that has no Win32
/// can still assert the decision.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    /// **Pass through this directory, and read nothing in it.** `FILE_TRAVERSE`
    /// and `FILE_READ_ATTRIBUTES`, with no `FILE_LIST_DIRECTORY`: a name may be
    /// resolved *through* the directory, and the directory itself cannot be
    /// enumerated and its files cannot be opened.
    ///
    /// **Why a granted path is not reachable without this.** Opening a file by a
    /// relative name resolves it against the working-directory handle the process
    /// was created with, and no component is re-checked. Opening the *same file*
    /// by absolute path walks every component from the volume root, and each one
    /// is an access check the container has to pass. So a payload the run granted
    /// in full was refused whenever it was named absolutely and permitted
    /// whenever it was named relative to the working directory — the same bytes,
    /// the same ACE, two different questions asked of the kernel.
    ///
    /// This is the weakest right that answers it. The crate still refuses to
    /// grant the user's profile directory, and traverse is not a retreat from
    /// that: it permits reaching `%TEMP%`, not reading what is beside it.
    Traverse,
    /// Read and execute. What a binary, a toolchain or a read-only input tree
    /// needs, and the most that should ever be given to one.
    ReadExecute,
    /// Everything. The workspace and the roots the run resolved, and nothing
    /// else — these are the directories the payload is *meant* to change.
    Full,
}

/// How far into a directory one grant is meant to reach.
///
/// Windows inheritance is static: a child carries the DACL it was created with,
/// so an inheritable ACE added to a directory reaches what that directory gains
/// *later* and nothing it already holds. Re-propagating to what is already there
/// is a second, much more expensive act — it rewrites every object under the
/// path — and it is not always the right one.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The directory and everything already inside it. What a path the run
    /// *names* needs: a workspace whose source files predate the run, a registry
    /// cache whose crates were downloaded last week.
    Tree,
    /// The directory itself, and what it comes to hold afterwards.
    DirectoryOnly,
}

/// One granted path.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrantedPath {
    pub(crate) path: PathBuf,
    pub(crate) grant: Grant,
    pub(crate) reach: Reach,
}

/// The grant set for one run.
///
/// **This is the problem 0.26.0 declined to solve, and 0.46.0 solved most of it
/// without meaning to.** An AppContainer is default-deny for reads, so selecting
/// one means naming every path an arbitrary toolchain needs — which 0.26.0
/// correctly called a discovery problem and left the module unwired for. Then
/// 0.46.0 made a run resolve its own writable roots: the workspace, the system
/// temporary directory and the detected toolchain's cache directories, already
/// exists-filtered. So the list is now *derived from facts the run has* rather
/// than guessed, and what remains is the small fixed read-execute set a process
/// needs in order to start at all.
///
/// "Derived" is not "complete", and the release says so rather than implying
/// otherwise: a toolchain reading a machine-wide configuration file outside this
/// set will be refused. The answers to that are the escape hatch
/// (`TaskContract::with_full_access`) and a named addition here with its reason,
/// not a wider default. In particular the user's profile directory is **not**
/// granted: that is where credentials live, and a default-deny boundary whose
/// first act is to hand over the home directory is not one.
#[allow(dead_code)]
pub(crate) fn grants(
    mode: ExecMode,
    workdir: &Path,
    writable_roots: &[PathBuf],
    toolchain_roots: &[PathBuf],
    program_dir: Option<&Path>,
    system_root: Option<&Path>,
    tmp: &Path,
) -> Vec<GrantedPath> {
    let mut out: Vec<GrantedPath> = Vec::new();

    // The workspace: writable unless the mode withholds it, readable either way.
    // Under `ReadOnly` this is the mode's entire difference, exactly as it is on
    // the unix rungs.
    out.push(GrantedPath {
        path: workdir.to_path_buf(),
        grant: if mode == ExecMode::ReadOnly {
            Grant::ReadExecute
        } else {
            Grant::Full
        },
        reach: Reach::Tree,
    });

    if mode != ExecMode::ReadOnly {
        for root in writable_roots {
            out.push(GrantedPath {
                path: root.clone(),
                grant: Grant::Full,
                reach: Reach::Tree,
            });
        }
    }
    // The temporary directory in every mode: a toolchain that cannot open a
    // temporary file cannot run at all. The same allowance every other backend
    // makes.
    //
    // **`DirectoryOnly`, and it is a boundary decision before it is a cost.**
    // What a toolchain needs here is the ability to *create* a temporary file,
    // and a new file inherits the ACE from the directory. What it does not need
    // — and what a default-deny container has no business handing over — is
    // every temporary file every other program on the machine has already
    // written. `%TEMP%` is shared, it is large, and it is being written to by
    // other processes while this runs, which made re-propagating it the one
    // grant in this set that was both expensive and racy: a payload whose ACE
    // this crate's own test could read a moment earlier was refused, because a
    // concurrent run's propagation had recomputed that file's DACL in between.
    out.push(GrantedPath {
        path: tmp.to_path_buf(),
        grant: Grant::Full,
        reach: Reach::DirectoryOnly,
    });

    // Read-execute on the toolchain homes this machine names, and the reason is a
    // launcher rather than a compiler. `rustc` on `PATH` is a rustup **shim**: it
    // reads `RUSTUP_HOME` to find out which toolchain it stands for and then
    // starts a second binary inside it. Granting the shim's own directory is not
    // enough, and the failure is not a permission message — the shim cannot see
    // its home, concludes it must create one, and reports
    // "could not create home directory ... Cannot create a file when that file
    // already exists". nvm, volta, pyenv and the JVM launchers all have the same
    // shape.
    //
    // `CARGO_HOME` is deliberately **not** in this set even though it is the same
    // kind of directory: it holds `credentials.toml`, and this set is read-execute
    // for a payload. What a cargo build needs out of it arrives as a writable
    // cache root, which is the run's own resolved fact and the caller's decision.
    for dir in toolchain_roots {
        out.push(GrantedPath {
            path: dir.clone(),
            grant: Grant::ReadExecute,
            reach: Reach::Tree,
        });
    }

    // Read-execute on the two places a process needs to start: its own program's
    // directory and the system root. Without these an AppContainer cannot load
    // the binary or the system libraries it links, and the run fails in a way
    // that looks like the payload being broken.
    for dir in [program_dir, system_root].into_iter().flatten() {
        out.push(GrantedPath {
            path: dir.to_path_buf(),
            grant: Grant::ReadExecute,
            reach: Reach::Tree,
        });
    }

    // **Every ancestor of every granted path, traverse-only.** A grant that
    // cannot be reached by name is not a grant, and an absolute path is checked
    // component by component — see `Grant::Traverse`. Added last and deduplicated
    // against the set above, so a directory that is already granted something
    // stronger is never weakened to a traverse: `retain` keeps the first entry
    // for a path and these are the last ones added.
    let named: Vec<PathBuf> = out.iter().map(|g| g.path.clone()).collect();
    let mut seen: Vec<PathBuf> = named.clone();
    for path in named {
        for ancestor in path.ancestors().skip(1) {
            // A bare prefix (`C:`) is not a directory and cannot carry an ACE.
            // The volume root is skipped outright rather than attempted: a
            // process that does not own the machine cannot rewrite its DACL, and
            // it already permits an AppContainer to pass through — a path under
            // the profile was reachable as far as its last granted component
            // before any of this existed, which is the evidence that the volume
            // root and `C:\Users` were never the missing link.
            if ancestor.as_os_str().is_empty()
                || ancestor.parent().is_none()
                || seen.iter().any(|p| p == ancestor)
            {
                continue;
            }
            seen.push(ancestor.to_path_buf());
            out.push(GrantedPath {
                path: ancestor.to_path_buf(),
                grant: Grant::Traverse,
                reach: Reach::DirectoryOnly,
            });
        }
    }
    out
}

/// Join an argv into one Windows command line.
///
/// Windows passes a *string*, not a vector, and every process parses it back
/// itself. The rules being followed are the documented MSVCRT ones: a backslash
/// run is literal unless it precedes a quote, in which case it is doubled, and a
/// quote inside an argument is escaped.
///
/// Out here rather than in the Win32 module so it is asserted on the build host.
/// A quoting bug is the kind of defect that only shows up on the one argument
/// containing a space, which is every path on this platform.
#[allow(dead_code)]
pub(crate) fn command_line(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut slashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => {
                    slashes += 1;
                    out.push('\\');
                }
                '"' => {
                    // The run before a quote is doubled, then the quote escaped.
                    for _ in 0..slashes {
                        out.push('\\');
                    }
                    slashes = 0;
                    out.push_str("\\\"");
                }
                _ => {
                    slashes = 0;
                    out.push(c);
                }
            }
        }
        // A run at the very end sits before the closing quote, so it is doubled
        // too — the case that is easiest to leave out and hardest to notice.
        for _ in 0..slashes {
            out.push('\\');
        }
        out.push('"');
    }
    out
}

/// The Win32 half. Compiled only on Windows; everything above this line builds
/// on every host so the mapping and its test do too.
///
/// `pub(crate)` since 0.26.0, and only so that [`Job`](job::Job) can be reached
/// from `crate::tools::handles`. A process handle needs exactly the containment
/// this module already builds — a job that kills its tree when its handle closes
/// — and the only thing it needs differently is a *lifetime*: the sandbox's job
/// lives for one call, a handle's lives until the handle is killed. That is a
/// difference in who owns the handle, not a difference in the mechanism, so the
/// mechanism is shared rather than written twice.
// The grant set, the container spawn and everything they reach are unselected
// with the rest of the Windows access half — see the note above
// `sandbox::appcontainer::win`. The Job Object below is what a contained Windows
// run gets and is reached normally.
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod job {
    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        JOBOBJECTINFOCLASS, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    use super::{Grant, JobLimits};
    use crate::error::{Error, Result};
    use crate::sandbox::{run_capped, run_capped_hooked, Backend, Cap, RunSpec, SandboxOutcome};

    /// Can this host build an AppContainer at all?
    ///
    /// Attempted, not inferred: a profile is created and dropped, which is where
    /// a host with AppContainers disabled by policy fails. One attempt per
    /// process — the answer cannot change under a running one.
    pub(crate) fn container_available() -> bool {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            crate::sandbox::appcontainer::win::Profile::create(&profile_name("probe"), false)
                .is_ok()
        })
    }

    /// Run one command inside an AppContainer **and** a Job Object.
    ///
    /// Access and resources together, which is the whole of what 0.47.0 adds to
    /// this platform. `None` means this path declined and the caller should take
    /// the Job Object alone: a profile that cannot be created, a grant that
    /// cannot be applied, a spawn that fails. Declining is a degradation with a
    /// backend name attached, never an error handed to a caller who asked for a
    /// contained run and would rather have had a weaker one than none.
    ///
    /// **Standard output and standard error arrive merged**, in that order, on
    /// `stdout`. The container path owns its own spawn — the container SID
    /// reaches a child only through a process-thread attribute list, which
    /// neither `std`'s nor `tokio`'s `Command` can carry on stable Rust — and it
    /// redirects both streams to one file rather than draining two pipes. A
    /// caller that was parsing `stderr` separately on Windows sees it empty; the
    /// text is not lost, it is in `stdout`. Stated here because it is the one
    /// observable difference between this backend and every other.
    /// Why the container path last declined, for whoever has to explain a run
    /// that reported the Job Object on a host that can build containers.
    ///
    /// Deliberately a process-global last-one-wins rather than a channel: it
    /// exists so a failure has something to print, and a decline is rare enough
    /// that the most recent one is the interesting one.
    /// The profile every contained run on this machine shares.
    ///
    /// Deterministic on purpose: the SID derives from the name, so one name means
    /// one SID means every process writes the identical ACE onto the paths they
    /// share. Two processes with two SIDs racing a read-modify-write over one
    /// DACL lose each other's entry, and the symptom is a container that cannot
    /// read the toolchain home. It is not deleted on drop — see `Profile`.
    ///
    /// **The name encodes the capability set**, so there are two of them rather
    /// than one. A profile registers its capabilities when it is *created*, and a
    /// shared profile is created once and re-entered forever after; if one name
    /// served both answers, whichever run created it would decide what every
    /// later run's profile was registered with. The token's array is what an
    /// access check reads, but registration and token disagreeing is a state
    /// nothing here has measured, and this release does not ship a boundary whose
    /// correctness rests on an untested equivalence.
    pub(crate) fn shared_profile(allow_network: bool) -> &'static str {
        if allow_network {
            "io-harness-sandbox-net"
        } else {
            "io-harness-sandbox"
        }
    }

    /// A throwaway profile name, **per process**, for the availability probe.
    ///
    /// **A deterministic name is a shared object, and this type deletes it on
    /// `Drop`.** Two processes containing a command at the same time — two agents,
    /// a test binary per test, a tree of children — both resolve the same name;
    /// the first to finish deletes the profile the second is still spawning into,
    /// and the second gets a decline. It cost the release PR a Windows leg: the
    /// first row of the capability table declined and every row after it passed,
    /// which is the signature of a container that existed and then did not.
    ///
    /// Per-process is enough because the profile has no reason to outlive the
    /// process that made it, and the SID derives from the name — so two processes
    /// now hold two containers and cannot grant, or delete, each other's. Well
    /// under the 64-character limit.
    fn profile_name(role: &str) -> String {
        format!("io-harness-{role}-{}", std::process::id())
    }

    pub(crate) fn last_decline() -> Option<String> {
        DECLINE
            .get()
            .and_then(|m| m.lock().ok().and_then(|g| g.clone()))
    }

    static DECLINE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
        std::sync::OnceLock::new();

    fn note_decline(why: &str) {
        if let Ok(mut slot) = DECLINE.get_or_init(|| std::sync::Mutex::new(None)).lock() {
            *slot = Some(why.to_string());
        }
    }

    pub(super) async fn run_contained(spec: &RunSpec<'_>) -> Option<Result<SandboxOutcome>> {
        use crate::sandbox::appcontainer::win::{grant_for, Profile, Spawned};

        let tmp = std::env::temp_dir();
        // The **resolved** program's directory, not `argv[0]`'s. A command is
        // named the way every command is named — `cargo`, `rustc`, `npm` — and
        // the parent of a bare filename is the empty path, so the one directory
        // an AppContainer cannot start without was being granted to nothing.
        let program_dir = crate::sandbox::resolve_program(&spec.argv[0])
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .filter(|p| !p.as_os_str().is_empty());
        let system_root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from);
        let toolchain_roots = crate::toolchain::Toolchain::launcher_homes();
        let granted = super::grants(
            spec.mode,
            spec.workdir,
            spec.writable_roots,
            &toolchain_roots,
            program_dir.as_deref(),
            system_root.as_deref(),
            &tmp,
        );

        // A deterministic name, so a profile stranded by a crashed run is
        // re-entered rather than becoming a permanent failure — the module's own
        // `ERROR_ALREADY_EXISTS` path. Bounded well under the 64-character limit.
        let profile = match Profile::shared(shared_profile(spec.allow_network), spec.allow_network)
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "sandbox: could not create an AppContainer profile ({e}); \
                     falling back to the job object, which contains resources and not access"
                );
                note_decline(&format!("could not create an AppContainer profile: {e}"));
                return None;
            }
        };
        for g in &granted {
            let Err(e) = grant_for(&g.path, profile.sid(), g.grant, g.reach) else {
                continue;
            };
            // **A read-execute grant that fails is not fatal, and this is not a
            // softening.** Granting means rewriting the path's DACL, which needs
            // `WRITE_DAC` on it — and a process that is not an administrator does
            // not have that on `%SystemRoot%` or on a toolchain installed under
            // `Program Files`. Those locations already carry an ALL APPLICATION
            // PACKAGES ACE by default, which is exactly the access an
            // AppContainer needs to load a binary and the system libraries it
            // links, so the grant is belt-and-braces rather than the mechanism.
            //
            // Treating it as fatal is what made `windows-latest` decline the
            // container on every run: the probe could create a profile, so
            // `backend()` reported `WindowsAppContainer`, and then the run took
            // the Job Object. The test that caught it was asserting a network
            // boundary against a run that had honestly reported it did not get
            // one.
            //
            // A **writable** grant that fails stays fatal. That one is the
            // mechanism: without it the payload cannot write to the workspace at
            // all, and a container that silently could not is worse than the Job
            // Object, which at least says what it is.
            if g.grant == Grant::ReadExecute || g.grant == Grant::Traverse {
                tracing::debug!(
                    "sandbox: no DACL write on {} ({e}); relying on its ALL APPLICATION \
                     PACKAGES access",
                    g.path.display()
                );
                // A traverse grant fails on exactly the directories nobody owns —
                // the volume root, `C:\Users` — and those already permit an
                // AppContainer to pass through, which is why a path under the
                // profile could be reached at all before this existed.
                continue;
            }
            tracing::warn!(
                "sandbox: could not grant {} to the container ({e}); \
                 falling back to the job object",
                g.path.display()
            );
            // **A decline must not be a silent fact.** The Job Object then runs
            // the command with no access boundary and every assertion about the
            // command still passes, so a test — and an operator reading a green
            // run — cannot tell containment from its absence. `tracing` is not
            // enough on its own: nothing subscribes to it in a test binary.
            note_decline(&format!(
                "could not grant {} ({:?}) to the container: {e}",
                g.path.display(),
                g.grant
            ));
            return None;
        }

        let limits = JobLimits::from(spec.limits);
        let job = match Job::create(&limits) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("sandbox: could not create a job object for the container ({e})");
                note_decline(&format!(
                    "could not create a job object for the container: {e}"
                ));
                return None;
            }
        };

        // Unique per *run*, not per process. Two contained commands running at
        // once in one embedding process — a tree with several children, or a
        // batch — would otherwise share one capture file and read each other's
        // output. The process id keeps two processes on the same machine apart;
        // the counter keeps two runs inside one process apart.
        static CAPTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let out_path = tmp.join(format!(
            "io-harness-{}-{}.out",
            std::process::id(),
            CAPTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = match std::fs::File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("sandbox: could not open the container's capture file ({e})");
                note_decline(&format!("could not open the container's capture file: {e}"));
                return None;
            }
        };
        let cmdline = super::command_line(spec.argv);
        let mut child = match Spawned::start(&cmdline, spec.workdir, &profile, &file) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("sandbox: could not spawn into the AppContainer ({e})");
                note_decline(&format!("could not spawn into the AppContainer: {e}"));
                let _ = std::fs::remove_file(&out_path);
                return None;
            }
        };
        drop(file);

        // The ordering that is the correctness argument, and it now has to hold
        // twice: the process is suspended, so it joins the job before it runs an
        // instruction, and only then is it resumed. A failure here kills rather
        // than continuing — a process running outside the job it was promised is
        // worse than a spawn that never happened.
        if let Err(e) = job.adopt_raw(child.process()) {
            child.kill();
            let _ = std::fs::remove_file(&out_path);
            return Some(Err(e));
        }
        if let Err(e) = child.resume() {
            child.kill();
            let _ = std::fs::remove_file(&out_path);
            return Some(Err(Error::Sandbox {
                reason: format!(
                    "the contained process was put in its job object but could not be \
                     resumed ({e}); it is being killed rather than left suspended"
                ),
            }));
        }

        let wall_ms = spec
            .limits
            .max_wall_secs
            .map(|s| s.saturating_mul(1000).min(u32::MAX as u64) as u32)
            .unwrap_or(u32::MAX);
        // **`spawn_blocking`, not `block_in_place`.** Both keep a blocking Win32
        // wait off a runtime worker, but `block_in_place` *panics* on a
        // current-thread runtime — which is what `#[tokio::test]` builds by
        // default and what an embedder writing
        // `#[tokio::main(flavor = "current_thread")]` gets. The panic was latent
        // for as long as the container path was declined on every host; the run
        // that stopped declining it failed four `verify::tests` at once, none of
        // which are about containment. A backend may not require a runtime
        // flavour of the process embedding it.
        //
        // `Spawned` is `Send`, so the handles move to a blocking thread and the
        // answer comes back. It is dropped there, which is the same teardown as
        // dropping it here: `wait` has already reaped or killed the process on
        // every path that returns.
        let waited = match tokio::task::spawn_blocking(move || child.wait(wall_ms)).await {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&out_path);
                return Some(Err(Error::Sandbox {
                    reason: format!("the thread waiting for the contained process failed: {e}"),
                }));
            }
        };
        let (exit_code, wall) = match waited {
            Ok(Some(code)) => (Some(code), false),
            // `wait` has already terminated it by the time it answers `None`.
            Ok(None) => (None, true),
            Err(e) => {
                let _ = std::fs::remove_file(&out_path);
                return Some(Err(Error::Sandbox {
                    reason: format!("waiting for the contained process failed: {e}"),
                }));
            }
        };

        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let _ = std::fs::remove_file(&out_path);

        let mut cap_hit = wall.then_some(Cap::Wall);
        if cap_hit.is_none() && exit_code != Some(0) {
            cap_hit = job.cap_hit(&limits);
        }

        Some(Ok(SandboxOutcome {
            // The container ran it, so the container is what the outcome says.
            // Until 0.59.0 this reported the job object, because nothing selected
            // this path and a backend no caller can be given is not one a caller
            // may be told about.
            backend: Backend::WindowsAppContainer,
            argv: spec.argv.to_vec(),
            exit_code,
            cap_hit,
            stdout,
            stderr: String::new(),
        }))
    }

    /// Run one command inside a fresh job object.
    ///
    /// The job handle lives in this function and nowhere else, which is what
    /// makes teardown unconditional: it is dropped on every exit path — a clean
    /// exit, a wall-clock kill, an error, a panic unwinding through here — and
    /// dropping it closes the handle, and closing the handle terminates
    /// everything still inside. There is no cleanup step to forget.
    pub(super) async fn run(spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        let limits = JobLimits::from(spec.limits);
        let job = match Job::create(&limits) {
            Ok(job) => job,
            Err(e) => {
                // Degrade rather than fail. A host that cannot create a job
                // object is a host this crate has never seen, but the standing
                // rule is that an unavailable primitive falls back to the floor
                // and reports the floor — the caller reads the backend off the
                // outcome and decides, instead of being handed an error for
                // something it did not ask for.
                tracing::warn!(
                    "sandbox: could not create a Windows job object ({e}); \
                     falling back to the portable floor, which on this platform \
                     enforces the wall clock and nothing else"
                );
                return run_capped(Backend::PortableFloor, spec, |_cmd| {}).await;
            }
        };

        let mut outcome = run_capped_hooked(
            Backend::WindowsJobObject,
            spec,
            |cmd| {
                // Frozen before its first instruction. The `started` hook below
                // is what lets it move again, and it only runs after the
                // assignment has succeeded.
                cmd.creation_flags(CREATE_SUSPENDED);
            },
            |child| job.adopt(child),
        )
        .await?;

        // Ask the job what it accounted for, but only about a run that failed:
        // a command that exited zero was not stopped by anything, and a
        // wall-clock kill already returned from the runner without coming here.
        if outcome.cap_hit.is_none() && outcome.exit_code != Some(0) {
            outcome.cap_hit = job.cap_hit(&limits);
        }
        Ok(outcome)
    }

    /// An owned job-object handle. Closing it is the teardown, so this type
    /// exists mainly to make that closing a `Drop` rather than a line someone
    /// has to remember to write on every return path.
    pub(crate) struct Job(HANDLE);

    // SAFETY: a Win32 kernel handle is a plain process-wide table index, not a
    // thread-affine resource: every API used here (`AssignProcessToJobObject`,
    // `QueryInformationJobObject`, `CloseHandle`) accepts it from any thread of
    // the owning process and is documented as thread-safe. `HANDLE` is only a raw
    // pointer typedef, which is the sole reason the auto traits are withheld.
    // Without these the sandbox future stops being `Send` and cannot satisfy the
    // `Sandbox` trait, which is how this was found rather than assumed.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A job for a **process handle**: the teardown guarantee and no resource
        /// limits at all.
        ///
        /// A dev server or a twenty-minute build is exactly the workload the
        /// sandbox's caps would kill, and a handle is not a sandbox — it is a
        /// lifetime. What it needs from a job is the one thing only a job
        /// provides on this platform: closing the handle takes down every
        /// descendant however it was spawned and whoever its parent is by then,
        /// which is the grandchild case `taskkill /T` provably cannot reach.
        pub(crate) fn for_handle() -> io::Result<Self> {
            Self::create(&JobLimits {
                process_memory: None,
                active_processes: None,
                cpu_ticks: None,
                kill_on_close: true,
            })
        }

        /// Create the job and apply `limits` to it, before any process is in it.
        pub(crate) fn create(limits: &JobLimits) -> io::Result<Self> {
            // SAFETY: both arguments are the documented "default security, no
            // name" nulls, which `CreateJobObjectW` is specified to accept and
            // not dereference. The returned handle is owned by the `Job` built
            // from it on the next line and closed exactly once, in `Drop`.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(handle);

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            let basic = &mut info.BasicLimitInformation;

            // Set unconditionally and never from configuration: this is the
            // teardown guarantee the whole backend rests on.
            debug_assert!(limits.kill_on_close);
            basic.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            if let Some(bytes) = limits.process_memory {
                // Saturating rather than truncating: a 32-bit host asked for
                // more than it can address should get "no effective cap", not a
                // wrapped-around tiny one that kills every run.
                info.ProcessMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
                basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            }
            if let Some(n) = limits.active_processes {
                basic.ActiveProcessLimit = u32::try_from(n).unwrap_or(u32::MAX);
                basic.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            }
            if let Some(ticks) = limits.cpu_ticks {
                // User-mode time only — see the module docs. The field is signed
                // and the mapping is unsigned, so clamp rather than cast.
                basic.PerJobUserTimeLimit = i64::try_from(ticks).unwrap_or(i64::MAX);
                basic.LimitFlags |= JOB_OBJECT_LIMIT_JOB_TIME;
            }

            // SAFETY: `info` is a live, fully initialised
            // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, the class constant is the
            // one that names that exact struct, and the length passed is the
            // struct's own `size_of` — so the kernel reads only bytes that exist.
            // `job.0` is the handle just created and not yet closed.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                // `job` drops here, closing the handle. Nothing is in it yet.
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Put `child` in the job and let it run — in that order, which is the
        /// point. Fails loudly rather than leaving a child outside the job.
        pub(crate) fn adopt(&self, child: &tokio::process::Child) -> Result<()> {
            let sandbox = |reason: String| Error::Sandbox { reason };
            let handle = child.raw_handle().ok_or_else(|| {
                sandbox("the sandboxed child exited before it could be put in the job".into())
            })?;
            let pid = child.id().ok_or_else(|| {
                sandbox("the sandboxed child exited before it could be put in the job".into())
            })?;

            // SAFETY: `handle` is the child's process handle, owned by `child`,
            // which is borrowed for this whole call and therefore outlives it —
            // the handle cannot be closed underneath the call. `self.0` is this
            // job's handle. Assigning a process that has not yet run is the
            // documented use of this API and the reason it exists.
            let ok = unsafe { AssignProcessToJobObject(self.0, handle.cast()) };
            if ok == 0 {
                return Err(sandbox(format!(
                    "could not assign the sandboxed process to its job object: {}",
                    io::Error::last_os_error()
                )));
            }

            resume(pid).map_err(|e| {
                sandbox(format!(
                    "the sandboxed process was put in its job object but could not be \
                     resumed ({e}); it is being killed rather than left suspended"
                ))
            })
        }

        /// Put an already-created process in the job, without resuming it.
        ///
        /// [`adopt`](Job::adopt) does both, because the `std::Command` path has
        /// no thread handle left and has to go the ToolHelp way round. The
        /// container path kept its thread handle from `CreateProcessW`, so it
        /// resumes itself and needs only this half.
        pub(crate) fn adopt_raw(&self, handle: HANDLE) -> Result<()> {
            // SAFETY: `handle` belongs to a `Spawned` borrowed by the caller for
            // longer than this call, so it cannot be closed underneath it, and
            // `self.0` is this job's own handle.
            if unsafe { AssignProcessToJobObject(self.0, handle) } == 0 {
                return Err(Error::Sandbox {
                    reason: format!(
                        "could not assign the contained process to its job object: {}",
                        io::Error::last_os_error()
                    ),
                });
            }
            Ok(())
        }

        /// What, if anything, the job stopped this run for. Consulted only after
        /// a failed run, and only when no other cap already fired.
        ///
        /// This is inference from the job's own accounting, not a notification:
        /// none of these three limits tells the parent it fired. Getting a
        /// notification would mean an IO completion port and a thread parked on
        /// it for the length of every run, and the accounting answers the
        /// question the trace actually asks.
        //
        // ponytail: heuristic attribution from end-of-run accounting. Upgrade to
        // a job completion port (`JobObjectAssociateCompletionPortInformation`)
        // if a caller ever needs to know *which* allocation was refused, or
        // needs to be told the instant a limit is hit rather than afterwards.
        pub(crate) fn cap_hit(&self, limits: &JobLimits) -> Option<Cap> {
            let acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                self.query(JobObjectBasicAccountingInformation)?;

            // CPU first, and on user time alone, because that is the only clock
            // the job's limit watches. Reaching it means the OS terminated
            // everything in the job; there is no other way to spend the whole
            // allotment and still be here.
            if let Some(ticks) = limits.cpu_ticks {
                if acct.TotalUserTime.max(0) as u64 >= ticks {
                    return Some(Cap::Cpu);
                }
            }

            if let Some(bytes) = limits.process_memory {
                let ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                    self.query(JobObjectExtendedLimitInformation)?;
                // Ninety percent, not the limit itself, and the fuzziness is
                // inherent rather than sloppy: the job reports the peak a process
                // *reached*, never the allocation it was *refused*. A payload
                // asking for memory in chunks is denied partway up a chunk, so
                // its recorded peak stops just short of the cap it could not
                // cross. A run that got that close to a commit ceiling and then
                // failed, failed because of the ceiling.
                if ext.PeakProcessMemoryUsed as u64 >= bytes - bytes / 10 {
                    return Some(Cap::Memory);
                }
            }

            if let Some(n) = limits.active_processes {
                // `TotalProcesses` counts every process ever associated with the
                // job, and a process that the limit refused was never associated
                // — so this equals the limit exactly when the limit was reached,
                // and is below it otherwise. Combined with a failed run that is
                // the strongest statement available without a completion port.
                if u64::from(acct.TotalProcesses) >= n {
                    return Some(Cap::Processes);
                }
            }

            None
        }

        /// Read one fixed-size information class off the job. `None` on any
        /// failure, since every caller is asking a question whose answer is
        /// optional anyway.
        ///
        /// The class/type pairing is unchecked, so this stays private and is
        /// called from exactly the two sites above, each with the struct Win32
        /// documents for its class.
        fn query<T: Default>(&self, class: JOBOBJECTINFOCLASS) -> Option<T> {
            let mut out = T::default();
            // SAFETY: `out` is a live, owned `T` and the length passed is `T`'s
            // own `size_of`, so the kernel writes only into bytes that exist and
            // cannot overrun. The null return-length pointer is documented as
            // permitted. `self.0` is this job's still-open handle.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    class,
                    std::ptr::from_mut(&mut out).cast(),
                    size_of::<T>() as u32,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then_some(out)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the last handle to a `KILL_ON_JOB_CLOSE` job is what kills
            // the tree. Everything still inside dies here, including a
            // grandchild whose parent exited long ago and which no `taskkill /T`
            // could have found.
            //
            // SAFETY: `self.0` came from `CreateJobObjectW` in `create`, is never
            // copied out of this type, and this is the only close. `Job` is not
            // `Clone`, so there is exactly one owner and therefore exactly one
            // close.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Resume every thread belonging to `pid`.
    ///
    /// For a `CREATE_SUSPENDED` process that is exactly one thread — its initial
    /// one — so "every thread" and "the thread" are the same set here. See the
    /// module docs for why the thread has to be found by snapshot instead of
    /// being handed over by the spawn.
    fn resume(pid: u32) -> io::Result<()> {
        // SAFETY: `TH32CS_SNAPTHREAD` snapshots system-wide threads and ignores
        // the process-id argument, which is why zero is passed. The returned
        // handle is owned here and closed on every path out.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut entry = THREADENTRY32 {
            // ToolHelp reads this field to decide how many bytes it may write.
            // Leaving it zero makes the very first call fail.
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = 0usize;

        // SAFETY: `snapshot` is a live snapshot handle and `entry` is a live,
        // correctly sized `THREADENTRY32` whose `dwSize` bounds what the call may
        // write into it. Iteration stops the first time a call returns false,
        // which is the documented end-of-list signal.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: only the right to resume is requested, and the thread
                // id comes from the snapshot the kernel just filled in. A null
                // return means the thread is gone, which is checked before use;
                // a non-null one is owned here and closed immediately below.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    // SAFETY: `thread` is the handle just opened with exactly the
                    // access `ResumeThread` requires, and it is closed once,
                    // here, with no other reference to it anywhere.
                    unsafe {
                        ResumeThread(thread);
                        CloseHandle(thread);
                    }
                    resumed += 1;
                }
            }
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            // SAFETY: as `Thread32First` above; same live handle, same live and
            // correctly sized entry.
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }

        // SAFETY: `snapshot` is the handle opened at the top of this function,
        // still open, closed once, and not used again afterwards.
        unsafe { CloseHandle(snapshot) };

        if resumed == 0 {
            // The child is in the job and frozen. Saying so is the only safe
            // move: the caller kills it, where pretending success would leave it
            // suspended until the wall clock.
            return Err(io::Error::other(format!(
                "no thread of the sandboxed process {pid} could be resumed"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxLimits;

    /// The container path passes a command *line*, not a vector, so the quoting
    /// is load-bearing. Asserted on the build host because a quoting bug only
    /// shows up on the one argument containing a space — which on this platform
    /// is every path.
    #[test]
    fn the_command_line_quotes_what_needs_quoting_and_nothing_else() {
        assert_eq!(command_line(&["cargo".into(), "test".into()]), "cargo test");
        assert_eq!(
            command_line(&["c:\\a b\\cargo.exe".into(), "--x".into()]),
            "\"c:\\a b\\cargo.exe\" --x"
        );
        // An argument with nothing to escape is passed through untouched, and a
        // trailing backslash is only dangerous *inside* quotes — so this one is
        // correct unquoted, and quoting it would be the bug.
        assert_eq!(command_line(&["c:\\dir\\".into()]), "c:\\dir\\");
        // Quoted, the same trailing run has to be doubled or it escapes the
        // closing quote and swallows the next argument. This is the case that is
        // easiest to leave out and hardest to notice.
        assert_eq!(
            command_line(&["c:\\a b\\".into(), "next".into()]),
            "\"c:\\a b\\\\\" next"
        );
        // A quote inside an argument is escaped, and the run before it doubled.
        assert_eq!(command_line(&["say \"hi\"".into()]), "\"say \\\"hi\\\"\"");
        // An empty argument must survive as an empty argument.
        assert_eq!(command_line(&["x".into(), String::new()]), "x \"\"");
    }

    /// N5 — what the container costs per command over the Job Object alone.
    ///
    /// The two Windows backends are timed against each other rather than against
    /// an unconfined spawn, because the Job Object is what a contained Windows run
    /// got before this release: the interesting number is what selecting the
    /// container adds, which is a profile lookup, a grant pass over the derived
    /// set, a suspended spawn and a resume.
    ///
    /// `#[ignore]`d for the reason the Linux twin is: it is a measurement with no
    /// threshold, and a wall-clock number on a shared runner must not gate a merge.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "a measurement, not an assertion — run by the overhead CI step"]
    async fn n5_per_command_overhead_by_backend() {
        use std::time::Instant;

        const ITERATIONS: u32 = 30;
        let dir = tempfile::tempdir().unwrap();
        let argv: Vec<String> = vec!["cmd".into(), "/c".into(), "exit /b 0".into()];
        let limits = SandboxLimits::none();
        let spec = || {
            RunSpec::new(&argv, dir.path(), &limits)
                .with_network(true)
                .with_mode(ExecMode::WorkspaceWrite)
        };

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            job::run(spec()).await.expect("the job object must run");
        }
        let job_only = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
        println!("N5 windows-job-object: {job_only:.2} ms/command over {ITERATIONS}");

        let started = Instant::now();
        let mut contained = 0u32;
        for _ in 0..ITERATIONS {
            match job::run_contained(&spec()).await {
                Some(outcome) => {
                    outcome.expect("the container must run");
                    contained += 1;
                }
                // The container declining is the designed degradation, and a
                // measurement that silently averaged in a Job Object run would
                // report the container's cost as the job's.
                None => break,
            }
        }
        if contained == ITERATIONS {
            let per = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
            println!(
                "N5 windows-appcontainer: {per:.2} ms/command over {ITERATIONS} \
                 (over the job object: {:+.2} ms)",
                per - job_only
            );
        } else {
            println!(
                "N5 windows-appcontainer: not measured — the container declined after \
                 {contained} of {ITERATIONS} runs on this host"
            );
        }
    }

    /// **What the container permits on this host, one capability per line.**
    ///
    /// Every Windows failure in this release was read wrongly at least once,
    /// because two very different outcomes look identical from outside a test
    /// that only asserts success: a container that permitted the operation, and a
    /// container that **declined** — `run_contained` answers `None` when a grant
    /// it must have cannot be applied, and the Job Object then runs the command
    /// with no access boundary at all and every assertion passes. A gate test
    /// written that way proved nothing about containment and read as if it had.
    ///
    /// So this asks each capability separately, through `run_contained` itself so
    /// there is no doubt which backend answered, and prints all of them before
    /// asserting. A failure here is a table, not a boolean.
    #[cfg(windows)]
    #[tokio::test]
    async fn what_the_container_actually_permits_on_this_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("probe.txt"), "io-harness-probe\r\n").expect("probe file");
        std::fs::write(
            dir.path().join("probe.bat"),
            "@echo off\r\necho io-harness-probe\r\n",
        )
        .expect("probe batch");

        // **The grant set itself, printed.** `run_contained` swallows a failed
        // read-execute grant by design — those paths carry an ALL APPLICATION
        // PACKAGES ACE of their own and a non-administrator cannot rewrite their
        // DACLs — so a path missing from this set and a path whose grant failed
        // look the same from outside, and neither is visible in a test that only
        // reports the command's exit code. Derived exactly as the backend derives
        // it, and applied to nothing: this is a report, not a second grant.
        let tmp = std::env::temp_dir();
        let program_dir = crate::sandbox::resolve_program("cmd")
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .filter(|p| !p.as_os_str().is_empty());
        let system_root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from);
        let derived = super::grants(
            ExecMode::WorkspaceWrite,
            dir.path(),
            &[],
            &crate::toolchain::Toolchain::launcher_homes(),
            program_dir.as_deref(),
            system_root.as_deref(),
            &tmp,
        );
        let mut report = String::from("\n  the grant set this run derives:");
        for g in &derived {
            report.push_str(&format!(
                "\n    {:?} {:?} exists={} {}",
                g.grant,
                g.reach,
                g.path.is_dir(),
                g.path.display()
            ));
        }

        // The long form of the workspace path. `std::env::temp_dir` answers with
        // whatever `%TEMP%` holds, and on this runner that is the **8.3 short
        // name** (`RUNNER~1`), so every payload path these tests build carries
        // one. Resolving a short name is not the same file-system operation as
        // opening a long one, and the two cases that still fail are the two that
        // use an absolute short-name path — so the comparison is made here rather
        // than assumed either way.
        let long = std::fs::canonicalize(dir.path())
            .map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
            .unwrap_or_else(|_| dir.path().display().to_string());
        let short_abs = dir.path().join("probe.bat").display().to_string();
        let long_abs = format!("{long}\\probe.bat");
        let txt_abs = dir.path().join("probe.txt").display().to_string();

        // A second workspace that is not under the user profile at all. The
        // target directory is beside the build output, which is on whichever
        // volume the checkout is on — `D:` on this runner, and never `%TEMP%`.
        let other = std::env::current_dir()
            .unwrap_or_default()
            .join("target")
            .join(format!("io-harness-probe-{}", std::process::id()));
        let other_bat = if std::fs::create_dir_all(&other).is_ok()
            && std::fs::write(
                other.join("probe.bat"),
                "@echo off\r\necho io-harness-probe\r\n",
            )
            .is_ok()
        {
            other.join("probe.bat").display().to_string()
        } else {
            String::new()
        };

        let limits = SandboxLimits::none();
        let roots: Vec<PathBuf> = if other_bat.is_empty() {
            Vec::new()
        } else {
            vec![other.clone()]
        };
        // `must_run` is false for the two cases this platform refuses, and they
        // are kept rather than deleted: they are the control that says the
        // refusal is `cmd.exe` starting a *script* by absolute path and nothing
        // about the grant set. The `type` case reads that same file by that same
        // kind of path, and the off-the-profile case runs from a workspace whose
        // ancestors are not the user profile at all. If Windows ever changes
        // this, the table fails and says so.
        let cases: [(&str, &[&str], bool); 9] = [
            // The control: a shell builtin needs nothing but `%SystemRoot%`,
            // which carries an ALL APPLICATION PACKAGES ACE of its own. If this
            // fails the container is not usable on this host at all.
            (
                "a shell builtin",
                &["cmd", "/c", "echo io-harness-probe"],
                true,
            ),
            // Reading a file the workspace grant covers. This is the claim the
            // whole grant set rests on and nothing asserted it directly.
            (
                "reading a granted file",
                &["cmd", "/c", "type probe.txt"],
                true,
            ),
            // Executing one. `cmd` opens a batch file itself, so this separates
            // "the payload cannot be read" from "the payload cannot be started".
            (
                "running a granted batch file",
                &["cmd", "/c", "probe.bat"],
                true,
            ),
            // Writing into the workspace, which `Full` is entirely about.
            (
                "writing into the workspace",
                &["cmd", "/c", "echo written> written.txt"],
                true,
            ),
            // There is deliberately no row here for a third-party binary resolved
            // off `PATH`. There was one until 0.71.0, running `rustc --version`,
            // and what it actually measured was the host: `rustc` on `PATH` is a
            // rustup **shim**, so the container started it and it then died
            // reading the runner's own `.rustup` with `os error 183`. It reported
            // a containment failure on 2 of 2 `pull_request` runs at 0.70.0 while
            // the `push` run of the same commit passed it. Re-aiming it at the
            // toolchain binary made it honest and made it redundant — that is
            // exactly the absolute-path row below, differing only in the flag —
            // so it is gone rather than duplicated. The coverage genuinely lost
            // is a third-party binary found by `PATH`; what remains is `PATH`
            // resolution of `cmd` (system32, which the container treats
            // differently) and the toolchain binary by absolute path. Restoring
            // the lost row needs a program that is on `PATH` and does not need a
            // home directory to start, which no row here has yet found.
            //
            // The same batch file by absolute path, in the two forms the path can
            // take. Every remaining failure in this release runs a payload by an
            // absolute path that carries an 8.3 short component, and every case
            // that passes names it relative to a granted working directory.
            (
                "a batch file by absolute path",
                &["cmd", "/c", "@SHORT@"],
                false,
            ),
            (
                "a batch file by long absolute path",
                &["cmd", "/c", "@LONG@"],
                false,
            ),
            // The toolchain binary cargo could not start, reached the way cargo
            // reaches it: by absolute path, not through the launcher shim.
            (
                "the toolchain binary by absolute path",
                &["@RUSTC@", "-vV"],
                true,
            ),
            // The case that separates "an absolute path" from "a batch file".
            // `type` is a builtin reading the same directory by the same kind of
            // path, so if it passes while the two above fail, the path resolves
            // and what cannot be reached is whatever `cmd` does to *start* a
            // script.
            (
                "reading a granted file by absolute path",
                &["cmd", "/c", "type @TXT@"],
                true,
            ),
            // And the case that separates "an absolute path" from "this chain".
            // The workspace here sits beside the checkout on the runner's data
            // volume rather than under the user profile, so `AppData\Local` and
            // every ACL on it are out of the picture.
            (
                "a batch file by absolute path off the profile",
                &["cmd", "/c", "@OTHER@"],
                false,
            ),
        ];

        // Where the toolchain's own rustc is, which is what `cargo` executes and
        // what it was refused. Asked of the launcher rather than guessed.
        let toolchain_rustc = std::process::Command::new("rustup")
            .args(["which", "rustc"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        report.push_str(&format!("\n  rustup which rustc: {toolchain_rustc:?}"));

        let mut failed = 0;
        for (what, args, must_run) in cases {
            let argv: Vec<String> = args
                .iter()
                .map(|s| match *s {
                    "@SHORT@" => short_abs.clone(),
                    "@LONG@" => long_abs.clone(),
                    "@RUSTC@" => toolchain_rustc.clone(),
                    "type @TXT@" => format!("type {txt_abs}"),
                    "@OTHER@" => other_bat.clone(),
                    other => other.to_string(),
                })
                .collect();
            // A case whose subject this host could not name is reported as such
            // rather than run as an empty argv, which would fail for a reason of
            // its own and read like a denial.
            if argv.iter().any(String::is_empty) {
                report.push_str(&format!(
                    "\n  {what}: not runnable here, no path to the subject"
                ));
                continue;
            }
            // The second workspace is named as a writable root so the case that
            // runs from it is testing the *path*, not an ungranted directory.
            let spec = RunSpec::new(&argv, dir.path(), &limits)
                .with_mode(ExecMode::WorkspaceWrite)
                .with_network(false)
                .with_writable_roots(&roots);
            match job::run_contained(&spec).await {
                None => {
                    failed += 1;
                    report.push_str(&format!(
                        "\n  {what}: THE CONTAINER DECLINED — {}",
                        job::last_decline().unwrap_or_else(|| "no reason recorded".into())
                    ));
                }
                Some(Err(e)) => {
                    failed += 1;
                    report.push_str(&format!("\n  {what}: the backend errored: {e}"));
                }
                Some(Ok(o)) => {
                    if o.success() != must_run {
                        failed += 1;
                    }
                    report.push_str(&format!(
                        "\n  {what}: backend {:?}, exit {:?}{}, output {:?}",
                        o.backend,
                        o.exit_code,
                        if must_run { "" } else { " (expected: refused)" },
                        o.stdout
                    ));
                }
            }
        }
        // **Printed whether or not it passes** (0.59.0). A table that only
        // appears on failure is a table nobody reads on the one run where it
        // would have told them the boundary is real, and this instrument exists
        // precisely because a pass and a silent decline look identical.
        println!("{report}");
        assert_eq!(
            failed, 0,
            "the AppContainer did not behave as the grant set says it does:{report}"
        );
        // Both program rows resolve through `rustup which rustc`, and a host that
        // cannot answer it skips them as "not runnable here" — which would leave
        // the table asserting containment with nothing proving the container can
        // start a program at all, and reading green while it did so.
        assert!(
            !toolchain_rustc.is_empty(),
            "`rustup which rustc` named nothing, so the one row that starts a program other \
             than a shell builtin was skipped and this table proved nothing about executing \
             one. That is a failure and not a skip on purpose: a host where this cannot be \
             answered must say so loudly rather than report a green table it did not earn. \
             A host with a toolchain but no rustup would need a different way to name the \
             binary before this row can run there:{report}"
        );
        assert!(
            !report.contains("THE CONTAINER DECLINED"),
            "a row was answered by a decline rather than by the container:{report}"
        );
        assert!(
            !report.contains("WindowsJobObject"),
            "a row was answered by the Job Object, which has no filesystem facility and no \
             network facility — so whatever that row proved, it did not prove containment:{report}"
        );
    }

    /// **F9 — selection is the caller's, and the default did not move.**
    ///
    /// The Windows default stays the Job Object because the grant set is derived
    /// and derived is not complete. Asserted on what `select` returns and on what
    /// that thing reports, not on the flag that was set — a config field proves
    /// only that a config field exists.
    #[cfg(windows)]
    #[test]
    fn the_container_is_chosen_only_when_the_caller_asks_for_it() {
        use crate::sandbox::{select, Backend, Sandbox, SandboxConfig};

        assert_eq!(
            select(&SandboxConfig::new()).backend(),
            Backend::WindowsJobObject,
            "the Windows default moved; it is the Job Object until the derived grant set is \
             proven against a real payload"
        );
        assert_eq!(
            select(&SandboxConfig::new().with_access_confinement()).backend(),
            Backend::WindowsAppContainer,
            "the caller asked for an access boundary and did not get the backend that has one"
        );
        // `FullAccess` says the payload may write anywhere, so putting it inside
        // a default-deny token would refuse the one thing the mode grants.
        assert_eq!(
            select(
                &SandboxConfig::new()
                    .with_access_confinement()
                    .with_mode(ExecMode::FullAccess)
            )
            .backend(),
            Backend::WindowsJobObject,
            "full access was put inside a default-deny container"
        );
    }

    /// **F5b — a boundary that cannot be applied is refused, not degraded.**
    ///
    /// Added because a sabotage survived: making a failed `Full` grant non-fatal
    /// broke nothing, since on a healthy host no `Full` grant fails and the
    /// capability table never constructs the case the guard exists for. A guard
    /// whose sabotage survives is a test that never built the situation, so this
    /// builds it — a writable root that cannot be granted because it is not
    /// there.
    ///
    /// The assertion is that **nothing ran**. A decline that fell back to the Job
    /// Object would run the command with no access boundary while every
    /// assertion about the command still passed, which is the failure this whole
    /// release exists to end.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_boundary_that_cannot_be_applied_refuses_rather_than_running_uncontained() {
        use crate::sandbox::Sandbox;

        let dir = tempfile::tempdir().expect("tempdir");
        let limits = SandboxLimits::none();
        // A root the grant pass must fail on, chosen so it fails for a reason no
        // privilege level changes: it does not exist.
        let missing = std::path::PathBuf::from(format!(
            r"C:\io-harness-no-such-root-{}",
            std::process::id()
        ));
        let roots = vec![missing.clone()];
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/c".into(),
            "echo io-harness-should-not-run> ran.txt".into(),
        ];
        let spec = RunSpec::new(&argv, dir.path(), &limits)
            .with_mode(ExecMode::WorkspaceWrite)
            .with_network(false)
            .with_writable_roots(&roots);

        let outcome = WindowsAppContainerSandbox.run(spec).await;
        let Err(e) = outcome else {
            panic!(
                "a run that asked for access confinement was given something else instead of \
                 an error: {outcome:?}"
            );
        };
        let said = e.to_string();
        assert!(
            said.contains("access confinement") && said.contains("Nothing was started"),
            "the refusal does not say what was refused or that nothing ran: {said}"
        );
        assert!(
            !dir.path().join("ran.txt").exists(),
            "the payload ran anyway, which means the container declined and something else \
             executed the command with no access boundary"
        );
    }

    /// **F11 — a contained command is given no proxy, and the absence is
    /// asserted rather than incidental.**
    ///
    /// A process inside an AppContainer cannot reach a loopback listener under
    /// any capability set, nor the host's own network address
    /// (`US-IO-HARNESS-0.59.0-I03`). So pointing a contained command at the run's
    /// egress proxy would not scope its traffic, it would hang every request it
    /// makes against something unreachable. Egress here is the capability and
    /// nothing else.
    ///
    /// The assertion is on what the child can actually see: its own environment,
    /// printed by the payload, carrying no address of ours.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_contained_command_is_not_pointed_at_a_proxy_it_cannot_reach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let limits = SandboxLimits::none();
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/c".into(),
            "echo [%HTTP_PROXY%][%HTTPS_PROXY%][%ALL_PROXY%]".into(),
        ];
        let proxy: std::net::SocketAddr = "127.0.0.1:65123".parse().expect("an address");
        let spec = RunSpec::new(&argv, dir.path(), &limits)
            .with_mode(ExecMode::WorkspaceWrite)
            .with_network(true)
            .with_proxy(Some(proxy));

        let outcome = job::run_contained(&spec)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the container declined: {}",
                    job::last_decline().unwrap_or_else(|| "no reason recorded".into())
                )
            })
            .expect("the contained command ran");
        assert_eq!(
            outcome.backend,
            Backend::WindowsAppContainer,
            "this assertion is about a contained command and this one was not contained"
        );
        assert!(
            !outcome.stdout.contains("65123"),
            "a contained command was handed the run's proxy, which it cannot reach — every \
             request it makes would wait out its own clock instead of being scoped: {:?}",
            outcome.stdout
        );
    }

    fn find<'a>(g: &'a [GrantedPath], p: &str) -> Option<&'a GrantedPath> {
        g.iter().find(|x| x.path == Path::new(p))
    }

    /// F8 — the grant set is derived from the run's own resolved facts.
    #[test]
    fn the_grant_set_comes_from_what_the_run_already_resolved() {
        let roots = vec![
            PathBuf::from(r"C:\cache\cargo"),
            PathBuf::from(r"C:\cache\npm"),
        ];
        let g = grants(
            ExecMode::WorkspaceWrite,
            Path::new(r"C:\work"),
            &roots,
            &[PathBuf::from(r"C:\Users\someone\.rustup")],
            Some(Path::new(r"C:\tools\bin")),
            Some(Path::new(r"C:\Windows")),
            Path::new(r"C:\Temp"),
        );

        assert_eq!(find(&g, r"C:\work").unwrap().grant, Grant::Full);
        assert_eq!(find(&g, r"C:\cache\cargo").unwrap().grant, Grant::Full);
        assert_eq!(find(&g, r"C:\cache\npm").unwrap().grant, Grant::Full);
        assert_eq!(find(&g, r"C:\Temp").unwrap().grant, Grant::Full);

        // A path the run named reaches what is already inside it; the shared
        // temporary directory is granted for what the run will *create* there,
        // and re-propagating it is the one grant in this set that was both
        // expensive and racy.
        assert_eq!(find(&g, r"C:\work").unwrap().reach, Reach::Tree);
        assert_eq!(find(&g, r"C:\cache\cargo").unwrap().reach, Reach::Tree);
        assert_eq!(
            find(&g, r"C:\Users\someone\.rustup").unwrap().reach,
            Reach::Tree
        );
        assert_eq!(find(&g, r"C:\Temp").unwrap().reach, Reach::DirectoryOnly);

        // Every ancestor of a granted path is reachable by name, traverse-only.
        // A granted directory that cannot be walked to is refused the moment the
        // payload is named absolutely rather than relative to the working
        // directory, which is the same file and a different question.
        //
        // **Windows only, and not because the code is.** `grants` is portable and
        // is asserted on the build host precisely so it does not need a Windows
        // runner — but `Path::ancestors` splits on the *host's* separator, and a
        // backslash is an ordinary character in a unix path, so on the build host
        // `C:\cache\cargo` is a single component with no ancestors to find. The
        // decision is the same on both; only this half of the assertion needs the
        // platform that can see it.
        #[cfg(not(windows))]
        let _ = &g;
        #[cfg(windows)]
        {
            let cache = find(&g, r"C:\cache").expect("the parent of a granted cache root");
            assert_eq!(cache.grant, Grant::Traverse);
            assert_eq!(cache.reach, Reach::DirectoryOnly);
            assert_eq!(find(&g, r"C:\Users").unwrap().grant, Grant::Traverse);
            assert_eq!(
                find(&g, r"C:\Users\someone").unwrap().grant,
                Grant::Traverse
            );

            // And a path that is already granted something stronger is never
            // weakened to a traverse by being some other path's ancestor.
            assert_eq!(find(&g, r"C:\Windows").unwrap().grant, Grant::ReadExecute);
        }
        // The two places a process needs in order to start at all, and no more
        // than read-execute on either.
        assert_eq!(find(&g, r"C:\tools\bin").unwrap().grant, Grant::ReadExecute);
        assert_eq!(find(&g, r"C:\Windows").unwrap().grant, Grant::ReadExecute);
        // A toolchain launcher's home: read-execute, never writable. This is what
        // a rustup shim reads to find the binary it stands for, and a shim that
        // cannot see its home does not report a permission error.
        assert_eq!(
            find(&g, r"C:\Users\someone\.rustup").unwrap().grant,
            Grant::ReadExecute
        );
        // Nothing is granted that was not named or walked to. The seven named
        // paths, and on Windows the ancestors that make them reachable — a count
        // that differs by platform because `Path::ancestors` splits on the host's
        // separator and a backslash is an ordinary character in a unix path.
        let named = 7;
        assert_eq!(
            g.iter().filter(|e| e.grant != Grant::Traverse).count(),
            named,
            "nothing is granted that was not named"
        );
        #[cfg(not(windows))]
        assert_eq!(g.len(), named, "no ancestors are found on this host");
        #[cfg(windows)]
        assert!(
            g.len() > named && g[named..].iter().all(|e| e.grant == Grant::Traverse),
            "the ancestors are added after the named set and are traverse-only: {g:?}"
        );
    }

    /// F8's mode arm — `ReadOnly` downgrades the workspace to read-execute and
    /// withholds the writable roots entirely, while the temp directory stays.
    #[test]
    fn read_only_downgrades_the_workspace_and_withholds_the_roots() {
        let roots = vec![PathBuf::from(r"C:\cache\cargo")];
        let g = grants(
            ExecMode::ReadOnly,
            Path::new(r"C:\work"),
            &roots,
            &[PathBuf::from(r"C:\Users\someone\.rustup")],
            None,
            None,
            Path::new(r"C:\Temp"),
        );
        assert_eq!(find(&g, r"C:\work").unwrap().grant, Grant::ReadExecute);
        assert!(
            find(&g, r"C:\cache\cargo").is_none(),
            "a read-only run has nothing to build and no cache to populate"
        );
        assert_eq!(find(&g, r"C:\Temp").unwrap().grant, Grant::Full);
        // The launcher home survives `ReadOnly` — it is already read-execute, and
        // a read-only run still has to be able to start the program it was given.
        assert_eq!(
            find(&g, r"C:\Users\someone\.rustup").unwrap().grant,
            Grant::ReadExecute
        );
    }

    /// The user's profile directory is deliberately **not** in the set. It is
    /// where credentials live, and a default-deny boundary whose first act is to
    /// hand over the home directory is not one. Asserted so that a future
    /// "convenience" addition has to argue with a test.
    #[test]
    fn the_home_directory_is_never_granted() {
        let home = PathBuf::from(r"C:\Users\someone");
        let g = grants(
            ExecMode::WorkspaceWrite,
            Path::new(r"C:\work"),
            &[],
            &[],
            Some(Path::new(r"C:\tools\bin")),
            Some(Path::new(r"C:\Windows")),
            Path::new(r"C:\Temp"),
        );
        assert!(
            !g.iter().any(|x| x.path == home),
            "the profile directory must never be granted by default"
        );
    }

    /// A path a run resolved but which is not there must not reach the grant
    /// list — the exists-filter is the caller's, and this asserts the derivation
    /// does not re-add anything of its own.
    #[test]
    fn nothing_is_granted_that_the_run_did_not_resolve() {
        let g = grants(
            ExecMode::WorkspaceWrite,
            Path::new(r"C:\work"),
            &[],
            &[],
            None,
            None,
            Path::new(r"C:\Temp"),
        );
        assert_eq!(
            g.len(),
            2,
            "the workspace and the temp directory, and that is all"
        );
    }

    #[test]
    fn maps_limits_to_job_object_fields_and_ticks() {
        let lim = SandboxLimits {
            max_cpu_secs: Some(3),
            max_memory_bytes: Some(64 * 1024 * 1024),
            max_processes: Some(8),
            ..SandboxLimits::default()
        };
        let job = JobLimits::from(&lim);
        assert_eq!(job.process_memory, Some(64 * 1024 * 1024));
        assert_eq!(job.active_processes, Some(8));
        assert_eq!(job.cpu_ticks, Some(30_000_000)); // 3s * 1e7 ticks/s
        assert!(job.kill_on_close, "job must kill the tree on close");
    }

    /// The absent limits must stay absent. A `None` that mapped to a zero would
    /// be a job with `ActiveProcessLimit = 0` or a CPU allotment of nothing —
    /// caps nobody asked for, and ones that would fail every run instantly.
    #[test]
    fn absent_limits_map_to_nothing_rather_than_zero() {
        let lim = SandboxLimits {
            max_cpu_secs: None,
            max_memory_bytes: None,
            max_processes: None,
            ..SandboxLimits::default()
        };
        let job = JobLimits::from(&lim);
        assert_eq!(job.process_memory, None);
        assert_eq!(job.active_processes, None);
        assert_eq!(job.cpu_ticks, None);
        // Never optional, whatever the limits say.
        assert!(job.kill_on_close);
    }
}
