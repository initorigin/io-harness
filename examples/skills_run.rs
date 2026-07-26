//! A live run whose required output shape is stated only in a skill: the agent
//! is told what skills exist, and loads the body of the one it judges relevant
//! through the built-in `read_skill` tool.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example skills_run
//! ```
//!
//! The skills directory is written into a temporary directory at startup rather
//! than committed, so the example is self-contained and shows both accepted
//! layouts side by side:
//!
//! ```text
//! skills/
//!   release-notes.md       -> skill "release-notes", named by its frontmatter
//!   commit-style/
//!     SKILL.md             -> skill "commit-style", named by its directory
//! ```
//!
//! Only the *names and descriptions* reach the system prompt — two lines, not
//! two files. The house rule the task depends on ("Filed under: ledger") lives
//! in one body and nowhere else, so a note that carries it is a note written
//! after that body was asked for. The other skill is about commit messages and
//! is irrelevant here on purpose: which one applies is the model's judgement,
//! not a match the harness performs.
//!
//! Nothing in a skill executes. It is prose the model reads, and every action it
//! then takes — including the read of the skill file itself — goes through the
//! same policy as any other.

use io_harness::{
    run_with, ApproveAll, Policy, RunOutcome, Skills, Store, TaskContract, Verification,
};

/// The line stated only in the release-notes body. Neither the goal nor the
/// verification criterion mentions it, so it can only reach `NOTES.md` by way of
/// the skill.
const HOUSE_RULE: &str = "Filed under: ledger";

/// Layout 1: a flat `<name>.md` with `name:`/`description:` frontmatter, which
/// is what a directory written for another agent tool usually already looks
/// like.
const RELEASE_NOTES: &str = "\
---
name: release-notes
description: How this project writes a release note. Read before writing one.
---

A release note is exactly three parts, in this order:

1. A heading line: `## io-harness <version>`.
2. One paragraph, at most three sentences, in the past tense.
3. A final line, on its own: `Filed under: ledger`.

No bullet lists, no headings other than the first line.
";

/// Layout 2: `<name>/SKILL.md` with no frontmatter at all. The name comes from
/// the directory and the description from the first prose line, so a plain
/// markdown file needs no ceremony to be a skill.
const COMMIT_STYLE: &str = "\
# How this project writes a commit message

Subject in the imperative mood, under 60 characters, no trailing period.
The body explains why, never what — the diff already says what.
";

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;

    // The skills directory sits outside the workspace, as it normally would: it
    // is the operator's instructions, not the agent's material.
    let skills = tempfile::tempdir()?;
    std::fs::write(skills.path().join("release-notes.md"), RELEASE_NOTES)?;
    std::fs::create_dir_all(skills.path().join("commit-style"))?;
    std::fs::write(skills.path().join("commit-style/SKILL.md"), COMMIT_STYLE)?;

    // Exactly what the agent will be told it can ask for — two lines, no bodies.
    println!("catalogue in the system prompt:");
    println!("{}", Skills::discover(skills.path())?.catalog());

    let contract = TaskContract::workspace(
        "Write the release note for io-harness 0.9.0 into NOTES.md with \
         write_file. The house style for a release note is not in this \
         repository — it is a skill. Read the skill that applies before you \
         write, and follow it exactly.",
        dir.path(),
        // The version is a shape the prompt already carries. The house-rule
        // check after the run is what proves a body was loaded.
        Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "0.9.0".into(),
        },
    )
    .with_skills(skills.path())
    .with_max_steps(6);

    // Workspace paths arrive relative; a skill body arrives as the absolute path
    // of its file, because the skills directory is outside the workspace root.
    // `*` spans separators, so one rule covers both — narrow it to the
    // workspace and `read_skill` is refused while the catalogue still shows,
    // which is the shape an operator uses to offer a skill set and withhold one.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*");

    let store = Store::open(dir.path().join("runs.db"))?;
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

    println!("\noutcome: {:?}", result.outcome);
    println!("\nskill loads the trace recorded:");
    for step in store.steps(result.run_id)? {
        if step.tool_call.contains("read_skill") {
            println!("  step {}: {}", step.step, step.tool_call);
        }
    }
    for e in store
        .events(result.run_id)?
        .iter()
        .filter(|e| e.kind == "refusal")
    {
        println!(
            "  refused: {} {} (rule {})",
            e.act,
            e.target,
            e.rule.clone().unwrap_or_else(|| "-".into())
        );
    }

    if matches!(result.outcome, RunOutcome::Success { .. }) {
        let written = std::fs::read_to_string(dir.path().join("NOTES.md"))?;
        println!("\nNOTES.md:\n{written}");
        println!(
            "  carries {HOUSE_RULE:?}, which is stated only in the skill body: {}",
            written.contains(HOUSE_RULE)
        );
    } else {
        eprintln!("note: the run did not reach success; see the trace above");
    }
    Ok(())
}
