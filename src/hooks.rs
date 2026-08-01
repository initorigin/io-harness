//! Shaping a run from `io.toml` rather than from Rust (0.28.0).
//!
//! Since 0.12.0 an application has been able to watch a run by implementing
//! [`Observer`] — one method, one enum, and a `Flow` that can stop the run. That is
//! the right shape for a Rust program and the wrong shape for the operator io-cli
//! serves, who has a config file and a shell script and no reason to own a compiler.
//! A `[[hook]]` table is the same capability reached from the file: name the events
//! you want, name a path to append them to or an argv to run, and the audit log, the
//! notification, the formatter or the local policy check is configuration.
//!
//! [`Config::hooks`](crate::Config::hooks) builds a [`Hooks`], which *is* an
//! [`Observer`]. The caller installs it exactly as it installs its own, so nothing
//! in any run loop changes and nothing is loaded that the caller did not load.
//!
//! ## Two actions, and no third
//!
//! `append` writes one JSON line per matching event — the same serialization
//! [`RunEvent`] has carried since 0.12.0, so no format was invented for this. `run`
//! spawns a fixed argv with that JSON on the child's stdin.
//!
//! There is no shell anywhere. The argv is a TOML array and stays an array, which is
//! the discipline `${cmd:}` and the `exec` tool already hold: this crate never hands
//! a string to a shell, so a hook has no metacharacter surface beyond its own
//! arguments.
//!
//! ## Refused in the project scope, whole
//!
//! 0.27.0 refused `${cmd:}` in `io.toml` because parsing a file must not be able to
//! run a command, and `io.toml` is the file a `git clone` delivers. A hook that runs
//! an argv is that primitive arriving one release later. A hook that *appends* is a
//! write to a path a stranger chose, which is the same hazard by a shorter route —
//! so the whole array is refused there, not its executing half, and `io.local.toml`
//! or the user-scope file is the stated alternative.
//!
//! ## A hook runs inside the run loop
//!
//! [`Observer::event`] is synchronous and returns a [`Flow`] the loop acts on
//! immediately, so a hook blocks the step that emitted the event. That is what makes
//! `on_failure = "cancel"` possible at all — an asynchronous hook could not refuse
//! anything in time — and it is why an executing hook is bounded by `timeout_ms`
//! and why hooking a hot event like `token` with a `run` action is an operator's
//! decision to spawn a process per streamed token. The guide says so beside the
//! feature rather than in a footnote.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::observe::{Flow, Observer, RunEvent, EVENT_NAMES};

/// How long an executing hook may take before it is killed, when the table says
/// nothing.
///
/// Five seconds is long enough for a notification, a formatter over one file, and a
/// policy script, and short enough that an operator who wires a hook to a hot event
/// notices rather than wonders. A hook that legitimately takes longer says so.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// How often the bounded wait asks whether the child has finished.
///
/// A poll rather than a blocking wait because [`Observer::event`] is synchronous and
/// `std::process::Child` has no wait-with-deadline. Five milliseconds is under the
/// cost of the spawn it is bounding, and the alternative — a timer crate — would be
/// a dependency for a loop.
const POLL: Duration = Duration::from_millis(5);

/// What a hook's failure means for the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnFailure {
    /// The failure is logged and the run continues. The default, because a
    /// notification that could not be delivered is not a reason to abandon work.
    #[default]
    Continue,
    /// The failure ends the run, through [`Flow::Cancel`]. This is the whole of "a
    /// local policy check": the mechanism already existed and this is the key that
    /// reaches it.
    Cancel,
}

/// One `[[hook]]` table.
///
/// A flat struct with `deny_unknown_fields` rather than a `kind`-tagged enum, and
/// deliberately. A tagged form would need `#[serde(flatten)]` for the keys the
/// variants share, serde refuses `flatten` beside `deny_unknown_fields`, and the
/// result is the hole `[[mcp]]` already carries — a misspelled key inside the table
/// silently accepted. Exactly-one-of `append`/`run` is enforced in code instead,
/// where the error can name the table's index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hook {
    /// The events this hook wants, by the wire tags [`crate::EventKind`] serializes
    /// to. Empty means every event.
    #[serde(default)]
    on: Vec<String>,
    /// A file to append one JSON line per matching event to, relative to the
    /// directory of the configuration that declared it.
    #[serde(default)]
    append: Option<PathBuf>,
    /// An argv to spawn with the event JSON on its stdin. Never a string, so there
    /// is nothing for a shell to interpret.
    #[serde(default)]
    run: Option<Vec<String>>,
    /// What a failure of this hook does to the run.
    #[serde(default)]
    on_failure: OnFailure,
    /// The wall-clock ceiling on `run`, in milliseconds.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Hook {
    /// Reject what deserialization cannot: a table with neither action or both, and
    /// an event name this crate does not emit.
    ///
    /// The event check is why [`EVENT_NAMES`] exists. A misspelled tag would
    /// otherwise be a hook that loads, installs, and never fires — a silence, and the
    /// failure mode this module can least afford.
    fn check(&self, index: usize, path: &Path) -> Result<()> {
        let at = format!("{}: key `hook[{index}]`", path.display());
        match (&self.append, &self.run) {
            (None, None) => {
                return Err(Error::Config(format!(
                    "{at}: a hook needs an action — set `append` to a path or `run` to an argv"
                )))
            }
            (Some(_), Some(_)) => {
                return Err(Error::Config(format!(
                    "{at}: a hook has one action — set `append` or `run`, not both"
                )))
            }
            _ => {}
        }
        if self.run.as_ref().is_some_and(Vec::is_empty) {
            return Err(Error::Config(format!("{at}: `run` names no program")));
        }
        for name in &self.on {
            if !EVENT_NAMES.contains(&name.as_str()) {
                return Err(Error::Config(format!(
                    "{at}: `{name}` is not an event this crate emits. It emits: {}",
                    EVENT_NAMES.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Whether this hook wants an event carrying `tag`. An empty `on` wants all of
    /// them, which is the reading that makes an audit log one line.
    fn wants(&self, tag: &str) -> bool {
        self.on.is_empty() || self.on.iter().any(|n| n == tag)
    }

    /// Do the hook's one thing, and say why if it did not happen.
    ///
    /// `dir` is the directory of the configuration that declared the hook, which is
    /// what `${file:...}` already resolves against — a relative path in a file means
    /// a path beside that file.
    fn fire(&self, dir: &Path, line: &str, lock: &Mutex<()>) -> Result<()> {
        if let Some(rel) = &self.append {
            let at = dir.join(rel);
            // One open per event rather than a held handle: a sub-agent tree emits
            // from several tasks at once, a `File` behind a lock would be the same
            // serialization with a descriptor kept open for the life of the run, and
            // an operator tailing the log wants each line to have landed.
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&at)
                .map_err(|e| {
                    Error::Config(format!("hook cannot append to {}: {e}", at.display()))
                })?;
            writeln!(f, "{line}").map_err(|e| {
                Error::Config(format!("hook cannot append to {}: {e}", at.display()))
            })?;
            return Ok(());
        }

        let argv = self.run.as_ref().expect("check() proved one action exists");
        let limit = Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .stdin(Stdio::piped())
            // Discarded rather than inherited: a library must not write to a
            // caller's terminal, and a hook that has something to say says it by
            // exiting non-zero. Stated in the guide's limits block.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Config(format!("hook could not run `{}`: {e}", argv[0])))?;

        // Written and then dropped, so the child sees EOF. An event is small enough
        // that this cannot fill the pipe, and a child that never reads its stdin is
        // therefore not a deadlock.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{line}");
        }

        match wait_bounded(&mut child, limit) {
            Some(status) if status.success() => Ok(()),
            Some(status) => Err(Error::Config(format!(
                "hook `{}` exited with {status}",
                argv[0]
            ))),
            None => Err(Error::Config(format!(
                "hook `{}` did not finish within {}ms and was killed",
                argv[0],
                limit.as_millis()
            ))),
        }
    }
}

/// Wait for `child` up to `limit`, killing it past the deadline.
///
/// `None` means the deadline won. `std::process::Child` has no wait-with-timeout and
/// [`Observer::event`] is synchronous, so this is a poll — not a `tokio` timer, which
/// would mean reaching for a runtime from inside a trait method a runtime is already
/// driving, and that is how a nested-runtime panic gets shipped.
fn wait_bounded(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            // An error asking is treated as the child being unanswerable, which is a
            // failure of the hook and not of the run.
            Err(_) => return None,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// The `[[hook]]` tables of one configuration, as an [`Observer`] (0.28.0).
///
/// Built by [`Config::hooks`](crate::Config::hooks) and installed by the caller, so
/// a hook obeys the same rule every other projection in `io.toml` obeys: the file
/// describes it, the caller loads it, and nothing happens implicitly.
///
/// ```
/// use io_harness::{Config, EventKind, Flow, Observer, RunEvent};
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// // A hook table is refused in `io.toml`, which is the file a clone delivers.
/// std::fs::write(
///     dir.path().join("io.local.toml"),
///     "[[hook]]\non = [\"refused\"]\nappend = \"audit.jsonl\"\n",
/// )?;
///
/// let hooks = Config::discover(dir.path())?.hooks();
/// hooks.event(&RunEvent::new(1, 1, EventKind::Refused {
///     act: "write".into(),
///     target: "/etc/passwd".into(),
///     rule: None,
///     layer: None,
/// }));
///
/// let log = std::fs::read_to_string(dir.path().join("audit.jsonl"))?;
/// assert!(log.contains("\"event\":\"refused\""), "{log}");
///
/// // An event the hook did not name is not written.
/// hooks.event(&RunEvent::new(1, 2, EventKind::Stalled));
/// assert_eq!(std::fs::read_to_string(dir.path().join("audit.jsonl"))?.lines().count(), 1);
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug)]
pub struct Hooks {
    hooks: Vec<Hook>,
    dir: PathBuf,
    /// Serializes appends. Sub-agents emit concurrently (0.5.0) and a JSON line can
    /// exceed the atomic-append size the platform guarantees, so two children
    /// writing at once would otherwise interleave halves of two events.
    lock: Mutex<()>,
}

impl Hooks {
    /// The hooks of a configuration, resolving relative `append` paths against `dir`.
    /// Every `append` path is created empty here if it does not exist, which is what
    /// keeps "the filter matched nothing" distinguishable from "the hook was never
    /// installed". A path that cannot be created is warned about and left to the
    /// first append to report properly.
    pub(crate) fn new(hooks: Vec<Hook>, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        for hook in &hooks {
            let Some(rel) = &hook.append else { continue };
            let at = dir.join(rel);
            if at.exists() {
                continue;
            }
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&at)
            {
                tracing::warn!("hook cannot create {}: {e}", at.display());
            }
        }
        Self {
            hooks,
            dir,
            lock: Mutex::new(()),
        }
    }

    /// Whether the file declared any. An embedder that wants to skip installing an
    /// observer that would do nothing can ask.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// assert!(Config::from_toml("[run]\nmax_steps = 3\n").unwrap().hooks().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Validate every table, naming the one that is wrong by its index.
    pub(crate) fn check(hooks: &[Hook], path: &Path) -> Result<()> {
        for (i, hook) in hooks.iter().enumerate() {
            hook.check(i, path)?;
        }
        Ok(())
    }
}

impl Observer for Hooks {
    fn event(&self, event: &RunEvent) -> Flow {
        // Serialized once for every hook: the tag decides which hooks want it and
        // the text is what they are given, and both come out of the same value.
        let Ok(value) = serde_json::to_value(event) else {
            return Flow::Continue;
        };
        let Some(tag) = value.get("event").and_then(serde_json::Value::as_str) else {
            return Flow::Continue;
        };
        let line = value.to_string();

        let mut flow = Flow::Continue;
        for (i, hook) in self.hooks.iter().enumerate() {
            if !hook.wants(tag) {
                continue;
            }
            if let Err(why) = hook.fire(&self.dir, &line, &self.lock) {
                // The reason, never the event: a `Started` carries the goal and a
                // `ToolCall` carries a target, and a warning is not the place for
                // either.
                tracing::warn!("hook[{i}] on `{tag}` failed: {why}");
                if hook.on_failure == OnFailure::Cancel {
                    flow = Flow::Cancel;
                }
            }
        }
        flow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::EventKind;

    fn hook(toml: &str) -> Hook {
        toml::from_str(toml).unwrap()
    }

    /// F2's control, at the unit level: every name the crate emits is accepted, so a
    /// rule written against a hand-typed subset is caught here.
    #[test]
    fn every_event_the_crate_emits_is_a_name_a_hook_may_use() {
        let names = EVENT_NAMES
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let h = hook(&format!("on = [{names}]\nappend = \"a.jsonl\"\n"));
        h.check(0, Path::new("io.local.toml")).unwrap();
    }

    #[test]
    fn an_event_the_crate_does_not_emit_is_refused_naming_it() {
        let h = hook("on = [\"finshed\"]\nappend = \"a.jsonl\"\n");
        let err = h.check(3, Path::new("io.local.toml")).unwrap_err();
        assert!(err.to_string().contains("finshed"), "{err}");
        assert!(err.to_string().contains("hook[3]"), "{err}");
    }

    #[test]
    fn a_hook_needs_exactly_one_action() {
        let none = hook("on = [\"stalled\"]\n");
        assert!(none
            .check(0, Path::new("io.local.toml"))
            .unwrap_err()
            .to_string()
            .contains("needs an action"));

        let both = hook("append = \"a.jsonl\"\nrun = [\"true\"]\n");
        assert!(both
            .check(1, Path::new("io.local.toml"))
            .unwrap_err()
            .to_string()
            .contains("not both"));

        let empty = hook("run = []\n");
        assert!(empty
            .check(2, Path::new("io.local.toml"))
            .unwrap_err()
            .to_string()
            .contains("names no program"));
    }

    /// An absent `on` is every event, which is what makes an audit log one line of
    /// configuration.
    #[test]
    fn a_hook_with_no_filter_wants_everything() {
        let all = hook("append = \"a.jsonl\"\n");
        for name in EVENT_NAMES {
            assert!(all.wants(name), "{name}");
        }
        let one = hook("on = [\"stalled\"]\nappend = \"a.jsonl\"\n");
        assert!(one.wants("stalled"));
        assert!(!one.wants("finished"));
    }

    #[test]
    fn an_append_hook_writes_one_json_line_per_matching_event() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::new(vec![hook("append = \"audit.jsonl\"\n")], dir.path());

        hooks.event(&RunEvent::new(1, 1, EventKind::Stalled));
        hooks.event(&RunEvent::new(1, 2, EventKind::Replan { window: 3 }));

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 2, "{log}");
        assert!(log
            .lines()
            .next()
            .unwrap()
            .contains("\"event\":\"stalled\""));
        assert!(log.lines().nth(1).unwrap().contains("\"event\":\"replan\""));
    }

    /// A hook whose filter never matches leaves an **empty** file rather than no
    /// file, so "the filter matched nothing" stays distinguishable from "the hook was
    /// never installed". That is the whole reason the path is created up front.
    #[test]
    fn a_hook_that_matches_nothing_leaves_an_empty_file_rather_than_none() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::new(
            vec![hook(
                "on = [\"question_asked\"]\nappend = \"audit.jsonl\"\n",
            )],
            dir.path(),
        );
        hooks.event(&RunEvent::new(1, 1, EventKind::Stalled));

        let at = dir.path().join("audit.jsonl");
        assert!(at.exists(), "an installed hook creates its log");
        assert_eq!(std::fs::read_to_string(&at).unwrap(), "");
    }
}
