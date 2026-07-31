//! Long-running processes a run owns beyond the call that started them.
//!
//! Every other tool in this crate spawns a process and awaits it: `exec` and
//! `shell` both hold their children in a local `Vec` with `kill_on_drop(true)`,
//! so the drop that ends the dispatch also ends the processes and there is
//! nothing left to leak. A handle gives that up on purpose — a dev server, a log
//! tail or a twenty-minute build has to outlive the step that started it — and
//! everything in this module exists because of what giving it up costs.
//!
//! A process can escape in four ways and each is closed here rather than by
//! care at the call sites:
//!
//! 1. **The run ends.** [`Handles::kill_all`] is called when a run finishes,
//!    however it finishes.
//! 2. **The process exits on its own.** The reaping task notices and records the
//!    status, so a poll after exit is answered from the recorded status rather
//!    than by waiting on something that is already gone.
//! 3. **Too many at once.** [`Handles::reserve`] refuses the start rather than
//!    queueing it, because a queue is a leak with a delay. See
//!    [`MAX_LIVE_HANDLES`].
//! 4. **The process this crate is running in goes away.** Nothing in this module
//!    can help there, which is exactly why a handle recorded by a previous
//!    process is orphaned on resume and never signalled. See
//!    [`ORPHAN_REASON`].
//!
//! ## Output goes to a file, not a buffer
//!
//! Each handle's stdout and stderr are redirected to a capture file, and
//! `shell_poll` reads from a byte cursor into it. That is the whole streaming
//! design, and it is deliberately not a set of drain tasks writing into a shared
//! in-memory buffer.
//!
//! The reason is the criterion that a handle nobody polls must not be able to
//! exhaust memory. A buffer needs a bound, a bound needs a policy for what to
//! discard, and discarding the middle of a log is how an operator loses the line
//! that mattered. A file has none of those problems: the kernel writes it, the
//! poll reads a window of it, the whole of it is still there afterwards, and the
//! memory cost is the size of one poll rather than the size of the output. It is
//! also how job control has always worked, which is a good sign for a design
//! rather than a coincidence.
//!
//! ## A handle's processes are a process group
//!
//! Killing a handle by pid, or by pid and whatever the process table still says
//! descends from it, is not enough and cannot be made enough. A dev server
//! starts a package manager which starts a runtime which starts a watcher, and
//! then the package manager exits — a completely ordinary shape for the exact
//! programs this tool exists to run. The runtime and the watcher are still
//! there, they are still the run's responsibility, and the parent/child links
//! that would have led to them are gone. Nothing about walking the table
//! recovers them; they have been reparented to init and are indistinguishable
//! from anything else on the machine.
//!
//! So each stage of a handle's line is spawned as the leader of its own process
//! group (see [`own_process_group`](crate::sandbox::own_process_group)), and the
//! kill signals the group. Membership is inherited across `fork` and outlives
//! every parent in the chain, so one signal reaches the whole tree no matter
//! what shape it has grown into or which of its middles are already dead. The
//! foreground `shell` and `exec` tools keep their old spawn exactly: they are
//! awaited and dropped inside the call that made them, so they never have the
//! problem this solves.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::{Error, Result};

/// Why a handle recorded before this process started is never re-attached.
///
/// Stated once, as a constant, because it is reasoning rather than a message and
/// it is quoted in the store, in the trace and to the model: the only thing a
/// checkpoint can record about a live process is its pid, and a pid is not an
/// identity. Between the crash and the resume the operating system may have
/// given that number to something unrelated, and there is no check that
/// distinguishes the two with enough confidence to justify signalling it —
/// every "but is it still our program" test is a race between the check and the
/// signal.
///
/// So the handle is marked, kept readable, and left alone. This is the one way
/// this crate could damage something outside its own workspace, and the cost of
/// being wrong is not a failed run, it is somebody else's process.
pub(crate) const ORPHAN_REASON: &str =
    "started by a previous process; its pid may since have been reused";

/// How many handles one run may have live at once.
///
/// A bound rather than a setting, for now. The number exists to stop a model in
/// a loop filling the host with dev servers, and any value that does that is as
/// good as any other — what matters is that going over it is a refusal rather
/// than a queue. A run that genuinely needs more than this is doing something
/// the crate should be asked about rather than quietly accommodated.
pub(crate) const MAX_LIVE_HANDLES: usize = 8;

/// How much of one poll's output is returned to the model at most.
///
/// The file keeps everything; this bounds the window. A log tail polled once
/// after ten minutes must not spend the whole context on the ten minutes.
pub(crate) const POLL_BYTES: usize = 16 * 1024;

/// What a handle is doing, as far as this process knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandleState {
    /// Started here and believed to be running.
    Running,
    /// Exited on its own. `None` is a death by signal.
    Exited(Option<i32>),
    /// Ended by [`Handles::kill`].
    Killed,
    /// Recorded by a previous process and never re-attached. Carries the reason,
    /// which an operator reads in the trace.
    ///
    /// This state is terminal and is never left. There is deliberately no
    /// transition back to `Running`: see [`ORPHAN_REASON`].
    Orphaned(String),
}

impl HandleState {
    /// Whether anything more can happen to this handle.
    pub(crate) fn is_over(&self) -> bool {
        !matches!(self, HandleState::Running)
    }

    /// How the trace names it.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            HandleState::Running => "running",
            HandleState::Exited(_) => "exited",
            HandleState::Killed => "killed",
            HandleState::Orphaned(_) => "orphaned",
        }
    }
}

/// One long-running process line, and where its output is going.
struct Record {
    /// The line as the model wrote it. Kept for the trace and for the poll's
    /// own report, so an operator reading a resumed run knows what was abandoned.
    line: String,
    /// Every process this handle spawned, in spawn order. A pipeline is several.
    /// Killing walks all of them, because killing the last stage of `a | b`
    /// leaves `a` writing into a closed pipe rather than dead.
    pids: Vec<u32>,
    /// The capture file. Absolute, inside the registry's own directory, never
    /// inside the workspace — a handle's output is not a file the agent wrote
    /// and must not appear to be one.
    capture: PathBuf,
    /// How far `shell_poll` has read. Bytes, not lines: a partial line is a real
    /// thing a running process produces and pretending otherwise loses it.
    cursor: u64,
    state: HandleState,
}

/// The live handles of one run.
///
/// Shared rather than owned by the dispatch, because a handle outlives the call
/// that made it. Interior mutability rather than `&mut` threading for the same
/// reason: every tool call sees the same registry, and the alternative is a
/// mutable borrow held across the whole run loop.
pub(crate) struct Handles {
    inner: Mutex<HashMap<u64, Record>>,
    next: AtomicU64,
    /// Where capture files live. One directory per registry, removed when the
    /// registry drops.
    dir: Option<tempfile::TempDir>,
    /// The most handles that may be live at once.
    cap: usize,
}

impl Handles {
    /// A registry admitting `cap` live handles at a time.
    pub(crate) fn new(cap: usize) -> Self {
        Handles {
            inner: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            dir: tempfile::tempdir().ok(),
            cap,
        }
    }

    /// How many handles are live — started here, not yet ended.
    pub(crate) fn live(&self) -> usize {
        self.lock().values().filter(|r| !r.state.is_over()).count()
    }

    /// Reserve an id and a capture path, refusing if the cap is already reached.
    ///
    /// Refusing here rather than after the spawn is the point: a queue would be
    /// a leak with a delay, and a process started and then rejected is a process
    /// that ran.
    pub(crate) fn reserve(&self, line: &str) -> std::result::Result<(u64, PathBuf), String> {
        let live = self.live();
        if live >= self.cap {
            return Err(format!(
                "this run already has {live} live process handles and the cap is {}; \
                 kill one with shell_kill before starting another",
                self.cap
            ));
        }
        let dir = self.dir.as_ref().ok_or_else(|| {
            "no writable temporary directory is available for a handle's output".to_string()
        })?;
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let capture = dir.path().join(format!("handle-{id}.out"));
        self.lock().insert(
            id,
            Record {
                line: line.to_string(),
                pids: Vec::new(),
                capture: capture.clone(),
                cursor: 0,
                state: HandleState::Running,
            },
        );
        Ok((id, capture))
    }

    /// Record one process a started handle spawned.
    ///
    /// Called per stage as the pipeline is built rather than once at the end,
    /// so a line that fails to spawn its third stage still leaves the first two
    /// killable. Ignores an unknown id: a handle killed while its own pipeline
    /// was still spawning is a race that ends in the right place either way,
    /// because `kill` walked what had been recorded by then and the rest die
    /// with the dropped `Child`.
    pub(crate) fn add_pid(&self, id: u64, pid: u32) {
        if let Some(r) = self.lock().get_mut(&id) {
            r.pids.push(pid);
        }
    }

    /// The processes a handle owns, in spawn order.
    ///
    /// For the store rather than for signalling — [`Handles::kill`] walks its
    /// own copy. Nothing outside this module should be signalling a pid it read
    /// from here, and the resume path in particular must not: see
    /// [`ORPHAN_REASON`].
    pub(crate) fn pids(&self, id: u64) -> Vec<u32> {
        self.lock()
            .get(&id)
            .map(|r| r.pids.clone())
            .unwrap_or_default()
    }

    /// Every handle that is still live, with its processes — for the run-ending
    /// sweep, which has to record what it is about to kill.
    pub(crate) fn live_handles(&self) -> Vec<(u64, Vec<u32>)> {
        let guard = self.lock();
        let mut v: Vec<(u64, Vec<u32>)> = guard
            .iter()
            .filter(|(_, r)| !r.state.is_over())
            .map(|(id, r)| (*id, r.pids.clone()))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// The line a handle was started with.
    pub(crate) fn line(&self, id: u64) -> Option<String> {
        self.lock().get(&id).map(|r| r.line.clone())
    }

    /// What a handle is doing.
    pub(crate) fn state(&self, id: u64) -> Option<HandleState> {
        self.lock().get(&id).map(|r| r.state.clone())
    }

    /// Record that a handle ended on its own.
    pub(crate) fn finished(&self, id: u64, code: Option<i32>) {
        if let Some(r) = self.lock().get_mut(&id) {
            if !r.state.is_over() {
                r.state = HandleState::Exited(code);
            }
        }
    }

    /// Read what a handle has produced since the last poll.
    ///
    /// Returns the text, how many bytes were skipped because the window is
    /// bounded, and whether anything is left. The cursor advances past
    /// everything read *and* everything skipped: a poll reports a gap rather
    /// than silently returning stale output from before it.
    pub(crate) fn poll(&self, id: u64) -> Result<(String, u64)> {
        let mut guard = self.lock();
        let r = guard
            .get_mut(&id)
            .ok_or_else(|| Error::Config(format!("no process handle {id} in this run")))?;
        let mut f = match std::fs::File::open(&r.capture) {
            Ok(f) => f,
            // The capture file is created by the spawn's redirect. A handle
            // polled before its first write has produced nothing, which is not
            // an error and must not read as one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
            Err(e) => return Err(Error::Io(e)),
        };
        let len = f.metadata().map_err(Error::Io)?.len();
        if len <= r.cursor {
            return Ok((String::new(), 0));
        }
        let available = len - r.cursor;
        // Over the window, the *end* is kept: the newest output is what a poll
        // is asking about, and the beginning is still in the file for the store.
        let skipped = available.saturating_sub(POLL_BYTES as u64);
        f.seek(SeekFrom::Start(r.cursor + skipped))
            .map_err(Error::Io)?;
        let mut buf = Vec::with_capacity(available.min(POLL_BYTES as u64) as usize);
        f.take(POLL_BYTES as u64)
            .read_to_end(&mut buf)
            .map_err(Error::Io)?;
        r.cursor = len;
        Ok((String::from_utf8_lossy(&buf).into_owned(), skipped))
    }

    /// End a handle and everything it spawned.
    ///
    /// Walks the recorded pids in reverse, so a pipeline's later stages go
    /// before the ones feeding them, and each goes through
    /// [`kill_tree_and_group`](crate::sandbox::kill_tree_and_group) rather than
    /// a bare signal — a package manager that spawned a runtime leaves
    /// grandchildren otherwise, and a grandchild whose parent has already
    /// exited is reachable by nothing but its process group.
    pub(crate) fn kill(&self, id: u64) -> std::result::Result<HandleState, String> {
        let (pids, was) = {
            let mut guard = self.lock();
            let r = guard
                .get_mut(&id)
                .ok_or_else(|| format!("no process handle {id} in this run"))?;
            let was = r.state.clone();
            // An orphaned handle is never signalled. Its recorded pid may
            // belong to something else entirely by now, and this is the branch
            // that would kill a stranger's program.
            if let HandleState::Orphaned(reason) = &was {
                return Err(format!(
                    "process handle {id} was started by a previous process and orphaned on \
                     resume ({reason}); it is not signalled, because the pid it recorded may \
                     since belong to something else"
                ));
            }
            if !was.is_over() {
                r.state = HandleState::Killed;
            }
            (r.pids.clone(), was)
        };
        if !was.is_over() {
            for pid in pids.into_iter().rev() {
                crate::sandbox::kill_tree_and_group(Some(pid));
            }
        }
        Ok(was)
    }

    /// Kill every live handle, for a run that is ending.
    ///
    /// Returns how many were still running, which the caller records: a run that
    /// routinely ends with live handles is a model that never cleans up, and
    /// that is worth seeing in a trace.
    pub(crate) fn kill_all(&self) -> usize {
        let live: Vec<u64> = {
            let guard = self.lock();
            guard
                .iter()
                .filter(|(_, r)| !r.state.is_over())
                .map(|(id, _)| *id)
                .collect()
        };
        let n = live.len();
        for id in live {
            let _ = self.kill(id);
        }
        n
    }

    /// Record a handle from a previous process as orphaned.
    ///
    /// It is inserted already-terminal. Nothing here signals, polls or waits.
    pub(crate) fn adopt_orphan(&self, id: u64, line: &str) {
        self.lock().insert(
            id,
            Record {
                line: line.to_string(),
                pids: Vec::new(),
                capture: PathBuf::new(),
                cursor: 0,
                state: HandleState::Orphaned(ORPHAN_REASON.to_string()),
            },
        );
        self.next.fetch_max(id + 1, Ordering::SeqCst);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Record>> {
        // A poisoned lock means a panic while a handle was being recorded. The
        // registry's invariants do not survive that, but killing what we know
        // about matters more than the invariant, so the guard is taken anyway.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Handles {
    /// A registry that goes away takes its processes with it.
    ///
    /// The run loop calls [`Handles::kill_all`] explicitly, and this is the
    /// backstop for every path that does not — a panic, an early return, an
    /// error carried out of the loop. Belt and braces on purpose: this is the
    /// failure that reaches the operator's machine rather than their run.
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_refuses_rather_than_queues() {
        let h = Handles::new(2);
        let (a, _) = h.reserve("sleep 1").expect("first");
        let (_b, _) = h.reserve("sleep 2").expect("second");
        let err = h.reserve("sleep 3").expect_err("the third is over the cap");
        assert!(err.contains("cap is 2"), "{err}");
        // Ending one makes room again, which is what makes the cap a bound on
        // concurrency rather than on the number of handles a run may ever start.
        h.finished(a, Some(0));
        h.reserve("sleep 4").expect("after one ended");
    }

    #[test]
    fn an_orphaned_handle_is_never_signalled() {
        let h = Handles::new(4);
        h.adopt_orphan(7, "npm run dev");
        let err = h.kill(7).expect_err("an orphan is not killed");
        assert!(err.contains("orphaned on resume"), "{err}");
        assert!(err.contains("may since belong to something else"), "{err}");
        assert_eq!(
            h.state(7),
            Some(HandleState::Orphaned(ORPHAN_REASON.to_string()))
        );
    }

    #[test]
    fn an_orphan_does_not_reuse_a_live_id() {
        let h = Handles::new(4);
        h.adopt_orphan(3, "old");
        let (id, _) = h.reserve("new").expect("reserve");
        assert!(
            id > 3,
            "a new handle must not take an orphan's id: got {id}"
        );
    }

    #[test]
    fn a_poll_returns_only_what_is_new() {
        let h = Handles::new(4);
        let (id, capture) = h.reserve("tail -f log").expect("reserve");
        std::fs::write(&capture, b"first\n").expect("write");
        let (text, skipped) = h.poll(id).expect("poll");
        assert_eq!(text, "first\n");
        assert_eq!(skipped, 0);
        // Nothing new: an empty poll, not the same output twice.
        let (text, _) = h.poll(id).expect("poll again");
        assert_eq!(text, "");
        std::fs::write(&capture, b"first\nsecond\n").expect("append");
        let (text, _) = h.poll(id).expect("third poll");
        assert_eq!(text, "second\n");
    }

    #[test]
    fn a_poll_before_any_output_is_empty_rather_than_an_error() {
        let h = Handles::new(4);
        let (id, _) = h.reserve("sleep 5").expect("reserve");
        let (text, skipped) = h.poll(id).expect("a handle that has written nothing");
        assert_eq!(text, "");
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_poll_over_the_window_keeps_the_end_and_reports_the_gap() {
        let h = Handles::new(4);
        let (id, capture) = h.reserve("noisy").expect("reserve");
        let big = vec![b'x'; POLL_BYTES + 500];
        let mut body = big.clone();
        body.extend_from_slice(b"TAIL");
        std::fs::write(&capture, &body).expect("write");
        let (text, skipped) = h.poll(id).expect("poll");
        assert!(text.ends_with("TAIL"), "the newest output is what is kept");
        assert_eq!(skipped, 504, "the gap is reported, not hidden");
        // The cursor moved past everything, including what was skipped, so the
        // next poll does not re-deliver the middle of the log.
        let (text, _) = h.poll(id).expect("second poll");
        assert_eq!(text, "");
    }

    #[test]
    fn killing_an_unknown_handle_says_so() {
        let h = Handles::new(4);
        let err = h.kill(99).expect_err("no such handle");
        assert!(err.contains("no process handle 99"), "{err}");
    }
}
