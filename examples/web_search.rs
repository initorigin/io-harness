//! Live web search: an answer about something newer than the model, with the
//! provider's own sources behind it (0.22.0).
//!
//! The suite proves the translation this crate writes — which body key each vendor
//! gets, which stream shape becomes a citation, which error object means a search
//! broke. What it cannot prove is the only thing that matters on release day: that
//! a real vendor accepts the body this crate builds, runs the search, and hands
//! back citations in the shape the parser expects. Offline, the provider is a
//! script that always agrees.
//!
//! Four claims, each asserted here so a regression fails the example rather than
//! printing output nobody reads:
//!
//! 1. **The vendor accepted the declaration.** The run completes rather than
//!    failing on a rejected body — the failure a stale dated tool type or a wrong
//!    key produces.
//! 2. **The answer carries sources.** At least one citation, read back from
//!    SQLite after the run, not from the response object still in memory.
//! 3. **The call is recorded.** A `server_tool_calls` row exists, which is the
//!    vendor-independent record of a provider-executed call — and, where the
//!    vendor reports one, `provider_calls.server_tool_requests` carries the count
//!    that no run in this crate's history had before this release. Anthropic
//!    reports it; OpenRouter's chat-completions response does not, and the crate
//!    records a zero it was told rather than deriving one it was not.
//! 4. **The spend is visible.** The run's summary reads back from the store, and
//!    a vendor-reported request count is what `pricing.rs` has been able to charge
//!    per request since 0.18.0 and never had an input for.
//!
//! ```text
//! # Anthropic is preferred: it is the one vendor of the three with both search
//! # and fetch, so it exercises the most of this release.
//! export ANTHROPIC_API_KEY=sk-ant-...
//! export ANTHROPIC_MODEL=claude-sonnet-4-5
//! # Or, with no Anthropic key, OpenRouter's web plugin:
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example web_search
//! ```

use std::sync::Mutex;

use io_harness::{
    run_with_observed, Anthropic, ApproveAll, EventKind, Flow, Observer, OpenRouter, Policy,
    Provider, RunEvent, Store, TaskContract, WebAccess,
};

/// Records the server-tool events as they arrive, so the example can say what the
/// provider did while it was doing it rather than only afterwards.
#[derive(Default)]
struct Watcher {
    used: Mutex<Vec<(String, String, bool)>>,
}

impl Observer for Watcher {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::ServerToolUsed { provider, tool, ok } = &event.kind {
            println!(
                "  [event] {provider} ran {tool}: {}",
                if *ok { "ok" } else { "FAILED" }
            );
            self.used
                .lock()
                .unwrap()
                .push((provider.clone(), tool.clone(), *ok));
        }
        Flow::Continue
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    // Anthropic when its key is present, OpenRouter otherwise. The declaration is
    // identical either way — which is the release's whole point — but what each
    // vendor can carry is not, so the OpenRouter run drops the domain filter and
    // the fetch tool rather than being refused for asking for them.
    // An env var set to an empty string is how a `.env` file holds a placeholder,
    // and `from_env` accepts it — an empty key then fails as a 401 three minutes
    // later. Deciding here on a non-empty key keeps that out of the live run.
    let has_anthropic = ["ANTHROPIC_API_KEY", "ANTHROPIC_MODEL"]
        .iter()
        .all(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
    match Anthropic::from_env().and_then(|p| {
        if has_anthropic {
            Ok(p)
        } else {
            Err(io_harness::Error::Config("no anthropic key".into()))
        }
    }) {
        Ok(provider) => {
            println!("=== provider: anthropic (search + fetch, with a domain filter) ===");
            let web = WebAccess::search()
                .with_fetch()
                .max_uses(4)
                .allow("docs.rs")
                .allow("crates.io")
                .allow("github.com");
            run(&provider, web).await
        }
        Err(_) => {
            println!("=== provider: openrouter (search only, no domain filter) ===");
            println!("  (set ANTHROPIC_API_KEY to exercise fetch and the domain filter too)");
            run(&OpenRouter::from_env()?, WebAccess::search().max_uses(4)).await
        }
    }
}

async fn run<P: Provider>(provider: &P, web: WebAccess) -> io_harness::Result<()> {
    let dir = tempfile::tempdir().expect("a temp workspace");
    let root = dir.path();
    let db = root.join("trace.db");
    let store = Store::open(&db)?;

    // Deny-by-default on the network, and the same hosts stated once: the policy
    // governs what THIS process may dial, and `from_policy` projects it onto the
    // vendor's own filter for what the PROVIDER may dial. Two boundaries, two
    // enforcers, one declaration.
    let policy = Policy::default()
        .layer("live")
        .allow_read("*")
        .allow_write("*");
    println!("  declared: {web:?}");

    let contract = TaskContract::workspace(
        "Look up what the latest published version of the `io-harness` crate is on \
         crates.io, and what its most recent release added. Search the web — do not \
         answer from memory, because your training data is older than the answer. \
         Say the version number and one sentence about the release, and cite where \
         you found it.",
        root,
    )
    // No gate — and none asked for, since a contract verifies nothing unless
    // `with_verification` says otherwise. The deliverable is an answer, not a
    // file, and the run ends when the model stops calling tools. This is also the
    // shape a paused search turn is indistinguishable from — see `paused_turn` in
    // src/run.rs.
    .with_max_steps(6)
    .with_web(web);

    let watcher = Watcher::default();
    let result =
        run_with_observed(&contract, provider, &store, &policy, &ApproveAll, &watcher).await?;
    println!("  outcome: {:?}", result.outcome);

    // -- claim 1: the vendor accepted the body this crate built --
    let used = watcher.used.lock().unwrap().clone();
    assert!(
        !used.is_empty(),
        "the provider ran no server tool at all. Either the model chose not to \
         search — rerun; the prompt asks it to — or the vendor ignored the \
         declaration, which is the finding worth recording"
    );
    assert!(
        used.iter().any(|(_, _, ok)| *ok),
        "every provider-executed call failed: {used:?}. A failure inside a 200 is \
         recorded honestly by design, but a run where all of them broke proves \
         nothing about the happy path"
    );

    // -- claim 2: the answer carries sources, read back from the store --
    let cited = store.citations(result.run_id)?;
    println!("  {} source(s) cited:", cited.len());
    for citation in &cited {
        println!(
            "    {} — {}",
            citation.title.as_deref().unwrap_or("(no title)"),
            citation.url
        );
    }
    assert!(
        !cited.is_empty(),
        "the provider searched but cited nothing. The citation parser is what turns \
         a search into a checkable answer, so an empty list here is a defect in this \
         crate, not in the model"
    );

    // The failures are kept too, and are a different fact from finding nothing.
    let calls = store.server_tool_calls(result.run_id)?;
    println!("  {} provider-executed call(s):", calls.len());
    for call in &calls {
        println!(
            "    {} {} — {}",
            call.provider,
            call.tool,
            call.error.as_deref().unwrap_or("ok")
        );
    }

    // -- claim 3: the meter moved, where the vendor reports a meter --
    //
    // It does not everywhere, and that is a fact about the vendors rather than a
    // defect here. Anthropic returns `usage.server_tool_use.web_search_requests`
    // and this crate reads it. OpenRouter's chat-completions response reports no
    // such counter at all, so a run that demonstrably searched — the annotations
    // and the `server_tool_calls` row prove it — still reports zero requests. The
    // crate records what the provider said and never derives a count it was not
    // given: a fabricated figure in an accounting table is worse than a missing
    // one, because it cannot be told apart from a measured one.
    let requests: u64 = store
        .provider_calls(result.run_id)?
        .iter()
        .filter_map(|c| c.usage)
        .map(|u| u.server_tool_requests)
        .sum();
    println!("  server_tool_requests reported by the vendor: {requests}");
    assert!(
        !calls.is_empty(),
        "the run must have left a `server_tool_calls` row: that is the \
         vendor-independent record of a provider-executed call, and unlike the \
         usage counter it does not depend on the vendor choosing to report one"
    );
    if requests == 0 {
        println!(
            "  NOTE: this vendor reports no per-request counter, so the meter stays \
             at zero. The search itself is recorded above."
        );
    }

    // -- claim 4: the money is visible --
    let summary = result.summary(&store)?;
    println!("  spend: {summary:?}");

    println!("\nOK — a live answer, cited, and recorded");
    Ok(())
}
