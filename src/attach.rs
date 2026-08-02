//! Watching and answering a run from a second process.
//!
//! Until 0.33.0 a run belonged to the process that started it. [`Observer`] is an
//! in-process callback, so the events only existed where the run did; the
//! `resume_*` family reattaches to a run that has *stopped*. A run left
//! unattended and parked on an approval was therefore unreachable — the only way
//! to make progress was to kill the process, at which point
//! [`resume_with_decision`](crate::resume_with_decision) worked, on a run that
//! was no longer live.
//!
//! Two things close that. [`Broadcast`](crate::Broadcast) makes the event stream
//! durable, and [`Attach`] is what a second process opens onto it:
//!
//! ```no_run
//! use io_harness::{Attach, Decision, Store, Waiting};
//!
//! # fn main() -> io_harness::Result<()> {
//! // The same file the run is writing. Nothing is coordinated beyond that.
//! let store = Store::open("runs.db")?;
//! let mut view = Attach::to(&store, 7);
//!
//! for event in view.poll()? {
//!     println!("step {} — {:?}", event.step, event.kind);
//! }
//!
//! for waiting in view.waiting()? {
//!     if let Waiting::Approval { request_id, target, .. } = waiting {
//!         println!("it wants to write {target}");
//!         // `false` means somebody else answered first. There is no third outcome.
//!         let mine = view.answer_approval(request_id, Decision::approve())?;
//!         println!("answered by me: {mine}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Reads and decides — it does not take ownership
//!
//! `Attach` has no method that starts, resumes or steps a run, and that is the
//! mechanism rather than the advice: driving a run from an attached process is
//! not something a caller can get wrong, because the type offers no way to try.
//! The run's own durable state stays the authority throughout — an answer written
//! here is a row the owning process reads, not a transfer of control.
//!
//! The failure modes are bounded in both directions, and both are executed in
//! `tests/attach.rs` rather than argued:
//!
//! - The attaching process dying changes nothing. It holds no lock, no slot and
//!   no lease; the owner never learns it was there.
//! - The owning process dying leaves exactly the resumable run 0.7.0 has always
//!   left. An approval nobody answered is an unresolved row, and
//!   [`resume_with_decision`](crate::resume_with_decision) consumes it unchanged.
//!
//! # First answer wins
//!
//! Every answer here is a compare-and-swap on the row the run already writes, and
//! it returns whether *this* caller was the one that landed. Two operators
//! answering one approval is not hypothetical the moment a run is reachable from
//! more than one place, and a harness that lets both writes land and then acts on
//! the second has a defect nobody can see.
//!
//! # It is a poll
//!
//! [`Attach::poll`] is called by you, at whatever rate suits — a terminal redraw,
//! an HTTP long-poll, a test loop. There is no push, and the run picks up an
//! answer at its own polling interval rather than instantly.
//!
//! [`Observer`]: crate::Observer

use std::fmt;

use crate::approve::{Decision, PlanVerdict};
use crate::observe::RunEvent;
use crate::state::Store;
use crate::Result;

/// How many events one [`Attach::poll`] returns at most.
///
/// A bound rather than the whole backlog, so a reader attaching at cursor zero to
/// a run that has been going for a day does not materialise its entire history in
/// one allocation. `poll` advances its cursor by what it returned, so calling it
/// again continues — a caller draining a backlog loops until it gets fewer than
/// this many.
pub const POLL_LIMIT: usize = 512;

/// What a live run is parked on, waiting for somebody to answer (0.33.0).
///
/// Three variants because there are exactly three things a run can be holding, and
/// naming them separately is the point: a pending plan is as much a stopped run as
/// a pending approval is, and leaving it out would have made "answer what it is
/// holding" a two-thirds claim.
///
/// `#[non_exhaustive]` because a later release that gives a run a fourth way to
/// wait should not break every `match` written against this one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// An action the policy routed to a human, which nobody has decided.
    ///
    /// Answer with [`Attach::answer_approval`].
    Approval {
        /// The id [`Attach::answer_approval`] and
        /// [`resume_with_decision`](crate::resume_with_decision) both take.
        request_id: i64,
        /// The step that asked.
        step: u32,
        /// `read`, `write`, `exec` or `net`.
        act: String,
        /// The path or host the action names.
        target: String,
    },
    /// A question the agent asked that nobody has answered.
    ///
    /// Answer with [`Attach::answer_question`].
    Question {
        /// The id [`Attach::answer_question`] and
        /// [`resume_with_answer`](crate::resume_with_answer) both take.
        question_id: i64,
        /// The step that asked.
        step: u32,
        /// What was asked.
        question: String,
        /// The options offered, if it was a closed question.
        choices: Vec<String>,
    },
    /// A plan the agent proposed that no gate has reviewed.
    ///
    /// Answer with [`Attach::answer_plan`]. Until it is answered the run is held
    /// in its planning phase and writes nothing.
    Plan {
        /// The id [`Attach::answer_plan`] and
        /// [`resume_with_plan_decision`](crate::resume_with_plan_decision) both
        /// take.
        plan_id: i64,
        /// The step that proposed.
        step: u32,
        /// The steps proposed, rendered one per line.
        steps: String,
    },
}

/// Which runs an [`Attach`] is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// One run and no other.
    Run(i64),
    /// Every run in the tree rooted here, interleaved as they happened.
    Tree(i64),
}

/// A second process's view of a live run: read its events, see what it is waiting
/// on, answer it (0.33.0).
///
/// Constructed against the same store file the run is writing. Nothing else is
/// coordinated — no socket, no lock, no lease — because both processes already
/// open that file, and [`Store::open`] has set `journal_mode = WAL` and a
/// five-second [`BUSY_TIMEOUT`](crate::BUSY_TIMEOUT) since 0.12.0.
///
/// See the [module documentation](self) for the ownership boundary and the
/// first-answer-wins rule.
///
/// ```
/// use io_harness::{Attach, Broadcast, EventKind, Ignore, Observer, RunEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let dir = std::env::temp_dir().join(format!("io-attach-doc-{}", std::process::id()));
/// # std::fs::create_dir_all(&dir).unwrap();
/// # let path = dir.join("runs.db");
/// let store = Store::open(&path)?;
/// let run_id = store.start_run("port it", "openrouter")?;
///
/// // What the owning process would do: broadcast as it runs.
/// let writer = Broadcast::new(Store::open(&path)?, &Ignore);
/// writer.event(&RunEvent::new(run_id, 1, EventKind::Stalled));
///
/// // What a second process does.
/// let mut view = Attach::to(&store, run_id);
/// let seen = view.poll()?;
/// assert_eq!(seen.len(), 1);
/// assert_eq!(seen[0].kind, EventKind::Stalled);
///
/// // The cursor advanced, so a second poll with nothing new is empty.
/// assert!(view.poll()?.is_empty());
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok(())
/// # }
/// ```
pub struct Attach<'a> {
    store: &'a Store,
    target: Target,
    cursor: i64,
}

// Written out rather than derived: `Store` is not `Debug`, and the connection
// inside it has nothing worth printing.
impl fmt::Debug for Attach<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attach")
            .field("target", &self.target)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl<'a> Attach<'a> {
    /// Watch one run, from the beginning of its stream.
    pub fn to(store: &'a Store, run_id: i64) -> Self {
        Self {
            store,
            target: Target::Run(run_id),
            cursor: 0,
        }
    }

    /// Watch every run in the tree rooted at `root`, from the beginning.
    ///
    /// The events come back interleaved in the order they happened, because the
    /// cursor is globally monotonic. [`RunEvent::run_id`] and [`RunEvent::depth`]
    /// say which agent produced each one.
    pub fn to_tree(store: &'a Store, root: i64) -> Self {
        Self {
            store,
            target: Target::Tree(root),
            cursor: 0,
        }
    }

    /// Skip the backlog: start from whatever the stream has already reached.
    ///
    /// For a caller that wants to watch from now on and does not care what has
    /// already happened — a terminal that has just been opened onto a run that has
    /// been going for an hour.
    ///
    /// ```
    /// use io_harness::{Attach, EventKind, RunEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("port it", "openrouter")?;
    /// store.put_event(&RunEvent::new(run_id, 1, EventKind::Stalled))?;
    ///
    /// // Attached after that event, so it is not in this reader's stream.
    /// let mut view = Attach::to(&store, run_id).from_now()?;
    /// assert!(view.poll()?.is_empty());
    ///
    /// store.put_event(&RunEvent::new(run_id, 2, EventKind::Stalled))?;
    /// assert_eq!(view.poll()?.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_now(mut self) -> Result<Self> {
        self.cursor = self.store.event_cursor()?;
        Ok(self)
    }

    /// Resume from a cursor this reader recorded earlier.
    ///
    /// The exclusive id of the last event you handled, so nothing is repeated and
    /// nothing between then and now is lost. This is what makes an attached reader
    /// restartable: store [`Attach::cursor`] with whatever you did about the
    /// events, and come back to it.
    pub fn from_cursor(mut self, cursor: i64) -> Self {
        self.cursor = cursor;
        self
    }

    /// The id of the last event this reader has returned.
    ///
    /// Zero before the first [`Attach::poll`] on a reader that started at the
    /// beginning. Persist it to make the reader restartable.
    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    /// The events since the last poll, oldest first, and advance the cursor.
    ///
    /// At most [`POLL_LIMIT`] of them; a caller draining a long backlog calls it
    /// until it gets fewer than that. These are the same [`RunEvent`] values the
    /// owning process's own observer received — not a reconstruction of them —
    /// because [`Broadcast`](crate::Broadcast) writes the event it is passing on.
    pub fn poll(&mut self) -> Result<Vec<RunEvent>> {
        let rows = match self.target {
            Target::Run(id) => self.store.events_since(id, self.cursor, POLL_LIMIT)?,
            Target::Tree(root) => self
                .store
                .tree_events_since(root, self.cursor, POLL_LIMIT)?,
        };
        if let Some((last, _)) = rows.last() {
            self.cursor = *last;
        }
        Ok(rows.into_iter().map(|(_, e)| e).collect())
    }

    /// What the run — or every run in the tree — is currently parked on.
    ///
    /// Empty means nothing is waiting for a human, which is not the same as the
    /// run being alive: this crate does not detect whether the owning process is
    /// still there. A run whose owner died still reports what it was holding, and
    /// answering it writes a row that nothing will read until somebody resumes.
    ///
    /// ```
    /// use io_harness::{Attach, Store, Waiting};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("ship it", "openrouter")?;
    /// store.put_pending(run_id, 3, "write", "deploy/prod.yaml", None)?;
    ///
    /// let view = Attach::to(&store, run_id);
    /// assert!(matches!(
    ///     view.waiting()?.as_slice(),
    ///     [Waiting::Approval { target, .. }] if target == "deploy/prod.yaml"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    pub fn waiting(&self) -> Result<Vec<Waiting>> {
        let mut out = Vec::new();
        for run_id in self.runs()? {
            for p in self.store.unresolved_approvals(run_id)? {
                out.push(Waiting::Approval {
                    request_id: p.id,
                    step: p.step,
                    act: p.act,
                    target: p.target,
                });
            }
            for q in self.store.questions(run_id)? {
                if !q.resolved {
                    out.push(Waiting::Question {
                        question_id: q.id,
                        step: q.step,
                        question: q.question,
                        choices: q.choices,
                    });
                }
            }
            for p in self.store.plans(run_id)? {
                if !p.resolved {
                    out.push(Waiting::Plan {
                        plan_id: p.id,
                        step: p.step,
                        steps: p.plan.render(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Decide an approval the run is holding. `true` if this answer is the one it
    /// acted on.
    ///
    /// `false` means somebody else — another attached process, or the owning
    /// process's own [`Approver`](crate::Approver) — answered first, and this call
    /// changed nothing. There is no third outcome and no silent second write.
    ///
    /// [`Decision::Defer`] is rejected rather than written: deferring is the
    /// owning process saying "park this", and an attached process choosing it
    /// would leave the run exactly as it was while reporting that it had answered.
    pub fn answer_approval(&self, request_id: i64, decision: Decision) -> Result<bool> {
        let word =
            match decision {
                Decision::Approve { .. } => "approve",
                Decision::Deny { .. } => "deny",
                Decision::Defer => return Err(crate::error::Error::Config(
                    "an attached process may approve or deny, not defer: deferring would report \
                     an answer while leaving the run exactly as it was"
                        .into(),
                )),
            };
        self.store.resolve_pending(request_id, word)
    }

    /// Answer a question the run is holding. `true` if this answer is the one it
    /// acted on.
    ///
    /// Recorded with `attached` as the answering party, against `responder` for
    /// the owning process's own [`Responder`](crate::Responder), so an audit can
    /// tell which answered.
    pub fn answer_question(&self, question_id: i64, answer: &str) -> Result<bool> {
        self.store.answer_question(question_id, answer, "attached")
    }

    /// Decide a plan the run is holding. `true` if this verdict is the one it
    /// acted on.
    ///
    /// [`PlanVerdict::Revise`] works from here as it does from a gate: the
    /// correction is text the agent re-plans from, and the run stays in its
    /// planning phase writing nothing.
    pub fn answer_plan(&self, plan_id: i64, verdict: PlanVerdict) -> Result<bool> {
        self.store.decide_plan(plan_id, &verdict, "attached")
    }

    /// The run ids this view covers.
    fn runs(&self) -> Result<Vec<i64>> {
        match self.target {
            Target::Run(id) => Ok(vec![id]),
            Target::Tree(root) => self.store.tree_run_ids(root),
        }
    }
}
