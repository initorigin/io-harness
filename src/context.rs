//! The observation log the model sees, and what bounds it.
//!
//! Until 0.10 the workspace loop kept one `String`, appended every tool result to
//! it, and re-sent the whole thing verbatim every turn: nothing bounded it,
//! nothing removed a read the agent had already superseded, and nothing noticed
//! when a write made an earlier read wrong. This module replaces that string with
//! a [`Ledger`] of typed [`Observation`]s — history, never mutated — and an
//! [`assemble`] step that decides, per turn, what of that history the model
//! actually sees under a [`ContextBudget`].
//!
//! Three things it does that a growing string cannot:
//!
//! - **Bounds.** One [`ContextBudget`] derives the whole prompt's ceiling *and*
//!   the per-observation cap ([`entry_cap_chars`]), so the two cannot drift apart
//!   the way four independent constants did.
//! - **Supersedes.** Two reads of one path, or two greps of one pattern, are one
//!   answer; the older becomes a one-line stub naming the newer.
//! - **Freshens.** A read the agent later wrote over is stale, and a stale read
//!   that would otherwise be carried is re-read at assembly time — through the
//!   policy, so freshening cannot read what the run may not read.
//!
//! What it deliberately does not do: nothing here shrinks what an operator can
//! audit. The store's `steps.result` keeps the full, unelided log
//! ([`Ledger::full_text`]); eliding is a decision about the *request*, not about
//! the trace.

use crate::error::Result;
use crate::policy::{Act, Effect, Policy};
use crate::state::{ContextEvent, Store};
use crate::tools::Workspace;

/// What an observation was, so assembly can reason about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsKind {
    /// A file read into context.
    Read,
    /// A content search.
    Grep,
    /// A filename search.
    Find,
    /// A file written.
    Write,
    /// A skill body loaded.
    Skill,
    /// A tool the embedding program registered.
    Tool,
    /// A tool an MCP server offered.
    Mcp,
    /// A sub-agent's composed result.
    Child,
    /// The model said something instead of calling a tool.
    Message,
    /// A tool failed, or the policy refused it.
    Error,
}

impl ObsKind {
    /// Whether a later observation of the same target replaces this one.
    ///
    /// True where the target *is* the subject of the answer: a path, a search
    /// pattern, a glob, a skill's name. False where the target is only the name
    /// of the thing that answered — a registered or MCP tool called twice with
    /// different arguments gave two different answers, and stubbing the first as
    /// "superseded" would throw one of them away.
    pub fn target_is_the_subject(self) -> bool {
        matches!(
            self,
            ObsKind::Read | ObsKind::Grep | ObsKind::Find | ObsKind::Write | ObsKind::Skill
        )
    }

    /// The word a stub uses for this kind — the same word the observation's own
    /// header uses, so a stub reads as the thing it replaced.
    pub fn label(self) -> &'static str {
        match self {
            ObsKind::Read => "read",
            ObsKind::Grep => "grep",
            ObsKind::Find => "find",
            ObsKind::Write => "wrote",
            ObsKind::Skill => "skill",
            ObsKind::Tool => "tool",
            ObsKind::Mcp => "mcp tool",
            ObsKind::Child => "child",
            ObsKind::Message => "note",
            ObsKind::Error => "error",
        }
    }
}

/// One observation exactly as it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The step it happened on.
    pub step: u32,
    /// What it was.
    pub kind: ObsKind,
    /// The path or subject the tool named, where it names one.
    pub target: Option<String>,
    /// The text the model would see, already bounded by [`entry_cap_chars`] at
    /// the point it entered the log.
    pub text: String,
}

impl Observation {
    /// One observation of `kind` about `target`.
    pub fn new(step: u32, kind: ObsKind, target: Option<String>, text: impl Into<String>) -> Self {
        Self {
            step,
            kind,
            target,
            text: text.into(),
        }
    }
}

/// The observations of one run, in order. Assembly reads it; nothing mutates
/// history.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    entries: Vec<Observation>,
}

impl Ledger {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one observation. Append-only by design: the elided view is built
    /// per turn by [`assemble`], so bounding a request never loses history.
    pub fn push(&mut self, obs: Observation) {
        self.entries.push(obs);
    }

    /// How many observations the run has made.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the run has observed nothing yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every observation, in order.
    pub fn entries(&self) -> &[Observation] {
        &self.entries
    }

    /// The whole log, unelided — what an operator reconstructing a run wants, so
    /// bounding what the model sees never bounds what can be audited.
    pub fn full_text(&self) -> String {
        self.entries.iter().map(|e| e.text.as_str()).collect()
    }

    /// The observations made on one step: what that step's trace row records.
    ///
    /// The trace stores a per-step delta rather than the whole log per step, so
    /// concatenating the rows in step order reproduces [`Ledger::full_text`]
    /// exactly while the trace stays linear in the step count instead of
    /// quadratic — which matters on the 24-hour runs 0.7.0 supports.
    pub fn text_for_step(&self, step: u32) -> String {
        self.entries
            .iter()
            .filter(|e| e.step == step)
            .map(|e| e.text.as_str())
            .collect()
    }
}

/// Estimated tokens for `text`. An estimate, never a count.
// ponytail: 4-chars-per-token heuristic — no tokenizer dependency is permitted in
// this release. Drift is recorded in the trace beside the provider's own number;
// upgrade path is a per-provider tokenizer if the recorded drift ever matters.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// How much of a request the observation log may occupy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// Absolute per-request ceiling for the assembled prompt.
    pub max_tokens: u64,
    /// Share of the token budget still unspent that the prompt may use.
    pub share: f32,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tokens: 24_000,
            share: 0.5,
        }
    }
}

/// The smallest assembled section a nearly-exhausted budget still gets: a prompt
/// too small to carry one observation is a turn the agent cannot act on.
const BUDGET_FLOOR: u64 = 2_000;

impl ContextBudget {
    /// The ceiling for this turn's assembled section.
    ///
    /// With no run token budget it is [`max_tokens`](ContextBudget::max_tokens)
    /// flat. With one, it is the configured `share` of what is *left*, so a run
    /// running out of budget spends less of it on re-sent history — floored at
    /// [`BUDGET_FLOOR`] so the last turns still send a usable prompt, and never
    /// above `max_tokens`.
    pub fn effective_tokens(&self, remaining_budget: Option<u64>) -> u64 {
        match remaining_budget {
            None => self.max_tokens,
            Some(remaining) => {
                let share = (remaining as f32 * self.share) as u64;
                self.max_tokens.min(share.max(BUDGET_FLOOR))
            }
        }
    }
}

/// Chars a single observation may contribute. Derived from the same budget as the
/// whole prompt, so the four independent constants this replaces cannot drift
/// apart.
pub fn entry_cap_chars(effective_tokens: u64) -> usize {
    // An eighth of the budget, in chars: one observation may not crowd out the
    // seven before it.
    (2_000).max(effective_tokens as usize * 4 / 8)
}

/// Bound one observation to `cap` chars, marked so the model can see what it is
/// missing and act on it.
///
/// The head is kept for most kinds; for a [`ObsKind::Read`] the tail is kept,
/// because the end of a file is what a writer needs. Never splits a char
/// boundary.
pub fn bound(text: &str, cap: usize, kind: ObsKind) -> String {
    let total = text.chars().count();
    if total <= cap {
        return text.to_string();
    }
    // "truncated" as well as "elided": one word for the operator reading a trace,
    // one for the model reading the prompt, and one marker rather than two.
    let mark = format!(
        "…[truncated: elided {} of {} chars — re-read or narrow the query if you need the rest]",
        commas(total - cap),
        commas(total)
    );
    let at = |n: usize| {
        text.char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(text.len())
    };
    if kind == ObsKind::Read {
        format!("{mark}\n{}", &text[at(total - cap)..])
    } else {
        format!("{}\n{mark}", &text[..at(cap)])
    }
}

/// `41220` -> `41,220`. Sizes in a stub are for a human and a model to judge
/// "is that worth re-reading", and unseparated digits read as noise.
fn commas(n: usize) -> String {
    let d = n.to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The observation section for one turn, and what it cost.
#[derive(Debug, Clone, Default)]
pub struct Assembled {
    /// The text to put in the prompt.
    pub text: String,
    /// Observations carried whole.
    pub carried: usize,
    /// Observations replaced by a one-line stub.
    pub stubbed: usize,
    /// Stale reads re-read at assembly time (whether or not the re-read worked).
    pub reread: usize,
    /// Estimated tokens for `text` — see [`estimate_tokens`].
    pub est_tokens: u64,
}

/// How one entry is going to appear this turn.
enum Shape {
    /// Carried, with the text to carry (a re-read entry's text is the fresh one).
    Whole(String),
    /// Elided, with the reason.
    Stub(String),
}

/// Build the observation section the model sees this turn.
///
/// Rules, in order: supersession (a later observation of the same kind and target
/// replaces an earlier one), invalidation (a write makes an earlier read of that
/// path stale), re-read (a stale read that would otherwise be carried is refreshed
/// through the policy), fit (newest first, whole while it fits, stubs after), and
/// chronological emission so the model reads the run forwards.
///
/// One `assembled` trace row per turn, plus one per re-read. Never a row per stub.
pub async fn assemble(
    ledger: &Ledger,
    budget_tokens: u64,
    ws: Option<&Workspace>,
    policy: &Policy,
    store: &Store,
    run_id: i64,
    step: u32,
) -> Result<Assembled> {
    let entries = ledger.entries();
    let n = entries.len();
    let cap = entry_cap_chars(budget_tokens);
    let mut out = Assembled::default();

    // 1. Supersession, and 2. invalidation. Both are "is there a later entry
    // that makes this one not the current answer".
    let superseded: Vec<Option<u32>> = (0..n)
        .map(|i| {
            if !entries[i].kind.target_is_the_subject() {
                return None;
            }
            entries[i].target.as_ref().and_then(|t| {
                entries[i + 1..]
                    .iter()
                    .find(|l| l.kind == entries[i].kind && l.target.as_deref() == Some(t.as_str()))
                    .map(|l| l.step)
            })
        })
        .collect();
    let invalidated: Vec<Option<u32>> = (0..n)
        .map(|i| {
            if entries[i].kind != ObsKind::Read {
                return None;
            }
            entries[i].target.as_ref().and_then(|t| {
                entries[i + 1..]
                    .iter()
                    .find(|l| l.kind == ObsKind::Write && l.target.as_deref() == Some(t.as_str()))
                    .map(|l| l.step)
            })
        })
        .collect();

    // 3. Re-read. A stale read is worth carrying only as its *current* contents,
    // so it is refreshed here — through the policy, at this step, because the
    // read the model would otherwise trust was decided many steps ago.
    let mut shapes: Vec<Option<Shape>> = (0..n).map(|_| None).collect();
    for i in 0..n {
        let (Some(wrote_at), None) = (invalidated[i], superseded[i]) else {
            continue;
        };
        let target = entries[i].target.clone().unwrap_or_default();
        out.reread += 1;
        match refresh(ws, policy, &target, cap) {
            Ok(fresh) => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::reread(step, format!("{target} (written at step {wrote_at})")),
                )?;
                shapes[i] = Some(Shape::Whole(format!(
                    "\n[read {target}] (re-read at step {step}; the read at step {} was invalidated \
                     by the write at step {wrote_at})\n{fresh}\n",
                    entries[i].step
                )));
            }
            Err(why) => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::reread_refused(step, format!("{target}: {why}")),
                )?;
                shapes[i] = Some(Shape::Stub(format!(
                    "invalidated by the write at step {wrote_at}; the re-read at step {step} could \
                     not be done ({why}) — read it yourself"
                )));
            }
        }
    }

    // 4. Fit: newest first, whole while the running total stays inside the
    // ceiling; once one does not fit, every older entry is a stub. Superseded and
    // stale-unrefreshable entries never consume budget — they are stubs already.
    let mut used = 0u64;
    let mut whole = vec![false; n];
    for i in (0..n).rev() {
        if superseded[i].is_some() || matches!(shapes[i], Some(Shape::Stub(_))) {
            continue;
        }
        let text = match &shapes[i] {
            Some(Shape::Whole(t)) => t.as_str(),
            _ => entries[i].text.as_str(),
        };
        let t = estimate_tokens(text);
        if used + t > budget_tokens {
            break;
        }
        used += t;
        whole[i] = true;
    }

    // 5. Emit chronologically, so the model reads the run forwards.
    // ponytail: one stub line per elided entry, so a very long run's section still
    // creeps up by ~20 tokens a step. Collapse consecutive stubs into one line if
    // that residual ever matters; naming each one is worth more while it does not.
    for i in 0..n {
        let e = &entries[i];
        if whole[i] {
            out.carried += 1;
            match &shapes[i] {
                Some(Shape::Whole(t)) => out.text.push_str(t),
                _ => out.text.push_str(&e.text),
            }
            continue;
        }
        out.stubbed += 1;
        let why = match (&shapes[i], superseded[i]) {
            (_, Some(at)) => format!("superseded by the {} at step {at}", e.kind.label()),
            (Some(Shape::Stub(why)), _) => why.clone(),
            _ => format!(
                "{} chars, older than the current context window — re-run if you need it",
                commas(e.text.chars().count())
            ),
        };
        let subject = match &e.target {
            Some(t) => format!("{} {t}", e.kind.label()),
            None => e.kind.label().to_string(),
        };
        out.text
            .push_str(&format!("\n[{subject}] (elided: {why})\n"));
    }

    out.est_tokens = estimate_tokens(&out.text);
    store.record_context_event(
        run_id,
        &ContextEvent::assembled(
            step,
            format!(
                "carried={} stubbed={} reread={}",
                out.carried, out.stubbed, out.reread
            ),
            out.est_tokens,
        ),
    )?;
    Ok(out)
}

/// Re-read `target`'s current contents for assembly, or say why not.
///
/// Two guards, both the same ones the read being refreshed passed. The policy
/// decides first, so freshening cannot read what the run itself may not read, and
/// `Effect::Ask` is a refusal here because assembly has no approver and no turn to
/// spend on one. The read itself then goes through [`Workspace::read_file`], whose
/// `resolve` is what keeps a target inside the root — checking the policy while
/// reading the filesystem directly would copy the wrong half of the pair.
fn refresh(
    ws: Option<&Workspace>,
    policy: &Policy,
    target: &str,
    cap: usize,
) -> std::result::Result<String, String> {
    let Some(ws) = ws else {
        return Err("this run has no workspace to re-read from".into());
    };
    let verdict = policy.check(Act::Read, target);
    if verdict.effect != Effect::Allow {
        let rule = verdict
            .rule
            .as_deref()
            .map(|r| format!(" by rule {r}"))
            .unwrap_or_default();
        let what = if verdict.effect == Effect::Deny {
            "the policy denies reading it"
        } else {
            // No approver and no turn to spend on one, so `Ask` is a refusal here.
            "the policy sends reading it to a human"
        };
        return Err(format!("{what}{rule}"));
    }
    match ws.read_file(target) {
        // `read_file` reads a missing path as empty rather than failing, so an
        // empty result is reported as what it is: nothing left to carry.
        Ok(body) if body.is_empty() => Err("it is now empty, or gone".into()),
        Ok(body) => Ok(bound(&body, cap, ObsKind::Read)),
        Err(e) => Err(format!("the re-read failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_four_chars_per_token_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(4_000)), 1_000);
        // Chars, not bytes: a multi-byte char is one char.
        assert_eq!(estimate_tokens("éééé"), 1);
    }

    #[test]
    fn effective_tokens_is_the_ceiling_when_no_budget_is_set() {
        let b = ContextBudget::default();
        assert_eq!(b.effective_tokens(None), 24_000);
    }

    #[test]
    fn effective_tokens_takes_the_configured_share_of_what_is_left() {
        let b = ContextBudget::default();
        assert_eq!(b.effective_tokens(Some(40_000)), 20_000);
        // Never above the absolute ceiling, however much budget is left.
        assert_eq!(b.effective_tokens(Some(10_000_000)), 24_000);
    }

    #[test]
    fn a_nearly_exhausted_budget_still_gets_a_usable_floor() {
        let b = ContextBudget::default();
        assert_eq!(b.effective_tokens(Some(10)), BUDGET_FLOOR);
        assert_eq!(b.effective_tokens(Some(0)), BUDGET_FLOOR);
        // A ceiling below the floor still wins: the floor never raises a
        // caller's explicit ceiling.
        let tiny = ContextBudget {
            max_tokens: 500,
            share: 0.5,
        };
        assert_eq!(tiny.effective_tokens(Some(0)), 500);
    }

    #[test]
    fn entry_cap_chars_is_an_eighth_of_the_budget_with_a_floor() {
        assert_eq!(entry_cap_chars(24_000), 12_000);
        assert_eq!(entry_cap_chars(2_000), 2_000);
        // The floor holds for any tiny budget, so one observation is still usable.
        assert_eq!(entry_cap_chars(0), 2_000);
    }

    #[test]
    fn bounding_keeps_the_head_for_most_kinds_and_the_tail_for_a_read() {
        let text: String = ('a'..='z').cycle().take(100).collect();
        let head = bound(&text, 10, ObsKind::Grep);
        assert!(head.starts_with(&text[..10]), "got {head}");
        assert!(head.contains("elided 90 of 100 chars"), "got {head}");
        let tail = bound(&text, 10, ObsKind::Read);
        assert!(tail.ends_with(&text[90..]), "got {tail}");
        assert!(tail.contains("elided 90 of 100 chars"), "got {tail}");
        // Under the cap, nothing is touched.
        assert_eq!(bound("short", 10, ObsKind::Read), "short");
    }

    #[test]
    fn bounding_never_splits_a_char_boundary() {
        let text = "é".repeat(100);
        for kind in [ObsKind::Read, ObsKind::Grep] {
            let b = bound(&text, 7, kind);
            assert!(b.contains("elided 93 of 100 chars"));
            assert_eq!(b.matches('é').count(), 7, "kept exactly the cap in chars");
        }
    }

    #[test]
    fn concatenating_each_steps_text_reproduces_the_whole_log() {
        let mut l = Ledger::new();
        for (step, text) in [
            (1u32, "\n[read a]\nA\n"),
            (1, "\n[grep x]\nX\n"),
            (2, "\n[wrote a] (1 chars)\n"),
            (4, "\n[read a]\nB\n"),
        ] {
            l.push(Observation::new(step, ObsKind::Read, None, text));
        }
        // The property the trace's per-step delta rests on: rows concatenated in
        // step order are the whole log, so nothing is lost by not repeating it.
        let joined: String = (0..=5).map(|s| l.text_for_step(s)).collect();
        assert_eq!(joined, l.full_text());
        assert_eq!(
            l.text_for_step(3),
            "",
            "a step with no observations is empty"
        );
        assert_eq!(l.text_for_step(1), "\n[read a]\nA\n\n[grep x]\nX\n");
    }

    #[test]
    fn only_a_target_that_is_the_subject_supersedes() {
        for kind in [
            ObsKind::Read,
            ObsKind::Grep,
            ObsKind::Find,
            ObsKind::Write,
            ObsKind::Skill,
        ] {
            assert!(kind.target_is_the_subject(), "{kind:?} names its subject");
        }
        for kind in [
            ObsKind::Tool,
            ObsKind::Mcp,
            ObsKind::Child,
            ObsKind::Message,
            ObsKind::Error,
        ] {
            assert!(
                !kind.target_is_the_subject(),
                "{kind:?} names the answerer, not the subject"
            );
        }
    }

    #[test]
    fn commas_group_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(49_220), "49,220");
        assert_eq!(commas(1_234_567), "1,234,567");
    }
}
