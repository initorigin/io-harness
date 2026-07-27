//! A live run that has to *see* something, and has to leave a commit behind.
//!
//! ```text
//! cargo run --example media_git_live --features "media barcode"
//! ```
//!
//! (`barcode` is only here because it brings the `image` crate, which this
//! example uses to write the PNG fixture. Nothing in the image capability needs
//! it — the harness never decodes an image, it encodes and forwards one.)
//!
//! # What this is evidence for, and what it is not
//!
//! The workspace is a real git repository holding one PNG of a solid colour that
//! nothing else in the run names. The agent is told to look at it, then record
//! the colour in a file and commit that file.
//!
//! **The image half.** The colour is chosen here and appears nowhere in the
//! prompt, the goal, the file names or the criterion. A model that never received
//! the image cannot name it — it can guess one of the common colours, which is
//! why the fixture is magenta rather than red or blue. So "the file says magenta"
//! is evidence the bytes reached the model, not evidence the model is agreeable.
//!
//! **The git half.** The criterion is on a *receipt* file the agent is told to
//! write only after committing, so the run cannot stop before the commit — the
//! first draft of this example gated on `COLOUR.md` itself and the run ended,
//! satisfied, one step before the interesting part. The receipt is still only a
//! file, so the gate is not the evidence either: after the run this example asks
//! git itself whether a new commit exists, whether it carries the file, and who
//! it is attributed to. A run that wrote both files and committed neither passes
//! the criterion and fails here, printed as exactly that.
//!
//! **What it is not.** One provider, one model, one image, one repository. It
//! exercises the two halves end to end under a permissive policy; the boundaries
//! are covered by tests/media.rs and tests/git.rs, which is where the refusals
//! and their controls live.

use std::path::Path;
use std::process::Command;

use io_harness::{
    approve::ApproveAll, policy::Policy, run_with, OpenRouter, Provider, Store, TaskContract,
    Verification,
};

/// The one fact the model can only get from the image. Deliberately not a colour
/// a guess would land on first.
const COLOUR: [u8; 3] = [255, 0, 255];
const COLOUR_NAME: &str = "magenta";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A real PNG, written by a real encoder.
    let img = image::RgbImage::from_pixel(64, 64, image::Rgb(COLOUR));
    img.save(root.join("swatch.png")).unwrap();
    std::fs::write(root.join("README.md"), "the swatch is in swatch.png\n").unwrap();

    git(root, &["init", "--initial-branch=main"]);
    git(root, &["add", "README.md", "swatch.png"]);
    git(root, &["commit", "-m", "fixture"]);
    let before = git(root, &["rev-parse", "HEAD"]);

    let contract = TaskContract::workspace(
        "Look at swatch.png. Write the name of the colour you see, as a single \
         lowercase word, into COLOUR.md. Then stage COLOUR.md and commit it, with \
         a message describing what you did. Only once the commit exists, write \
         DONE.md containing the single word: committed",
        root,
        // Deliberately NOT on COLOUR.md: that file exists one step before the
        // commit, and gating on it stops the run there.
        Verification::WorkspaceFileContains {
            file: "DONE.md".into(),
            needle: "committed".into(),
        },
    )
    .with_max_steps(12)
    .with_commit_identity("io-harness agent", "agent@io-harness.invalid");

    // Outside the workspace: a store inside it is an untracked file the agent
    // would see in `git_status` and might helpfully stage.
    let store_dir = tempfile::tempdir().unwrap();
    let store = Store::open(store_dir.path().join("trace.db"))?;
    let provider = OpenRouter::from_env()?;

    println!("--- running against {} ---", provider.name());
    let result = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await?;
    println!("outcome: {:?}", result.outcome);

    // ---- the image half -------------------------------------------------
    let wrote = std::fs::read_to_string(root.join("COLOUR.md")).unwrap_or_default();
    let saw_it = wrote.to_lowercase().contains(COLOUR_NAME);
    println!(
        "\nimage: COLOUR.md = {:?} -> {}",
        wrote.trim(),
        if saw_it {
            "NAMED THE COLOUR — the bytes reached the model"
        } else {
            "did NOT name the colour"
        }
    );

    // ---- the git half ---------------------------------------------------
    let after = git(root, &["rev-parse", "HEAD"]);
    let committed = after != before;
    let files = git(root, &["show", "--name-only", "--format=", "HEAD"]);
    let status = git(root, &["status", "--porcelain"]);
    println!(
        "git: HEAD {} -> {}\n     new commit: {}\n     message: {:?}\n     files in it: {:?}\n     working tree: {}",
        &before[..7.min(before.len())],
        &after[..7.min(after.len())],
        committed,
        git(root, &["log", "-1", "--format=%s"]),
        files,
        if status.is_empty() {
            "clean".to_string()
        } else {
            format!("still dirty:\n{status}")
        }
    );
    println!(
        "     author: {}",
        git(root, &["log", "-1", "--format=%an <%ae>"])
    );

    let ok = saw_it && committed && files.contains("COLOUR.md");
    println!(
        "\n=== {} ===",
        if ok {
            "BOTH HALVES PROVEN: the model saw the image, and the run ended as a commit"
        } else {
            "NOT PROVEN — see above for which half failed"
        }
    );
    Ok(())
}
