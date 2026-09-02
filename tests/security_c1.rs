//! C1 — a path cannot rewrite the macOS sandbox profile.
//!
//! The macOS backend confines a run by handing `sandbox-exec` an SBPL profile it
//! builds by hand, and every path in that profile sits inside a double-quoted
//! string literal. SBPL's last matching rule wins, so a directory whose *name*
//! can close that literal appends rules after it — and up to 0.73.0 those rules
//! were the ones in force while `Sandbox::backend` still answered
//! `MacosSandboxExec`. That is the product's own thesis failing: not a boundary
//! that is too wide, but a boundary the harness reports and does not have.
//!
//! The injection fixture below is a directory name, not a recipe: it is the
//! input the acceptance criterion names, and it is here so the assertion is made
//! against the real thing rather than a sanitised stand-in.
//!
//! These tests run on every platform. `sandbox::macos` is compiled everywhere
//! (see the module comment above `pub mod macos` in `src/sandbox.rs`), and the
//! refusal happens while the profile is being built — before anything is
//! spawned — so nothing here needs a `sandbox-exec` binary or a macOS host.

use io_harness::sandbox::macos::MacosSandbox;
use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{Error, SandboxLimits};

/// The injection fixture from the private audit: interpolated raw, it closes the
/// `subpath` literal and appends a write grant on `/` and a network grant.
const HOSTILE_DIR: &str = "p\")) (allow network*) (allow file-write* (subpath \"/";

/// C1 — the backend refuses the run rather than building a profile it cannot
/// vouch for. On 0.73.0 this spawned the command with the injected grants in the
/// profile and returned an outcome; the refusal is the whole fix.
// Not on Windows, and the reason is the guard working rather than failing.
//
// These two drive a real `tempfile::tempdir()`, so the path is absolute — and on
// Windows an absolute path is `C:\Users\…`, where every separator is a
// backslash, which is one of the three characters this guard refuses. So the
// refusal fires on the path separator before it ever reaches the injected quote,
// and the assertions below would be checking the wrong cause. The rendering
// itself is asserted on every platform by the unit tests in `src/sandbox/macos.rs`,
// which build relative paths and therefore say what they mean everywhere.
#[cfg(not(windows))]
#[tokio::test]
async fn c1_the_macos_backend_refuses_a_workdir_it_cannot_name_in_its_profile() {
    let workdir = std::env::temp_dir().join(HOSTILE_DIR);
    let argv = vec!["/bin/echo".to_string(), "contained".to_string()];
    let limits = SandboxLimits::default();

    let refused = MacosSandbox
        .run(RunSpec::new(&argv, &workdir, &limits))
        .await
        .expect_err("a profile that cannot be built is a refusal, not a run");

    match refused {
        Error::Sandbox { reason } => {
            // A refusal has to teach, or the next agent works around it: the
            // path it is about, why it cannot be used, and what to do instead.
            assert!(reason.contains(HOSTILE_DIR), "names the path: {reason}");
            assert!(
                reason.contains("double quote"),
                "names the reason: {reason}"
            );
            assert!(
                reason.contains("Rename or move the directory"),
                "names the alternative: {reason}"
            );
        }
        other => panic!("expected a sandbox refusal, got {other}"),
    }
}

/// C1 — the companion, and the reason the guard rejects rather than rejecting
/// everything unusual: a space, a hyphen, a dot, a unicode character, an
/// apostrophe and a pair of parentheses are all ordinary in a macOS directory
/// name, and none of them can end a string literal. The run must get a profile.
// Not on Windows, for the reason given above: the temp path's own separators are
// refused characters, so "an ordinary name still gets a profile" cannot be
// asserted from an absolute Windows path.
#[cfg(not(windows))]
#[tokio::test]
async fn c1_an_ordinary_directory_name_is_still_given_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("Project (old) - josé's data.v2");
    std::fs::create_dir_all(&workdir).unwrap();
    let argv = vec!["/bin/echo".to_string(), "contained".to_string()];
    let limits = SandboxLimits::default();

    // Off macOS this fails at the spawn instead, because `sandbox-exec` is not
    // there — a different failure, and asserting on the absence of *this* one is
    // what makes the test portable rather than macOS-only.
    if let Err(Error::Sandbox { reason }) = MacosSandbox
        .run(RunSpec::new(&argv, &workdir, &limits))
        .await
    {
        assert!(
            !reason.contains("cannot be written into a sandbox profile"),
            "an ordinary directory name must still get a profile: {reason}"
        );
    }
}
