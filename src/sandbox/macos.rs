//! macOS native backend: `sandbox-exec` profile + rlimits + RSS monitor.
//!
//! The profile keeps a permissive base (so `rustc` can read its sysroot, fork
//! the linker, and look up mach services) but **denies outbound network** and
//! **confines filesystem writes to the run's workdir**. CPU/procs/fds are capped
//! by the shared `run_capped` rlimits; memory by its RSS monitor, since macOS
//! does not enforce address-space rlimits. This is the one native backend the
//! build host can live-run.
//!
//! A path the profile cannot name is a refusal. Every path is rendered into an
//! SBPL string literal, and a path that can end that literal goes on to append
//! rules of its own — which the last-matching-rule-wins evaluation then honours,
//! under a backend still reporting that it confined the run. So the backend
//! returns [`Error::Sandbox`] rather than run a command whose boundary it could
//! not write down.

use std::path::{Path, PathBuf};

use super::{run_capped, Backend, ExecMode, RunSpec, Sandbox, SandboxOutcome};
use crate::error::{Error, Result};

/// The macOS `sandbox-exec` backend.
pub struct MacosSandbox;

impl Sandbox for MacosSandbox {
    async fn run(&self, spec: RunSpec<'_>) -> Result<SandboxOutcome> {
        // A path this profile cannot name is a refusal, not a degradation: the
        // error comes back before anything is spawned, so a run whose
        // confinement could not be written down does not run at all rather than
        // running with a boundary nobody can vouch for.
        let profile = try_profile_for(
            spec.workdir,
            spec.allow_network,
            spec.mode,
            spec.writable_roots,
            spec.proxy,
        )
        .map_err(|reason| Error::Sandbox { reason })?;
        // Wrap the command in sandbox-exec with an inline profile.
        let mut wrapped: Vec<String> = vec!["sandbox-exec".into(), "-p".into(), profile];
        wrapped.extend(spec.argv.iter().cloned());
        let read_only = spec.mode == ExecMode::ReadOnly;
        let workdir = spec.workdir.to_path_buf();
        let wspec = RunSpec::new(&wrapped, spec.workdir, spec.limits)
            .with_network(spec.allow_network)
            .with_mode(spec.mode)
            .with_writable_roots(spec.writable_roots)
            .with_proxy(spec.proxy);
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
///
/// Every path is rendered by `sbpl_literal`, and a path it refuses collapses the
/// whole profile to `REFUSED_PROFILE` — a profile that grants nothing, so the
/// command cannot run. This signature cannot say why, because `wrap_argv` builds
/// an argv rather than a `Result`; `try_profile_for` is the same profile with the
/// reason kept, and the backend's own `run` takes that path so a refusal reaches
/// the caller as [`Error::Sandbox`] instead of as a command that mysteriously
/// could not read its own files.
// Live only on macOS; see the note on `REFUSED_PROFILE`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn profile_for(
    workdir: &Path,
    allow_network: bool,
    mode: ExecMode,
    writable_roots: &[PathBuf],
    proxy: Option<std::net::SocketAddr>,
) -> String {
    try_profile_for(workdir, allow_network, mode, writable_roots, proxy).unwrap_or_else(|reason| {
        tracing::warn!(
            %reason,
            "sandbox profile refused; the command gets a profile that grants nothing"
        );
        REFUSED_PROFILE.to_string()
    })
}

/// The profile a run gets when one of its paths cannot be named: nothing is
/// allowed, so `sandbox-exec` cannot even exec the program it was handed.
///
/// Fail closed means *this* rather than a profile with a line missing. A missing
/// `(allow file-write* …)` line still runs the command, under a boundary that no
/// longer matches what the run was told it had; a profile that grants nothing
/// fails where it can be seen.
// This module is compiled on every platform so its tests run on every platform —
// the profile is a pure string rendering and the C1 assertions are worth having on
// the Linux and Windows legs too. Its one production caller, `sandbox::wrap_argv`,
// is behind `cfg(target_os = "macos")`, so off macOS the lib build has no caller
// and `-D warnings` reads that as dead code. The `cfg_attr` is scoped to exactly
// that: on macOS the item is live and an unused one would still fail the build.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const REFUSED_PROFILE: &str = "(version 1)\n(deny default)\n";

/// Render `path` as an SBPL string literal, or say why it cannot be one.
///
/// Every path in the profile sits inside a double-quoted literal and SBPL's last
/// matching rule wins, so a path that can close its own literal can append rules
/// after it — `(allow file-write* (subpath "/"))` among them — while the backend
/// goes on reporting `MacosSandboxExec` and a confinement that is no longer in
/// force. The rendering is therefore verbatim and the check is on the input:
/// `"`, `\` and every control character are **refused rather than escaped**,
/// because SBPL's escape rules are not something this crate can pin down from
/// the platform's documentation, and a guess about them would be a guess about
/// where the boundary is. `(` and `)` pass through unchanged: inside a literal
/// they are characters and not structure, and `Project (old)` is an ordinary
/// directory name that has to keep working.
fn sbpl_literal(path: &Path) -> std::result::Result<String, String> {
    let text = path.to_str().ok_or_else(|| {
        format!(
            "the path {} is not valid UTF-8, so it cannot be named in a sandbox profile; \
             run from a directory whose name is UTF-8",
            path.display()
        )
    })?;
    if let Some(bad) = text
        .chars()
        .find(|&c| matches!(c, '"' | '\\') || c.is_control())
    {
        let what = match bad {
            '"' => "a double quote".to_string(),
            '\\' => "a backslash".to_string(),
            '\n' => "a newline".to_string(),
            c => format!("the control character U+{:04X}", c as u32),
        };
        return Err(format!(
            "the path {text} holds {what}, which cannot be written into a sandbox profile: \
             it would end the profile's own string literal and whatever followed would \
             become rules. This backend refuses rather than run a command it may not have \
             confined. Rename or move the directory so its name holds no quote, backslash \
             or control character."
        ));
    }
    Ok(format!("\"{text}\""))
}

/// [`profile_for`] with the refusal kept, for the caller that can report one.
fn try_profile_for(
    workdir: &Path,
    allow_network: bool,
    mode: ExecMode,
    writable_roots: &[PathBuf],
    proxy: Option<std::net::SocketAddr>,
) -> std::result::Result<String, String> {
    // 0.48.0 — when the run owns a proxy, everything is denied and that one
    // loopback address is allowed back. SBPL can name an address and a port
    // exactly, so on this platform "the proxy is the only route out" is a kernel
    // decision rather than a convention a payload could ignore. The deny comes
    // first because the last matching rule wins.
    let net = match (proxy, allow_network) {
        (Some(addr), _) => format!(
            "(deny network*)\n(allow network-outbound (remote ip \"localhost:{}\"))",
            addr.port()
        ),
        (None, true) => "(allow network*)".to_string(),
        (None, false) => "(deny network*)".to_string(),
    };
    // - allow default: let rustc read/exec/fork freely
    // - deny writes under / then re-allow what the mode grants and the tty/dev
    //   nodes a normal process needs, so writes are confined without breaking
    //   exec.
    let mut allows = String::new();
    if mode != ExecMode::ReadOnly {
        allows.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_literal(workdir)?
        ));
    }
    for root in writable_roots {
        allows.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_literal(root)?
        ));
    }
    Ok(format!(
        "(version 1)\n\
         (allow default)\n\
         {net}\n\
         (deny file-write* (subpath \"/\"))\n\
         {allows}\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/dtracehelper\"))\n\
         (allow file-write* (subpath \"/private/var/folders\"))\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(mode: ExecMode, roots: &[PathBuf]) -> String {
        profile_for(Path::new("/tmp/sbx"), false, mode, roots, None)
    }

    #[test]
    fn profile_denies_network_by_default_and_confines_writes() {
        let p = profile(ExecMode::WorkspaceWrite, &[]);
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/sbx\"))"));
        assert!(p.contains("(deny file-write* (subpath \"/\"))"));
    }

    /// 0.80.0 F2 — the second of the three states a widened run can be in, and
    /// the one nothing asserted before.
    ///
    /// The three are: no widening (`profile_denies_network_by_default…` above),
    /// widened with no host list (here), and widened with one — which on this
    /// crate is the egress proxy, because SBPL can name an address and a port
    /// and cannot name a host, so a list of hosts is enforced by the proxy that
    /// resolves them rather than by the kernel
    /// (`a_proxy_denies_everything_and_allows_the_loopback_port_back` below).
    ///
    /// The assertion is against the *whole* profile rather than for one
    /// substring, because the failure this guards is a fix that grants network
    /// by granting everything: `(allow default)` with no `deny file-write*`
    /// would contain `(allow network*)` too and pass a substring test while
    /// handing the run the filesystem.
    #[test]
    fn widening_grants_the_network_and_moves_nothing_else() {
        let denied = profile(ExecMode::WorkspaceWrite, &[]);
        let widened = profile_for(
            Path::new("/tmp/sbx"),
            true,
            ExecMode::WorkspaceWrite,
            &[],
            None,
        );

        assert!(
            widened.contains("(allow network*)"),
            "a widened run may bind and may dial — SBPL has no narrower verb \
             that permits a local `bind`, which is the first thing the field \
             test tried: {widened}"
        );
        assert_eq!(
            widened,
            denied.replace("(deny network*)", "(allow network*)"),
            "and the widened profile differs from the denied one in that clause \
             and in nothing else"
        );
    }

    #[test]
    fn a_proxy_denies_everything_and_allows_the_loopback_port_back() {
        let addr: std::net::SocketAddr = "127.0.0.1:54321".parse().unwrap();
        let p = profile_for(
            Path::new("/tmp/sbx"),
            true,
            ExecMode::WorkspaceWrite,
            &[],
            Some(addr),
        );
        assert!(p.contains("(deny network*)"), "everything is denied first");
        assert!(
            p.contains("(allow network-outbound (remote ip \"localhost:54321\"))"),
            "and exactly the proxy is allowed back: {p}"
        );
        // Even though the run permits egress: with a proxy, permission is the
        // proxy's decision to make per host, not the profile's to grant wholesale.
        assert!(
            !p.contains("(allow network*)"),
            "no blanket allow survives: {p}"
        );
    }

    #[test]
    fn profile_allows_network_when_asked() {
        let p = profile_for(
            Path::new("/tmp/sbx"),
            true,
            ExecMode::WorkspaceWrite,
            &[],
            None,
        );
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

    /// The injection fixture from the private audit of 2026-08-29 (C1): a
    /// directory name which, interpolated raw, closes the `subpath` literal and
    /// appends two grants of its own. SBPL's last matching rule wins, so up to
    /// 0.73.0 those grants were the ones in force — on `/`, for writes and for
    /// the network — while the backend went on reporting `MacosSandboxExec` and
    /// a confinement it no longer had.
    const HOSTILE_DIR: &str = "p\")) (allow network*) (allow file-write* (subpath \"/";

    /// C1 — a workdir that can close the profile's string literal is refused,
    /// and the refusal is a profile that grants nothing rather than one with a
    /// line missing.
    #[test]
    fn c1_a_workdir_that_closes_the_profiles_string_literal_is_refused() {
        let workdir = PathBuf::from("/tmp").join(HOSTILE_DIR);
        let p = profile_for(&workdir, false, ExecMode::WorkspaceWrite, &[], None);

        assert_eq!(p, REFUSED_PROFILE, "the profile grants nothing");
        assert!(!p.contains("(allow network*)"), "{p}");
        assert!(
            !p.contains("(allow file-write* (subpath \"/\"))"),
            "no write grant on /: {p}"
        );
        assert!(
            !p.contains(HOSTILE_DIR),
            "nothing of the path is rendered: {p}"
        );
    }

    /// C1 — the same guard on the other interpolation site. `writable_roots` is
    /// env-derived rather than model-chosen, which makes it less reachable, not
    /// safe: the rendering is the same rendering.
    #[test]
    fn c1_a_writable_root_that_closes_the_profiles_string_literal_is_refused_too() {
        let roots = vec![PathBuf::from("/tmp").join(HOSTILE_DIR)];
        let p = profile(ExecMode::WorkspaceWrite, &roots);

        assert_eq!(p, REFUSED_PROFILE, "the profile grants nothing");
        assert!(!p.contains("(allow network*)"), "{p}");
        assert!(!p.contains(HOSTILE_DIR), "{p}");
    }

    /// C1 — every character that could end the literal refuses, and the refusal
    /// names the path, the reason and what to do instead.
    #[test]
    fn c1_a_path_that_cannot_be_quoted_refuses_with_a_reason_that_teaches() {
        fn refusal(path: &str) -> String {
            try_profile_for(Path::new(path), false, ExecMode::WorkspaceWrite, &[], None)
                .expect_err("a path that can end the literal is refused")
        }
        for bad in [
            "/tmp/a\"b",
            "/tmp/a\\b",
            "/tmp/a\nb",
            "/tmp/a\rb",
            "/tmp/a\u{0}b",
        ] {
            let reason = refusal(bad);
            assert!(reason.contains(bad), "names the path: {reason}");
            assert!(
                reason.contains("Rename or move the directory"),
                "says what to do instead: {reason}"
            );
        }
    }

    /// C1 — the companion. An ordinary macOS directory name still gets its
    /// grant, and gets exactly the profile a plain path gets with the path
    /// substituted: no rule appears, disappears or moves. Parentheses are in
    /// this list on purpose — inside a string literal they are characters, not
    /// structure, so they need no escaping and refusing them would break
    /// directories people really have.
    #[test]
    fn c1_ordinary_directory_names_still_get_their_allow_line() {
        let baseline = profile(ExecMode::WorkspaceWrite, &[]);
        for ordinary in [
            "/tmp/my project",
            "/tmp/my-project",
            "/tmp/my.project",
            "/tmp/проект",
            "/tmp/josé's notes",
            "/tmp/Project (old)",
        ] {
            let p = profile_for(
                Path::new(ordinary),
                false,
                ExecMode::WorkspaceWrite,
                &[],
                None,
            );
            assert!(
                p.contains(&format!("(allow file-write* (subpath \"{ordinary}\"))")),
                "{p}"
            );
            assert_eq!(
                p,
                baseline.replace("/tmp/sbx", ordinary),
                "only the path itself differs from a plain profile"
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
