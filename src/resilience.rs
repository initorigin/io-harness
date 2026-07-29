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

use serde::{Deserialize, Serialize};

/// How long to wait between provider attempts.
///
/// Applied only to a failure that [`is_retryable`](crate::error::ProviderErrorKind::is_retryable);
/// an authentication failure or an unacceptable request is escalated on its first
/// occurrence rather than re-sent, because sending the same bad request again is two
/// failures instead of one.
///
/// ```
/// use std::time::Duration;
///
/// use io_harness::{ProviderErrorKind, RetryPolicy};
///
/// let policy = RetryPolicy::default();
///
/// // The failure this exists for: a provider mid-deploy answering 503. Wait half
/// // a second, then a second, then two — deterministic, so a run's behaviour is
/// // reproducible rather than jittered.
/// assert!(ProviderErrorKind::Server.is_retryable());
/// assert_eq!(policy.wait(1, None), Duration::from_millis(500));
/// assert_eq!(policy.wait(2, None), Duration::from_secs(1));
/// // Growth stops at the ceiling, so a long unattended run is never parked.
/// assert_eq!(policy.wait(30, None), policy.max);
///
/// // A rate limit that names its own wait wins outright, the ceiling included:
/// // arguing with a server about its limit is how a client earns a longer ban.
/// assert_eq!(policy.wait(1, Some(Duration::from_secs(90))), Duration::from_secs(90));
///
/// // And what is never waited on at all: a rejected key stays rejected, so it is
/// // escalated on the first failure instead of being asked twice.
/// assert!(!ProviderErrorKind::Auth.is_retryable());
///
/// // A slower schedule for a provider that rate-limits aggressively.
/// let gentle = RetryPolicy { base: Duration::from_secs(2), max: Duration::from_secs(60) };
/// assert_eq!(gentle.wait(3, None), Duration::from_secs(8));
/// ```
/// `Serialize`/`Deserialize` since 0.19.0, so a schedule can come from a config
/// file. Both fields cross the wire as **milliseconds** — `base_ms` and
/// `max_ms` — because serde's own form for a [`Duration`] is `{secs, nanos}`,
/// which nobody would write in a config by hand. Each is `#[serde(default)]`, so
/// a file may name one and leave the other at its default.
///
/// ```
/// use std::time::Duration;
/// use io_harness::RetryPolicy;
///
/// let gentle: RetryPolicy = serde_json::from_str(r#"{"base_ms": 2000}"#).unwrap();
/// assert_eq!(gentle.base, Duration::from_secs(2));
/// assert_eq!(gentle.max, RetryPolicy::default().max, "what a file omits keeps its default");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    /// Wait before the first retry.
    #[serde(rename = "base_ms", with = "millis")]
    pub base: Duration,
    /// Ceiling for one wait, so exponential growth cannot park a run for an hour.
    #[serde(rename = "max_ms", with = "millis")]
    pub max: Duration,
}

/// A [`Duration`] as whole milliseconds — the config-file form of the two
/// [`RetryPolicy`] fields. Sub-millisecond precision is not representable and is
/// not wanted: a retry schedule measured in microseconds is a schedule nobody
/// meant to write.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub(super) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
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
///
/// ```
/// use io_harness::{Progress, Progressing, StallPolicy};
///
/// // Patient: five unproductive repeats before the nudge, and up to two nudges
/// // before the run is ended. A longer window costs budget when an agent really
/// // is stuck; a shorter one risks calling a slow exploration phase a stall.
/// let patient = StallPolicy { window: 5, max_replans: 2 };
///
/// // The escape hatch, and the reason `window` is not a `NonZeroU32`: zero turns
/// // stall detection off entirely and restores pre-0.11.0 behaviour exactly, for
/// // a caller whose workload legitimately repeats itself.
/// let off = StallPolicy { window: 0, max_replans: 0 };
/// let mut progress = Progress::new();
/// for _ in 0..50 {
///     assert_eq!(progress.step(off, false, "read src/lib.rs"), Progressing::Fine);
/// }
/// assert_eq!(progress.replans(), 0);
/// # let _ = patient;
/// ```
///
/// `Serialize`/`Deserialize` since 0.19.0, so an operator can set the window in
/// a config file. Both fields are `#[serde(default)]`:
///
/// ```
/// use io_harness::StallPolicy;
///
/// let patient: StallPolicy = serde_json::from_str(r#"{"window": 5}"#).unwrap();
/// assert_eq!(patient.window, 5);
/// assert_eq!(patient.max_replans, StallPolicy::default().max_replans);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
///
/// ```
/// use io_harness::{Progress, Progressing, StallPolicy};
///
/// let policy = StallPolicy::default();
/// let mut progress = Progress::new();
/// let mut ended = false;
///
/// // What a run loop does with the verdict: `Fine` carries on, `Replan` adds one
/// // directive to the context and carries on, `Stalled` is terminal. Treating
/// // `Replan` as terminal would end runs that were one nudge from working.
/// for _ in 0..6 {
///     match progress.step(policy, false, "read src/lib.rs") {
///         Progressing::Fine => {}
///         Progressing::Replan => {
///             let _directive = progress.replan_directive(policy.window, &["read src/lib.rs".into()]);
///         }
///         Progressing::Stalled => ended = true,
///     }
/// }
/// assert!(ended, "told once, still going in circles, so the run ends");
/// ```
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
///
/// ```
/// use io_harness::{Progress, Progressing, StallPolicy};
///
/// let policy = StallPolicy::default(); // three steps, one nudge
/// let mut progress = Progress::new();
///
/// // Opening a repository: four different reads that change nothing. This is what
/// // working looks like at the start of a run, and flagging it would degrade
/// // healthy runs in the name of resilience.
/// for call in ["read src/lib.rs", "grep TODO", "find *.toml", "read Cargo.toml"] {
///     assert_eq!(progress.step(policy, false, call), Progressing::Fine);
/// }
///
/// // The recorded 0.10.0 failure instead: a call already made this window, made
/// // again, with nothing written in between. Both halves now hold, so the agent
/// // is told — thirteen steps before it would have hit its step cap.
/// assert_eq!(progress.step(policy, false, "read src/lib.rs"), Progressing::Replan);
///
/// // A write that actually moved the workspace clears the window — an agent that
/// // got somewhere may repeat itself on the way to getting somewhere else.
/// assert_eq!(progress.step(policy, true, "write NOTES.md"), Progressing::Fine);
/// assert_eq!(progress.replans(), 1);
/// ```
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
