//! macOS native backend: `sandbox-exec` profile + rlimits + RSS monitor.
//!
//! The profile keeps a permissive base (so `rustc` can read its sysroot, fork
//! the linker, and look up mach services) but **denies outbound network** and
//! **confines filesystem writes to the run's workdir**. CPU/procs/fds are capped
//! by the shared `run_capped` rlimits; memory by its RSS monitor, since macOS
//! does not enforce address-space rlimits. This is the one native backend the
//! build host can live-run.

use std::path::{Path, PathBuf};

use super::{run_capped, Backend, ExecMode, RunSpec, Sandbox, SandboxOutcome};
use crate::error::Result;

/// The macOS `sandbox-exec` backend.
pub struct MacosSandbox;

impl Sandbox for MacosSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        let profile = profile_for(
            spec.workdir,
            spec.allow_network,
            spec.mode,
            spec.writable_roots,
        );
        // Wrap the command in sandbox-exec with an inline profile.
        let mut wrapped: Vec<String> = vec!["sandbox-exec".into(), "-p".into(), profile];
        wrapped.extend(spec.argv.iter().cloned());
        let read_only = spec.mode == ExecMode::ReadOnly;
        let workdir = spec.workdir.to_path_buf();
        let wspec = RunSpec::new(&wrapped, spec.workdir, spec.limits)
            .with_network(spec.allow_network)
            .with_mode(spec.mode)
            .with_writable_roots(spec.writable_roots);
        run_capped(Backend::MacosSandboxExec, wspec, move |cmd| {
            // Keep rustc's temp writes inside the confined workdir — except under
            // `ReadOnly`, where the workdir is exactly what may not be written to
            // and the system temp directory is the one place that can be.
            if !read_only {
                cmd.env("TMPDIR", &workdir);
            }
        })
        .await
    }

    fn backend(&self) -> Backend {
        Backend::MacosSandboxExec
    }
}

/// Build an SBPL profile: permissive base, network denied (unless allowed), and
/// writes confined to what `mode` grants. The last matching rule wins in SBPL, so
/// the broad allows come first and the narrowing denies/allows after.
///
/// `mode` decides whether `workdir` is among the writable places at all —
/// [`ExecMode::ReadOnly`] grants the temp directory and the dev nodes and nothing
/// else — and `writable_roots` are the extra grants the run resolved, which on
/// this platform is one `(allow file-write* (subpath …))` line each.
pub(crate) fn profile_for(
    workdir: &Path,
    allow_network: bool,
    mode: ExecMode,
    writable_roots: &[PathBuf],
) -> String {
    let net = if allow_network {
        "(allow network*)"
    } else {
        "(deny network*)"
    };
    // - allow default: let rustc read/exec/fork freely
    // - deny writes under / then re-allow what the mode grants and the tty/dev
    //   nodes a normal process needs, so writes are confined without breaking
    //   exec.
    let mut allows = String::new();
    if mode != ExecMode::ReadOnly {
        allows.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            workdir.display()
        ));
    }
    for root in writable_roots {
        allows.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            root.display()
        ));
    }
    format!(
        "(version 1)\n\
         (allow default)\n\
         {net}\n\
         (deny file-write* (subpath \"/\"))\n\
         {allows}\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/dtracehelper\"))\n\
         (allow file-write* (subpath \"/private/var/folders\"))\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(mode: ExecMode, roots: &[PathBuf]) -> String {
        profile_for(Path::new("/tmp/sbx"), false, mode, roots)
    }

    #[test]
    fn profile_denies_network_by_default_and_confines_writes() {
        let p = profile(ExecMode::WorkspaceWrite, &[]);
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/sbx\"))"));
        assert!(p.contains("(deny file-write* (subpath \"/\"))"));
    }

    #[test]
    fn profile_allows_network_when_asked() {
        let p = profile_for(Path::new("/tmp/sbx"), true, ExecMode::WorkspaceWrite, &[]);
        assert!(p.contains("(allow network*)"));
    }

    /// F7 — one writable-root list, rendered here as one `allow` line each, and
    /// the deny-all still ahead of them.
    #[test]
    fn every_writable_root_gets_exactly_one_allow_line() {
        let roots = vec![
            PathBuf::from("/home/u/.cargo"),
            PathBuf::from("/home/u/.npm"),
        ];
        let p = profile(ExecMode::WorkspaceWrite, &roots);

        let deny_at = p.find("(deny file-write* (subpath \"/\"))").unwrap();
        for root in &roots {
            let line = format!("(allow file-write* (subpath \"{}\"))\n", root.display());
            assert_eq!(p.matches(&line).count(), 1, "{line} in\n{p}");
            assert!(
                p.find(&line).unwrap() > deny_at,
                "an allow must follow the deny-all"
            );
        }
    }

    /// F3 — the mode decides whether the workdir itself is writable. Under
    /// `ReadOnly` it is not, and that is the whole difference between the two
    /// contained modes on this platform.
    #[test]
    fn read_only_does_not_grant_the_workdir() {
        let p = profile(ExecMode::ReadOnly, &[]);
        assert!(
            !p.contains("(allow file-write* (subpath \"/tmp/sbx\"))"),
            "{p}"
        );
        assert!(p.contains("(deny file-write* (subpath \"/\"))"));
        // The temp directory stays writable, or nothing runs under this mode.
        assert!(p.contains("(allow file-write* (subpath \"/private/var/folders\"))"));
    }
}
