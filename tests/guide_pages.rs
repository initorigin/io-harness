//! Nothing was lost when the README was split.
//!
//! The 0.15.0 README was 1072 lines and carried the whole of every capability.
//! Splitting it into a landing page plus a page per capability is the release's
//! headline, and its quiet failure mode is that a section simply does not arrive
//! — the result still reads well, because what is missing is not there to notice.
//!
//! So the expectation is written down explicitly rather than counted. A test
//! that asserted "twelve pages exist" would pass with the right number of wrong
//! pages. This one names each capability and, for the six that had one, the
//! honest limits block that has to have travelled with it.
//!
//! Those blocks are the most trustworthy paragraphs in the repository. They are
//! why the rest is credible, and they are exactly what a rewrite is tempted to
//! smooth away.

use std::fs;
use std::path::{Path, PathBuf};

/// Every capability that had a section in the 0.15.0 README, or that had none
/// and should have — observability was never documented for a reader at all.
///
/// `limits_block` is true where the 0.15.0 section carried a "stated plainly"
/// block that must survive into the page.
const GUIDES: &[(&str, bool)] = &[
    ("permissions.md", false),
    ("verification.md", false),
    // 0.17.0. Both carry a limits block because both document capabilities whose
    // bound is the interesting part: `exec` runs outside the sandbox with the
    // embedding program's privileges, and the toolchain table is a default that
    // will be wrong for someone on day one.
    ("command-execution.md", true),
    ("language-support.md", true),
    ("composition.md", false),
    ("sandbox.md", true),
    ("durable-runs.md", false),
    ("mcp-and-network.md", true),
    ("tools-and-skills.md", true),
    ("context-and-memory.md", true),
    ("resilience.md", true),
    ("observability.md", false),
    // 0.18.0. It carries a limits block because the interesting part of an
    // accounting figure is its provenance: a token count is the provider's
    // report, a cost is only as right as the operator's own price table, and a
    // run older than the release has no rows at all rather than zeros.
    // 0.19.0. It carries a limits block because the interesting part of a
    // config file is what it is *not*: not a boundary against the agent, not
    // read by the run loop, and not loaded by anything the caller did not call.
    // 0.20.0. It carries a limits block because the interesting part of a session
    // is what a conversation does *not* make true: steering is text and not
    // authorization, a streamed delta is provisional until the completion returns,
    // and one session driven by two processes at once is unsupported.
    ("sessions.md", true),
    // 0.21.0. It carries a limits block because every one of the four primitives is
    // defined by what it cannot do: a plan is never enforced and not gated, an answer is
    // not authorization, a definition can only narrow and never grant, and a template
    // sets nothing.
    ("agency.md", true),
    // 0.22.0. It carries a limits block because the capability is defined by where
    // it is *not* enforced: the provider dials the URL, so `Act::Net` never sees it
    // and the domain filter is the vendor's; a citation is what the provider
    // returned rather than a checked source; and a paused turn resumes as a fresh
    // request that may repeat a search already paid for.
    ("web.md", true),
    ("configuration.md", true),
    ("accounting.md", true),
    ("documents.md", true),
    ("images-and-git.md", false),
    // 0.28.0. It carries a limits block because a hook is defined by what it costs
    // and what it is not: it blocks the run loop it is called from, it is refused in
    // the project scope without that making a cloned repository safe, and it grants
    // nothing — it cannot approve or deny an action, and its output is discarded.
    ("hooks.md", true),
    // 0.29.0. It carries a limits block because twenty-one vendors behind one type
    // is a claim that has to state what it is not: one wire and no per-vendor
    // rewriting, a vendor catalogue that returns identifiers and no prices, a
    // reference price that is the aggregator's rather than the vendor's — and vLLM
    // and SGLang emitting no tool calls at all unless the server was started for
    // them, which is the failure that reports nothing and errors nowhere.
    ("providers.md", true),
    // 0.35.0. It carries a limits block because a packaging format is defined by
    // what it does not promise: nothing verifies that a directory is what its
    // author published, nothing fetches or installs one, a bundle contributes data
    // and never code, and a bundle that fails to load is dropped quietly enough
    // that an operator watching neither report channel can run without deny rules
    // they believe are installed.
    ("plugins.md", true),
    // 0.53.0. It carries a limits block because a browser is defined by what it
    // is not allowed to do: one platform is unsupported by design, subresources
    // are not individually decided, nothing is ever downloaded, and a selector
    // that matches nothing must fail rather than read as a click that happened.
    ("browser.md", true),
];

fn guide_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/guide")
}

/// A page is missing, or is present and lost the limits block it had to carry.
#[derive(Debug, PartialEq, Eq)]
enum Loss {
    MissingPage(String),
    MissingLimits(String),
    Empty(String),
}

/// The check, as a pure function over "does this page exist and what does it
/// say", so the negative controls can drive it without a filesystem.
fn losses(pages: &[(&str, bool, Option<String>)]) -> Vec<Loss> {
    let mut out = Vec::new();
    for (name, needs_limits, body) in pages {
        let Some(body) = body else {
            out.push(Loss::MissingPage((*name).to_string()));
            continue;
        };
        if body.trim().len() < 200 {
            out.push(Loss::Empty((*name).to_string()));
            continue;
        }
        if *needs_limits && !states_limits_plainly(body) {
            out.push(Loss::MissingLimits((*name).to_string()));
        }
    }
    out
}

/// The 0.15.0 blocks were headed "The limit, stated plainly", "The limits,
/// stated plainly", "The boundary, stated plainly", and — inline, in bold —
/// "Windows, stated plainly". The common thread is the phrase, not the heading
/// level, so match the phrase.
fn states_limits_plainly(body: &str) -> bool {
    body.contains("stated plainly")
}

#[test]
fn every_capability_has_a_guide_page() {
    let dir = guide_dir();
    let pages: Vec<(&str, bool, Option<String>)> = GUIDES
        .iter()
        .map(|(name, needs)| (*name, *needs, fs::read_to_string(dir.join(name)).ok()))
        .collect();

    let found = losses(&pages);
    assert!(
        found.is_empty(),
        "the README split lost something. A capability with no page is depth \
         deleted rather than moved, and a page that dropped its limits block is \
         the honest half removed from the honest half:\n{found:#?}"
    );
}

#[test]
fn no_guide_page_is_orphaned_from_the_index() {
    let index =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/CAPABILITIES.md"))
            .expect("docs/CAPABILITIES.md");

    let missing: Vec<&str> = GUIDES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !index.contains(name))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/CAPABILITIES.md is the way in for a reader who arrived at docs/ \
         rather than at the README. These pages exist and nothing links them: \
         {missing:?}"
    );
}

#[test]
fn the_guide_directory_holds_nothing_unlisted() {
    // The other direction: a page written and never added to GUIDES would sit
    // outside the check entirely, which is how a checker quietly stops covering
    // the thing it was written for.
    let dir = guide_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        panic!("docs/guide/ does not exist");
    };

    let unlisted: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .filter(|name| !GUIDES.iter().any(|(known, _)| known == name))
        .collect();

    assert!(
        unlisted.is_empty(),
        "guide pages exist that this test does not know about, so nothing \
         checks them: {unlisted:?}. Add them to GUIDES."
    );
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------

fn body(text: &str) -> Option<String> {
    Some(text.to_string())
}

fn long_body(extra: &str) -> Option<String> {
    Some(format!(
        "# A capability\n\nA paragraph long enough to be a real page rather \
         than a stub, repeated so the length floor is cleared and the test is \
         measuring what it means to measure rather than a word count. {extra}"
    ))
}

#[test]
fn control_a_missing_page_is_reported() {
    let found = losses(&[("permissions.md", false, None)]);
    assert_eq!(found, vec![Loss::MissingPage("permissions.md".into())]);
}

#[test]
fn control_a_dropped_limits_block_is_reported() {
    // The page is present, substantial, well written — and quietly missing the
    // one paragraph that made the capability honest. This is the failure the
    // whole test exists for, and it is invisible to a reader.
    let found = losses(&[("sandbox.md", true, long_body("Everything works nicely."))]);
    assert_eq!(found, vec![Loss::MissingLimits("sandbox.md".into())]);
}

#[test]
fn control_a_stub_is_reported() {
    let found = losses(&[("resilience.md", true, body("# Resilience\n\nTODO.\n"))]);
    assert_eq!(found, vec![Loss::Empty("resilience.md".into())]);
}

#[test]
fn control_a_complete_page_is_not_reported() {
    // The other half: the checker must not flag everything. A page that is
    // present, substantial, and keeps its limits block reports nothing.
    let found = losses(&[(
        "sandbox.md",
        true,
        long_body("**Windows, stated plainly.** The Job Object is not implemented."),
    )]);
    assert!(found.is_empty(), "expected no loss, got {found:#?}");
}
