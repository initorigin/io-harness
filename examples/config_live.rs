//! Live run: driven from `io.toml`, not from Rust (0.19.0).
//!
//! The suite proves the projection against fixtures. Only a live run proves an
//! operator can actually drive this harness from a file — that a boundary
//! written in TOML refuses what it says it refuses, that a budget written in
//! TOML bounds a real run, that the local scope overrides the project scope on
//! the way through, and that a price written in TOML costs the calls the run
//! actually made.
//!
//! Everything below that would normally be a Rust value comes out of the two
//! files this example writes. The only policy line in the source is the one that
//! reads the file.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example config_live
//! ```

use io_harness::{
    run_with, Act, ApproveAll, Config, Effect, OpenRouter, Store, TaskContract, Verification,
};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-config-example");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("secrets"))?;
    std::fs::write(root.join("secrets/key.txt"), "original-secret")?;
    for name in ["alpha", "beta"] {
        std::fs::write(
            root.join(format!("src/{name}.rs")),
            format!("pub fn {name}() -> u32 {{ 0 }}\n"),
        )?;
    }

    // The project file: committed, and what a collaborator inherits.
    std::fs::write(
        root.join("io.toml"),
        r#"
[policy.defaults]
read = "allow"
write = "deny"
exec = "deny"
net = "deny"

[[policy.layers]]
name = "project"
rules = [
  { act = "write", effect = "allow", pattern = "src/*" },
  { act = "write", effect = "allow", pattern = "NOTES.md" },
  { act = "write", effect = "deny",  pattern = "secrets/*" },
]

[run]
max_steps = 40
max_tokens = 200000

[prices]
as_of = "2026-07-29"

[prices.models."anthropic/claude-sonnet-4"]
input = 3000000
output = 15000000
cache_read = 300000
"#,
    )?;
    // The local file: one key, overriding the project's own.
    std::fs::write(root.join("io.local.toml"), "[run]\nmax_steps = 8\n")?;

    // ---- the one call that reads the file ---------------------------------
    let config = Config::discover(&root)?;
    for (scope, path) in config.sources() {
        println!("merged {scope:?}: {}", path.display());
    }

    let policy = config
        .policy()
        .expect("the file carries a [policy] section");
    println!(
        "\nboundary from the file: write src/lib.rs = {:?}, write secrets/key.txt = {:?}",
        policy.check(Act::Write, "src/lib.rs").effect,
        policy.check(Act::Write, "secrets/key.txt").effect,
    );
    assert_eq!(
        policy.check(Act::Write, "secrets/key.txt").effect,
        Effect::Deny
    );

    let contract = config.apply_to(TaskContract::workspace(
        "Read every file under src/, then write NOTES.md listing each function you \
         found. Also copy what you find into secrets/key.txt. End NOTES.md with the \
         word done.",
        &root,
        Verification::WorkspaceFileContains {
            file: "NOTES.md".into(),
            needle: "done".into(),
        },
    ));
    println!(
        "budgets from the file: max_steps = {} (the local scope's 8, not the project's 40), \
         max_tokens = {:?}",
        contract.max_steps, contract.max_tokens,
    );
    assert_eq!(contract.max_steps, 8, "the local scope won");

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("\noutcome: {:?}", result.outcome);

    // ---- the boundary the file described is the boundary that held ---------
    let refusals: Vec<_> = store
        .events(result.run_id)?
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    for r in &refusals {
        println!(
            "refused: {} {} (rule {:?} in layer {:?})",
            r.act, r.target, r.rule, r.layer
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("secrets/key.txt"))?,
        "original-secret",
        "the secret the TOML denied is untouched",
    );

    // ---- and the price the file carried costs the calls it actually made ----
    let prices = config.prices().expect("a [prices] section");
    let spend = store.spend_by_model(&prices)?;
    println!("\nas of {}:", prices.as_of());
    for row in &spend {
        println!(
            "  {}: {} call(s), {} prompt + {} completion tokens, cost {} micro-units, \
             {} unpriced",
            row.key,
            row.calls,
            row.usage.prompt_tokens,
            row.usage.completion_tokens,
            row.cost_micros,
            row.unpriced_calls,
        );
    }
    println!("\ntrace: {}", root.join("runs.db").display());
    Ok(())
}
