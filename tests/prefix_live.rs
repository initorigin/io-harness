//! What a real vendor actually served from cache (0.85.0).
//!
//! Every other case in this release drives a fixture written in this repository,
//! so every one of them agrees with this crate's own idea of what a vendor does.
//! This is the only arm that would notice the idea being wrong — a cached count
//! reported on a surface the crate does not read, a routing key a vendor ignores,
//! a body field that changes the prefix it was meant to preserve.
//!
//! `#[ignore]`d, and needs `FIREWORKS_API_KEY` and `FIREWORKS_MODEL`:
//!
//! ```sh
//! set -a; source .env; set +a
//! cargo test --test prefix_live -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use io_harness::provider::Compatible;
use io_harness::{
    ApproveAll, EventKind, Flow, Observer, Policy, RunEvent, Session, Store, TaskContract,
    Verification,
};

/// One step's accounting: the step, its uncached prompt, its cache read, and the
/// share in permille.
type Share = (u32, u64, u64, u64);

/// Every step's cached share, in step order.
#[derive(Default)]
struct Shares(Arc<Mutex<Vec<Share>>>);

impl Observer for Shares {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::StepUsage {
            fresh_prompt_tokens,
            cache_read_tokens,
            cached_fraction,
            ..
        } = &event.kind
        {
            self.0.lock().unwrap().push((
                event.step,
                *fresh_prompt_tokens,
                *cache_read_tokens,
                *cached_fraction,
            ));
        }
        Flow::Continue
    }
}

fn from_env() -> Option<(String, String)> {
    let key = std::env::var("FIREWORKS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let model = std::env::var("FIREWORKS_MODEL")
        .ok()
        .filter(|m| !m.is_empty())?;
    Some((key, model))
}

/// A workspace with enough to read that `files` steps have something to do.
fn workspace_of(files: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..files {
        std::fs::write(
            dir.path().join(format!("mod_{i}.rs")),
            format!(
                "//! Module {i}.\n\npub fn helper_{i}(input: &str) -> String {{\n    \
                 format!(\"{{input}}-{i}\")\n}}\n\n{}",
                "// padding so a read is worth caching\n".repeat(40)
            ),
        )
        .unwrap();
    }
    dir
}

// ------------------------------------------------ the operational criterion

/// A ten-step run against Fireworks reports cached tokens above 90% of prompt
/// tokens from step 3 on.
///
/// Step 1 has no prefix to hit and step 2 is the first that could, so the claim
/// starts at step 3 — and it is a claim about a *replica-local* cache, which is
/// why the run is a session turn: without `session_key` on every request there is
/// no reason for two of them to land on the same machine.
///
/// The number it measured is printed, and it is what goes into the release
/// record. A measurement that only ever appears as `assert!` has nowhere to be
/// read from afterwards.
#[tokio::test]
#[ignore = "live provider — needs FIREWORKS_API_KEY, see the module docs"]
async fn the_live_cached_fraction_holds_from_step_three() {
    let Some((key, model)) = from_env() else {
        panic!("FIREWORKS_API_KEY and FIREWORKS_MODEL must be set; see .env.example");
    };
    let dir = workspace_of(10);
    let store = Store::open(dir.path().join("runs.db")).unwrap();
    let provider = Compatible::fireworks(key, &model);
    let shares = Shares::default();

    let contract = TaskContract::workspace(
        "Read every mod_*.rs file in this directory, one at a time, and after each \
         one say in a sentence what its helper does. Read them all before you stop.",
        dir.path(),
    )
    .with_max_steps(10)
    // Nothing to satisfy, so the run spends its whole step budget and the case
    // measures ten steps rather than however many the model felt like taking.
    .with_verification(Verification::WorkspaceFileContains {
        file: "unreachable.txt".into(),
        needle: "never".into(),
    });

    // A session turn and not a bare run: `session_key` rides on a request only
    // when there is a session to name, and without it two requests of one
    // conversation have no reason to land on the same replica — which is the whole
    // property being measured.
    let mut session = Session::open(&store, dir.path()).unwrap();
    let _ = session
        .turn_bounded_observed(
            &contract,
            &provider,
            &store,
            &Policy::default().layer("live").allow_read("*"),
            &ApproveAll,
            &shares,
        )
        .await;

    let seen = shares.0.lock().unwrap().clone();
    println!("model: {model}");
    for (step, fresh, cached, permille) in &seen {
        println!(
            "step {step}: {cached} cached / {} prompt = {}.{}%",
            fresh + cached,
            permille / 10,
            permille % 10
        );
    }
    assert!(
        seen.len() >= 3,
        "the run must reach step 3 for the claim to mean anything, got {} step(s)",
        seen.len()
    );
    let from_three: Vec<&Share> = seen.iter().filter(|(step, ..)| *step >= 3).collect();
    let worst = from_three
        .iter()
        .map(|(.., permille)| *permille)
        .min()
        .expect("at least one step from step 3 on");
    assert!(
        worst >= 900,
        "the contract's floor is 90% from step 3 on; the worst step served {}.{}%",
        worst / 10,
        worst % 10
    );
}

/// N5 — the cached share over a longer session, printed and asserted on nothing.
///
/// The ten-step case above measures the floor the contract names. This measures
/// the shape, because the shape is what the number means: the share a vendor can
/// serve is bounded by `(prompt − newest message) / prompt`, so it rises as a
/// session's history grows past the size of one step's observation. A run that
/// reads a 450-token file per step climbs from 90% to 94% over ten steps and
/// keeps climbing; a session with a long system prompt starts higher.
///
/// It is a measurement, so it prints and asserts nothing — a share asserted
/// against a threshold is a claim about a vendor's fleet on the day it ran.
#[tokio::test]
#[ignore = "measurement, not a gate — see docs/MEASUREMENTS.md"]
async fn n5_the_cached_share_over_a_longer_session() {
    let Some((key, model)) = from_env() else {
        panic!("FIREWORKS_API_KEY and FIREWORKS_MODEL must be set; see .env.example");
    };
    let dir = workspace_of(20);
    let store = Store::open(dir.path().join("runs.db")).unwrap();
    let provider = Compatible::fireworks(key, &model);
    let shares = Shares::default();

    let contract = TaskContract::workspace(
        "Read every mod_*.rs file in this directory, one at a time, and after each \
         one say in a sentence what its helper does. Read them all before you stop.",
        dir.path(),
    )
    .with_max_steps(20)
    .with_verification(Verification::WorkspaceFileContains {
        file: "unreachable.txt".into(),
        needle: "never".into(),
    });

    let mut session = Session::open(&store, dir.path()).unwrap();
    let _ = session
        .turn_bounded_observed(
            &contract,
            &provider,
            &store,
            &Policy::default().layer("live").allow_read("*"),
            &ApproveAll,
            &shares,
        )
        .await;

    let seen = shares.0.lock().unwrap().clone();
    println!("model: {model}");
    for (step, fresh, cached, permille) in &seen {
        let prompt = fresh + cached;
        println!(
            "step {step}: {cached} cached / {prompt} prompt = {}.{}% (uncached {fresh})",
            permille / 10,
            permille % 10
        );
    }
}
