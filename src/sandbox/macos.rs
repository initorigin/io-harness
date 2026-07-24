//! macOS native backend: `sandbox-exec` profile + rlimits + RSS monitor.
//!
//! The profile keeps a permissive base (so `rustc` can read its sysroot, fork
//! the linker, and look up mach services) but **denies outbound network** and
//! **confines filesystem writes to the run's workdir**. CPU/procs/fds are capped
//! by the shared `run_capped` rlimits; memory by its RSS monitor, since macOS
//! does not enforce address-space rlimits. This is the one native backend the
//! build host can live-run.

use std::path::Path;

use super::{run_capped, Backend, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The macOS `sandbox-exec` backend.
pub struct MacosSandbox;

impl Sandbox for MacosSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        let profile = profile_for(spec.workdir, spec.allow_network);
        // Wrap the command in sandbox-exec with an inline profile.
        let mut wrapped: Vec<String> = vec!["sandbox-exec".into(), "-p".into(), profile];
        wrapped.extend(spec.argv.iter().cloned());
        let workdir = spec.workdir.to_path_buf();
        let wspec = RunSpec {
            argv: &wrapped,
            workdir: spec.workdir,
            limits: spec.limits,
            allow_network: spec.allow_network,
        };
        run_capped(Backend::MacosSandboxExec, wspec, move |cmd| {
            // Keep rustc's temp writes inside the confined workdir.
            cmd.env("TMPDIR", &workdir);
        })
        .await
    }

    fn backend(&self) -> Backend {
        Backend::MacosSandboxExec
    }
}

/// Build an SBPL profile: permissive base, network denied (unless allowed), and
/// writes confined to `workdir`. The last matching rule wins in SBPL, so the
/// broad allows come first and the narrowing denies/allows after.
pub(crate) fn profile_for(workdir: &Path, allow_network: bool) -> String {
    let wd = workdir.display();
    let net = if allow_network {
        "(allow network*)"
    } else {
        "(deny network*)"
    };
    // - allow default: let rustc read/exec/fork freely
    // - deny writes under / then re-allow the workdir and the tty/dev nodes a
    //   normal process needs, so writes are confined without breaking exec.
    format!(
        "(version 1)\n\
         (allow default)\n\
         {net}\n\
         (deny file-write* (subpath \"/\"))\n\
         (allow file-write* (subpath \"{wd}\"))\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/dtracehelper\"))\n\
         (allow file-write* (subpath \"/private/var/folders\"))\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_denies_network_by_default_and_confines_writes() {
        let p = profile_for(Path::new("/tmp/sbx"), false);
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/sbx\"))"));
        assert!(p.contains("(deny file-write* (subpath \"/\"))"));
    }

    #[test]
    fn profile_allows_network_when_asked() {
        let p = profile_for(Path::new("/tmp/sbx"), true);
        assert!(p.contains("(allow network*)"));
    }
}
