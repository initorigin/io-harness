//! Windows backend — **not yet native**. What runs on Windows today is the
//! [portable floor](super::FloorSandbox), and the trace says so.
//!
//! The Job Object was designed but never implemented: no Win32 API is called
//! here, so there is **no** job object, **no** kill-on-close process-tree
//! teardown, **no** active-process limit, and **no** restricted token. The CPU
//! and memory caps are unenforced too — the shared path applies them via
//! `RLIMIT_CPU` and an RSS monitor over `ps`, all unix-only. What a Windows run
//! does get is the floor: a fresh subprocess in an ephemeral workdir,
//! `kill_on_drop`, the wall-clock timeout, and the best-effort proxy-env strip.
//! Filesystem scoping and the wall cap, nothing stronger.
//!
//! **On Windows the floor enforces the wall clock and nothing else.** Stated
//! plainly because it is the whole contract here:
//!
//! - **Wall clock** — enforced, and the kill reaches the whole process tree via
//!   `taskkill /T` (Windows has no signals and no `ps`; `taskkill` ships with the
//!   OS, so this costs no dependency). It is the only thing standing between a
//!   Windows run and running forever, so `max_wall_secs: None` there means
//!   *unbounded*.
//! - **CPU, memory, processes** — not applied. `SandboxLimits` still carries
//!   them, and `JobLimits` still maps them, but nothing enforces them until the
//!   Job Object lands. They are never *claimed* either: a Windows outcome never
//!   reports [`super::Cap::Cpu`] or [`super::Cap::Memory`], and the first run
//!   configured with either cap logs a warning saying so.
//! - **Network** — no kernel boundary. Only the floor's proxy-env strip, which a
//!   payload that does not read those variables ignores entirely.
//!
//! So [`WindowsSandbox::run`] reports [`Backend::PortableFloor`] rather than
//! [`Backend::WindowsJobObject`]: a run that creates no job object must not name
//! one in an audit trail. The real implementation needs `windows-sys`, which is
//! a new runtime dependency, and lands in its own release; until then the type
//! stays as the wiring point [`super::select`] already targets.

use super::{run_capped, Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The Windows backend. Currently the portable floor — see the module docs; the
/// Job Object is not implemented yet.
pub struct WindowsSandbox;

impl Sandbox for WindowsSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        // No job object is created. The limits are mapped so the shape is ready
        // for the real implementation, then dropped unapplied; the run gets the
        // floor's capture/teardown and nothing more.
        let _job = JobLimits::from(spec.limits);
        run_capped(Backend::PortableFloor, spec, |_cmd| {}).await
    }

    fn backend(&self) -> Backend {
        Backend::PortableFloor
    }
}

/// The Job Object limits derived from [`super::SandboxLimits`], as the flags and
/// values a `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` would carry. Factored out as
/// pure data so it is unit-testable without the Win32 API.
///
/// **Prepared but not applied.** Nothing consumes this yet — it is the mapping
/// the real Job Object implementation will use once it exists.
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
