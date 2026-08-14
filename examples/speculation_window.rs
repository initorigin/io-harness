//! 0.54.0 — N5. What starting a read early is worth, measured rather than
//! asserted.
//!
//! There is exactly one number that decides whether this release helps a given
//! run: **the window** — how long the provider keeps streaming after a tool
//! call's arguments are complete. Everything the model says after its last tool
//! call is time the harness used to spend idle and can now spend reading. A model
//! that emits a bare tool call and stops has no window and gains nothing; a model
//! that narrates its plan around the call has a large one.
//!
//! This example makes both halves explicit against a scripted provider, because a
//! live provider's window is a property of the model and the day rather than of
//! this crate. Run it and read the numbers as *shape*, not as a benchmark:
//!
//! ```text
//! cargo run --example speculation_window
//! ```
//!
//! **No test asserts any of this.** A duration is a flake on a CI runner, and the
//! release's own criteria are all rendezvous-based for that reason. This is an
//! instrument, and its output belongs in `docs/MEASUREMENTS.md` with the machine
//! named.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Session, Store,
    TaskContract, ToolSpec,
};
use serde_json::json;

/// How long the model keeps talking after its tool call is complete. This is the
/// window; on a real model it is however many tokens of narration follow the last
/// call, at that model's tokens-per-second.
const TAIL: Duration = Duration::from_millis(400);

/// How long the read itself takes. A warm small file is well under a millisecond;
/// a large file, a cold page cache or a grep across a big repository is not.
const READ: Duration = Duration::from_millis(300);

/// One delta every this often, so the tail is a stream rather than one sleep.
const TICK: Duration = Duration::from_millis(20);

/// The sink a provider hands finished tool calls to.
type CallSink<'a> = &'a (dyn Fn(usize, &ToolCall) + Send + Sync);

struct Narrating {
    served: AtomicUsize,
    /// When the tool call was reported, and when the completion returned.
    reported_at: Mutex<Option<Instant>>,
    returned_at: Mutex<Option<Instant>>,
}

impl Narrating {
    fn new() -> Self {
        Self {
            served: AtomicUsize::new(0),
            reported_at: Mutex::new(None),
            returned_at: Mutex::new(None),
        }
    }

    async fn answer(
        &self,
        on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: Option<CallSink<'_>>,
    ) -> io_harness::Result<CompletionResponse> {
        let step = self.served.fetch_add(1, Ordering::SeqCst);
        if step > 0 {
            return Ok(CompletionResponse {
                text: Some("done".into()),
                ..Default::default()
            });
        }

        let call = ToolCall {
            name: "slow_read".into(),
            arguments: json!({}),
        };

        // The model says a little, then makes its call, then keeps talking.
        on_token("Let me look at that file. ");
        if let Some(sink) = on_call {
            sink(0, &call);
        }
        if self.reported_at.lock().unwrap().is_none() {
            *self.reported_at.lock().unwrap() = Some(Instant::now());
        }

        let started = Instant::now();
        while started.elapsed() < TAIL {
            tokio::time::sleep(TICK).await;
            on_token("and while that runs, here is what I am checking for. ");
        }

        *self.returned_at.lock().unwrap() = Some(Instant::now());
        Ok(CompletionResponse {
            text: Some("Let me look at that file.".into()),
            tool_calls: vec![call],
            ..Default::default()
        })
    }
}

impl Provider for Narrating {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.answer(&|_| {}, None).await
    }

    async fn complete_streaming(
        &self,
        _req: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.answer(on_token, None).await
    }

    async fn complete_streaming_calls(
        &self,
        _req: CompletionRequest,
        on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.answer(on_token, Some(on_call)).await
    }

    fn name(&self) -> &str {
        "narrating"
    }
}

/// A read-only tool that takes a realistic amount of time.
struct SlowRead;

impl Tool for SlowRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow_read".into(),
            description: "Reads something that takes a moment.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(READ).await;
            Ok("read".to_string())
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

/// Watches for the one event that says whether anything started early.
///
/// Streaming — and therefore speculation — is a property of the turn entry point:
/// only the `_observed` and `_steered` shapes stream, so a turn driven through
/// `turn_bounded` alone would measure the 0.53.0 path and report no saving for a
/// reason that has nothing to do with the feature.
#[derive(Default)]
struct Watcher {
    speculated: Mutex<Option<(usize, usize, usize)>>,
}

impl Observer for Watcher {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Speculated {
            started,
            used,
            discarded,
        } = &event.kind
        {
            *self.speculated.lock().unwrap() = Some((*started, *used, *discarded));
        }
        Flow::Continue
    }
}

async fn one_turn(cap: usize) -> (Duration, Option<Duration>, Option<(usize, usize, usize)>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Narrating::new();
    let contract = TaskContract::workspace("measure the window", dir.path())
        .with_tools(Toolbox::new().with(SlowRead))
        .with_max_steps(3)
        .with_max_parallel_reads(cap);
    let policy = Policy::default()
        .layer("measure")
        .allow_read("*")
        .allow_exec("*");
    let mut session = Session::open(&store, dir.path()).unwrap();
    let watcher = Watcher::default();

    let started = Instant::now();
    session
        .turn_bounded_observed(&contract, &provider, &store, &policy, &ApproveAll, &watcher)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    let window = match (
        *provider.reported_at.lock().unwrap(),
        *provider.returned_at.lock().unwrap(),
    ) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    };
    let counts = *watcher.speculated.lock().unwrap();
    (elapsed, window, counts)
}

#[tokio::main]
async fn main() {
    println!("io-harness 0.54.0 — what the speculation window is worth\n");
    println!("  tail after the tool call : {TAIL:?}  (the window)");
    println!("  the read itself          : {READ:?}");
    println!();

    // Warm the paths so the first measurement is not paying for the store's own
    // first-use cost.
    let _ = one_turn(10).await;

    let (with, window, counts) = one_turn(10).await;
    let (without, _, none) = one_turn(1).await;

    println!("  window measured          : {window:?}");
    println!("  turn, starting early     : {with:?}   speculated: {counts:?}");
    println!("  turn, cap of 1           : {without:?}   speculated: {none:?}");
    match without.checked_sub(with) {
        Some(saved) => println!("  saved                    : {saved:?}"),
        None => {
            println!("  saved                    : none — the window was shorter than the read")
        }
    }
    println!();
    println!("Read it as a shape, not a benchmark: what is saved is bounded above by");
    println!("min(window, read). A model that stops talking at its tool call has no");
    println!("window and this release does nothing for it.");
}
