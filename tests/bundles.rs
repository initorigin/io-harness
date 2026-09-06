//! The capability bundles this repository ships (0.81.0).
//!
//! Until this release there were none. `docs/guide/plugins.md` described the
//! layout and every bundle in the suite was built inline in a temporary directory,
//! so the documented shape was checked against nothing and the flagship claim for
//! the bundle system — that a real protocol can ship as one, adding no crate
//! surface — had no example to point at.
//!
//! `bundles/long-horizon/` is the first. These tests load it from its real path,
//! so a bundle broken by a later change fails a gate rather than a person: a
//! renamed manifest key, a skills directory that moved, or frontmatter the parser
//! stopped reading are all silent to every other test in this repository.

use std::path::PathBuf;

use io_harness::config::Scope;
use io_harness::plugin::Plugins;
use io_harness::Skills;

/// The repository's bundle directory, from the manifest rather than from the
/// current working directory — a test binary's cwd is not the crate root under
/// every runner.
fn bundles() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bundles")
}

/// O4 — the long-horizon bundle loads from the path it ships at.
#[test]
fn the_long_horizon_bundle_loads_from_its_shipped_path() {
    let dir = bundles().join("long-horizon");
    assert!(
        dir.join("plugin.toml").exists(),
        "the bundle is missing its manifest at {}",
        dir.display()
    );

    let plugin = Plugins::inspect(Scope::User, &dir).expect("the shipped bundle must load");
    assert_eq!(plugin.id(), "long-horizon");
    assert!(
        plugin.description().is_some_and(|d| !d.is_empty()),
        "a bundle with no description is a line in a catalogue that says nothing"
    );

    // Skills and nothing else. The protocol is composed of primitives this crate
    // already ships, so a bundle declaring an agent, a server or a policy would be
    // claiming a contribution it does not make — and would be crate surface
    // arriving by the back door, which is the thing shipping it as a bundle
    // avoids.
    assert_eq!(plugin.contributions(), vec!["skills"]);
}

/// O4 — its skills are discoverable, which is the whole of what it contributes.
///
/// `contributions()` reads the manifest; this reads the disk. A `skills` key
/// pointing at a directory that does not exist satisfies the first and fails the
/// second, which is exactly the drift a shipped bundle invites.
#[test]
fn the_long_horizon_bundle_publishes_the_skills_it_declares() {
    let dir = bundles().join("long-horizon");
    let plugin = Plugins::inspect(Scope::User, &dir).unwrap();
    let skills_dir = plugin
        .skills_dir()
        .expect("the bundle declares a skills directory");
    assert!(
        skills_dir.is_dir(),
        "the declared skills directory does not exist: {}",
        skills_dir.display()
    );

    let skills = Skills::discover(&skills_dir).expect("the skills directory must read");
    let catalog = skills.catalog();
    assert!(
        skills.len() >= 4,
        "the protocol has four parts and an overview: {catalog}"
    );

    // Every skill has to reach the catalogue with a description, because the
    // catalogue line is the only thing a model sees until it reads the body — a
    // skill with an empty description is a skill nothing will ever open.
    for name in [
        "long-horizon",
        "long-horizon-init",
        "git-baseline",
        "feature-list",
        "progress-log",
    ] {
        assert!(
            catalog.contains(name),
            "{name} is missing from the catalogue:\n{catalog}"
        );
    }
}
