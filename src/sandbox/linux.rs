//! Linux native backend: user/mount/pid/**net** namespaces via `unshare`, plus
//! the shared rlimit + RSS caps. The new network namespace is a *hard* boundary
//! — a process in an empty net namespace has no route out — which is stronger
//! than the floor's best-effort env strip.
//!
//! cfg-gated to `target_os = "linux"`; compiled and unit-tested on the macOS
//! build host under its cfg but **not live-run here** (see the 0.6.0 contract's
//! excluded scope). Seccomp tightening is layered by the kernel default under
//! the unprivileged user namespace.
//!
//! Note what this backend does **not** do: it never probes for `unshare`, and
//! [`super::select`] returns it unconditionally on Linux rather than falling back
//! to the floor. On a kernel with unprivileged user namespaces disabled — or a
//! host without `unshare(1)` — the wrapper fails at spawn time instead of
//! degrading; there is no runtime capability check yet.

use super::{run_capped, Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The Linux namespaces backend.
pub struct LinuxSandbox;

impl Sandbox for LinuxSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        // Wrap in `unshare`: new user (map root), mount, pid, and — when network
        // is denied — a new empty network namespace with no route out.
        let wrapped = unshare_argv(spec.argv, spec.allow_network);
        let wspec = RunSpec {
            argv: &wrapped,
            workdir: spec.workdir,
            limits: spec.limits,
            allow_network: spec.allow_network,
        };
        run_capped(Backend::LinuxNamespaces, wspec, |_cmd| {}).await
    }

    fn backend(&self) -> Backend {
        Backend::LinuxNamespaces
    }
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
}
