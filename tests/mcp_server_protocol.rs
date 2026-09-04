//! The MCP server's public surface and the manifest claim underneath it — F10,
//! F11 and F12 of 0.78.0.
//!
//! The protocol itself is exercised in `src/mcp_server.rs`'s own `mod tests`,
//! because the handler is crate-private: an external test that could reach it
//! would mean the framing was part of this crate's public surface, and it is
//! not. [`serve_mcp`](io_harness::serve_mcp) is the door.
//!
//! What is left for an external crate is the part no unit test can see. The
//! server was written by hand rather than over `rmcp`'s server half, and the
//! whole argument for that is a dependency count: `rmcp` keeps exactly the three
//! client features it has today, and the shipped crate stays client-side. That
//! is a fact about `Cargo.toml`, so it is checked by reading `Cargo.toml` —
//! prose saying it would be prose, and prose is what rots.
//!
//! Each checker here is a pure function over the manifest text plus one test
//! that runs it against the real file, and each carries a negative control — a
//! fixture that must fail — because a checker that has never failed is a checker
//! nobody has shown to work. `tests/docs_drift.rs` set that shape.

use std::fs;
use std::path::PathBuf;

/// The features the shipped `rmcp` dependency carries, in the order the manifest
/// lists them. Adding one is a decision, and this is where the decision is made
/// visible rather than merged.
const SHIPPED_RMCP_FEATURES: &[&str] = &[
    "client",
    "transport-child-process",
    "transport-streamable-http-client-reqwest",
];

/// The sentence above the dev-dependency that states the claim this file exists
/// to keep true.
const CLIENT_ONLY_NOTE: &str = "the crate itself ships client-side only";

fn manifest() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The lines of one table, from its header to the next one.
///
/// Line scanning rather than a toml parser, for the reason `tests/docs_drift.rs`
/// gives: what is needed from this file is one entry in one table, and a parser
/// dependency to read a manifest this crate already owns is not worth it.
fn table<'a>(manifest: &'a str, header: &str) -> Result<Vec<&'a str>, String> {
    let mut lines = manifest.lines().skip_while(|l| l.trim() != header);
    if lines.next().is_none() {
        return Err(format!("{header} is not in the manifest"));
    }
    Ok(lines
        .take_while(|l| !l.trim_start().starts_with('['))
        .collect())
}

/// The features `rmcp` is asked for in one table.
///
/// The entry wraps across lines in `[dependencies]` and sits on one line in
/// `[dev-dependencies]`, so it is gathered until its closing brace rather than
/// read as a single line.
fn rmcp_features(manifest: &str, header: &str) -> Result<Vec<String>, String> {
    let lines = table(manifest, header)?;
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("rmcp"))
        .ok_or_else(|| format!("{header} declares no rmcp dependency"))?;
    let mut entry = String::new();
    for line in &lines[start..] {
        entry.push_str(line);
        entry.push('\n');
        if line.contains('}') {
            break;
        }
    }
    let features = entry
        .split_once("features = [")
        .ok_or_else(|| format!("{header}'s rmcp entry names no features"))?
        .1;
    let features = features
        .split_once(']')
        .ok_or_else(|| format!("{header}'s rmcp feature list is unterminated"))?
        .0;
    // Every quoted run inside the list, which is every feature name: the list
    // holds nothing else.
    Ok(features
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect())
}

/// The shipped dependency asks for the three client features and nothing else.
fn rmcp_stays_client_side(manifest: &str) -> Result<(), String> {
    let features = rmcp_features(manifest, "[dependencies]")?;
    if features != SHIPPED_RMCP_FEATURES {
        return Err(format!(
            "the shipped rmcp dependency asks for {features:?}, not {SHIPPED_RMCP_FEATURES:?}"
        ));
    }
    Ok(())
}

/// The claim the dev-dependency's comment makes is still written down.
fn carries_the_client_only_note(manifest: &str) -> Result<(), String> {
    match manifest.contains(CLIENT_ONLY_NOTE) {
        true => Ok(()),
        false => Err(format!("the manifest no longer says `{CLIENT_ONLY_NOTE}`")),
    }
}

/// A manifest shaped like the real one, for the controls to damage.
fn fixture(shipped_features: &str) -> String {
    format!(
        "[package]\nname = \"io-harness\"\n\n\
         [dependencies]\nserde_json = \"1\"\nrmcp = {{ version = \"3.0.0\", \
         default-features = false, features = [\n{shipped_features}\n] }}\ntokio = \"1\"\n\n\
         [dev-dependencies]\n# Dev-only: {CLIENT_ONLY_NOTE}.\n\
         rmcp = {{ version = \"3.0.0\", features = [\"server\"] }}\n"
    )
}

fn real_features() -> String {
    SHIPPED_RMCP_FEATURES
        .iter()
        .map(|f| format!("    \"{f}\","))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn f10_the_shipped_rmcp_dependency_keeps_exactly_its_three_client_features() {
    if let Err(why) = rmcp_stays_client_side(&manifest()) {
        panic!("{why}");
    }
}

#[test]
fn f10_the_manifest_still_says_the_crate_ships_client_side_only() {
    if let Err(why) = carries_the_client_only_note(&manifest()) {
        panic!("{why}");
    }
}

#[test]
fn f10_the_server_half_of_rmcp_is_a_dev_dependency_and_only_that() {
    // The two tables are told apart, which is what makes the check above mean
    // something: the dev-dependency does carry `server`, and finding it there is
    // how a checker that read the wrong table would be caught.
    let dev = rmcp_features(&manifest(), "[dev-dependencies]").expect("a dev rmcp entry");
    assert!(dev.contains(&"server".to_string()), "{dev:?}");
    let shipped = rmcp_features(&manifest(), "[dependencies]").expect("a shipped rmcp entry");
    assert!(
        !shipped.iter().any(|f| f.contains("server")),
        "no server feature is shipped: {shipped:?}"
    );
}

#[test]
fn control_the_rmcp_checker_rejects_a_server_feature() {
    let damaged = fixture(&format!("{}\n    \"server\",", real_features()));
    assert!(rmcp_stays_client_side(&damaged).is_err());
}

#[test]
fn control_the_rmcp_checker_rejects_a_dropped_feature() {
    let damaged = fixture("    \"client\",");
    assert!(rmcp_stays_client_side(&damaged).is_err());
}

#[test]
fn control_the_rmcp_checker_rejects_a_manifest_with_no_rmcp() {
    let damaged = "[package]\nname = \"io-harness\"\n\n[dependencies]\nserde_json = \"1\"\n";
    assert!(rmcp_stays_client_side(damaged).is_err());
}

#[test]
fn control_the_client_only_checker_rejects_a_manifest_that_dropped_the_note() {
    let damaged = fixture(&real_features()).replace(CLIENT_ONLY_NOTE, "dev-only");
    assert!(carries_the_client_only_note(&damaged).is_err());
}

#[test]
fn control_the_checkers_accept_the_fixture_they_damage() {
    // Without this the controls above prove nothing: a fixture that fails every
    // checker for an unrelated reason would make all four pass.
    let intact = fixture(&real_features());
    assert!(rmcp_stays_client_side(&intact).is_ok());
    assert!(carries_the_client_only_note(&intact).is_ok());
}

#[cfg(feature = "mcp-server")]
mod served {
    use io_harness::{
        McpServerConfig, Policy, ASK_QUESTIONS_TOOL, ASK_QUESTION_TOOL,
        MCP_SERVER_PROTOCOL_VERSION, MCP_SERVER_UNSERVED, PROPOSE_PLAN_TOOL, READ_MESSAGES_TOOL,
        SEND_MESSAGE_TOOL, SPAWN_TOOL,
    };

    #[test]
    fn f11_the_offered_version_is_the_one_this_crates_own_client_speaks() {
        // rmcp 3.0.0's latest. The two halves of this product agree about the
        // protocol they are on, which is only checkable by naming it in both
        // places and comparing.
        assert_eq!(MCP_SERVER_PROTOCOL_VERSION, "2025-11-25");
    }

    #[test]
    fn f12_every_tool_a_served_session_cannot_honour_is_named_as_unserved() {
        for name in [
            ASK_QUESTION_TOOL,
            ASK_QUESTIONS_TOOL,
            PROPOSE_PLAN_TOOL,
            SPAWN_TOOL,
            SEND_MESSAGE_TOOL,
            READ_MESSAGES_TOOL,
        ] {
            assert!(
                MCP_SERVER_UNSERVED.contains(&name),
                "`{name}` needs a person, a plan gate or a tree of children"
            );
        }
    }

    #[test]
    fn f10_a_served_session_is_configured_before_it_is_served() {
        // The door takes a built config and nothing else, so what a client may
        // reach is decided by whoever starts the server rather than by the
        // client asking.
        let config = McpServerConfig::new(".", "runs.db")
            .with_policy(Policy::default())
            .with_server_name("io-harness tools");
        assert_eq!(config.server_name(), "io-harness tools");
        assert_eq!(config.server_version(), env!("CARGO_PKG_VERSION"));
    }
}
