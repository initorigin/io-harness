//! Watching a run while it happens.
//!
//! Until 0.12.0 the crate had no live surface at all: no observer, no callback,
//! no channel. Everything a run did was durable in the rusqlite trace and
//! readable *afterwards*, so an application that wanted to show progress had to
//! open the SQLite file with a second connection and poll it — against a schema
//! the crate never promised, having first configured the file itself because
//! [`Store::open`](crate::Store::open) set no pragmas. For a run designed to
//! last 24 hours that is not a detail; it is the difference between an agent an
//! operator can watch and one they cannot.
//!
//! Register an [`Observer`] and the run calls it as things happen. The events
//! report the same facts the trace records — that is asserted, not assumed —
//! so the two surfaces cannot drift into disagreeing.
//!
//! ```no_run
//! use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
//!
//! struct Printer;
//!
//! impl Observer for Printer {
//!     fn event(&self, event: &RunEvent) -> Flow {
//!         if let EventKind::Step { decision, tokens, .. } = &event.kind {
//!             println!("step {} — {decision} ({tokens} tokens)", event.step);
//!         }
//!         Flow::Continue
//!     }
//! }
//! ```
//!
//! # Which surface is authoritative
//!
//! The trace is. An event is a notification that something happened, not the
//! record of it: the durable row is what a resume, an audit and an evaluation
//! all read. If an event and the trace ever disagree, the trace is right and the
//! event is a bug.
//!
//! # Ordering and timing
//!
//! Events arrive in the order the run produces them, synchronously, on the task
//! driving the run. [`Observer::event`] is therefore on the run's critical path:
//! a slow observer slows the run down. Do the minimum — push to a queue, send on
//! a channel — and do the work elsewhere.
//!
//! One event does not mean one committed row. A step is committed inside a
//! transaction and its [`EventKind::Step`] is emitted after that transaction
//! succeeds, but a retry emits an [`EventKind::Retry`] having written a row of
//! its own under the *same* step number, and a sub-agent step that pauses
//! because one of its children deferred is deliberately left uncommitted so a
//! resume replays it. Count events if you want to show activity; read the store
//! if you need to know what is durable.
//!
//! # Serialisation
//!
//! Every event serialises, because the process driving a run is often not the
//! process showing it to a person. A host can forward an event as JSON to a user
//! interface written in another language without hand-writing a mapping. The
//! wire shape is flat and tagged:
//!
//! ```json
//! {"run_id": 1, "step": 3, "depth": 0, "event": "step",
//!  "decision": "wrote src/a.rs", "tool_call": "write_file:{…}",
//!  "tokens": 412, "changed": true}
//! ```
//!
//! # Failure
//!
//! [`Observer::event`] returns no `Result`. An observer is a spectator, and a
//! run must not fail because something watching it did. If your observer can
//! fail, absorb the failure and report it out of band.
//!
//! A *panic* is different: `event` is called on the run's own task, so a
//! panicking observer takes the run's future with it and leaves the run row
//! `running`. Do not panic in an observer.

use serde::{Deserialize, Serialize};

/// Whether the run should keep going.
///
/// Returned from every [`Observer::event`] call. This is the only way to stop a
/// run from outside it: before 0.12.0 a caller's only option was to drop the
/// run's future, which abandoned it mid-step and left `runs.status` as
/// `running` forever, so nothing could tell it from a process that had crashed.
///
/// Which makes an observer that returns anything other than
/// [`Flow::Continue`] a control, not a spectator — the place to enforce a
/// ceiling the [`TaskContract`](crate::TaskContract) budgets cannot express:
///
/// ```
/// use std::sync::atomic::{AtomicU64, Ordering};
///
/// use io_harness::{EventKind, Flow, Observer, RunEvent};
///
/// /// Stops a run once it has spent more than it was meant to, wherever in a
/// /// tree that spend happened.
/// struct SpendCap {
///     limit: u64,
///     spent: AtomicU64,
/// }
///
/// impl Observer for SpendCap {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Step { tokens, .. } = event.kind {
///             if self.spent.fetch_add(tokens, Ordering::Relaxed) + tokens > self.limit {
///                 // Honoured at the next step boundary, not here: the points
///                 // in between are not safe to stop at — a tool call is
///                 // mid-flight, a file may be half-written. The run finishes
///                 // the step, records `cancelled`, and stays resumable.
///                 return Flow::Cancel;
///             }
///         }
///         Flow::Continue
///     }
/// }
///
/// // `Continue` is the default, so a watcher that only ever looks can
/// // `Flow::default()` and never think about this type again.
/// assert_eq!(Flow::default(), Flow::Continue);
/// assert!(Flow::Cancel.is_cancel());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// Keep running. What a watcher returns.
    #[default]
    Continue,
    /// Stop the run.
    ///
    /// Honoured at the next step boundary rather than immediately, because the
    /// points in between are not safe to stop at: a tool call is mid-flight, a
    /// file may be half-written, a child may be running. The run finishes the
    /// step it is on, records `cancelled`, and returns
    /// [`RunOutcome::Cancelled`](crate::RunOutcome::Cancelled). It stays
    /// resumable — cancelling is not abandoning.
    Cancel,
}

impl Flow {
    /// Whether this flow asks the run to stop.
    pub fn is_cancel(self) -> bool {
        self == Flow::Cancel
    }
}

/// One thing that happened during a run.
///
/// The common fields are here rather than repeated on every variant, so a
/// consumer can route on `run_id`/`depth` without matching the payload first.
/// That is what makes a tree legible while it is running: `depth` is how deep
/// the agent is, and `run_id` is that agent's *own* run id, never the root's.
///
/// ```
/// use io_harness::{EventKind, Flow, Observer, RunEvent};
///
/// /// Prints a tree as it happens, indented by depth.
/// struct Trace;
///
/// impl Observer for Trace {
///     fn event(&self, event: &RunEvent) -> Flow {
///         let indent = "  ".repeat(event.depth as usize);
///         match &event.kind {
///             EventKind::Spawned { child_run_id, goal } => {
///                 println!("{indent}run {} spawned {child_run_id}: {goal}", event.run_id);
///             }
///             EventKind::Step { decision, tokens, .. } => {
///                 println!("{indent}step {} — {decision} ({tokens} tokens)", event.step);
///             }
///             _ => {}
///         }
///         Flow::Continue
///     }
/// }
/// ```
///
/// Every event serialises flat and tagged, because the process driving a run is
/// often not the process showing it to a person — so a host can forward one to
/// a user interface written in another language without hand-writing a mapping:
///
/// ```
/// use io_harness::{EventKind, RunEvent};
///
/// let event = RunEvent::new(7, 3, EventKind::Stalled);
/// let json = serde_json::to_value(&event).unwrap();
///
/// // One flat object: the payload is not nested under a `kind` key, and the
/// // variant is a string tag a `switch` in any language can read.
/// assert_eq!(json["run_id"], 7);
/// assert_eq!(json["step"], 3);
/// assert_eq!(json["event"], "stalled");
/// assert!(json.get("kind").is_none());
/// ```
///
/// `step` is `0` for anything that happens before the first step — authorizing
/// network access to the provider, for instance — so a consumer numbering
/// steps from an event stream should not assume the first one it sees is 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// The run this belongs to. In a tree, the *agent's* own run id, not the
    /// root's — a child has its own.
    pub run_id: i64,
    /// The step it happened on. `0` for anything that happens before the first
    /// step, such as authorizing network access to the provider.
    pub step: u32,
    /// Depth in the agent tree: `0` for a single run or a tree's root.
    pub depth: u32,
    /// What happened.
    #[serde(flatten)]
    pub kind: EventKind,
}

impl RunEvent {
    /// An event at depth 0, the common case.
    pub fn new(run_id: i64, step: u32, kind: EventKind) -> Self {
        Self {
            run_id,
            step,
            depth: 0,
            kind,
        }
    }

    /// An event from a specific depth in an agent tree.
    pub fn at_depth(run_id: i64, step: u32, depth: u32, kind: EventKind) -> Self {
        Self {
            run_id,
            step,
            depth,
            kind,
        }
    }
}

/// What happened, and the detail that goes with it.
///
/// One enum with one `Observer` method, rather than a method per kind: adding a
/// kind is then a new variant a consumer can ignore with a `_` arm, instead of a
/// new trait method every implementer inherits. It is also what makes a single
/// serialised wire shape possible.
///
/// Every variant reports something the rusqlite trace already records. This
/// release added no new facts about a run — it added a way to see them while the
/// run is still going.
///
/// The example matches the handful an operator watching an unattended run
/// actually needs — the ones that mean the run is blocked on a person, is
/// going in circles, or is slow for a reason — and lets the rest fall through:
///
/// ```
/// use io_harness::{EventKind, Flow, Observer, RunEvent};
///
/// struct Alerts;
///
/// impl Observer for Alerts {
///     fn event(&self, event: &RunEvent) -> Flow {
///         match &event.kind {
///             // The run has stopped and will not restart on its own.
///             EventKind::ApprovalRequested { act, target } => {
///                 eprintln!("waiting on a human: {act} {target}");
///             }
///             // Told once already, still repeating itself. Terminal.
///             EventKind::Stalled => eprintln!("stalled — this run is over"),
///             // Not a failure: the run continues, and this says why it looks
///             // stuck and for how much longer.
///             EventKind::Retry { kind, attempt, delay_ms } => {
///                 eprintln!("{kind} on attempt {attempt}, waiting {delay_ms}ms");
///             }
///             // The policy stopped something. The action did not happen, and
///             // the model was told so it can adapt.
///             EventKind::Refused { act, target, rule, .. } => {
///                 eprintln!("refused {act} {target} by rule {rule:?}");
///             }
///             // One enum and one `Observer` method, so a variant added in a
///             // later release is a `_` arm here rather than a trait method
///             // every implementer suddenly has to write.
///             _ => {}
///         }
///         Flow::Continue
///     }
/// }
/// ```
///
/// One event is not one committed row. A [`Retry`](EventKind::Retry) writes a
/// row under the *same* step number as the [`Step`](EventKind::Step) that
/// follows it, and a sub-agent step that pauses on a deferred child is left
/// uncommitted on purpose so a resume replays it. Count events to show
/// activity; read the store to know what is durable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    /// The run began. Emitted once, before the first step.
    Started {
        /// What the run was asked to do.
        goal: String,
        /// The provider that will serve it, by name.
        provider: String,
    },
    /// A step completed and was committed to the store.
    ///
    /// Emitted from one place for all three loops (single-file, workspace and
    /// sub-agent), which before 0.12.0 each had their own copy of the boundary
    /// and their own differently-named log line.
    Step {
        /// What the agent decided, as recorded in `steps.decision`.
        decision: String,
        /// The tool calls it made, as recorded in `steps.tool_call`.
        tool_call: String,
        /// Tokens this step spent, or `0` if the provider reported no usage.
        tokens: u64,
        /// Whether this step changed anything in the workspace. The signal stall
        /// detection rests on, added in 0.11.0.
        changed: bool,
    },
    /// A tool was invoked, before its result is known.
    ToolCall {
        /// The tool's name.
        name: String,
        /// What it was pointed at — a path, a host, a server.
        target: String,
    },
    /// The policy refused an action. It did not happen.
    Refused {
        /// `"read"`, `"write"`, `"exec"` or `"net"`.
        act: String,
        /// What was refused.
        target: String,
        /// The rule that refused it, when a rule did.
        rule: Option<String>,
        /// The policy layer that rule came from.
        layer: Option<String>,
    },
    /// A sensitive action stopped to ask a human. The run is waiting.
    ApprovalRequested {
        /// The act being asked about.
        act: String,
        /// What it would affect.
        target: String,
    },
    /// A human answered.
    ApprovalDecided {
        /// The act that was asked about.
        act: String,
        /// What it would affect.
        target: String,
        /// `"approve"`, `"deny"` or `"defer"`.
        decision: String,
    },
    /// A step's tokens were drawn against a tree's shared ceiling.
    SpendDraw {
        /// Tokens drawn.
        tokens: u64,
        /// What the tree has left afterwards.
        remaining: Option<u64>,
    },
    /// A provider call failed in a way worth trying again, and will be.
    Retry {
        /// The failure's kind, as classified in 0.11.0.
        kind: String,
        /// Which attempt this is.
        attempt: u32,
        /// How long the run will wait first.
        delay_ms: u64,
    },
    /// A `Fallback` provider fell over, and this is who answered instead.
    FellBackTo {
        /// The provider that served the step.
        provider: String,
    },
    /// The agent has changed nothing for a while and has been told once to try
    /// something else. The run continues.
    Replan {
        /// How many steps without progress triggered it.
        window: u32,
    },
    /// The agent had already been told and is still going in circles. Terminal.
    Stalled,
    /// A sub-agent was started.
    Spawned {
        /// The child's own run id.
        child_run_id: i64,
        /// What the child was asked to do.
        goal: String,
    },
    /// A spawn was refused by containment rather than performed.
    SpawnRefused {
        /// Which cap refused it — agents, depth or concurrency.
        cap: String,
    },
    /// The agent wrote something to durable cross-run memory.
    MemoryWrote {
        /// The note's key.
        key: String,
    },
    /// A sandbox was created, ran something, hit a cap, or was destroyed.
    Sandbox {
        /// `"create"`, `"exec"`, `"cap_hit"`, `"destroy"` or
        /// `"gate_phase_failed"`.
        kind: String,
        /// The backend that isolated it, when one is known.
        backend: Option<String>,
    },
    /// An MCP server was reached, or one of its tools was called.
    Mcp {
        /// The server, by the name it was configured under.
        server: String,
        /// The tool called, for a call.
        tool: Option<String>,
        /// Whether the call succeeded.
        ok: Option<bool>,
        /// How long it took.
        millis: Option<u64>,
    },
    /// The run ended. Emitted once, last.
    Finished {
        /// The outcome string as written to `runs.outcome`.
        outcome: String,
        /// Steps completed.
        steps: u32,
        /// Tokens spent across the run.
        tokens: u64,
    },
}

/// Watches a run as it happens.
///
/// Shaped after [`Approver`](crate::Approver), the crate's other
/// inversion-of-control point: `Send + Sync` with `&self` methods, held as
/// `&dyn Observer`. `&self` rather than `&mut self` is not a style choice — a
/// tree runs up to `max_concurrent` children as concurrent futures on one task,
/// and a `&mut self` observer could not be shared between them. Keep whatever
/// state you need behind a `Mutex`, an atomic, or a channel.
///
/// This is what an embedding application registers instead of opening the
/// SQLite file with a second connection and polling it — against a schema the
/// crate does not promise, having first configured the file itself. On a run
/// designed to last 24 hours that is the difference between an agent an
/// operator can watch and one they cannot.
///
/// ```no_run
/// use std::sync::mpsc::{channel, Sender};
///
/// use io_harness::{run_observed, EventKind, Flow, Observer, OpenRouter, RunEvent,
///                  Store, TaskContract, Verification};
///
/// /// Hands the run to whatever is showing it to a person.
/// struct Forward(Sender<RunEvent>);
///
/// impl Observer for Forward {
///     fn event(&self, event: &RunEvent) -> Flow {
///         // `event` is called synchronously on the run's own task, so this
///         // is the run's critical path: send and return. Anything slower
///         // slows the run down, and a panic here takes the run's future
///         // with it and leaves the run row `running`.
///         let _ = self.0.send(event.clone());
///         Flow::Continue
///     }
/// }
///
/// # async fn demo() -> io_harness::Result<()> {
/// let (tx, rx) = channel::<RunEvent>();
///
/// // Drained elsewhere, while the run is still going — the whole point of
/// // the surface. A `for` over `rx` on this task would deadlock instead.
/// std::thread::spawn(move || {
///     for event in rx {
///         match &event.kind {
///             EventKind::Step { decision, .. } => println!("step {}: {decision}", event.step),
///             EventKind::Finished { outcome, steps, tokens } => {
///                 println!("{outcome} after {steps} steps, {tokens} tokens");
///             }
///             _ => {}
///         }
///     }
/// });
///
/// let contract = TaskContract::new(
///     "add a hello function returning 42",
///     "src/hello.rs",
///     Verification::FileContains("fn hello".into()),
/// );
/// let result = run_observed(
///     &contract,
///     &OpenRouter::from_env()?,
///     &Store::memory()?,
///     &Forward(tx),
/// )
/// .await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
///
/// The events report the same facts the trace records — asserted, not assumed
/// — but the trace is the authoritative one. An event is a notification that
/// something happened, not the record of it; if the two ever disagree, the
/// trace is right and the event is a bug.
pub trait Observer: Send + Sync {
    /// Called once per event, in order, on the run's own task.
    ///
    /// Return [`Flow::Cancel`] to stop the run at its next step boundary.
    /// Watchers return [`Flow::Continue`].
    ///
    /// This returns no `Result` on purpose: see the [module docs](self#failure).
    fn event(&self, event: &RunEvent) -> Flow;
}

/// Watches nothing. The default when a caller registers no observer.
///
/// Exists so the run has one code path rather than `Option<&dyn Observer>`
/// threaded through every call site, and so "no observer" costs a call to an
/// empty function that optimises away rather than a branch per event.
///
/// Which means these two runs are the same run:
///
/// ```no_run
/// use io_harness::{run, run_observed, Ignore, OpenRouter, Store, TaskContract,
///                  Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// # let contract = TaskContract::new(
/// #     "add a hello function", "src/hello.rs",
/// #     Verification::FileContains("fn hello".into()));
/// # let provider = OpenRouter::from_env()?;
/// # let store = Store::memory()?;
/// let a = run(&contract, &provider, &store).await?;
/// let b = run_observed(&contract, &provider, &store, &Ignore).await?;
/// # let _ = (a, b);
/// # Ok(())
/// # }
/// ```
///
/// So reach for it when a function of yours takes a `&dyn Observer` and one
/// caller has nothing to watch with — pass `&Ignore` rather than making the
/// parameter an `Option`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ignore;

impl Observer for Ignore {
    fn event(&self, _event: &RunEvent) -> Flow {
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_shape_is_flat_and_tagged() {
        let e = RunEvent::new(
            7,
            3,
            EventKind::Step {
                decision: "wrote src/a.rs".into(),
                tool_call: "write_file:{}".into(),
                tokens: 412,
                changed: true,
            },
        );
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        // Flat: a consumer in another language reads one object, not a nested
        // one. Pinned because this is a wire contract, not an internal detail.
        assert_eq!(v["run_id"], 7);
        assert_eq!(v["step"], 3);
        assert_eq!(v["depth"], 0);
        assert_eq!(v["event"], "step");
        assert_eq!(v["decision"], "wrote src/a.rs");
        assert_eq!(v["tokens"], 412);
        assert_eq!(v["changed"], true);
        assert!(v.get("kind").is_none(), "the payload must not be nested");
    }

    #[test]
    fn a_unit_variant_still_carries_its_tag() {
        let v = serde_json::to_value(RunEvent::new(1, 9, EventKind::Stalled)).unwrap();
        assert_eq!(v["event"], "stalled");
        assert_eq!(v["step"], 9);
    }

    #[test]
    fn every_variant_round_trips() {
        for kind in every_kind() {
            let e = RunEvent::at_depth(1, 2, 3, kind);
            let json = serde_json::to_string(&e).unwrap();
            let back: RunEvent = serde_json::from_str(&json).unwrap_or_else(|err| {
                panic!("{json} did not round-trip: {err}");
            });
            assert_eq!(e, back, "round-trip changed the event: {json}");
        }
    }

    #[test]
    fn each_variant_has_a_distinct_tag() {
        let mut tags = Vec::new();
        for kind in every_kind() {
            let v = serde_json::to_value(RunEvent::new(1, 1, kind)).unwrap();
            tags.push(v["event"].as_str().unwrap().to_string());
        }
        let mut unique = tags.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            tags.len(),
            unique.len(),
            "two variants share a wire tag, so a consumer cannot tell them apart: {tags:?}"
        );
    }

    #[test]
    fn ignore_never_cancels() {
        assert_eq!(
            Ignore.event(&RunEvent::new(1, 1, EventKind::Stalled)),
            Flow::Continue
        );
        assert!(!Flow::Continue.is_cancel());
        assert!(Flow::Cancel.is_cancel());
        assert_eq!(Flow::default(), Flow::Continue);
    }

    /// One of every variant. Kept exhaustive by the `match` below: adding a
    /// variant without adding it here fails to compile, so the round-trip and
    /// distinct-tag tests cannot silently stop covering the enum.
    fn every_kind() -> Vec<EventKind> {
        let all = vec![
            EventKind::Started {
                goal: "g".into(),
                provider: "p".into(),
            },
            EventKind::Step {
                decision: "d".into(),
                tool_call: "t".into(),
                tokens: 1,
                changed: true,
            },
            EventKind::ToolCall {
                name: "n".into(),
                target: "t".into(),
            },
            EventKind::Refused {
                act: "read".into(),
                target: "t".into(),
                rule: Some("r".into()),
                layer: None,
            },
            EventKind::ApprovalRequested {
                act: "write".into(),
                target: "t".into(),
            },
            EventKind::ApprovalDecided {
                act: "write".into(),
                target: "t".into(),
                decision: "approve".into(),
            },
            EventKind::SpendDraw {
                tokens: 1,
                remaining: Some(2),
            },
            EventKind::Retry {
                kind: "rate_limited".into(),
                attempt: 1,
                delay_ms: 40,
            },
            EventKind::FellBackTo {
                provider: "p".into(),
            },
            EventKind::Replan { window: 3 },
            EventKind::Stalled,
            EventKind::Spawned {
                child_run_id: 2,
                goal: "g".into(),
            },
            EventKind::SpawnRefused {
                cap: "depth".into(),
            },
            EventKind::MemoryWrote { key: "k".into() },
            EventKind::Sandbox {
                kind: "create".into(),
                backend: Some("b".into()),
            },
            EventKind::Mcp {
                server: "s".into(),
                tool: Some("t".into()),
                ok: Some(true),
                millis: Some(5),
            },
            EventKind::Finished {
                outcome: "success".into(),
                steps: 4,
                tokens: 9,
            },
        ];
        // Exhaustiveness guard. Never executed for its result; it exists so the
        // compiler refuses a new variant that `all` does not mention.
        for k in &all {
            match k {
                EventKind::Started { .. }
                | EventKind::Step { .. }
                | EventKind::ToolCall { .. }
                | EventKind::Refused { .. }
                | EventKind::ApprovalRequested { .. }
                | EventKind::ApprovalDecided { .. }
                | EventKind::SpendDraw { .. }
                | EventKind::Retry { .. }
                | EventKind::FellBackTo { .. }
                | EventKind::Replan { .. }
                | EventKind::Stalled
                | EventKind::Spawned { .. }
                | EventKind::SpawnRefused { .. }
                | EventKind::MemoryWrote { .. }
                | EventKind::Sandbox { .. }
                | EventKind::Mcp { .. }
                | EventKind::Finished { .. } => {}
            }
        }
        all
    }
}
