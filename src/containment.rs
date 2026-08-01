//! The containment boundary for a tree of agents.
//!
//! A [`Containment`] is handed in once, at the root, and caps the whole tree
//! with limits no spawned [`crate::TaskContract`] can raise: how many agents may
//! exist, how many may run at once, how deep they may nest, and an aggregate
//! spend ceiling the entire tree draws down *together*. It is serde-serializable
//! like [`crate::Policy`], so io-cli and io-studio load it from config rather
//! than hand-build it.
//!
//! Two of those caps are deliberately different in kind (0.32.0).
//! [`Containment::max_total_agents`] **refuses**: crossing it is a
//! [`SpawnRefusal`] the parent is told about, in the same family as the spend
//! and duration ceilings, because it is a limit meant to stop a run.
//! [`Containment::max_concurrent_agents`] **throttles**: a spawn past it is not
//! refused, it takes a place in a FIFO queue and starts when a slot frees,
//! because it is a limit meant to shape a run. Before 0.32.0 there was one agent
//! cap doing both jobs, so a task that wanted a hundred agents failed at its
//! hundred-and-first child instead of running a hundred at a time until it was
//! done.
//!
//! The [`Ledger`] is the runtime accounting for one tree: a single shared point
//! that every agent draws its token spend, its right-to-exist and its turn to run
//! from. It is the one place spend is serialized, so a hundred concurrent agents
//! cannot overspend past the ceiling through a race — the critical section is a
//! plain lock held only for the arithmetic.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The caps a whole agent tree runs under. Tokens are the hard spend ceiling
/// (no price telemetry exists, so spend is counted in tokens); an optional cost
/// and duration are carried for callers that supply them.
///
/// ```
/// use io_harness::Containment;
///
/// // Twelve agents in the tree, four working at once, two levels of nesting, and
/// // 200k tokens for all of them together. A spawned contract can tighten any of
/// // these and can raise none of them.
/// let containment = Containment::new(12, 4, 2, 200_000);
///
/// // The two agent caps are different in kind. The thirteenth spawn is refused;
/// // the fifth *simultaneous* one merely waits its turn.
/// assert_eq!(containment.max_total_agents, 12);
/// assert_eq!(containment.max_concurrent_agents, 4);
///
/// // The ceiling actually enforced is the token one: a provider reports tokens
/// // and never money, so `max_total_cost` is inert and stays `None`.
/// assert_eq!(containment.max_total_tokens, 200_000);
/// assert_eq!(containment.max_total_cost, None);
///
/// // Serde, so an operator's config file is the source of the caps rather than a
/// // recompile.
/// let stored = serde_json::to_string(&containment).unwrap();
/// let loaded: Containment = serde_json::from_str(&stored).unwrap();
/// assert_eq!(loaded, containment);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Containment {
    /// Maximum number of agents that may exist in the tree, root included.
    ///
    /// This one **refuses**. The spawn that would cross it comes back to the
    /// parent as [`SpawnRefusal::AgentCap`], the same way the spend ceiling comes
    /// back as [`SpawnRefusal::BudgetExhausted`] — a limit meant to stop a run.
    /// To bound how many run *at once* without stopping anything, use
    /// [`Self::max_concurrent_agents`].
    pub max_total_agents: u32,
    /// Maximum number of agents that may be *working* at once, per tier.
    ///
    /// This one **throttles**. A spawn past it is not refused: the child takes a
    /// place in a FIFO queue and starts when a slot frees, so a fleet drains
    /// rather than failing. Renamed from `max_concurrent` in 0.32.0, which was
    /// only ever a per-step fan-out width inside one parent and invisible to the
    /// rest of the tree; `#[serde(alias)]` keeps stored configuration readable.
    ///
    /// **Per tier, not tree-global, and that is the deadlock argument rather than
    /// an oversight.** Each nesting level has its own set of slots. A parent holds
    /// a slot at its own tier while it waits for children at the tier below, so
    /// the wait graph runs strictly downward and cannot contain a cycle; one
    /// tree-global pool would hang the first time the agent holding the last slot
    /// spawned a child, because only that child could free it. The honest
    /// consequence is that a tree of depth *d* can hold up to
    /// `max_concurrent_agents * d` agents working at once, not
    /// `max_concurrent_agents`.
    #[serde(alias = "max_concurrent")]
    pub max_concurrent_agents: u32,
    /// Maximum nesting depth, counted from the root (the root is depth 0).
    pub max_depth: u32,
    /// Aggregate token ceiling drawn down by the entire tree together.
    pub max_total_tokens: u64,
    /// Optional aggregate cost ceiling, in whatever unit the caller supplies
    /// (there is no price telemetry, so the crate never derives this itself).
    #[serde(default)]
    /// **Reserved, and not enforced.** Setting it has no effect.
    ///
    /// Enforcing a cost ceiling needs a price per token, and the crate has no
    /// price telemetry — a provider reports tokens, never money, so any figure
    /// the harness compared against would be one it invented. The field is kept
    /// rather than removed because it serialises in callers' stored configuration
    /// and deleting it would break their deserialisation for no gain; it is
    /// documented as inert instead, which is the honest state.
    ///
    /// Spend that *is* enforced is [`Self::max_total_tokens`]. To bound money,
    /// convert your budget to tokens at your provider's rate and set that.
    pub max_total_cost: Option<u64>,
    /// Optional wall-clock ceiling for the whole tree, measured from when the
    /// ROOT run started — so it counts a 24-hour tree's whole life, including
    /// time the process was down, not the age of whichever agent notices.
    ///
    /// Crossing it halts the tree with
    /// [`RunOutcome::BudgetCeilingReached`](crate::RunOutcome::BudgetCeilingReached),
    /// the same way the token ceiling does, and a child's own contract cannot
    /// raise it. Declared in 0.5.0 and not actually enforced until 0.12.0.
    #[serde(default)]
    pub max_total_duration: Option<Duration>,
}

impl Containment {
    /// A containment with token, agent, concurrency, and depth caps and no
    /// cost/duration ceiling.
    pub fn new(
        max_total_agents: u32,
        max_concurrent_agents: u32,
        max_depth: u32,
        max_total_tokens: u64,
    ) -> Self {
        Self {
            max_total_agents,
            max_concurrent_agents,
            max_depth,
            max_total_tokens,
            max_total_cost: None,
            max_total_duration: None,
        }
    }
}

/// Why a spawn was refused by the containment boundary. Returned to the
/// requesting agent as a typed tool result it can adapt to, never a panic.
///
/// Every variant here is a limit meant to *stop* the work. Concurrency is not
/// among them and never was, as of 0.32.0: crossing
/// [`Containment::max_concurrent_agents`] queues the child instead of refusing
/// it, so there is nothing for the parent to adapt to.
///
/// ```
/// use io_harness::{Containment, Ledger, SpawnRefusal};
///
/// // Room for two agents in the whole tree, and the root is already one of them.
/// let ledger = Ledger::new(&Containment::new(2, 2, 1, 1_000));
/// ledger.register_agent(1).expect("the first child fits");
///
/// // The second child does not. The parent agent is told, in a form it can act
/// // on — do the work itself, or narrow what it was going to delegate.
/// let refusal = ledger.register_agent(1).unwrap_err();
/// assert_eq!(refusal, SpawnRefusal::AgentCap { max: 2 });
/// assert_eq!(refusal.to_string(), "agent cap reached (2 agents)");
///
/// // `cap` is the short label the trace records, so an audit can count refusals
/// // by which boundary produced them without parsing the English above.
/// assert_eq!(refusal.cap(), "agents");
/// assert_eq!(SpawnRefusal::DepthCap { max: 1, requested: 2 }.cap(), "depth");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    /// The tree already holds `max_total_agents`.
    AgentCap { max: u32 },
    /// The child would nest past `max_depth` (counted from the root).
    DepthCap { max: u32, requested: u32 },
    /// The aggregate spend ceiling is already exhausted, so a new agent has no
    /// budget to run under.
    BudgetExhausted,
}

impl SpawnRefusal {
    /// Which cap this refusal breached, for the trace.
    pub fn cap(&self) -> &'static str {
        match self {
            SpawnRefusal::AgentCap { .. } => "agents",
            SpawnRefusal::DepthCap { .. } => "depth",
            SpawnRefusal::BudgetExhausted => "budget",
        }
    }
}

impl std::fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnRefusal::AgentCap { max } => {
                write!(f, "agent cap reached ({max} agents)")
            }
            SpawnRefusal::DepthCap { max, requested } => {
                write!(f, "depth cap reached (max {max}, requested {requested})")
            }
            SpawnRefusal::BudgetExhausted => write!(f, "the tree's spend ceiling is exhausted"),
        }
    }
}

/// The outcome of drawing token spend against the ledger.
///
/// ```
/// use io_harness::{Containment, Draw, Ledger, Usage};
///
/// let ledger = Ledger::new(&Containment::new(4, 2, 1, 5_000));
/// let usage = Usage {
///     prompt_tokens: 4_000,
///     completion_tokens: 800,
///     total_tokens: 4_800,
///     ..Default::default()
/// };
///
/// // What the run loop does with a completion's usage: draw it, then branch. This
/// // is the only place the tree's ceiling is checked, so ignoring the return is
/// // how a tree overspends.
/// match ledger.draw_tokens(usage.total_tokens) {
///     Draw::Ok => { /* take another step */ }
///     Draw::Halted => unreachable!("4,800 fits under 5,000"),
/// }
///
/// // The next step does not fit. `Halted` stops the whole tree, not just this
/// // agent — and the rejected draw is not recorded, so the total never drifts
/// // above the ceiling even though the provider did charge for that step.
/// assert_eq!(ledger.draw_tokens(usage.total_tokens), Draw::Halted);
/// assert_eq!(ledger.spent_tokens(), 4_800);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Draw {
    /// The draw was within the ceiling; the tree may continue.
    Ok,
    /// This draw crossed the aggregate ceiling. The spend is still recorded (a
    /// step already happened and its tokens were spent), but the tree must halt
    /// as a whole — no agent gets another step.
    Halted,
}

/// How one tier of an agent tree is doing, right now: how many of its agents are
/// working, how many are queued behind
/// [`Containment::max_concurrent_agents`], and how many have finished (0.32.0).
///
/// Counted per tier rather than per tree because a single number cannot tell an
/// operator whether the fan-out at depth two is stuck behind the one at depth
/// one. It reaches an application two ways: pushed, as
/// [`EventKind::Fleet`](crate::EventKind::Fleet), and pulled, as
/// [`Ledger::tally`].
///
/// ```
/// use io_harness::{Containment, FleetTally, Ledger};
///
/// // A fresh tree: nothing has been spawned, so every tier is empty.
/// let ledger = Ledger::new(&Containment::new(100, 4, 2, 200_000));
/// assert_eq!(ledger.tally(1), FleetTally::default());
///
/// // What a mid-flight tier looks like to a progress bar: four slots busy, a
/// // hundred children still to come, and eleven already folded back into their
/// // parent. `working + queued` is what is left to watch.
/// let tier = FleetTally { working: 4, queued: 100, done: 11 };
/// assert_eq!(tier.working + tier.queued, 104);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetTally {
    /// Agents at this tier that hold a concurrency slot and are running.
    pub working: u32,
    /// Agents at this tier that are waiting for a slot. Nothing about them has
    /// been started, and nothing about them has been charged.
    pub queued: u32,
    /// Agents at this tier that have finished and released their slot.
    pub done: u32,
}

/// One agent's concurrency slot, held for as long as it runs.
///
/// Dropping it releases the permit — which admits the next child waiting at that
/// tier — and moves the agent from `working` to `done`. Drop rather than an
/// explicit release because every way out of a child (finished, paused on a
/// human, or an error propagating with `?`) has to free the slot, and only one of
/// those three is the happy path.
#[derive(Debug)]
pub(crate) struct AgentSlot {
    /// Held, never read. Returning it to the tier's semaphore is the whole job.
    _permit: OwnedSemaphorePermit,
    fleet: Arc<Mutex<Vec<FleetTally>>>,
    tier: usize,
}

impl Drop for AgentSlot {
    fn drop(&mut self) {
        let mut f = self.fleet.lock().unwrap();
        f[self.tier].working = f[self.tier].working.saturating_sub(1);
        f[self.tier].done += 1;
    }
}

/// Shared accounting for one agent tree: the aggregate spend, the agent
/// count, and the per-tier admission queue, behind a single lock so concurrent
/// draws cannot overspend.
///
/// Wrap in an [`std::sync::Arc`] and hand a clone of the arc to every agent in
/// the tree; they all draw on the one ledger.
///
/// ```
/// use std::sync::Arc;
///
/// use io_harness::{Containment, Draw, Ledger};
///
/// // One ledger for the tree; every agent holds a clone of the same arc.
/// let ledger = Arc::new(Ledger::new(&Containment::new(10, 4, 3, 100)));
/// let child = Arc::clone(&ledger);
///
/// // A child's contract can ask for 500 tokens and still only get what the tree
/// // has left. This is the containment property: budgets narrow, never widen.
/// assert_eq!(child.effective_token_budget(Some(500)), 100);
/// assert_eq!(child.effective_token_budget(Some(20)), 20);
///
/// // Two agents, each well inside its own budget, still halt the tree between
/// // them — the ceiling is aggregate, and the second draw is rejected rather
/// // than letting the recorded total pass 100.
/// assert_eq!(child.draw_tokens(60), Draw::Ok);
/// assert_eq!(ledger.draw_tokens(60), Draw::Halted);
/// assert_eq!(ledger.spent_tokens(), 60);
/// ```
#[derive(Debug)]
pub struct Ledger {
    max_total_tokens: u64,
    max_total_agents: u32,
    max_depth: u32,
    /// One semaphore per tier, each holding `max_concurrent_agents` permits.
    /// Index 0 is the root's tier and is never acquired — the root was not
    /// spawned, so it holds no slot and cannot be the agent that blocks its own
    /// descendants.
    tiers: Vec<Arc<Semaphore>>,
    /// Held in its own arc rather than inside `state` so an [`AgentSlot`] can
    /// carry a handle to it and update the counts from `Drop` without the ledger
    /// itself having to be an `Arc<Self>` at every call site.
    fleet: Arc<Mutex<Vec<FleetTally>>>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    spent_tokens: u64,
    agents: u32,
}

/// How many tiers of semaphore a ledger builds. `max_depth` is a `u32` a caller
/// supplies, and allocating one semaphore per unit of a number that could be
/// `u32::MAX` is a denial of service written by the API. Nesting deeper than this
/// is refused by [`Ledger::register_agent`] against the real `max_depth` long
/// before the clamp matters.
const MAX_TIERS: u32 = 64;

impl Ledger {
    /// A fresh ledger for a tree running under `c`. The root counts as the first
    /// agent, so the ledger starts with one agent registered.
    pub fn new(c: &Containment) -> Self {
        Self::build(c, 0, 1)
    }

    /// A ledger restored from durable state, for resuming a crashed tree: the
    /// spend and agent count are the totals already recorded in the store, so
    /// the resumed tree draws against the same continuous ceiling instead of
    /// restarting the budget at zero. `agents` already includes the root and
    /// every child previously spawned, so an adopted (already-registered) child
    /// is not re-counted on resume.
    ///
    /// A child that was only ever *queued* is deliberately not among them: it has
    /// no run row, so it was never counted and was never charged. Its place in
    /// the queue is restored separately, from the store, by machinery
    /// [`run_tree`](crate::run_tree) drives — not a plain link, because the
    /// method doing it is crate-internal and a public page must not link into
    /// what a reader cannot open.
    pub fn from_state(c: &Containment, spent_tokens: u64, agents: u32) -> Self {
        Self::build(c, spent_tokens, agents.max(1))
    }

    fn build(c: &Containment, spent_tokens: u64, agents: u32) -> Self {
        let tiers = (c.max_depth.min(MAX_TIERS) + 1) as usize;
        let slots = c.max_concurrent_agents.max(1) as usize;
        Self {
            max_total_tokens: c.max_total_tokens,
            max_total_agents: c.max_total_agents,
            max_depth: c.max_depth,
            tiers: (0..tiers)
                .map(|_| Arc::new(Semaphore::new(slots)))
                .collect(),
            fleet: Arc::new(Mutex::new(vec![FleetTally::default(); tiers])),
            state: Mutex::new(State {
                spent_tokens,
                agents,
            }),
        }
    }

    /// The tier index for `depth`, clamped to the vector this ledger built.
    fn tier(&self, depth: u32) -> usize {
        (depth as usize).min(self.tiers.len() - 1)
    }

    /// How this tier of the tree is doing right now (0.32.0). A tier that has
    /// never held an agent reads back as [`FleetTally::default`].
    pub fn tally(&self, depth: u32) -> FleetTally {
        self.fleet.lock().unwrap()[self.tier(depth)]
    }

    /// Take a concurrency slot at `depth` if one is free right now, without
    /// waiting. `None` means the tier is full and the caller must queue.
    pub(crate) fn try_admit(&self, depth: u32) -> Option<AgentSlot> {
        let tier = self.tier(depth);
        let permit = Arc::clone(&self.tiers[tier]).try_acquire_owned().ok()?;
        self.fleet.lock().unwrap()[tier].working += 1;
        Some(AgentSlot {
            _permit: permit,
            fleet: Arc::clone(&self.fleet),
            tier,
        })
    }

    /// Record that a child is waiting at `depth`.
    ///
    /// `newly_recorded` is what the store said when the entry was written: `true`
    /// when this is a fresh wait, `false` when the store already held it. The
    /// second case is a resumed tree replaying the step that queued the child —
    /// [`Self::restore_queue`] has already counted it, and counting it again
    /// would report a backlog twice the size of the one on disk. This is the one
    /// place the difference between a restored queue and a re-derived one is
    /// load-bearing in the code rather than only in a test.
    pub(crate) fn mark_queued(&self, depth: u32, newly_recorded: bool) {
        if newly_recorded {
            self.fleet.lock().unwrap()[self.tier(depth)].queued += 1;
        }
    }

    /// Take a restored wait out of the count without having waited for it
    /// (0.32.0).
    ///
    /// A queue restored from the store describes waits from a process that is
    /// dead; the slots it was holding died with it. So a child whose wait was
    /// restored can be admitted immediately by [`Self::try_admit`], and when it
    /// is, it never passes through [`Self::admit`] — the only other place `queued`
    /// comes down. Without this the counter would drift above the rows the store
    /// actually holds, and a resumed fleet would report a backlog that never
    /// reached zero.
    ///
    /// Call it only when the store confirmed a row was removed, so the count and
    /// the rows move together.
    pub(crate) fn drop_queued(&self, depth: u32) {
        let tier = self.tier(depth);
        let mut f = self.fleet.lock().unwrap();
        f[tier].queued = f[tier].queued.saturating_sub(1);
    }

    /// Wait for a concurrency slot at `depth`, FIFO. Call only after
    /// [`Self::mark_queued`]: acquiring the permit is what moves this child out
    /// of `queued` and into `working`.
    pub(crate) async fn admit(&self, depth: u32) -> AgentSlot {
        let tier = self.tier(depth);
        // `acquire_owned` on a semaphore that is never closed, so the error is
        // unreachable; `expect` rather than a silent unwrap so a future close
        // would name itself.
        let permit = Arc::clone(&self.tiers[tier])
            .acquire_owned()
            .await
            .expect("a tier's semaphore is never closed");
        {
            let mut f = self.fleet.lock().unwrap();
            f[tier].queued = f[tier].queued.saturating_sub(1);
            f[tier].working += 1;
        }
        AgentSlot {
            _permit: permit,
            fleet: Arc::clone(&self.fleet),
            tier,
        }
    }

    /// Restore a backlog read back from the store on resume: `(depth, waiting)`
    /// pairs, so a process that comes up after a crash reports the queue at the
    /// depth it had rather than at zero.
    pub(crate) fn restore_queue(&self, backlog: &[(u32, u32)]) {
        let mut f = self.fleet.lock().unwrap();
        for &(depth, waiting) in backlog {
            f[self.tier(depth)].queued = waiting;
        }
    }

    /// Tokens still available to the whole tree.
    pub fn remaining_tokens(&self) -> u64 {
        let s = self.state.lock().unwrap();
        self.max_total_tokens.saturating_sub(s.spent_tokens)
    }

    /// The budget an agent actually runs under, given the budget its own
    /// contract asked for. It is the smaller of what the contract wanted and
    /// what the tree has left — so a contract can tighten the budget but can
    /// never raise it above the tree's remaining ceiling.
    pub fn effective_token_budget(&self, contract_max: Option<u64>) -> u64 {
        let remaining = self.remaining_tokens();
        remaining.min(contract_max.unwrap_or(u64::MAX))
    }

    /// Record `tokens` of spend against the tree. A draw that would cross the
    /// aggregate ceiling is *rejected* — it is not added — and returns
    /// [`Draw::Halted`], so recorded spend never exceeds the ceiling however many
    /// agents draw concurrently. The single lock is what makes that hold under a
    /// hundred concurrent draws: the check-and-add is atomic, so no race can slip
    /// spend past the ceiling.
    ///
    /// (The model tokens of the halting step were still spent by the provider;
    /// the ledger declines to count them and stops the tree rather than letting
    /// the recorded total drift over the ceiling.)
    pub fn draw_tokens(&self, tokens: u64) -> Draw {
        let mut s = self.state.lock().unwrap();
        let next = s.spent_tokens.saturating_add(tokens);
        if next > self.max_total_tokens {
            Draw::Halted
        } else {
            s.spent_tokens = next;
            Draw::Ok
        }
    }

    /// Total tokens the tree has spent so far.
    pub fn spent_tokens(&self) -> u64 {
        self.state.lock().unwrap().spent_tokens
    }

    /// Register one new child agent at `depth` (the root is depth 0, so a
    /// child's depth is its parent's depth + 1). Fails, without registering,
    /// if the agent or depth cap would be breached or the budget is exhausted.
    ///
    /// Concurrency is deliberately not checked here. A child that would exceed
    /// [`Containment::max_concurrent_agents`] is admitted later rather than
    /// refused now, so it registers, waits, and runs.
    pub fn register_agent(&self, depth: u32) -> std::result::Result<(), SpawnRefusal> {
        if depth > self.max_depth {
            return Err(SpawnRefusal::DepthCap {
                max: self.max_depth,
                requested: depth,
            });
        }
        if self.remaining_tokens() == 0 {
            return Err(SpawnRefusal::BudgetExhausted);
        }
        let mut s = self.state.lock().unwrap();
        if s.agents >= self.max_total_agents {
            return Err(SpawnRefusal::AgentCap {
                max: self.max_total_agents,
            });
        }
        s.agents += 1;
        Ok(())
    }

    /// How many agents the tree currently holds.
    pub fn agents(&self) -> u32 {
        self.state.lock().unwrap().agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn containment() -> Containment {
        // 10 agents, 4 concurrent, depth 3, 100 tokens for the whole tree.
        Containment::new(10, 4, 3, 100)
    }

    #[test]
    fn the_ceiling_is_tree_wide_not_per_agent() {
        // Three children each draw 40 tokens — none exceeds its own 50-token
        // contract budget, yet the tree halts because their *combined* draw
        // crosses the aggregate ceiling of 100.
        let led = Ledger::new(&containment());
        let per_child_contract_budget = 50;
        assert!(40 < per_child_contract_budget);

        assert_eq!(led.draw_tokens(40), Draw::Ok); // 40
        assert_eq!(led.draw_tokens(40), Draw::Ok); // 80
        assert_eq!(led.draw_tokens(40), Draw::Halted); // 80+40 > 100: rejected, tree halts
                                                       // Recorded spend never crosses the ceiling — the over-draw is not counted.
        assert_eq!(led.spent_tokens(), 80);
        assert!(led.spent_tokens() <= 100);
    }

    #[test]
    fn a_contract_cannot_raise_the_ceiling() {
        let led = Ledger::new(&containment()); // 100 remaining
                                               // A child asking for 500 tokens is capped at what the tree has left.
        assert_eq!(led.effective_token_budget(Some(500)), 100);
        // After the tree spends 70, a greedy child is capped at the remaining 30.
        led.draw_tokens(70);
        assert_eq!(led.remaining_tokens(), 30);
        assert_eq!(led.effective_token_budget(Some(500)), 30);
        // A child that asks for *less* than remaining keeps its tighter budget.
        assert_eq!(led.effective_token_budget(Some(10)), 10);
        // A child with no budget of its own inherits the tree's remaining.
        assert_eq!(led.effective_token_budget(None), 30);
    }

    #[test]
    fn concurrent_draws_never_overspend_the_ceiling() {
        // Many threads hammer one ledger; the single lock keeps recorded spend
        // from ever crossing the ceiling, and the successful draws sum to exactly
        // what was recorded — no double-count, no slip-past.
        use std::sync::Arc;
        use std::thread;

        let led = Arc::new(Ledger::new(&Containment::new(10_000, 64, 3, 1_000)));
        let mut handles = Vec::new();
        for _ in 0..64 {
            let l = Arc::clone(&led);
            handles.push(thread::spawn(move || {
                let mut ok = 0u64;
                for _ in 0..100 {
                    if l.draw_tokens(10) == Draw::Ok {
                        ok += 10;
                    }
                }
                ok
            }));
        }
        let granted: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(led.spent_tokens() <= 1_000, "never exceeds the ceiling");
        // Every Ok draw is accounted for exactly once.
        assert_eq!(granted, led.spent_tokens());
    }

    #[test]
    fn serde_roundtrips_and_is_stable() {
        let c = Containment {
            max_total_cost: Some(500),
            max_total_duration: Some(Duration::from_secs(3600)),
            ..containment()
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Containment = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn a_stored_config_written_before_the_rename_still_loads() {
        // The break is a rename, and this is what keeps it off an operator's
        // configuration file: the 0.31.0 key still deserializes into the 0.32.0
        // field. Without the alias the field would silently take its `Default`,
        // which for a concurrency cap is the worst possible failure — a tree that
        // suddenly runs one agent at a time and looks merely slow.
        let old = r#"{"max_total_agents":10,"max_concurrent":7,"max_depth":3,
                      "max_total_tokens":100}"#;
        let c: Containment = serde_json::from_str(old).unwrap();
        assert_eq!(c.max_concurrent_agents, 7);
    }

    #[test]
    fn the_concurrency_cap_queues_where_the_total_cap_refuses() {
        // Two slots per tier, ten agents allowed in the whole tree.
        let led = Ledger::new(&Containment::new(10, 2, 3, 100));
        let a = led.try_admit(1).expect("slot 1");
        let _b = led.try_admit(1).expect("slot 2");

        // The third child does NOT get a refusal — the cap it met throttles.
        assert!(led.try_admit(1).is_none(), "the tier is full");
        assert!(
            led.register_agent(1).is_ok(),
            "concurrency never refuses a registration"
        );
        assert_eq!(led.tally(1).working, 2);

        // A slot frees and the queue drains into it.
        drop(a);
        assert_eq!(
            led.tally(1),
            FleetTally {
                working: 1,
                queued: 0,
                done: 1
            }
        );
        assert!(led.try_admit(1).is_some(), "the freed slot is reusable");
    }

    #[test]
    fn each_tier_holds_its_own_slots() {
        // The deadlock argument, executed: a parent that has filled its own tier
        // can still admit children one tier down. A tree-global pool would return
        // `None` here and the tree would hang.
        let led = Ledger::new(&Containment::new(100, 1, 3, 100));
        let _parent = led.try_admit(1).expect("the only slot at tier 1");
        assert!(led.try_admit(1).is_none(), "tier 1 is full");
        assert!(
            led.try_admit(2).is_some(),
            "tier 2 has its own slot, which is what makes the wait graph acyclic"
        );
    }

    #[tokio::test]
    async fn a_queued_child_waits_and_then_runs() {
        use std::sync::Arc;

        let led = Arc::new(Ledger::new(&Containment::new(10, 1, 3, 100)));
        let held = led.try_admit(1).expect("the only slot");
        led.mark_queued(1, true);
        assert_eq!(
            led.tally(1),
            FleetTally {
                working: 1,
                queued: 1,
                done: 0
            }
        );

        let waiter = {
            let l = Arc::clone(&led);
            tokio::spawn(async move { l.admit(1).await })
        };
        // Still queued while the slot is held.
        tokio::task::yield_now().await;
        assert_eq!(led.tally(1).queued, 1, "nothing admitted it early");

        drop(held);
        let slot = waiter.await.unwrap();
        assert_eq!(
            led.tally(1),
            FleetTally {
                working: 1,
                queued: 0,
                done: 1
            }
        );
        drop(slot);
        assert_eq!(
            led.tally(1),
            FleetTally {
                working: 0,
                queued: 0,
                done: 2
            }
        );
    }

    #[test]
    fn a_restored_backlog_is_not_counted_twice_by_the_replay() {
        // What a resumed tree does: read the depth back from the store, then
        // replay the step that queued those children. The replay re-queues each
        // one, the store says "already recorded", and the depth stays put.
        let led = Ledger::from_state(&Containment::new(100, 1, 3, 1_000), 40, 3);
        led.restore_queue(&[(1, 4)]);
        assert_eq!(led.tally(1).queued, 4);

        for _ in 0..4 {
            led.mark_queued(1, false); // the store already held the row
        }
        assert_eq!(
            led.tally(1).queued,
            4,
            "the replay did not double the queue"
        );

        // A genuinely new wait still counts.
        led.mark_queued(1, true);
        assert_eq!(led.tally(1).queued, 5);
        // And the restored spend is untouched by any of it.
        assert_eq!(led.spent_tokens(), 40);
    }
}
