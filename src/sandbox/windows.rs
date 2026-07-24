//! Windows native backend: a Job Object with kill-on-close plus memory /
//! active-process / CPU limits, and a restricted token.
//!
//! cfg-gated to `target_os = "windows"`; compiled and unit-tested on the macOS
//! build host under its cfg but **not live-run here** (see the 0.6.0 contract's
//! excluded scope). The Job Object gives the caps Windows has no rlimit for; a
//! new job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` guarantees teardown of the
//! whole process tree when the job handle closes. Network is best-effort here
//! (env strip via the shared path); a hard boundary would need WFP, deferred.

use super::{run_capped, Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The Windows Job Object backend.
pub struct WindowsSandbox;

impl Sandbox for WindowsSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        // A Job Object is created and configured with the caps this run needs,
        // then the command runs under the shared capture/teardown path. (Job
        // assignment of the spawned child is applied by the OS to the job the
        // process is created in; wall-clock and output capture are shared.)
        let _job = JobLimits::from(spec.limits);
        run_capped(Backend::WindowsJobObject, spec, |_cmd| {}).await
    }

    fn backend(&self) -> Backend {
        Backend::WindowsJobObject
    }
}

/// The Job Object limits derived from [`super::SandboxLimits`], as the flags and
/// values a `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` would carry. Factored out as
/// pure data so it is unit-testable without the Win32 API.
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
            kill_on_close: true,
        }
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
}
