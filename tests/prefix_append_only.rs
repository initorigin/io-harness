//! The prefix is append-only within a run (0.85.0).
//!
//! Prefix caching serves a request only up to the first byte that differs from
//! the request before it, so anything this crate rewrites *before* the newest
//! message throws away the cache from that point on for the rest of the run.
//! Through 0.84.0 six things in assembly did exactly that. These tests assert the
//! property directly — step N's rendered text is a byte prefix of step N+1's,
//! except across a fold — rather than asserting the absence of any one of them,
//! because the property is what the vendor actually charges on.
//!
//! The assertions are on `Assembled::text` and on recorded request bodies, not on
//! a counter: a counter can be right while the bytes move.

use io_harness::context::{
    assemble, Assembly, Collapse, Ladder, Ledger, ObsKind, Observation, Origin,
};
use io_harness::tools::Workspace;
use io_harness::{Policy, Store};

/// A workspace with an open read policy, and the policy beside it.
fn ws(dir: &std::path::Path) -> (Workspace, Policy) {
    let policy = Policy::default().layer("test").allow_read("*");
    (Workspace::with_policy(dir, policy.clone()), policy)
}

/// One assembly at `step`, over a ledger the call may extend.
async fn at_step(
    ledger: &mut Ledger,
    store: &Store,
    workspace: &Workspace,
    policy: &Policy,
    step: u32,
) -> String {
    assemble(
        ledger,
        24_000,
        &[],
        &[],
        Assembly {
            ws: Some(workspace),
            policy,
            store,
            run_id: 1,
            step,
            collapse: Collapse::default(),
            ladder: Ladder::default(),
        },
    )
    .await
    .unwrap()
    .text
}

// ------------------------------------------------------- F1: a re-read appends

/// F1 — a stale read is refreshed by *appending* the current contents at the
/// tail, and the entry that went stale renders as one stable stub from then on.
///
/// Through 0.84.0 the refresh was written in place with the assembling step's own
/// number baked into it (`re-read at step {step}`), so once a file had been read
/// and then written every later step rendered that observation differently. It
/// sat early in the transcript and it changed on every step, which made it the
/// worst of the six.
#[tokio::test]
async fn f1_a_stale_read_is_appended_at_the_tail_and_the_original_becomes_a_stable_stub() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "NEW-CONTENT\n").unwrap();
    let (workspace, policy) = ws(dir.path());
    let store = Store::memory().unwrap();

    let mut ledger = Ledger::new();
    ledger.push(Observation::new(
        2,
        ObsKind::Read,
        Some("a.rs".into()),
        "\n[read a.rs]\nOLD-CONTENT\n",
        Origin::File,
    ));
    ledger.push(Observation::new(
        4,
        ObsKind::Write,
        Some("a.rs".into()),
        "\n[wrote a.rs] (12 chars)\n",
        Origin::File,
    ));

    let five = at_step(&mut ledger, &store, &workspace, &policy, 5).await;
    assert!(
        five.contains("re-read at step 5") && five.contains("NEW-CONTENT"),
        "the refreshed contents must arrive as a new entry naming the step that read them, \
         got:\n{five}"
    );
    assert!(
        !five.contains("OLD-CONTENT"),
        "the stale contents must not be presented as current, got:\n{five}"
    );

    // The stale entry's own line, which is what the next three steps must repeat
    // byte for byte.
    let stub = five
        .lines()
        .find(|line| line.contains("[read a.rs] (elided:"))
        .unwrap_or_else(|| panic!("the stale read must render as a stub, got:\n{five}"))
        .to_string();
    assert!(
        stub.contains("invalidated by the write at step 4"),
        "the stub must still say why it went stale, got:\n{stub}"
    );

    let mut previous = five;
    for step in 6..=8 {
        let text = at_step(&mut ledger, &store, &workspace, &policy, step).await;
        assert!(
            text.contains(&stub),
            "step {step} must render the stale entry exactly as step 5 did.\nwanted: \
             {stub}\ngot:\n{text}"
        );
        assert!(
            text.contains("re-read at step 5") && !text.contains(&format!("re-read at step {step}")),
            "the refresh belongs to the step that made it, not to step {step}, got:\n{text}"
        );
        assert_eq!(
            text, previous,
            "nothing in the assembly may change between steps that observe nothing"
        );
        previous = text;
    }
}
