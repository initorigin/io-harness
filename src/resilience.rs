//! What a run does when things go wrong but nothing has crashed.
//!
//! 0.7.0 made a run survive a crash. This module is the other half: surviving a
//! provider that is down, rate-limited or returning garbage, and surviving an agent
//! that has stopped making progress. Both are failures a long unattended run meets
//! routinely and neither ended anything but the run before 0.11.
//!
//! Two policies live here, because they answer the same question from two sides:
//!
//! - [`RetryPolicy`] — how long to wait before asking a provider again, and
//!   whether asking again is worth doing at all. That second half is
//!   [`ProviderErrorKind::is_retryable`](crate::error::ProviderErrorKind::is_retryable);
//!   this decides the waiting.
//! - [`StallPolicy`] and [`Progress`] — whether the agent is getting anywhere, and
//!   what to do when it is not.
//!
//! Neither invents randomness. The backoff is deterministic and documented as such
//! rather than jittered, because a jittered backoff would need a dependency this
//! release does not take and would make the waits untestable without loosening the
//! assertions until they assert nothing.

use std::time::Duration;

/// How long to wait between provider attempts.
///
/// Applied only to a failure that [`is_retryable`](crate::error::ProviderErrorKind::is_retryable);
/// an authentication failure or an unacceptable request is escalated on its first
/// occurrence rather than re-sent, because sending the same bad request again is two
/// failures instead of one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Wait before the first retry.
    pub base: Duration,
    /// Ceiling for one wait, so exponential growth cannot park a run for an hour.
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // Long enough that a provider mid-deploy has a moment to come back,
            // short enough that two retries do not dominate a step.
            base: Duration::from_millis(500),
            // A rate limit that wants longer than this says so through
            // `Retry-After`, which is honoured above this ceiling.
            max: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// How long to wait before attempt `attempt` (1 = the first retry).
    ///
    /// Doubles per attempt up to [`max`](RetryPolicy::max). A server-supplied
    /// `Retry-After` wins outright, above the ceiling included: the server knows
    /// its own limit better than a default does, and ignoring it is how a client
    /// earns a longer ban.
    pub fn wait(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(server) = retry_after {
            return server;
        }
        let shift = attempt.saturating_sub(1).min(16);
        let grown = self.base.saturating_mul(1u32 << shift);
        grown.min(self.max)
    }
}

/// When to decide an agent has stopped making progress.
///
/// The 0.10.0 live evidence recorded the failure this exists for twice: the model
/// re-read the same four files for sixteen consecutive turns and ended in
/// `StepCapReached`, having spent its whole step budget proving it was stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallPolicy {
    /// Consecutive steps that change nothing before the agent is told so.
    /// `0` disables stall detection entirely, restoring 0.10.0 behaviour exactly.
    pub window: u32,
    /// How many times one run may be told to change approach. Past this, a stall
    /// ends the run: an agent that stalls twice will not be talked out of it by a
    /// third message, and an unbounded replan loop is a way to spend a whole budget
    /// politely.
    pub max_replans: u32,
}

impl Default for StallPolicy {
    fn default() -> Self {
        Self {
            // Three steps is long enough that a read-then-think-then-write rhythm
            // is never mistaken for a stall, and short enough to catch the recorded
            // failure thirteen steps before its cap.
            window: 3,
            max_replans: 1,
        }
    }
}

/// What to do about the step just taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progressing {
    /// Nothing to do.
    Fine,
    /// The agent is going in circles and has replans left: tell it once.
    Replan,
    /// It has been told and is still going in circles. End the run.
    Stalled,
}

/// Tracks whether an agent is getting anywhere across steps.
///
/// A stall needs BOTH halves: no change to the workspace, AND a tool call this
/// window has already seen. One alone is not enough — a legitimate exploration
/// phase changes nothing either, and a run that greps four different patterns is
/// working, not stuck. What distinguished the recorded failure is that it was doing
/// the same thing over and over while nothing moved.
#[derive(Debug, Default, Clone)]
pub struct Progress {
    /// Tool-call signatures seen since the last productive step.
    seen: Vec<String>,
    unproductive: u32,
    replans: u32,
}

impl Progress {
    /// A fresh tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times this run has been told to change approach.
    pub fn replans(&self) -> u32 {
        self.replans
    }

    /// Record one step and say what to do about it.
    ///
    /// `changed` is whether the step moved the workspace — see
    /// [`Wrote::moved_the_workspace`](crate::tools::workspace::Wrote::moved_the_workspace).
    /// `signature` identifies what the step did, so a repeat can be recognised; the
    /// run loops pass the same joined tool-call string they write to the trace.
    pub fn step(&mut self, policy: StallPolicy, changed: bool, signature: &str) -> Progressing {
        if policy.window == 0 {
            return Progressing::Fine;
        }
        if changed {
            // Progress resets everything. An agent that got somewhere is allowed to
            // repeat itself on the way to getting somewhere else.
            self.seen.clear();
            self.unproductive = 0;
            return Progressing::Fine;
        }

        let repeated = self.seen.iter().any(|s| s == signature);
        self.seen.push(signature.to_string());
        self.unproductive += 1;

        if self.unproductive < policy.window || !repeated {
            return Progressing::Fine;
        }

        // Stalled. Reset the window either way, so a replanned agent gets a clean
        // `window` steps to show it changed approach rather than being condemned by
        // the history it was just told to abandon.
        self.seen.clear();
        self.unproductive = 0;
        if self.replans < policy.max_replans {
            self.replans += 1;
            Progressing::Replan
        } else {
            Progressing::Stalled
        }
    }

    /// The directive put into the agent's context when it is told to change
    /// approach.
    ///
    /// Plain about what happened and what it already tried. It is an observation in
    /// the 0.10.0 ledger like any other, so it is subject to the same budget — and
    /// it carries no target, so nothing can supersede it away.
    pub fn replan_directive(&self, window: u32, tried: &[String]) -> String {
        let mut out = format!(
            "\n[no progress] The last {window} steps changed nothing in the workspace, and you have \
             repeated a tool call you already made. Whatever you are doing is not working.\n"
        );
        if !tried.is_empty() {
            out.push_str("Already tried, to no effect:\n");
            for t in tried {
                out.push_str(&format!("- {t}\n"));
            }
        }
        out.push_str(
            "Change approach: write the file, or gather something you have not gathered yet. \
             Repeating the same call will end the run.\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> StallPolicy {
        StallPolicy {
            window: 3,
            max_replans: 1,
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling() {
        let p = RetryPolicy {
            base: Duration::from_millis(100),
            max: Duration::from_millis(500),
        };
        assert_eq!(p.wait(1, None), Duration::from_millis(100));
        assert_eq!(p.wait(2, None), Duration::from_millis(200));
        assert_eq!(p.wait(3, None), Duration::from_millis(400));
        // Capped, and it stays capped rather than overflowing.
        assert_eq!(p.wait(4, None), Duration::from_millis(500));
        assert_eq!(p.wait(64, None), Duration::from_millis(500));
    }

    #[test]
    fn a_servers_retry_after_wins_over_the_ceiling() {
        let p = RetryPolicy {
            base: Duration::from_millis(100),
            max: Duration::from_millis(500),
        };
        // The server knows its own limit; ignoring it is how a client earns worse.
        assert_eq!(
            p.wait(1, Some(Duration::from_secs(9))),
            Duration::from_secs(9)
        );
    }

    #[test]
    fn a_window_of_zero_disables_detection_entirely() {
        let off = StallPolicy {
            window: 0,
            max_replans: 1,
        };
        let mut p = Progress::new();
        for _ in 0..50 {
            assert_eq!(p.step(off, false, "read a"), Progressing::Fine);
        }
        assert_eq!(p.replans(), 0);
    }

    #[test]
    fn repeating_one_call_while_nothing_changes_is_a_stall() {
        let mut p = Progress::new();
        // The recorded 0.10.0 failure: the same read, over and over, no write.
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Replan);
        // Told once; the window restarts so it gets a clean chance.
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Stalled);
    }

    #[test]
    fn varied_calls_that_change_nothing_are_exploration_not_a_stall() {
        let mut p = Progress::new();
        // Reading four different files is how a run starts. Flagging it would
        // degrade healthy runs to add resilience, which is the worst outcome here.
        for sig in [
            "read a",
            "read b",
            "read c",
            "grep x",
            "find *.rs",
            "read d",
        ] {
            assert_eq!(p.step(policy(), false, sig), Progressing::Fine);
        }
        assert_eq!(p.replans(), 0);
    }

    #[test]
    fn changing_the_workspace_clears_the_window() {
        let mut p = Progress::new();
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        // Progress: the two repeats before it no longer count against the agent.
        assert_eq!(p.step(policy(), true, "wrote a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "read a"), Progressing::Replan);
    }

    #[test]
    fn a_write_that_changed_nothing_does_not_count_as_progress() {
        let mut p = Progress::new();
        // The whole point of the `Wrote::Unchanged` signal: writing a file back
        // exactly as it was is not movement, however many times it is done.
        assert_eq!(p.step(policy(), false, "wrote a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "wrote a"), Progressing::Fine);
        assert_eq!(p.step(policy(), false, "wrote a"), Progressing::Replan);
    }

    #[test]
    fn more_replans_than_allowed_is_a_configuration_a_caller_can_make() {
        let patient = StallPolicy {
            window: 2,
            max_replans: 3,
        };
        let mut p = Progress::new();
        let mut replans = 0;
        for _ in 0..8 {
            if p.step(patient, false, "read a") == Progressing::Replan {
                replans += 1;
            }
        }
        assert_eq!(replans, 3, "it stops telling after max_replans");
        assert_eq!(p.step(patient, false, "read a"), Progressing::Fine);
        assert_eq!(p.step(patient, false, "read a"), Progressing::Stalled);
    }

    #[test]
    fn the_directive_names_what_was_tried() {
        let p = Progress::new();
        let d = p.replan_directive(3, &["read a".into(), "read b".into()]);
        assert!(d.contains("last 3 steps changed nothing"));
        assert!(d.contains("- read a") && d.contains("- read b"));
        assert!(d.contains("Change approach"));
        // No target, so the assembler can never supersede it away.
        assert!(d.starts_with("\n[no progress]"));
    }
}
