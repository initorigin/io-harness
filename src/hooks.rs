//! Shaping a run from `io.toml` rather than from Rust (0.28.0).
//!
//! Since 0.12.0 an application has been able to watch a run by implementing
//! [`Observer`] — one method, one enum, and a `Flow` that can stop the run. That is
//! the right shape for a Rust program and the wrong shape for the operator a terminal
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

/// The one lifecycle point a `[[hook]]` may attach to (0.42.0).
///
/// Named rather than free-form for the reason [`EVENT_NAMES`] is: a misspelling
/// that loads, installs and never fires is a check that silently approves
/// everything, and that is the worst thing this module can ship.
const AT_BEFORE_TOOL: &str = "before_tool";

/// Every lifecycle point, for the error that lists them.
const AT_NAMES: &[&str] = &[AT_BEFORE_TOOL];

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
///
/// It is `#[non_exhaustive]` from the release it becomes public, because the
/// obvious next lifecycle point takes an answer these three do not: an
/// `after_tool` hook decides about a result rather than about a call. Paying for
/// a `_ =>` arm once now is what keeps that addition from being a break, and it
/// costs nothing while the set is still three.
///
/// ```
/// use io_harness::hooks::OnFailure;
/// use io_harness::Config;
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(
///     dir.path().join("io.local.toml"),
///     "[[hook]]\non = [\"finished\"]\nrun = [\"notify\"]\n\n\
///      [[hook]]\nat = \"before_tool\"\nrun = [\"gate\"]\n",
/// )?;
///
/// let hooks = Config::discover(dir.path())?.hooks();
///
/// // Neither table wrote an `on_failure`, and the two do not get the same
/// // answer: `Default` is right for exactly one of the two kinds of hook.
/// assert_eq!(hooks.declarations()[0].on_failure(), OnFailure::Continue);
/// assert_eq!(hooks.declarations()[1].on_failure(), OnFailure::Refuse);
/// assert_eq!(OnFailure::default(), OnFailure::Continue);
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnFailure {
    /// The failure is logged and the run continues. The default, because a
    /// notification that could not be delivered is not a reason to abandon work.
    #[default]
    Continue,
    /// The failure ends the run, through [`Flow::Cancel`]. This is the whole of "a
    /// local policy check": the mechanism already existed and this is the key that
    /// reaches it.
    Cancel,
    /// The tool call does not happen, and the run continues (0.42.0). The hook's
    /// reason reaches the model, which adapts — the same shape as a policy deny.
    ///
    /// Only a lifecycle hook can take this answer, because only a lifecycle hook
    /// runs while there is still a call to stop. It is also the **default** for
    /// one: a check attached to a point that exists to stop something is not a
    /// notification.
    Refuse,
}

/// One `[[hook]]` table.
///
/// A flat struct with `deny_unknown_fields` rather than a `kind`-tagged enum, and
/// deliberately. A tagged form would need `#[serde(flatten)]` for the keys the
/// variants share, serde refuses `flatten` beside `deny_unknown_fields`, and the
/// result is the hole `[[mcp]]` already carries — a misspelled key inside the table
/// silently accepted. Exactly-one-of `append`/`run` is enforced in code instead,
/// where the error can name the table's index.
///
/// Readable through [`Hooks::declarations`] for a configuration's own tables and
/// through [`Plugin::hooks`](crate::Plugin::hooks) for a bundle's, so an
/// application can show an operator *which* hooks are installed rather than only
/// that some are. Every field is behind an accessor rather than public, because
/// one of them is not what the file said: [`Hook::on_failure`] resolves the
/// per-kind default the table left out.
///
/// **The `[[hook]]` table is a committed shape from this release.** Making the
/// type public publishes its `Serialize`/`Deserialize` derives, so the seven keys
/// below — their names, their types and their `deny_unknown_fields` strictness —
/// are API: renaming one breaks a file an operator already wrote, and an eighth
/// is additive only while it carries `#[serde(default)]`. That is accepted
/// deliberately. The table was already this module's operator-facing contract,
/// spelled out in the module docs and in `io.toml`, and keeping the type private
/// would not have made it any less committed — it would only have hidden which
/// release committed it.
///
/// ```
/// use io_harness::hooks::OnFailure;
/// use io_harness::Config;
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(
///     dir.path().join("io.local.toml"),
///     "[[hook]]\nat = \"before_tool\"\ntools = [\"write_file\"]\nrun = [\"gate\"]\n",
/// )?;
///
/// let hooks = Config::discover(dir.path())?.hooks();
/// let hook = &hooks.declarations()[0];
///
/// assert_eq!(hook.at(), Some("before_tool"));
/// assert_eq!(hook.tools().to_vec(), ["write_file"]);
/// assert!(hook.on().is_empty(), "a lifecycle hook wants no events");
/// assert_eq!(hook.run(), Some(&["gate".to_string()][..]));
/// assert_eq!(hook.append(), None);
/// assert_eq!(hook.timeout_ms(), None, "absent, not the module's own default");
/// // The one answer that is computed rather than copied.
/// assert_eq!(hook.on_failure(), OnFailure::Refuse);
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// The events this hook wants, by the wire tags [`crate::EventKind`] serializes
    /// to. Empty means every event.
    #[serde(default)]
    on: Vec<String>,
    /// The point in the tool lifecycle this hook attaches to (0.42.0), or `None`
    /// for an event hook. Exactly one of `on` and `at`.
    ///
    /// One value, `before_tool`, and the validator names it rather than pretending
    /// to a set: an `after_tool` needs a result shape to put on the child's stdin,
    /// and this crate does not have one yet.
    #[serde(default)]
    at: Option<String>,
    /// The tool names a lifecycle hook wants, or empty for every call (0.42.0).
    ///
    /// Not decoration: a lifecycle hook is a process spawn on the hottest path in
    /// the loop, and a filter is how an operator pays for the check they wanted
    /// rather than for one per read.
    #[serde(default)]
    tools: Vec<String>,
    /// A file to append one JSON line per matching event to, relative to the
    /// discovery root the configuration was loaded for — so a user-scope hook
    /// writes its log beside the project it is watching rather than beside itself.
    #[serde(default)]
    append: Option<PathBuf>,
    /// An argv to spawn with the event JSON on its stdin, in the discovery root.
    /// Never a string, so there is nothing for a shell to interpret.
    #[serde(default)]
    run: Option<Vec<String>>,
    /// What a failure of this hook does to the run, or `None` for its kind's own
    /// default — [`OnFailure::Continue`] for an event hook, [`OnFailure::Refuse`]
    /// for a lifecycle one. Read through [`Hook::on_failure`], never directly.
    #[serde(default)]
    on_failure: Option<OnFailure>,
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
        // 0.42.0 — an event hook and a lifecycle hook are different things, and a
        // table claiming both is a mistake worth naming rather than resolving by
        // precedence.
        if self.at.is_some() && !self.on.is_empty() {
            return Err(Error::Config(format!(
                "{at}: a hook attaches to events or to a lifecycle point — set `on` or `at`, \
                 not both"
            )));
        }
        match &self.at {
            Some(point) if !AT_NAMES.contains(&point.as_str()) => {
                return Err(Error::Config(format!(
                    "{at}: `{point}` is not a lifecycle point this crate has. It has: {}",
                    AT_NAMES.join(", ")
                )))
            }
            // Appending a line cannot stop a call, so a lifecycle hook that only
            // appends is a check that always passes — which is the silence this
            // module can least afford.
            Some(_) if self.run.is_none() => {
                return Err(Error::Config(format!(
                    "{at}: a lifecycle hook decides whether a call happens, so it needs `run` \
                     — an `append` cannot stop anything"
                )))
            }
            None if !self.tools.is_empty() => {
                return Err(Error::Config(format!(
                    "{at}: `tools` filters a lifecycle hook, and this one has no `at`"
                )))
            }
            None if self.on_failure == Some(OnFailure::Refuse) => {
                return Err(Error::Config(format!(
                    "{at}: `on_failure = \"refuse\"` needs a call to refuse, and an event hook \
                     runs after one"
                )))
            }
            _ => {}
        }
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
    /// them, which is the reading that makes an audit log one line — and a
    /// lifecycle hook wants none of them, whatever the tag.
    fn wants(&self, tag: &str) -> bool {
        self.at.is_none() && (self.on.is_empty() || self.on.iter().any(|n| n == tag))
    }

    /// Whether this hook wants to decide about a call to `tool` (0.42.0). An empty
    /// `tools` wants every call, the same reading `on` has.
    fn wants_tool(&self, tool: &str) -> bool {
        self.at.as_deref() == Some(AT_BEFORE_TOOL)
            && (self.tools.is_empty() || self.tools.iter().any(|n| n == tool))
    }

    /// The event tags this hook named, or empty for every event.
    ///
    /// Empty and [`Hook::at`] set is a lifecycle hook, which wants no events at
    /// all — the two are read together, never one alone.
    pub fn on(&self) -> &[String] {
        &self.on
    }

    /// The lifecycle point it attached to, or `None` for an event hook.
    pub fn at(&self) -> Option<&str> {
        self.at.as_deref()
    }

    /// The tool names a lifecycle hook filters on, or empty for every call.
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// The file it appends to, relative to the discovery root — not to the file
    /// that declared it.
    pub fn append(&self) -> Option<&Path> {
        self.append.as_deref()
    }

    /// The argv it spawns, program first. Never a string: there is nothing here
    /// for a shell to interpret.
    pub fn run(&self) -> Option<&[String]> {
        self.run.as_deref()
    }

    /// What a failure of this hook does, with its kind's default applied.
    ///
    /// A lifecycle hook that says nothing refuses the call: it was attached to a
    /// point that exists to stop something. An event hook that says nothing lets
    /// the run continue, as it has since 0.28.0.
    ///
    /// This is a resolved answer and not the key: a table that wrote no
    /// `on_failure` still has one, and it is the kind's — never the enum's own
    /// `Default`, which is [`OnFailure::Continue`] and is right for only one of
    /// the two kinds. There is deliberately no accessor for the raw
    /// `Option`, because a reader who took it would have to re-derive this rule
    /// to say anything true.
    pub fn on_failure(&self) -> OnFailure {
        self.on_failure.unwrap_or(if self.at.is_some() {
            OnFailure::Refuse
        } else {
            OnFailure::Continue
        })
    }

    /// The wall-clock ceiling the table wrote for `run`, or `None` for the
    /// module's own default. The key rather than a resolved number, because a
    /// caller that wants to *show* the table needs to know whether the operator
    /// chose the value.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    /// Do the hook's one thing, and say why if it did not happen.
    ///
    /// `dir` is the discovery root the configuration was loaded for, which is also
    /// the child's working directory — so `run = ["cargo", "fmt"]` runs against the
    /// project rather than against wherever the caller happened to be.
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

        self.spawn_and_wait(dir, line, false).map(|_| ())
    }

    /// Spawn the hook's argv with `line` on its stdin and wait, bounded.
    ///
    /// One spawn for both kinds of hook, so an event hook and a lifecycle hook
    /// cannot drift apart on the argv, the working directory, the payload or the
    /// deadline. `capture` is the single difference and it is the lifecycle
    /// hook's: its stdout becomes the reason the model reads, so it is read
    /// rather than discarded, while an event hook keeps 0.28.0's `null` on both
    /// streams — a library must not write to a caller's terminal, and reading
    /// into a `String` is not writing to one.
    fn spawn_and_wait(&self, dir: &Path, line: &str, capture: bool) -> Result<Option<String>> {
        let argv = self.run.as_ref().expect("check() proved one action exists");
        let limit = Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(if capture {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Config(format!("hook could not run `{}`: {e}", argv[0])))?;

        // Written and then dropped, so the child sees EOF. An event is small enough
        // that this cannot fill the pipe, and a child that never reads its stdin is
        // therefore not a deadlock.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{line}");
        }

        // Drained on its own thread, and only then waited on. Reading after the
        // wait would deadlock the other way round: a child that fills the pipe
        // blocks writing, never exits, and is killed at the deadline — turning a
        // chatty hook into a timeout instead of an answer.
        let drain = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut text = String::new();
                use std::io::Read;
                let _ = out.read_to_string(&mut text);
                text
            })
        });
        let status = wait_bounded(&mut child, limit);
        let said = drain
            .and_then(|h| h.join().ok())
            .map(|text| first_line(&text))
            .filter(|s| !s.is_empty());

        match status {
            Some(status) if status.success() => Ok(said),
            Some(status) => Err(Error::Config(match &said {
                Some(why) => format!("hook `{}` exited with {status}: {why}", argv[0]),
                None => format!("hook `{}` exited with {status}", argv[0]),
            })),
            None => Err(Error::Config(format!(
                "hook `{}` did not finish within {}ms and was killed",
                argv[0],
                limit.as_millis()
            ))),
        }
    }
}

/// The first non-empty line of a hook's output, bounded.
///
/// Bounded because a hook is an operator's own program and a refusal reason
/// belongs in a model's context: a hook that prints a 4 MB diff would otherwise
/// put it there. The cap is on what is *used*, never on what is read — the whole
/// stream is drained either way, or the child would block on a full pipe.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .chars()
        .take(MAX_REASON_CHARS)
        .collect()
}

/// How much of a hook's answer reaches the model.
const MAX_REASON_CHARS: usize = 4096;

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

    /// The tables these hooks were built from, so a plugin's can be added to a
    /// configuration's without a second [`Observer`] to install (0.35.0).
    ///
    /// Public since the release that made [`Hook`] public, and for the reason
    /// [`Hooks::is_empty`] alone was not enough: an application that installs
    /// hooks on an operator's behalf could say *how many* were configured and
    /// nothing about what any of them does. This is the configuration half of
    /// that answer; [`Plugin::hooks`](crate::Plugin::hooks) is the bundle half,
    /// and after [`Plugins::apply_to_hooks`](crate::Plugins::apply_to_hooks) this
    /// returns both, in that order.
    ///
    /// ```
    /// use io_harness::hooks::OnFailure;
    /// use io_harness::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// // `io.local.toml`: a `[[hook]]` is refused in the committed `io.toml`.
    /// std::fs::write(
    ///     dir.path().join("io.local.toml"),
    ///     "[[hook]]\non = [\"finished\"]\nrun = [\"notify\"]\ntimeout_ms = 250\n",
    /// )?;
    ///
    /// let hooks = Config::discover(dir.path())?.hooks();
    /// let table = &hooks.declarations()[0];
    /// assert_eq!(table.on().to_vec(), ["finished"]);
    /// assert_eq!(table.run(), Some(&["notify".to_string()][..]));
    /// assert_eq!(table.at(), None);
    /// assert_eq!(table.timeout_ms(), Some(250));
    /// // The table wrote no `on_failure`; an event hook's own default is read.
    /// assert_eq!(table.on_failure(), OnFailure::Continue);
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn declarations(&self) -> &[Hook] {
        &self.hooks
    }

    /// Validate every table, naming the one that is wrong by its index.
    pub(crate) fn check(hooks: &[Hook], path: &Path) -> Result<()> {
        for (i, hook) in hooks.iter().enumerate() {
            hook.check(i, path)?;
        }
        Ok(())
    }

    /// Ask every `before_tool` hook that wants this call whether it may happen
    /// (0.42.0).
    ///
    /// Called on the loop's own thread, before the call runs. The first hook to
    /// object decides — there is nothing for a second opinion to add once a call
    /// has been stopped, and spawning the rest would be work whose answer cannot
    /// change the outcome.
    pub(crate) fn before_tool(&self, tool: &str, payload: &str) -> ToolGate {
        for (i, hook) in self.hooks.iter().enumerate() {
            if !hook.wants_tool(tool) {
                continue;
            }
            let Err(why) = hook.spawn_and_wait(&self.dir, payload, true) else {
                continue;
            };
            // The reason, and the hook's index, never the payload: a call's
            // arguments carry a path and a file's bytes, and a warning is not the
            // place for either.
            tracing::warn!("hook[{i}] on `{tool}` refused: {why}");
            let argv0 = hook
                .run
                .as_ref()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or_else(|| "hook".to_string());
            return match hook.on_failure() {
                OnFailure::Continue => continue,
                OnFailure::Cancel => ToolGate::Cancel { argv0 },
                OnFailure::Refuse => ToolGate::Refused {
                    argv0,
                    why: why.to_string(),
                },
            };
        }
        ToolGate::Go
    }

    /// Whether any table wants to decide about tool calls at all, so a loop with
    /// no lifecycle hook does not serialize a payload nobody reads.
    pub(crate) fn gates_tools(&self) -> bool {
        self.hooks.iter().any(|h| h.at.is_some())
    }
}

/// What the `before_tool` hooks decided about one call (0.42.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolGate {
    /// Nothing objected. The call runs.
    Go,
    /// The call does not run, and the model is told why.
    Refused {
        /// The program that refused, named in the trace where a rule would be.
        argv0: String,
        /// What it said, or why it failed.
        why: String,
    },
    /// The call does not run and the run stops at the next step boundary.
    Cancel {
        /// The program that stopped it.
        argv0: String,
    },
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
                if hook.on_failure() == OnFailure::Cancel {
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
