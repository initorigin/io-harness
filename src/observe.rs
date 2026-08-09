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

use std::fmt;
use std::sync::Mutex;

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
#[non_exhaustive]
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
        /// Which cap refused it — agents, depth or budget. Never concurrency:
        /// crossing [`Containment::max_concurrent_agents`](crate::Containment)
        /// queues the child instead of refusing it, and reports
        /// [`Fleet`](Self::Fleet).
        cap: String,
    },
    /// One tier of the agent tree changed shape (0.32.0): a child queued for a
    /// concurrency slot, was admitted to one, or finished and gave one back.
    ///
    /// This is what lets an application show a fleet *draining* rather than a
    /// number that stopped moving. It is per tier because a single tree-wide
    /// count cannot tell an operator whether the fan-out at depth two is stuck
    /// behind the one at depth one.
    ///
    /// A resumed tree emits one of these per non-empty tier before its provider
    /// is authorised or called, carrying the backlog read back out of the store —
    /// so a restart reports the queue it inherited rather than starting from
    /// zero and spiking.
    Fleet {
        /// The nesting level being counted. The root is 0, so a fleet of children
        /// spawned by the root is tier 1.
        tier: u32,
        /// Agents at this tier holding a slot and running.
        working: u32,
        /// Agents at this tier waiting for one. Nothing about them is started and
        /// nothing about them is charged.
        queued: u32,
        /// Agents at this tier that have finished and released their slot.
        done: u32,
    },
    /// The agent wrote something to durable cross-run memory.
    MemoryWrote {
        /// The note's key.
        key: String,
    },
    /// The agent wrote down its plan (0.21.0).
    ///
    /// Carries the items rather than a count, so a UI renders the plan from the
    /// event it just received instead of querying the store on every write. The
    /// store holds the same list — [`Store::todos`](crate::Store::todos) — and is
    /// the authority for a consumer that joined late.
    ///
    /// A plan is the agent's stated intent and nothing more. Nothing verifies it and
    /// no outcome depends on it, so an item that says `Done` is a claim, not a fact.
    TodoWrote {
        /// The whole plan as it now stands, in the order the agent wrote it. A
        /// write replaces the list, so this is never a delta.
        items: Vec<crate::state::TodoItem>,
    },
    /// The agent asked the operator what they actually wanted (0.21.0).
    ///
    /// Not an approval request. [`ApprovalRequested`](EventKind::ApprovalRequested)
    /// asks whether an act is permitted; this asks what was meant, and its answer
    /// authorizes nothing.
    QuestionAsked {
        /// What the agent asked.
        question: String,
        /// Options the agent offered, if it offered any. A UI renders these as
        /// choices; an answer is not obliged to be one of them.
        choices: Vec<String>,
    },
    /// The question was answered and the run went on (0.21.0).
    ///
    /// Emitted whether a `Responder` in this process answered or a human did after a
    /// pause; `by` says which, because "the machine decided" and "a person decided"
    /// are different facts about a run.
    QuestionAnswered {
        /// The answer, as the model will read it.
        answer: String,
        /// `"responder"` for an in-process answer, `"human"` for one that arrived
        /// through [`resume_with_answer`](crate::resume_with_answer) after a pause.
        by: String,
    },
    /// The agent proposed a plan and has done nothing yet (0.31.0).
    ///
    /// Not [`TodoWrote`](EventKind::TodoWrote), and the difference is the whole
    /// point: that one is a plan the agent is executing while an operator watches,
    /// this one is a plan the run will not act on until an answer comes back. At
    /// the moment this is emitted the workspace has not been written to.
    PlanProposed {
        /// The plan's row id, and what
        /// [`resume_with_plan_decision`](crate::resume_with_plan_decision) takes if
        /// the run pauses on it.
        plan_id: i64,
        /// The steps, in the order the agent intends them.
        steps: Vec<crate::approve::PlanStep>,
    },
    /// The plan was decided and the run acted on that decision (0.31.0).
    ///
    /// `verdict` is `"approve"`, `"revise"` or `"cancel"`. A `"revise"` leaves the
    /// run in its planning phase, still writing nothing, so this event is not
    /// necessarily the end of the negotiation — [`Store::plans`](crate::Store::plans)
    /// is the whole of it.
    PlanDecided {
        /// The plan this decided.
        plan_id: i64,
        /// `"approve"`, `"revise"` or `"cancel"`.
        verdict: String,
        /// `"gate"` for a [`PlanGate`](crate::PlanGate) in this process, `"human"`
        /// for a decision that arrived through a resume after a pause.
        by: String,
    },
    /// The thinking the model produced before answering this step (0.31.0).
    ///
    /// The **only** place it is visible. It is deliberately not written to the
    /// observation ledger and therefore never appears in the prompt assembled for
    /// the next turn — a vendor charges for thinking once as output, and a harness
    /// that folded it into the next request would be charged for it again as input
    /// every turn for the rest of the run. It is not stored either;
    /// [`Usage::reasoning_tokens`](crate::Usage::reasoning_tokens) is the durable
    /// record of what it cost.
    ///
    /// A provider that returns no thinking emits nothing, so an absent event means
    /// "the model did not think", never "the model thought nothing".
    Reasoning {
        /// What the model thought, as the provider returned it.
        text: String,
        /// The tokens the provider billed for it, or 0 when it did not say.
        tokens: u64,
    },
    /// The provider ran a web search or fetch for the model (0.22.0).
    ///
    /// Emitted once per call the provider reported, wherever the answer came from
    /// — a search it ran itself, a page it fetched — and emitted for the failures
    /// too. `ok: false` is a search that broke *inside* an otherwise successful
    /// response, which is a different fact from a search that found nothing and is
    /// why this variant carries a flag rather than only a name.
    ///
    /// Nothing in this process dialled the URL: the provider did. See
    /// [`WebAccess`](crate::WebAccess) for what the declaration does and does not
    /// enforce.
    ServerToolUsed {
        /// The provider that ran it, as [`Provider::name`](crate::Provider::name)
        /// reports it.
        provider: String,
        /// The vendor's own name for the tool — `web_search`, `web_fetch`.
        tool: String,
        /// Whether the provider reported it as having worked.
        ok: bool,
    },
    /// A chunk of assistant text arrived from the provider, while the model was
    /// still producing the rest of it.
    ///
    /// Emitted only on a [`Session`](crate::Session) turn given an observer: a
    /// one-shot [`run_with_observed`](crate::run_with_observed) never produces
    /// one, so adding this variant changes no existing run's event stream. The
    /// deltas of one step concatenate to that step's final assistant text, in
    /// order.
    ///
    /// **Provisional.** A delta is what the model has said so far, not a decision
    /// it has made: the turn may still fall over to another provider, be retried,
    /// or be interrupted, and text already emitted is not withdrawn. Render it;
    /// do not act on it. What is settled is the committed step —
    /// [`EventKind::Step`].
    Token {
        /// The chunk, exactly as the provider sent it. Not trimmed, not
        /// re-segmented: concatenation is the property that matters.
        text: String,
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
    /// A long-running process was started and registered as a handle (0.25.0).
    ///
    /// The first of the five handle events this release adds. They are additive and
    /// break no consumer: 0.24.0 made this enum `#[non_exhaustive]`, and this is the
    /// first release in which that is true — before it, a new variant was a breaking
    /// change for everyone who matched exhaustively, and a handle's lifecycle stayed
    /// invisible on the channel for exactly that reason. It was never missing from
    /// the store. It was missing from the one place an operator was already looking,
    /// and reading it meant opening the SQLite file behind the crate's back, which is
    /// the thing [`Observer`] exists to make unnecessary.
    ///
    /// A handle outlives the step that started it, so this is the last event that
    /// mentions it until something polls it or it ends. A handle started here ends in
    /// exactly one of [`HandleKilled`](EventKind::HandleKilled) or
    /// [`HandleExited`](EventKind::HandleExited) — never both, and never neither,
    /// because a run that finishes with handles still live kills them on the way out.
    /// A `HandleStarted` with no ending anywhere after it is a process that went away
    /// with this one.
    HandleStarted {
        /// The handle's id, as `shell_poll` and `shell_kill` take it.
        handle: u64,
        /// The command line as the model wrote it, unmodified.
        line: String,
    },
    /// A poll returned new output from a handle (0.25.0).
    ///
    /// Carries how many bytes arrived, not the bytes. The channel is for watching a
    /// run, not for carrying its payload: a log tail polled in a loop would put its
    /// whole output through every registered observer, and an observer that only
    /// wants to know the thing is alive would pay for text it never reads. The output
    /// is in the store and in the handle's capture file, both of which outlive this
    /// event and neither of which needs a copy of it.
    ///
    /// `bytes: 0` is a real and ordinary poll — a process that is running and has
    /// said nothing since the last one. It is not an error and not an ending.
    HandlePolled {
        /// Which handle was polled.
        handle: u64,
        /// New output this poll returned, in bytes. Bounded by the poll window, so a
        /// process that produced more than one poll can carry reports the window here
        /// and the rest arrives on the next poll rather than being lost.
        bytes: usize,
    },
    /// A handle was ended by this process (0.25.0).
    ///
    /// Emitted both for a `shell_kill` the model asked for and for the sweep that
    /// ends every live handle when a run finishes, however it finishes. They go
    /// through the same kill and they are the same fact from outside: the process did
    /// not choose to stop. Which of the two it was is readable from where the event
    /// falls, since the sweep happens after the run's last step.
    ///
    /// The kill walks the whole process tree, so a pipeline that was several
    /// processes is still one event.
    HandleKilled {
        /// Which handle was ended.
        handle: u64,
    },
    /// A handle's process ended on its own (0.25.0).
    ///
    /// The exit is reaped when it happens rather than discovered by the next poll, so
    /// this arrives at the time the process actually stopped and a poll afterwards is
    /// answered from the recorded status.
    ///
    /// A non-zero code is not a failed run. A dev server told to shut down, a build
    /// that found a compile error and a watcher that fell over all land here, and only
    /// the run's own logic knows which of those matters.
    HandleExited {
        /// Which handle ended.
        handle: u64,
        /// The exit code, or `None` for a death by signal. `None` is not `0`: a
        /// consumer that treats a missing code as success reports a process the
        /// kernel killed as one that finished cleanly.
        code: Option<i32>,
    },
    /// A handle recorded by a previous process was never re-attached (0.25.0).
    ///
    /// A resume finds handles in the checkpoint that this process did not start. It
    /// does not re-attach, poll or signal them — ever — and this event is that
    /// decision being announced, not a failure being reported. The handle is inserted
    /// already-terminal and stays readable; nothing more will happen to it.
    ///
    /// It is a distinct and terminal event because the reasoning behind it is not the
    /// reasoning behind any other ending. All a checkpoint can record about a live
    /// process is its pid, and a pid is not an identity. Between the crash and the
    /// resume the operating system may have handed that number to something
    /// unrelated, and no test separates the two safely — every "is it still our
    /// program" check is a race between the check and the signal. Signalling a reused
    /// pid is the one way this crate could damage something outside its own
    /// workspace, and the cost of being wrong there is not a failed run, it is
    /// somebody else's process. So the handle is marked, kept readable, and left
    /// alone.
    ///
    /// This is the variant the other four were worth adding for. The orphaning was
    /// already a row in the store, and a row is something an operator has to go and
    /// look for. On the channel they were already watching, a process that was running
    /// before the crash simply stopped being mentioned: a silent drop, and the one
    /// ending here that cannot be inferred from the events around it. It is also the
    /// only ending a handle reaches without this process having started it — there is
    /// no [`HandleStarted`](EventKind::HandleStarted) for it anywhere in this stream,
    /// so an observer keeping its own table of live handles learns about this one
    /// here or not at all.
    HandleOrphaned {
        /// Which handle was abandoned.
        handle: u64,
        /// Why, in the words the trace and the model are both given — that it was
        /// started by a previous process and its pid may since have been reused.
        reason: String,
    },
    /// A review criterion returned a verdict (0.34.0).
    ///
    /// The reasons are carried rather than summarised because a refusal a human
    /// cannot argue with is a gate nobody trusts twice. A verdict that never
    /// happened — a transport failure, an unreadable answer — emits nothing here
    /// and is recorded as [`GateOutcome::Errored`](crate::GateOutcome): "the
    /// review said no" and "the review did not run" are different facts and the
    /// stream keeps them apart.
    Reviewed {
        /// Whether the work satisfied the rubric.
        passed: bool,
        /// Why, in the reviewer's own words.
        reasons: Vec<String>,
    },
    /// The run changed which model it is asking (0.34.0).
    ///
    /// Emitted once, at the transition, not once per step: a rule that fires on
    /// every request afterwards would make a change of model indistinguishable
    /// from a run that always used it.
    Routed {
        /// The model the run was asking, or empty for the provider's own default.
        from: String,
        /// The model it is asking now.
        to: String,
        /// Which rule fired, in words an operator reads.
        why: String,
    },
    /// A capability bundle was loaded (0.35.0).
    ///
    /// Emitted once per plugin, by the caller that loaded them, before the run —
    /// see [`crate::plugin`]. What a bundle contributed is worth a line in the
    /// trace on its own: a run whose skill catalogue or policy stack is not what
    /// its operator expected is answered by this event rather than by reading
    /// four directories.
    PluginLoaded {
        /// The plugin's id, which namespaces every name it contributed.
        plugin: String,
        /// Which kinds it declared — `skills`, `templates`, `agents`, `mcp`,
        /// `hooks`, `policy` — in that order.
        contributions: Vec<String>,
    },
    /// A declared capability bundle was not loaded, and the run went on (0.35.0).
    ///
    /// The other half of "dropped and reported, never fatal": a bundle that fails
    /// to load costs exactly itself, and this is how an operator finds out rather
    /// than discovering three weeks later that deny rules they believed in were
    /// never installed.
    PluginDropped {
        /// The plugin's id where its manifest named one, else the directory's own
        /// name.
        plugin: String,
        /// What stopped it, worded for whoever has to fix it.
        why: String,
    },
    /// A run was put back: its files, what it remembered and what it had queued
    /// (0.36.0).
    ///
    /// Emitted by [`rewind_run_observed`](crate::rewind_run_observed) once the
    /// work is done. The three numbers are taken from the
    /// [`Rewound`](crate::Rewound) value being returned rather than re-read from
    /// the store — a count re-queried afterwards is true whether or not anything
    /// was restored, which is the shape 0.32.0 paid to learn.
    ///
    /// Nothing about this event says a rewind erased anything: the steps, the
    /// event stream, the spawn records and the ledger of the rewound run are all
    /// still there, and [`Store::rewinds`](crate::Store::rewinds) is the durable
    /// half naming exactly what changed.
    ///
    /// Which run was put back is the envelope's own `run_id`. A second copy here
    /// does not survive the wire at all: the kind is flattened into the envelope,
    /// so a `run_id` field is a duplicate key and serde refuses it — caught by
    /// `every_variant_round_trips` rather than in production.
    Rewound {
        /// How many paths were rewound, whatever verdict each got.
        files: u32,
        /// How many memory entries were restored or removed.
        memory: u32,
        /// How many queued children were dropped.
        queued: u32,
    },
    /// A conversational turn was answered rather than run (0.37.0).
    ///
    /// Emitted by [`Session`](crate::Session) when a turn's own first completion
    /// stopped on text: one completion was made, billed and recorded, and nothing
    /// was staged — no step, no gate attempt, no checkpoint, no snapshot, no plan
    /// gate and no call to the [`Approver`](crate::Approver). An attached process,
    /// a hook and a transcript can tell an answer from a run without opening the
    /// store.
    ///
    /// Emitted once, before [`Finished`](EventKind::Finished), and never for a
    /// [`run_with`](crate::run_with) — a one-shot contract is work by declaration
    /// and is never classified.
    ///
    /// Which run served the turn is the envelope's own `run_id`. A second copy
    /// here does not survive the wire at all: the kind is flattened into the
    /// envelope, so a `run_id` field is a duplicate key and serde refuses it — the
    /// same constraint [`Rewound`](EventKind::Rewound) records, caught by
    /// `every_variant_round_trips` rather than in production.
    ///
    /// ```
    /// use io_harness::{EventKind, Flow, Observer, RunEvent};
    /// use std::sync::atomic::{AtomicI64, Ordering};
    ///
    /// /// Counts what a conversation cost in runs rather than in turns.
    /// #[derive(Default)]
    /// struct Answered(AtomicI64);
    ///
    /// impl Observer for Answered {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         if let EventKind::Answered { turn_id } = &event.kind {
    ///             // No run was opened for this one; `event.run_id` is the run row
    ///             // that records what the single completion cost.
    ///             println!("turn {turn_id} answered under run {}", event.run_id);
    ///             self.0.fetch_add(1, Ordering::Relaxed);
    ///         }
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// let seen = Answered::default();
    /// seen.event(&RunEvent::new(7, 0, EventKind::Answered { turn_id: 3 }));
    /// assert_eq!(seen.0.load(Ordering::Relaxed), 1);
    /// ```
    Answered {
        /// The turn that was answered, in the session's tree — the handle
        /// [`Session::branch_from`](crate::Session::branch_from) takes.
        turn_id: i64,
    },
    /// The run's older observations were folded into one written summary
    /// (0.43.0).
    ///
    /// Emitted by the compaction helper the moment the fold lands, whether it was
    /// triggered by the ledger crossing [`Compaction::at_share`](crate::Compaction)
    /// or by a provider refusing the request as too large. What the summary *says*
    /// is not on the wire: it is an observation the model reads, and a durable row
    /// ([`Store::summaries`](crate::Store::summaries)) an operator reads. The
    /// event is the fact and its cost.
    ///
    /// The two token figures are the estimate for the observation section before
    /// and after the fold, by the same estimator assembly uses — so a reader can
    /// see what the fold bought without re-deriving it, and a fold that bought
    /// nothing is visible as such.
    ///
    /// ```
    /// use io_harness::{EventKind, Flow, Observer, RunEvent};
    ///
    /// /// Reports what each fold saved.
    /// struct Folds;
    ///
    /// impl Observer for Folds {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         if let EventKind::Compacted { through_step, before_tokens, after_tokens } = &event.kind {
    ///             println!("step {through_step}: {before_tokens} -> {after_tokens} tokens");
    ///         }
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// let flow = Folds.event(&RunEvent::new(
    ///     7,
    ///     12,
    ///     EventKind::Compacted { through_step: 12, before_tokens: 19_400, after_tokens: 5_100 },
    /// ));
    /// assert_eq!(flow, Flow::Continue);
    /// ```
    Compacted {
        /// The step whose assembly triggered the fold, so a trace and the
        /// `summaries` row agree on when it happened. The row is looked *up* by
        /// [`Summary::folded`](crate::Summary::folded), which is stable
        /// across a resume in a way a step number is not.
        through_step: u32,
        /// Estimated tokens the observation section held before the fold.
        before_tokens: u64,
        /// Estimated tokens it holds after it.
        after_tokens: u64,
    },
    /// The request began asking a vendor to cache a prefix of the transcript
    /// (0.44.0).
    ///
    /// Emitted when the marked prefix **changes**, never once per step: the step it is
    /// first offered on, and again whenever a later fold or a moved prefix makes the
    /// crate mark different bytes. A run marking the same prefix for forty steps emits
    /// this once. That is `Routed`'s rule and it is here for `Routed`'s reason — an
    /// event recomputed from each freshly built request reports a transition every
    /// step and stops meaning anything.
    ///
    /// **The absence of this event is the signal that nothing was marked**, and it is
    /// the answer to "why is this run getting no cache reads". The marker is withheld
    /// until a run folds (there is no frozen prefix before that) and until the prefix
    /// has repeated byte-identically since the previous step (the crate never asks a
    /// vendor to cache bytes it has not already sent). A run with no `CacheMarked` never
    /// asked; a run with three marked three different prefixes; and one of these beside
    /// a zero `cache_read_tokens` on the same step's `provider_calls` row says the
    /// vendor declined a marker the crate did send — most often because the prefix is
    /// under the vendor's minimum cacheable length.
    ///
    /// Named for what the crate knows. At the moment a request is built nothing has
    /// been cached; a marker has been asked for. What was actually served from a cache
    /// is [`Usage::cache_read_tokens`](crate::Usage::cache_read_tokens).
    ///
    /// ```
    /// use io_harness::{EventKind, Flow, Observer, RunEvent};
    ///
    /// struct Marks;
    ///
    /// impl Observer for Marks {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         if let EventKind::CacheMarked { through_step, prefix_bytes } = &event.kind {
    ///             println!("step {through_step}: caching {prefix_bytes} bytes of prefix");
    ///         }
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// let flow = Marks.event(&RunEvent::new(
    ///     7,
    ///     13,
    ///     EventKind::CacheMarked { through_step: 13, prefix_bytes: 8_412 },
    /// ));
    /// assert_eq!(flow, Flow::Continue);
    /// ```
    CacheMarked {
        /// The step the marker was first sent on, so a trace reads the same way
        /// [`Compacted`](EventKind::Compacted) does beside it.
        through_step: u32,
        /// Bytes of the request's `user` the vendor was asked to cache. Not tokens:
        /// this is what the crate measured, and converting it would be a guess at the
        /// vendor's tokeniser.
        prefix_bytes: u64,
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

/// Every wire tag [`EventKind`] can serialize to, in declaration order (0.28.0).
///
/// The names an operator writes in a `[[hook]]`'s `on` list, and therefore the list
/// a name is validated against at config load. A tag missing from here is a filter
/// an operator can write and that will never fire, which is a silence rather than an
/// error — so the completeness of this list is asserted rather than maintained: the
/// test below reads `pub enum EventKind` out of this file, snake-cases each variant
/// and requires the two sets to be equal.
///
/// That test is also what closes a defect recorded in 0.25.0. `every_kind()` matches
/// on the items of its own vector, which proves the match arms exhaustive and never
/// proves the vector complete, and `TodoWrote`, `QuestionAsked` and
/// `QuestionAnswered` were absent from it — and therefore untested — from 0.21.0
/// until this release.
///
/// `pub(crate)` deliberately. [`crate::Config`] is the only caller, and a public
/// constant would be a semver commitment to the order and spelling of a list whose
/// whole job is to be regenerated by hand whenever the enum grows.
pub(crate) const EVENT_NAMES: &[&str] = &[
    "started",
    "step",
    "tool_call",
    "refused",
    "approval_requested",
    "approval_decided",
    "spend_draw",
    "retry",
    "fell_back_to",
    "replan",
    "stalled",
    "spawned",
    "spawn_refused",
    "fleet",
    "memory_wrote",
    "todo_wrote",
    "question_asked",
    "question_answered",
    "plan_proposed",
    "plan_decided",
    "reasoning",
    "server_tool_used",
    "token",
    "sandbox",
    "mcp",
    "handle_started",
    "handle_polled",
    "handle_killed",
    "handle_exited",
    "handle_orphaned",
    "reviewed",
    "routed",
    "plugin_loaded",
    "plugin_dropped",
    "rewound",
    "answered",
    "compacted",
    "cache_marked",
    "finished",
];

/// Watches a run as it happens.
///
/// Shaped after [`Approver`](crate::Approver), the crate's other
/// inversion-of-control point: `Send + Sync` with `&self` methods, held as
/// `&dyn Observer`. `&self` rather than `&mut self` is not a style choice — a
/// tree runs up to `max_concurrent_agents` children as concurrent futures on one task,
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

/// Writes every event to the store on its way to another observer, so a second
/// process can read the run's stream (0.33.0).
///
/// [`Observer`] solved serialisation in 0.12.0 and left transport unsolved: the
/// events existed only inside the process driving the run, so an application that
/// wanted to watch a run it had not started was back to polling the trace against
/// a schema this crate does not promise — the exact thing the observer exists to
/// stop people doing. `Broadcast` closes that gap using the store both processes
/// already open.
///
/// Wrap it around whatever observer you already have and pass it to any
/// `*_observed` entry point:
///
/// ```no_run
/// use io_harness::{run_observed, Broadcast, Ignore, OpenRouter, Store, TaskContract,
///                  Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let contract = TaskContract::new(
///     "add a hello function", "src/hello.rs",
///     Verification::FileContains("fn hello".into()));
/// // A file, not `Store::memory()`: a second process has to be able to open it.
/// let store = Store::open("runs.db")?;
///
/// // `Ignore` when this process has nothing of its own to do with the events —
/// // the point is that another one does.
/// let watching = Broadcast::new(Store::open("runs.db")?, &Ignore);
/// run_observed(&contract, &OpenRouter::from_env()?, &store, &watching).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Why a decorator
///
/// The durable stream is the *same* [`RunEvent`] the inner observer receives, not
/// a reconstruction assembled from the trace. That is the whole design: a
/// reconstruction would drift the first time one of the trace's tables gained a
/// column the event did not, and there is no test that could catch it. Here there
/// is only one value, written and forwarded, so the two surfaces cannot disagree.
///
/// # Why it takes a [`Store`](crate::Store) of its own
///
/// [`Observer`] is `Send + Sync`, because one observer serves a whole tree of
/// concurrent agents. `rusqlite::Connection` is `Send` and **not** `Sync`, so a
/// `&Store` borrowed from the run cannot live inside one. `Broadcast` therefore
/// opens its own connection to the same file and holds it behind a `Mutex` —
/// which is not a workaround so much as the release's own premise: two
/// connections to one store is exactly what an attaching process does, and
/// [`Store::open`](crate::Store::open) has set `journal_mode = WAL` and a
/// [`BUSY_TIMEOUT`](crate::BUSY_TIMEOUT) since 0.12.0 precisely so that works.
///
/// It follows that a [`Store::memory`](crate::Store::memory) store cannot be
/// broadcast usefully: a private in-memory database is not a file a second
/// connection can open, and there is no second process to read it.
///
/// # What it costs
///
/// One `INSERT` per event, on the run's own task, because [`Observer::event`] is
/// synchronous and on the critical path — the same caution the trait's own
/// documentation gives applies here, and this is the cheapest thing that can be
/// on it. A run with no `Broadcast` writes nothing at all.
///
/// # Failure
///
/// An observer is a spectator and `event` returns no `Result`, so a write that
/// fails is logged at `warn` and dropped rather than taking the run with it. A
/// reader that must not miss an event should treat a gap in the cursor sequence as
/// what it is; the run's own durable trace is unaffected either way, and remains
/// the authority.
///
/// [`Flow`] is passed through from the inner observer unchanged: `Broadcast`
/// records, it does not decide.
pub struct Broadcast<'a> {
    store: Mutex<crate::Store>,
    inner: &'a dyn Observer,
}

impl<'a> Broadcast<'a> {
    /// Write every event to `store`, then pass it to `inner`.
    ///
    /// `store` is this observer's own connection — open a second one on the same
    /// path rather than trying to share the run's, for the reason above.
    pub fn new(store: crate::Store, inner: &'a dyn Observer) -> Self {
        Self {
            store: Mutex::new(store),
            inner,
        }
    }
}

// Written out rather than derived: `Observer` is not `Debug` and requiring it
// would be a break for every observer already implemented out of tree.
impl fmt::Debug for Broadcast<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Broadcast").finish_non_exhaustive()
    }
}

impl Observer for Broadcast<'_> {
    fn event(&self, event: &RunEvent) -> Flow {
        // A poisoned lock means a previous `event` panicked mid-write. The run
        // must not die of it — recover and keep broadcasting.
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = store.put_event(event) {
            tracing::warn!(run_id = event.run_id, error = %e, "event not broadcast");
        }
        drop(store);
        self.inner.event(event)
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

    /// F3, first assertion. [`EVENT_NAMES`] is the list a `[[hook]]`'s `on` is
    /// validated against, so a variant missing from it is a filter an operator can
    /// write and that will never fire — a silence, not an error. The only way to
    /// know it is complete is to read the enum, which is the technique
    /// `tests/public_api.rs` has used over a much larger surface since 0.16.0 and
    /// that 0.27.0's F2 used for `#[non_exhaustive]`. A `strum` derive would be a
    /// dependency, and NF2 forbids one.
    ///
    /// The control is the whole point: `missing_from` is run against a list with one
    /// entry taken out and must name exactly that entry. A helper that always
    /// answers "complete" is the failure mode that produced the 0.25.0 defect, and
    /// it has to be seen answering no.
    #[test]
    fn event_names_is_a_census_of_the_enum_rather_than_a_list_someone_maintained() {
        assert!(
            missing_from(EVENT_NAMES).is_empty(),
            "EVENT_NAMES does not name every EventKind variant: {:?}",
            missing_from(EVENT_NAMES)
        );

        let short: Vec<&str> = EVENT_NAMES.iter().copied().skip(1).collect();
        assert_eq!(
            missing_from(&short),
            vec![EVENT_NAMES[0].to_string()],
            "the helper must be able to report an omission, or it proves nothing"
        );
    }

    /// The variants declared by `pub enum EventKind` in this file that `names` does
    /// not mention, snake-cased the way `#[serde(rename_all = "snake_case")]` does.
    ///
    /// A text parse, and the shape of the enum is what makes it safe: a variant sits
    /// at exactly four spaces of indentation and begins with an uppercase letter,
    /// where a doc line begins with `/`, an attribute with `#`, a field is indented
    /// eight, and a variant's closing brace begins with `}`.
    ///
    /// Line endings are normalised first. A Windows checkout may hold this file with
    /// CRLF, and a parse that looked for `"\n}\n"` would find nothing there and fail
    /// on one platform only — which is precisely the class of thing this repository
    /// keeps paying for.
    fn missing_from(names: &[&str]) -> Vec<String> {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/observe.rs"),
        )
        .expect("this file is readable from its own test")
        .replace("\r\n", "\n");
        let body = src
            .split_once("pub enum EventKind {")
            .expect("the enum is declared in this file")
            .1;
        let body = body.split_once("\n}\n").expect("the enum is closed").0;

        let mut missing = Vec::new();
        for line in body.lines() {
            let Some(rest) = line.strip_prefix("    ") else {
                continue;
            };
            if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            let variant: String = rest
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            let mut snake = String::new();
            for (i, c) in variant.char_indices() {
                if c.is_ascii_uppercase() && i > 0 {
                    snake.push('_');
                }
                snake.push(c.to_ascii_lowercase());
            }
            if !names.contains(&snake.as_str()) {
                missing.push(snake);
            }
        }
        assert!(
            missing.len() < names.len(),
            "the parse found nothing, so it is measuring itself rather than the enum"
        );
        missing
    }

    /// F3, second assertion, and the one `src/observe.rs` has been missing since
    /// 0.21.0. The guard inside [`every_kind`] matches on the items of its own
    /// vector: it proves the arms exhaustive and says nothing about what the vector
    /// holds, which is how three variants stayed out of the round-trip for seven
    /// releases. Equality against [`EVENT_NAMES`] — which the test above proves is
    /// the enum — is the assertion that was wanted all along.
    #[test]
    fn every_kind_produces_one_of_every_tag_and_no_others() {
        let mut tags: Vec<String> = every_kind()
            .into_iter()
            .map(|k| {
                serde_json::to_value(RunEvent::new(1, 1, k)).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        tags.sort();
        let mut expected: Vec<String> = EVENT_NAMES.iter().map(|s| (*s).to_string()).collect();
        expected.sort();
        assert_eq!(
            tags, expected,
            "every_kind() is not a census: the sample set and the enum disagree"
        );
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

    /// One of every variant.
    ///
    /// Kept complete by two things, and it needed both. The `match` below refuses to
    /// compile when a variant is added and not named — which catches the new
    /// variant, and caught nothing else: it matches on the items of this vector, so
    /// an arm can name a variant the vector never holds, and from 0.21.0 to 0.28.0
    /// three of them did. `every_kind_produces_one_of_every_tag_and_no_others` is the
    /// other half, asserting the tags this produces are exactly [`EVENT_NAMES`].
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
            EventKind::Fleet {
                tier: 1,
                working: 4,
                queued: 116,
                done: 0,
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
            EventKind::Reviewed {
                passed: false,
                reasons: vec!["`parse` still panics on empty input".into()],
            },
            EventKind::Routed {
                from: "small-model".into(),
                to: "big-model".into(),
                why: "2 consecutive gate failures".into(),
            },
            EventKind::PluginLoaded {
                plugin: "rust-review".into(),
                contributions: vec!["skills".into(), "policy".into()],
            },
            EventKind::PluginDropped {
                plugin: "broken".into(),
                why: "no plugin.toml".into(),
            },
            EventKind::Rewound {
                files: 2,
                memory: 1,
                queued: 0,
            },
            EventKind::Answered { turn_id: 3 },
            EventKind::Compacted {
                through_step: 12,
                before_tokens: 19_400,
                after_tokens: 5_100,
            },
            EventKind::CacheMarked {
                through_step: 13,
                prefix_bytes: 8_412,
            },
            EventKind::Token {
                text: "hello".into(),
            },
            EventKind::ServerToolUsed {
                provider: "anthropic".into(),
                tool: "web_search".into(),
                ok: true,
            },
            EventKind::HandleStarted {
                handle: 1,
                line: "npm run dev".into(),
            },
            EventKind::HandlePolled {
                handle: 1,
                bytes: 0,
            },
            EventKind::HandleKilled { handle: 1 },
            // `None` rather than a code, because the signal death is the case a
            // consumer is most likely to get wrong and the round-trip is where a
            // missing field would show up.
            EventKind::HandleExited {
                handle: 1,
                code: None,
            },
            EventKind::HandleOrphaned {
                handle: 1,
                reason: "started by a previous process".into(),
            },
            // The three that were missing from 0.21.0 until 0.28.0. They were named
            // in the match below the whole time, which is exactly why nobody noticed:
            // the guard proves the arms exhaustive over the items of this vector and
            // says nothing about what the vector holds.
            EventKind::TodoWrote {
                items: vec![crate::state::TodoItem::new(
                    "write the thing",
                    crate::state::TodoState::Active,
                )],
            },
            EventKind::QuestionAsked {
                question: "which database?".into(),
                choices: vec!["sqlite".into(), "postgres".into()],
            },
            EventKind::QuestionAnswered {
                answer: "sqlite".into(),
                by: "human".into(),
            },
            // 0.31.0. Added to the vector in the same commit as the enum, which is
            // the lesson the three above cost seven releases to learn.
            EventKind::PlanProposed {
                plan_id: 1,
                steps: vec![crate::approve::PlanStep::new("read the call sites")],
            },
            EventKind::PlanDecided {
                plan_id: 1,
                verdict: "approve".into(),
                by: "human".into(),
            },
            EventKind::Reasoning {
                text: "the parser is the only caller".into(),
                tokens: 120,
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
                | EventKind::Fleet { .. }
                | EventKind::MemoryWrote { .. }
                | EventKind::Sandbox { .. }
                | EventKind::Mcp { .. }
                // The five handle events sit with `Sandbox` and `Mcp` rather than
                // anywhere else: all of them report something outside this process
                // that the run caused to happen. `HandleOrphaned` is here with the
                // rest because this arm is a compile-time census and not a severity
                // ranking — nothing in this file grades events, and an orphaning
                // filed as routine is the mistake the variant exists to stop.
                | EventKind::HandleStarted { .. }
                | EventKind::HandlePolled { .. }
                | EventKind::HandleKilled { .. }
                | EventKind::HandleExited { .. }
                | EventKind::HandleOrphaned { .. }
                | EventKind::Token { .. }
                | EventKind::TodoWrote { .. }
                | EventKind::QuestionAsked { .. }
                | EventKind::QuestionAnswered { .. }
                | EventKind::PlanProposed { .. }
                | EventKind::PlanDecided { .. }
                | EventKind::Reasoning { .. }
                | EventKind::ServerToolUsed { .. }
                // 0.34.0 — the verdict a review returned, and a run changing which
                // model it asks.
                | EventKind::Reviewed { .. }
                | EventKind::Routed { .. }
                | EventKind::PluginLoaded { .. }
                | EventKind::PluginDropped { .. }
                // 0.36.0 — a whole run put back.
                | EventKind::Rewound { .. }
                // 0.37.0 — a turn answered instead of run.
                | EventKind::Answered { .. }
                // 0.43.0 — the run's older observations folded into a summary.
                | EventKind::Compacted { .. }
                | EventKind::CacheMarked { .. }
                | EventKind::Finished { .. } => {}
            }
        }
        all
    }
}
