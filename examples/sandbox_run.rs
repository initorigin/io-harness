//! A live run showing the 0.6.0 execution sandbox end to end: model-produced
//! code is compiled *inside* the sandbox by the verification gate, a resource
//! cap kills a runaway, outbound network is denied, and every workdir is torn
//! down so nothing is left behind.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example sandbox_run
//! ```
//!
//! On macOS the native `sandbox-exec` backend runs; on another OS the strongest
//! backend available there is selected, falling back to the portable floor.

use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{
    run, select, RunOutcome, SandboxConfig, SandboxLimits, Store, TaskContract, Verification,
};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("runs.db"))?;

    // 1) A real task. The verification gate compiles the model's code INSIDE the
    //    sandbox by default — no rustc ever runs on the host directly.
    let contract = TaskContract::new(
        "Write a Rust library file with a public function `hello` returning the u32 42.",
        dir.path().join("hello.rs").to_str().unwrap(),
        Verification::CompilesRust,
    )
    .with_max_steps(6);

    let result = run(&contract, &provider, &store).await?;
    println!("outcome: {:?}", result.outcome);

    println!("\nsandbox trace (where the code actually ran):");
    for e in store.sandbox_events(result.run_id)? {
        println!(
            "  {:>8} backend={:<18} {}",
            e.kind,
            e.backend.as_deref().unwrap_or("-"),
            e.detail.as_deref().unwrap_or("")
        );
    }

    let backend = select(&SandboxConfig::new()).backend();
    println!("\nselected backend: {}", backend.as_str());

    // 2) A resource cap kills a runaway instead of hanging.
    let cfg = SandboxConfig {
        limits: SandboxLimits {
            max_cpu_secs: Some(1),
            max_wall_secs: Some(20),
            ..Default::default()
        },
        ..Default::default()
    };
    let busy: Vec<String> = ["sh", "-c", "while :; do :; done"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let capped = select(&cfg)
        .run(RunSpec {
            argv: &busy,
            workdir: dir.path(),
            limits: &cfg.limits,
            allow_network: false,
        })
        .await?;
    println!(
        "\ncap demo: cap_hit = {:?} (a runaway was killed, not left hanging)",
        capped.cap_hit
    );

    // 3) Network is denied by default — enforced by the sandbox, not the prompt.
    let curl: Vec<String> = ["curl", "-s", "-m", "5", "https://example.com"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let net = select(&SandboxConfig::new())
        .run(RunSpec {
            argv: &curl,
            workdir: dir.path(),
            limits: &SandboxLimits::default(),
            allow_network: false,
        })
        .await?;
    println!(
        "network demo: outbound allowed = {} (default-deny)",
        net.success()
    );

    if matches!(result.outcome, RunOutcome::Success { .. }) {
        println!(
            "\nverified — the model's code compiled inside the sandbox, and every workdir is gone."
        );
    }
    Ok(())
}
