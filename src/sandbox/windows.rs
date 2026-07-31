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

use super::{Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The Windows backend: every command runs inside a fresh Job Object whose
/// handle is held for exactly the length of the run, so teardown is the drop.
pub struct WindowsSandbox;

impl Sandbox for WindowsSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        #[cfg(windows)]
        {
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

/// The Win32 half. Compiled only on Windows; everything above this line builds
/// on every host so the mapping and its test do too.
#[cfg(windows)]
mod job {
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

    use super::JobLimits;
    use crate::error::{Error, Result};
    use crate::sandbox::{run_capped, run_capped_hooked, Backend, Cap, RunSpec, SandboxOutcome};

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
    struct Job(HANDLE);

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
        /// Create the job and apply `limits` to it, before any process is in it.
        fn create(limits: &JobLimits) -> io::Result<Self> {
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
        fn adopt(&self, child: &tokio::process::Child) -> Result<()> {
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
        fn cap_hit(&self, limits: &JobLimits) -> Option<Cap> {
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
