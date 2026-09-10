//! An MCP server's stderr goes into the run's trace and nowhere else (0.86.0).
//!
//! Until this release the harness spawned a stdio server with rmcp's default
//! stdio configuration, whose stderr is `Stdio::inherit()`. Every banner, warning
//! and stack trace the server wrote therefore went to whatever this process's
//! stderr was — an operator's CI log carried another product's lines, attributed
//! to this one, and the run's own trace recorded nothing about it.
//!
//! **The claim needs two processes to prove.** That the line was captured is a
//! row in the run's trace, readable from inside. That it did *not also* reach the
//! host's stderr is only visible to whoever owns that stderr. So the subject is
//! `examples/mcp_stderr_probe.rs`, run here as a child with both streams piped,
//! and each half of the claim is read off a different stream.
//!
//! Negative control: `the_probe_writes_the_banner_to_stderr_when_it_is_inherited`
//! spawns the fixture server directly, with its stderr inherited the way the
//! harness used to spawn it, and fails if the banner is not there — so the
//! assertions above cannot pass by the server having stopped printing one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The banner `examples/mcp_fixture_server.rs` writes before it speaks the
/// protocol. Included from the example rather than repeated, so the two cannot
/// drift into asserting different bytes.
#[allow(dead_code)]
#[path = "../examples/mcp_fixture_server.rs"]
mod fixture_server;

/// An example binary `cargo build --examples` produced.
///
/// The same two layouts `examples/mcp_run.rs` handles: a test binary runs from
/// `deps/`, and its examples are one directory up in `examples/`.
fn example_binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("this test has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    if !dir.ends_with("examples") {
        dir.push("examples");
    }
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Run `bin` to completion with both streams piped, and return them.
fn run_example(bin: &Path) -> (String, String) {
    assert!(
        bin.exists(),
        "fixture not built at {}. Run: cargo build --examples",
        bin.display()
    );
    let out = Command::new(bin)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", bin.display()));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_servers_stderr_is_recorded_in_the_run_and_never_reaches_the_hosts() {
    let (stdout, stderr) = run_example(&example_binary("mcp_stderr_probe"));
    assert!(
        stdout.contains("probe finished"),
        "the probe ran to completion. stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Captured: the run holds the server's line as a trace row of its own.
    assert!(
        stdout.contains("captured:") && stdout.contains(fixture_server::BANNER),
        "the server's banner was recorded as a context event. stdout:\n{stdout}"
    );

    // And not leaked: the host's stderr never saw it. This is the half that
    // only a second process can answer.
    assert!(
        !stderr.contains(fixture_server::BANNER),
        "the harness's own stderr carried none of the server's output. stderr:\n{stderr}"
    );
}

/// Negative control. Without it, a fixture that quietly stopped printing a
/// banner would make the test above pass while proving nothing.
#[test]
fn the_fixture_still_writes_a_banner_when_its_stderr_is_inherited() {
    let bin = example_binary("mcp_fixture_server");
    assert!(
        bin.exists(),
        "fixture not built. Run: cargo build --examples"
    );
    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the fixture server starts");
    // It writes the banner before it reads anything, so closing stdin is enough
    // to make it exit without a protocol exchange.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("the fixture server exits");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(fixture_server::BANNER),
        "the fixture writes a banner to its stderr, which is what the test above \
         proves the harness captures"
    );
}
