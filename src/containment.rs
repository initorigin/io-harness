//! The containment boundary for a tree of agents.
//!
//! A [`Containment`] is handed in once, at the root, and caps the whole tree
//! with limits no spawned [`crate::TaskContract`] can raise: how many agents may
//! exist, how many may run at once, how deep they may nest, and an aggregate
//! spend ceiling the entire tree draws down *together*. It is serde-serializable
//! like [`crate::Policy`], so io-cli and io-studio load it from config rather
//! than hand-build it.
//!
//! The [`Ledger`] is the runtime accounting for one tree: a single shared point
//! that every agent draws its token spend and its right-to-exist from. It is the
//! one place spend is serialized, so a hundred concurrent agents cannot overspend
//! past the ceiling through a race — the critical section is a plain lock held
//! only for the arithmetic.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The caps a whole agent tree runs under. Tokens are the hard spend ceiling
/// (no price telemetry exists, so spend is counted in tokens); an optional cost
/// and duration are carried for callers that supply them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Containment {
    /// Maximum number of agents that may exist in the tree, root included.
    pub max_total_agents: u32,
    /// Maximum number of agents that may run at once.
    pub max_concurrent: u32,
    /// Maximum nesting depth, counted from the root (the root is depth 0).
    pub max_depth: u32,
    /// Aggregate token ceiling drawn down by the entire tree together.
    pub max_total_tokens: u64,
    /// Optional aggregate cost ceiling, in whatever unit the caller supplies
    /// (there is no price telemetry, so the crate never derives this itself).
    #[serde(default)]
    pub max_total_cost: Option<u64>,
    /// Optional wall-clock ceiling for the whole tree.
    #[serde(default)]
    pub max_total_duration: Option<Duration>,
}

impl Containment {
    /// A containment with token, agent, concurrency, and depth caps and no
    /// cost/duration ceiling.
    pub fn new(
        max_total_agents: u32,
        max_concurrent: u32,
        max_depth: u32,
        max_total_tokens: u64,
    ) -> Self {
        Self {
            max_total_agents,
            max_concurrent,
            max_depth,
            max_total_tokens,
            max_total_cost: None,
            max_total_duration: None,
        }
    }
}

/// Why a spawn was refused by the containment boundary. Returned to the
/// requesting agent as a typed tool result it can adapt to, never a panic.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Draw {
    /// The draw was within the ceiling; the tree may continue.
    Ok,
    /// This draw crossed the aggregate ceiling. The spend is still recorded (a
    /// step already happened and its tokens were spent), but the tree must halt
    /// as a whole — no agent gets another step.
    Halted,
}

/// Shared accounting for one agent tree: the aggregate spend and the agent
/// count, behind a single lock so concurrent draws cannot overspend.
///
/// Wrap in an [`std::sync::Arc`] and hand a clone of the arc to every agent in
/// the tree; they all draw on the one ledger.
#[derive(Debug)]
pub struct Ledger {
    max_total_tokens: u64,
    max_total_agents: u32,
    max_depth: u32,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    spent_tokens: u64,
    agents: u32,
}

impl Ledger {
    /// A fresh ledger for a tree running under `c`. The root counts as the first
    /// agent, so the ledger starts with one agent registered.
    pub fn new(c: &Containment) -> Self {
        Self {
            max_total_tokens: c.max_total_tokens,
            max_total_agents: c.max_total_agents,
            max_depth: c.max_depth,
            state: Mutex::new(State {
                spent_tokens: 0,
                agents: 1, // the root
            }),
        }
    }

    /// A ledger restored from durable state, for resuming a crashed tree: the
    /// spend and agent count are the totals already recorded in the store, so
    /// the resumed tree draws against the same continuous ceiling instead of
    /// restarting the budget at zero. `agents` already includes the root and
    /// every child previously spawned, so an adopted (already-registered) child
    /// is not re-counted on resume.
    pub fn from_state(c: &Containment, spent_tokens: u64, agents: u32) -> Self {
        Self {
            max_total_tokens: c.max_total_tokens,
            max_total_agents: c.max_total_agents,
            max_depth: c.max_depth,
            state: Mutex::new(State {
                spent_tokens,
                agents: agents.max(1),
            }),
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
}
