//! read: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// What this call is contained under, decided before it is dispatched (0.48.0).
///
/// Three outcomes, and the order they are decided in is the release's own claim
/// that a requirement is *resolved* rather than discovered:
///
/// 1. The tool needs more than the contract granted — refused here, with nothing
///    spawned and nothing to attribute a permission error to.
/// 2. The tool needs less — the call is contained under the narrower of the two,
///    with the writable roots recomputed for it.
/// 3. The tool declares nothing, or exactly what it was granted — the run's own
///    containment, unchanged, which is every call made before this release.
///
/// A run that granted [`ExecMode::FullAccess`](crate::ExecMode::FullAccess) has
/// no containment at all, and nothing here invents one: `exec_sandbox` is `None`,
/// so there is nothing to narrow and nothing a declaration could be refused
/// against. That is the documented escape hatch and it stays absolute.
pub(super) enum CallMode {
    /// Run under this containment. `None` is uncontained, as before.
    Contained(Option<std::sync::Arc<crate::sandbox::ExecContainment>>),
    /// Refuse the call. The tool needs more than this run was granted.
    Refused { needed: crate::sandbox::ExecMode },
}

pub(super) fn resolve_call_mode(
    name: &str,
    custom: &Toolbox,
    exec_sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> CallMode {
    let Some(containment) = exec_sandbox else {
        // FullAccess: no backend, no roots, nothing to narrow or refuse against.
        return CallMode::Contained(None);
    };
    let granted = containment.config.mode;
    let Some(needed) = tool_mode(name, custom) else {
        return CallMode::Contained(Some(std::sync::Arc::clone(containment)));
    };
    if !needed.satisfied_by(granted) {
        return CallMode::Refused { needed };
    }
    let resolved = granted.narrower(needed);
    if resolved == granted {
        CallMode::Contained(Some(std::sync::Arc::clone(containment)))
    } else {
        CallMode::Contained(Some(std::sync::Arc::new(containment.with_mode(resolved))))
    }
}

/// The part of a read-only call that can run at the same time as another one.
///
/// Everything a call needs from the run has already been decided by the time one
/// of these exists: the policy has been consulted, an approver has answered, and
/// the target is whatever the approver left it as. What remains is the read
/// itself, which touches the workspace and the registered tool and nothing else —
/// no `Store` (`rusqlite::Connection` is `Send` and not `Sync`, so it could not
/// cross into a task even if this wanted it to), no `Watch`, no run-scoped
/// mutable state.
pub(super) enum ReadWork {
    Grep {
        pattern: String,
        path_glob: Option<String>,
    },
    Find {
        glob: String,
    },
    Read {
        target: String,
        remember: Vec<Rule>,
        /// 0.55.0 — the first line to return, 1-based, as the model asked for it.
        offset: Option<u64>,
        /// 0.55.0 — how many lines to return from `offset`.
        limit: Option<u64>,
    },
    /// 0.75.0 — one directory listing, at the target an approver left behind.
    ListDir {
        target: String,
        remember: Vec<Rule>,
    },
    /// 0.75.0 — one `git` reader, already built and already contained.
    ///
    /// The argv is settled before this exists: [`prepare_read`] and
    /// [`speculable`] both build the [`GitCmd`] and both resolve the
    /// containment, because a refusal `Git::argv` would raise has a policy row
    /// to write and this half of the batch holds no [`Store`](crate::Store) to
    /// write one with.
    Git {
        name: String,
        cmd: GitCmd,
        remember: Vec<Rule>,
        /// This call's own containment, narrowed by
        /// [`resolve_call_mode`] exactly as `dispatch` narrows it, with the
        /// run's egress answer folded in. `None` is a `FullAccess` run, which
        /// wraps nothing.
        contained: Option<std::sync::Arc<crate::sandbox::ExecContainment>>,
    },
    Custom {
        name: String,
        tool: std::sync::Arc<dyn crate::tools::Tool>,
        arguments: serde_json::Value,
        remember: Vec<Rule>,
        /// 0.65.0 — the open journal row for this call, when the tool's
        /// [`ToolRecovery`](crate::ToolRecovery) is `Indeterminate`. `None` for a
        /// replayable tool, which is journalled nowhere and costs nothing.
        attempt: Option<i64>,
    },
}

/// The line range a `read_file` call asked for, if it asked for one (0.55.0).
pub(super) fn read_range_of(arguments: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    (
        arguments.get("offset").and_then(|v| v.as_u64()),
        arguments.get("limit").and_then(|v| v.as_u64()),
    )
}

/// Take the 1-based line range the model asked for, returning the body and the
/// header note that makes a slice legible as a slice (0.55.0).
///
/// A read with no range is the whole file and says nothing new. An `offset` past
/// the end is an error naming the total rather than an empty success: an empty
/// success is exactly the answer that reads like an empty file.
pub(super) fn line_slice(
    text: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> std::result::Result<(String, String), String> {
    if offset.is_none() && limit.is_none() {
        return Ok((text.to_string(), String::new()));
    }
    // A trailing newline terminates the last line rather than starting an empty
    // one, so `a\nb\n` is two lines and an operator counting in an editor agrees.
    let lines: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect();
    let total = lines.len();
    let first = offset.unwrap_or(1).max(1) as usize;
    if first > total {
        return Err(format!(
            "offset {first} is past the end — the file has {total} lines, so there is nothing \
             at that line to read"
        ));
    }
    let count = limit.map(|l| l as usize).unwrap_or(total);
    let last = first.saturating_add(count).saturating_sub(1).min(total);
    let mut body: String = lines[first - 1..last].join("\n");
    body.push('\n');
    Ok((body, format!(" lines {first}-{last} of {total}")))
}

/// The refusal for a read whose content will not fit (0.55.0).
///
/// It names the file, the size, the ceiling and both ways forward, because a
/// refusal a model cannot act on turns a working run into a stuck one.
///
/// **It also names *which* ceiling.** A read can be over the operator's
/// `[run] max_read_chars`, which is a fixed number somebody chose, or over what
/// this turn's remaining budget can carry, which moves as the run spends. The
/// two call for different answers — raise the key, or read a range now — so a
/// message that covered both would tell the model to try the wrong one half the
/// time.
pub(super) fn over_ceiling(
    target: &str,
    size: usize,
    budget_cap: usize,
    max_read: Option<usize>,
    offset: Option<u64>,
) -> String {
    let suggestion = if offset.is_some() {
        "ask for fewer lines".to_string()
    } else {
        format!(
            "read a range instead — `{{\"path\": \"{target}\", \"offset\": 1, \"limit\": 200}}`"
        )
    };
    // The operator's ceiling is reported whenever it is the one that bit, which
    // includes the case where both would have: a number somebody set is the one
    // they can act on.
    match max_read {
        Some(operator) if size > operator => format!(
            "{target} is {size} chars, over the {operator}-char ceiling set by \
             `[run] max_read_chars`, so nothing was read. A shortened read would look like the \
             whole file. To proceed, {suggestion}, or raise that key."
        ),
        _ => format!(
            "{target} is {size} chars, over the {budget_cap}-char ceiling this turn's remaining \
             context budget allows, so nothing was read. A shortened read would look like the \
             whole file. To proceed, {suggestion} — the ceiling this one is measured against \
             moves as the run spends, so `[run] max_read_chars` is what makes it predictable."
        ),
    }
}

impl ReadWork {
    /// The journal row this work must close when it returns (0.65.0).
    ///
    /// Read before the work is moved — into a `JoinSet` task by
    /// [`read_batch`], or into `run` by the serial arm — because closing is the
    /// caller's job: `run` holds no [`Store`](crate::Store) and must not, since
    /// the batch runs it on a spawned task and a `Store` owns a rusqlite
    /// `Connection`.
    pub(super) fn attempt(&self) -> Option<i64> {
        match self {
            ReadWork::Custom { attempt, .. } => *attempt,
            _ => None,
        }
    }

    /// Whether performing this work spawns a contained process (0.75.0).
    ///
    /// Read for the reason [`ReadWork::attempt`] is read, and by the same
    /// caller: the containment a spawn ran under is a row, a row needs a
    /// [`Store`](crate::Store), and the concurrent half holds none. The creation
    /// is recorded before the work is queued and the destruction after it joins,
    /// so a git reader that ran inside a batch leaves the rows `dispatch` leaves
    /// for the same call rather than none.
    pub(super) fn spawns(&self) -> bool {
        matches!(
            self,
            ReadWork::Git {
                contained: Some(_),
                ..
            }
        )
    }

    /// (0.75.0) The containment this work will actually spawn under, narrowed by
    /// [`resolve_call_mode`] and carrying the run's egress answer.
    ///
    /// The speculative path needs it *before* the work moves into its task, so
    /// the loop can record the containment that was enforced rather than the
    /// run's own grant. Those two differ by construction and by design: a git
    /// reader runs `read-only` inside a run granting `workspace-write`, and a row
    /// naming the run's grant would say the opposite of what confined the spawn.
    pub(super) fn contained(&self) -> Option<&std::sync::Arc<crate::sandbox::ExecContainment>> {
        match self {
            ReadWork::Git { contained, .. } => contained.as_ref(),
            _ => None,
        }
    }

    /// Perform it. The same code the serial path runs, so the two cannot drift:
    /// a batched read and a lone read are the same function called from two
    /// places.
    pub(super) async fn run(
        self,
        ws: &Workspace,
        cap: usize,
        max_read: Option<usize>,
        run_id: i64,
        step: u32,
    ) -> Dispatched {
        match self {
            ReadWork::Grep { pattern, path_glob } => {
                match ws.grep(&pattern, path_glob.as_deref()) {
                    Ok(hits) => {
                        let shown: Vec<String> = hits
                            .iter()
                            .take(OBS_GREP_CAP)
                            .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                            .collect();
                        Dispatched::seen(
                            format!("grep {pattern:?} ({} hits)", hits.len()),
                            bound(
                                &format!("\n[grep {pattern:?}]\n{}\n", shown.join("\n")),
                                cap,
                                ObsKind::Grep,
                            ),
                            ObsKind::Grep,
                            Some(pattern),
                        )
                    }
                    Err(e) => Dispatched::go("grep error", format!("\n[grep error] {e}\n")),
                }
            }
            ReadWork::Find { glob } => match ws.find(&glob) {
                Ok(paths) => Dispatched::seen(
                    format!("find {glob:?} ({} paths)", paths.len()),
                    bound(
                        &format!("\n[find {glob:?}]\n{}\n", paths.join("\n")),
                        cap,
                        ObsKind::Find,
                    ),
                    ObsKind::Find,
                    Some(glob),
                ),
                Err(e) => Dispatched::go("find error", format!("\n[find error] {e}\n")),
            },
            ReadWork::Read {
                target,
                remember,
                offset,
                limit,
            } => match ws.read_typed(&target) {
                // 0.55.0 — the read has a type. Text carries the encoding it was
                // decoded from when that is not the ordinary one; everything else
                // is named rather than decoded, because a binary read used to
                // arrive here as an empty string and read like an empty file.
                Ok(crate::tools::FileContent::Text { text, encoding }) => {
                    let mut note = if encoding == crate::tools::TextEncoding::Utf8 {
                        String::new()
                    } else {
                        format!(" ({})", encoding.as_str())
                    };
                    let body = match line_slice(&text, offset, limit) {
                        Ok((body, range)) => {
                            note.push_str(&range);
                            body
                        }
                        Err(why) => {
                            return Dispatched::go(
                                format!("read {target} refused"),
                                format!("\n[read {target} error] {why}\n"),
                            )
                        }
                    };
                    // 0.55.0 — whole, the range that was asked for, or nothing.
                    // A truncated read has the shape of a successful one and
                    // nothing downstream can tell the difference, so the read
                    // that will not fit returns no content at all.
                    let size = body.chars().count();
                    if size > cap || max_read.is_some_and(|m| size > m) {
                        return Dispatched::go(
                            format!("read {target} refused"),
                            format!(
                                "\n[read {target} error] {}\n",
                                over_ceiling(&target, size, cap, max_read, offset)
                            ),
                        );
                    }
                    Dispatched::Continue {
                        decision: format!("read {target}"),
                        obs: format!("\n[read {target}{note}]\n{body}\n"),
                        kind: ObsKind::Read,
                        target: Some(target),
                        changed: false,
                        remember,
                    }
                }
                Ok(other) => {
                    let why = other
                        .refusal(&target)
                        .unwrap_or_else(|| format!("{target} is not text"));
                    Dispatched::go(
                        format!("read {target} refused"),
                        format!("\n[read {target} error] {why}\n"),
                    )
                }
                Err(e) => Dispatched::go("read error", format!("\n[read error] {e}\n")),
            },
            // 0.75.0 — a listing is a filename answer about a path, like
            // `find`'s, so it carries `find`'s [`ObsKind`]: a later listing of
            // the same directory is the same question asked again and
            // supersedes this one.
            ReadWork::ListDir { target, remember } => match ws.list_dir(&target) {
                Ok(entries) => {
                    let shown: Vec<String> = entries
                        .iter()
                        .take(OBS_LIST_DIR_CAP)
                        .map(Entry::to_string)
                        .collect();
                    // Said in the listing rather than only in the count the
                    // trace keeps: the model reads the text and nothing else,
                    // and what it does about a truncated directory is a decision
                    // it can only make if it is told.
                    let note = match entries.len() - shown.len() {
                        0 => String::new(),
                        n => format!(
                            "\n[showing {} of {} entries; {n} not listed — list a subdirectory \
                             or use find to narrow]",
                            shown.len(),
                            entries.len()
                        ),
                    };
                    Dispatched::Continue {
                        decision: format!("list_dir {target} ({} entries)", entries.len()),
                        obs: bound(
                            &format!("\n[list_dir {target}]\n{}{note}\n", shown.join("\n")),
                            cap,
                            ObsKind::Find,
                        ),
                        kind: ObsKind::Find,
                        target: Some(target),
                        changed: false,
                        remember,
                    }
                }
                Err(e) => Dispatched::go("list_dir error", format!("\n[list_dir error] {e}\n")),
            },
            // 0.75.0 — the spawn, and only the spawn. Every question about this
            // call was answered before it was queued: the policy was asked about
            // `Act::Exec` on `git` and `Act::Read` on `.git` and on each path,
            // the argv was built, and the containment was resolved. `.gated()`
            // because the `Act::Exec` question has already been through the run
            // loop's approval gate — a second raw policy check inside `Git::run`
            // would read the same `Ask` and refuse a call a human had approved.
            ReadWork::Git {
                name,
                cmd,
                remember,
                contained,
            } => {
                let git = Git::new(ws.policy(), ws.root(), cap)
                    .contained(contained)
                    .gated();
                match git.run(&cmd).await {
                    Ok(GitOutcome::Unavailable { reason }) => Dispatched::go(
                        "git unavailable",
                        format!(
                            "\n[git unavailable] {reason}. This workspace cannot be worked as a \
                             git repository; carry on without it.\n"
                        ),
                    ),
                    Ok(out) => {
                        let GitOutcome::Ran {
                            code,
                            stdout,
                            stderr,
                        } = &out
                        else {
                            unreachable!()
                        };
                        let ok = out.ok();
                        let body = if stdout.trim().is_empty() && !ok {
                            stderr.clone()
                        } else {
                            stdout.clone()
                        };
                        // A git that ran and failed is an observation, not a run
                        // failure — the same treatment a malformed regex gets
                        // from `grep`. The model reads the message and adapts.
                        Dispatched::Continue {
                            decision: format!(
                                "{name} {}",
                                if ok {
                                    "ok".to_string()
                                } else {
                                    format!(
                                        "exit {}",
                                        code.map_or("signal".into(), |c| c.to_string())
                                    )
                                }
                            ),
                            obs: format!(
                                "\n[{name}{}]\n{}\n",
                                if ok {
                                    String::new()
                                } else {
                                    " failed".to_string()
                                },
                                if body.trim().is_empty() {
                                    "(no output)"
                                } else {
                                    body.trim_end()
                                }
                            ),
                            kind: ObsKind::Tool,
                            target: None,
                            changed: false,
                            remember,
                        }
                    }
                    // The spawn itself failed. `dispatch` can end the run on
                    // this and does; the overlapping half returns a
                    // [`Dispatched`] and nothing else, so here it is an
                    // observation the model reads. The refusals that carry a
                    // policy row are not among them: `Git::argv` was called on
                    // the caller's own thread, where the row could still be
                    // written.
                    Err(e) => {
                        Dispatched::go(format!("{name} failed"), format!("\n[{name} error] {e}\n"))
                    }
                }
            }
            ReadWork::Custom {
                name,
                tool,
                arguments,
                remember,
                attempt: _,
            } => match tool.invoke(&arguments).await {
                Ok(out) => {
                    let (out, truncated) = crate::tools::cap_result(out, cap);
                    info!(run_id, step, tool = name, truncated, "registered tool call");
                    Dispatched::Continue {
                        decision: format!("called {name}"),
                        obs: format!("\n[{name}]\n{out}\n"),
                        kind: ObsKind::Tool,
                        target: Some(name),
                        changed: false,
                        remember,
                    }
                }
                // A tool's own failure is the model's problem to route around,
                // not the run's to die on — the same treatment a bad regex gets
                // from grep.
                Err(e) => {
                    info!(run_id, step, tool = name, error = %e, "registered tool failed");
                    Dispatched::Continue {
                        decision: format!("{name} failed"),
                        obs: format!("\n[{name} error] {e}\n"),
                        kind: ObsKind::Error,
                        target: None,
                        changed: false,
                        remember,
                    }
                }
            },
        }
    }
}

/// The paths a git call named, as data (0.75.0).
///
/// One definition for both halves that build a git reader's work, because this
/// list is what the policy is asked about *and* what lands after `--` in the
/// argv: two readings of the same argument object are two chances for the check
/// and the command to disagree about which paths were named. `dispatch`'s git
/// arm reads them the same way, and is the third reading this pair exists to
/// replace.
pub(super) fn git_paths(arguments: &serde_json::Value) -> Vec<String> {
    arguments
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|v| {
            v.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The [`GitCmd`] one of the three git readers means (0.75.0).
///
/// `None` for every other name, so a caller that matched a wider set than it
/// meant to gets nothing rather than a command for the wrong subcommand.
pub(super) fn git_read_cmd(
    name: &str,
    arguments: &serde_json::Value,
    paths: Vec<String>,
) -> Option<GitCmd> {
    match name {
        GIT_STATUS_TOOL => Some(GitCmd::Status { paths }),
        GIT_DIFF_TOOL => Some(GitCmd::Diff {
            staged: arguments.get("staged").and_then(serde_json::Value::as_bool) == Some(true),
            paths,
        }),
        GIT_LOG_TOOL => Some(GitCmd::Log {
            // Clamped rather than trusted: a model asking for the whole history
            // of a large repository would blow the observation cap and learn
            // nothing the first twenty commits do not say.
            max_count: arguments
                .get("max_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 200) as u32,
            paths,
        }),
        _ => None,
    }
}

/// This git call's own containment, as `dispatch` resolves it (0.75.0).
///
/// `Refused` is not reachable for a reader — [`ExecMode::ReadOnly`] is the
/// widest confinement there is, so every grant satisfies it — but the match is
/// written out rather than unwrapped, because the day a reader declares
/// something else this returns `None` and the call falls to the serial path
/// instead of overlapping under a mode nobody checked.
fn git_containment(
    ws: &Workspace,
    name: &str,
    custom: &Toolbox,
    sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> Option<Option<std::sync::Arc<crate::sandbox::ExecContainment>>> {
    match resolve_call_mode(name, custom, sandbox) {
        CallMode::Refused { .. } => None,
        CallMode::Contained(resolved) => Some(
            resolved.map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress()))),
        ),
    }
}

/// 0.54.0 — the work a read-only call would do, if it can be started before the
/// completion carrying it has settled.
///
/// `None` for every call that needs a decision this function is not allowed to
/// make, and each of those is a refusal to *speculate* rather than a refusal to
/// run: the call still runs, in order, through the serial path, exactly as it
/// did on 0.53.0.
///
/// The policy must allow the call **outright**. An `Ask` verdict is never
/// speculated, which is what keeps every approver question inside a completion
/// that settled — asking a human about a turn the model may still abandon is a
/// question nobody can answer honestly, and it would put 0.41.0's
/// collapse-on-pause rule somewhere other than where the model asked for it.
///
/// `remember` is empty because an outright allow carries no remembered rule,
/// which is exactly what [`gate`] returns on [`Effect::Allow`]. A call that is
/// deferred and then approved *can* carry one, and that call is not speculated.
///
/// `sandbox` is the run's own containment (0.75.0). A git reader reaches the
/// world through a process, so what it may do has to be resolved before it is
/// started — the same question [`Speculation::offer`] already asks to decide
/// whether to speculate at all, asked here to get the answer rather than the
/// verdict.
pub(super) fn speculable(
    ws: &Workspace,
    call: &ToolCall,
    custom: &Toolbox,
    sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> Option<ReadWork> {
    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    let allowed = |act: Act, target: &str| policy_verdict(ws, act, target).effect == Effect::Allow;
    match call.name.as_str() {
        // Neither search is gated at all — 0.3.0's decision, which `prepare_read`
        // states — so there is no verdict here to be short of an allow.
        GREP_TOOL => Some(ReadWork::Grep {
            pattern: s("pattern").unwrap_or_default().to_string(),
            path_glob: s("path_glob").map(str::to_string),
        }),
        FIND_TOOL => Some(ReadWork::Find {
            glob: s("name_glob")
                .or_else(|| s("glob"))
                .unwrap_or_default()
                .to_string(),
        }),
        READ_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let (offset, limit) = read_range_of(&call.arguments);
            allowed(Act::Read, path).then(|| ReadWork::Read {
                target: path.to_string(),
                remember: Vec::new(),
                offset,
                limit,
            })
        }
        // 0.75.0 — the same act, the same target, the same check a `read_file`
        // on this path gets: enumerating a directory the operator denied reading
        // is that read, done one level up. An empty path is the workspace root,
        // which `resolve` turns into the root and the policy sees as such.
        LIST_DIR_TOOL => {
            let path = s("path").unwrap_or_default();
            allowed(Act::Read, path).then(|| ReadWork::ListDir {
                target: path.to_string(),
                remember: Vec::new(),
            })
        }
        // 0.75.0 — a git reader. Three questions rather than one, because
        // `dispatch` asks three: the program, the repository, and every path the
        // model named. Widening the overlappable set must not widen what is
        // asked before a process starts, so an `Act::Exec` deny on `git` leaves
        // with nothing started here and the call runs where it always did.
        GIT_LOG_TOOL | GIT_STATUS_TOOL | GIT_DIFF_TOOL => {
            let name = call.name.as_str();
            let paths = git_paths(a);
            if !allowed(Act::Exec, crate::tools::git::GIT)
                || !allowed(Act::Read, GIT_DIR)
                || !paths.iter().all(|p| allowed(Act::Read, p))
            {
                return None;
            }
            let contained = git_containment(ws, name, custom, sandbox)?;
            let cmd = git_read_cmd(name, a, paths)?;
            // A refusal [`GitCmd::argv`] raises — a path `git` would read as an
            // option, a repository whose own config names a program to run — is
            // a policy row `dispatch` writes, and this function holds no
            // [`Store`](crate::Store) to write one with. So a command that
            // cannot be built is not speculated: it runs in order, through
            // `dispatch`, and is refused there with its row. The cap is zero
            // because building an argv captures no output.
            Git::new(ws.policy(), ws.root(), 0).argv(&cmd).ok()?;
            Some(ReadWork::Git {
                name: name.to_string(),
                cmd,
                remember: Vec::new(),
                contained,
            })
        }
        name => {
            let tool = custom.get(name)?;
            // 0.65.0 — a speculated call starts before the completion that asked
            // for it has settled, and this function holds no
            // [`Store`](crate::Store), so it cannot open a journal row. That is
            // only sound if nothing needing one can be started here, so the
            // requirement is stated as a condition rather than left to the
            // caller: `Speculation::offer` already refuses anything that is not
            // `ToolEffect::ReadOnly`, and a `ReadOnly` tool derives
            // `ToolRecovery::Replayable`, so the two agree — but a release that
            // moved either would otherwise start an indeterminate call with no
            // record that it had begun, which is the exact defect 0.65.0 exists
            // to prevent.
            (tool.recovery() == crate::ToolRecovery::Replayable && allowed(Act::Exec, name)).then(
                || ReadWork::Custom {
                    name: name.to_string(),
                    tool: std::sync::Arc::clone(tool),
                    arguments: call.arguments.clone(),
                    remember: Vec::new(),
                    attempt: None,
                },
            )
        }
    }
}

/// 0.54.0 — read-only calls started off the provider's stream and held until the
/// completion carrying them settles.
///
/// **Nothing observable happens here.** No event is emitted, no row is written,
/// no approver is consulted and no ledger is drawn. All of that stays in the
/// serial fold, in the order the model asked, after the completion returned —
/// which is what lets this release claim the trace and the replay are identical
/// either way, structurally rather than by inspection. The only thing that moves
/// is when [`ReadWork::run`] starts.
pub(super) struct Speculation<'a> {
    /// Owned, not borrowed: a step may rebuild its `Workspace` when an approver
    /// remembers a rule, and speculation must not pin the one it started with.
    /// The clone is the same one `read_batch` already makes per spawned task.
    pub(super) ws: Workspace,
    pub(super) tools: &'a Toolbox,
    /// The turn's tool mask (0.76.0).
    ///
    /// **The third place a tool call can begin.** A speculated read never reaches
    /// `dispatch` or `read_batch` — the loop folds its result directly — so a mask
    /// enforced only at those two gates is bypassed by every streaming provider,
    /// which is all four shipped ones. The gates cannot cover this path and this
    /// path cannot borrow them: `Speculation` runs off the stream with no `Store`
    /// and no `Watch`, so it refuses by declining to start rather than by emitting
    /// a refusal, and the settled completion then dispatches the call normally,
    /// where `mask_gate` refuses it and says so.
    pub(super) mask: crate::ToolMask,
    /// 0.48.0's containment for this run, so a registered tool needing more than
    /// the run grants is refused here rather than started here. `dispatch` makes
    /// that decision before any tool arm (`resolve_call_mode`), and a speculated
    /// call never reaches `dispatch` — so without this, the single-call case would
    /// start a tool 0.53.0 refuses, and start it before the completion settled.
    pub(super) sandbox: Option<std::sync::Arc<crate::sandbox::ExecContainment>>,
    pub(super) cap: usize,
    /// 0.55.0 — the operator's `[run] max_read_chars`, when one is set. Carried
    /// beside `cap` because a read is measured against both and the refusal has
    /// to say which one bound it.
    pub(super) max_read: Option<usize>,
    pub(super) max_parallel: usize,
    pub(super) run_id: i64,
    pub(super) step: u32,
    /// The calls started for this attempt, in position order.
    pub(super) started: Vec<(usize, ToolCall)>,
    /// (0.75.0) For each position whose work spawned a contained process — a git
    /// reader, today — the containment it spawned **under**.
    ///
    /// `Speculation` holds no [`Store`], deliberately: it runs off the stream and
    /// `rusqlite::Connection` is not `Sync`. So the sandbox rows a spawn owes the
    /// trace cannot be written where the batch path writes them, and this is what
    /// lets the loop write them when it collects the call.
    ///
    /// The **narrowed** containment and not the run's, because those differ: a git
    /// reader runs `read-only` inside a run granting `workspace-write`, and the
    /// row has to name what confined the process rather than what the run was
    /// allowed.
    pub(super) spawned:
        std::collections::HashMap<usize, std::sync::Arc<crate::sandbox::ExecContainment>>,
    pub(super) set: tokio::task::JoinSet<(usize, Dispatched)>,
    /// Set by the first call this run will not speculate, after which nothing is
    /// speculated for the rest of the completion. The rule is the completion's
    /// **leading** run of read-only calls, deliberately narrower than the maximal
    /// run 0.41.0 batches: a read started after an unstarted write would answer
    /// from before the write, which is a wrong value rather than a wrong order.
    pub(super) closed: bool,
    /// What survived [`settle`](Speculation::settle), keyed by position.
    pub(super) done: std::collections::HashMap<usize, Dispatched>,
    /// Across every attempt of this step, so a retry's wasted work is counted
    /// rather than forgotten — the discard rate is the number an operator needs.
    pub(super) started_total: usize,
    pub(super) used_total: usize,
}

/// What consulting the policy about a read-only call left behind.
pub(super) enum Prepared {
    /// Cleared to run, on its own or beside others.
    Work(ReadWork),
    /// Already answered — a refusal, or a call with nothing to do. Nothing runs.
    Done(Dispatched),
    /// An approver deferred. This result stands and the batch ends here: no call
    /// after it in the completion is prepared, let alone started.
    Stop(Dispatched),
}

/// Consult the policy for one read-only call, on the caller's own thread.
///
/// Split out from the concurrent half deliberately. Every durable write a
/// decision makes — the policy event, the pending approval row — lands here, in
/// call order, before anything overlaps; the batch is only ever concurrent in the
/// part that touches the workspace. That is what makes a pause honest: the run
/// stops holding an approval for the third call in a completion having recorded
/// nothing for the fourth and fifth.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_read(
    ws: &Workspace,
    call: &ToolCall,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    custom: &Toolbox,
    watch: &Watch<'_>,
    depth: u32,
    goal: &str,
    // 0.75.0 — the run's containment, for the one prepared call that spawns.
    sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> Result<Prepared> {
    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    Ok(match call.name.as_str() {
        // Neither search is gated, and that is 0.3.0's decision rather than this
        // release's: a pattern names no path until it has matched one, and the
        // hits are drawn from a workspace the policy already bounds.
        GREP_TOOL => Prepared::Work(ReadWork::Grep {
            pattern: s("pattern").unwrap_or_default().to_string(),
            path_glob: s("path_glob").map(str::to_string),
        }),
        FIND_TOOL => Prepared::Work(ReadWork::Find {
            glob: s("name_glob")
                .or_else(|| s("glob"))
                .unwrap_or_default()
                .to_string(),
        }),
        READ_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                path,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Prepared::Done(Dispatched::go(decision, obs)),
                Gated::Paused { request_id } => Prepared::Stop(Dispatched::Pause { request_id }),
                Gated::Go {
                    target, remember, ..
                } => {
                    let (offset, limit) = read_range_of(&call.arguments);
                    Prepared::Work(ReadWork::Read {
                        target,
                        remember,
                        offset,
                        limit,
                    })
                }
            }
        }
        // 0.75.0 — the same act and the same code as a `read_file` on this path.
        LIST_DIR_TOOL => {
            let path = s("path").unwrap_or_default();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                path,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Prepared::Done(Dispatched::go(decision, obs)),
                Gated::Paused { request_id } => Prepared::Stop(Dispatched::Pause { request_id }),
                Gated::Go {
                    target, remember, ..
                } => Prepared::Work(ReadWork::ListDir { target, remember }),
            }
        }
        // 0.75.0 — a git reader, prepared here so that everything durable about
        // it happens on the caller's own thread, in the model's call order: the
        // three approvals, the containment rows, and the refusal a command that
        // cannot be built produces. What is left for the concurrent half is the
        // spawn.
        //
        // The order is `dispatch`'s order, which is the coarsest question first:
        // a run that may not spawn `git` at all is not asked about the
        // individual files it wanted to read.
        name @ (GIT_LOG_TOOL | GIT_STATUS_TOOL | GIT_DIFF_TOOL) => {
            let paths = git_paths(a);
            let mut remembered: Vec<Rule> = Vec::new();
            let mut targets: Vec<(Act, String)> = vec![
                (Act::Exec, crate::tools::git::GIT.to_string()),
                (Act::Read, GIT_DIR.to_string()),
            ];
            targets.extend(paths.iter().map(|p| (Act::Read, p.clone())));
            let mut stopped: Option<Prepared> = None;
            for (act, target) in targets {
                match gate(
                    ws, approver, store, run_id, step, act, &target, None, watch, depth, goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => {
                        stopped = Some(Prepared::Done(Dispatched::go(decision, obs)));
                        break;
                    }
                    Gated::Paused { request_id } => {
                        stopped = Some(Prepared::Stop(Dispatched::Pause { request_id }));
                        break;
                    }
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }
            if let Some(stopped) = stopped {
                return Ok(stopped);
            }
            let cmd =
                git_read_cmd(name, a, paths).expect("this arm matches only the three git readers");
            let Some(contained) = git_containment(ws, name, custom, sandbox) else {
                // Unreachable for a reader, and an observation rather than a
                // panic if a later release makes it reachable: `dispatch` says
                // "Nothing was started" for the same case and so does this.
                return Ok(Prepared::Done(Dispatched::go(
                    format!("{name} refused: needs more containment than this run grants"),
                    format!(
                        "\n[{name} refused] this tool needs a containment mode this run does not \
                         grant. Nothing was started.\n"
                    ),
                )));
            };
            // Both of `Git::argv`'s refusals land here rather than inside the
            // spawned task — a path that would be read as an option, and a
            // repository whose own config names a program to run — because the
            // row is written exactly as `gate` writes one and a reader cannot
            // tell a git refusal from any other.
            //
            // Asked BEFORE the containment is recorded, because a refusal returns
            // from this arm and nothing is ever spawned: recording the creation
            // first left a sandbox announced as created, never torn down, for a
            // process that did not exist.
            match Git::new(ws.policy(), ws.root(), 0).argv(&cmd) {
                Ok(_) => {}
                Err(Error::Refused {
                    act,
                    target,
                    rule,
                    layer,
                }) => {
                    let mut ev = PolicyEvent::refusal(step, act.clone(), target.clone());
                    ev.rule = rule.clone();
                    ev.layer = layer;
                    store.record_event(run_id, &ev)?;
                    refused(watch, run_id, depth, &ev);
                    let why = rule
                        .as_deref()
                        .map(|r| format!(" (rule {r})"))
                        .unwrap_or_default();
                    return Ok(Prepared::Done(Dispatched::go(
                        format!("{name} refused"),
                        format!(
                            "\n[{act} refused] {target}{why} — the policy forbids this; carry on \
                             without git\n"
                        ),
                    )));
                }
                Err(e) => return Err(e),
            }
            // The containment is recorded before the spawn and torn down after it
            // joins ([`ReadWork::spawns`]), so a git reader that ran inside a
            // batch leaves the rows it leaves when it runs alone.
            if let Some(containment) = &contained {
                for event in [
                    sandbox_create(run_id, step, containment),
                    crate::state::SandboxEvent::exec(
                        run_id,
                        step,
                        containment.backend().as_str(),
                        name,
                    ),
                ] {
                    record_sandbox_step(store, watch, depth, &event);
                }
            }
            Prepared::Work(ReadWork::Git {
                name: name.to_string(),
                cmd,
                remember: remembered,
                contained,
            })
        }
        name => {
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Exec,
                name,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Prepared::Done(Dispatched::go(decision, obs)),
                Gated::Paused { request_id } => Prepared::Stop(Dispatched::Pause { request_id }),
                Gated::Go { remember, .. } => {
                    // `validate` ran at run start, so the lookup cannot miss.
                    let tool = custom.get(name).expect("owns() and get() agree");
                    // 0.65.0 — the journal opens HERE, before the work exists,
                    // because this is the last point at which the crate can still
                    // say "nothing has been started". `open_attempt` writes
                    // nothing for a replayable tool, so the decision lives in one
                    // place and a built-in run pays no write.
                    let attempt = store.open_attempt(run_id, step, name, tool.recovery())?;
                    Prepared::Work(ReadWork::Custom {
                        name: name.to_string(),
                        tool: std::sync::Arc::clone(tool),
                        arguments: call.arguments.clone(),
                        remember,
                        attempt,
                    })
                }
            }
        }
    })
}

/// Dispatch a run of read-only calls from one completion, overlapping them.
///
/// Returns one [`Dispatched`] per call the batch reached, **in the order the
/// model asked for them** — never in the order they finished. The caller folds
/// them exactly as it folds a serial result, which is the whole guarantee: a
/// run's trace, its ledger and its replay cannot tell that this happened.
///
/// The bound is a [`JoinSet`](tokio::task::JoinSet) with at most `max_parallel`
/// tasks alive, refilled as each finishes. It is a `JoinSet` rather than loose
/// tasks because it aborts its children when it is dropped: a run that ends
/// mid-batch leaves nothing running behind it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn read_batch(
    ws: &Workspace,
    calls: &[ToolCall],
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    custom: &Toolbox,
    cap: usize,
    max_read: Option<usize>,
    watch: &Watch<'_>,
    depth: u32,
    max_parallel: usize,
    goal: &str,
    hooks: Option<&crate::hooks::Hooks>,
    // 0.75.0 — the run's containment, for the git readers in this batch.
    sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
    // 0.76.0 — the turn's tool mask. Here as well as in `dispatch`, because this
    // is the other place a call can begin and it does not route through it.
    mask: &crate::ToolMask,
) -> Result<std::collections::VecDeque<Dispatched>> {
    let mut out: Vec<Option<Dispatched>> = Vec::with_capacity(calls.len());
    let mut queued: std::collections::VecDeque<(usize, Option<i64>, bool, ReadWork)> =
        std::collections::VecDeque::new();
    for call in calls {
        // Announced here rather than in the concurrent half, so a watcher sees
        // the calls in the order the model made them however they then run.
        announce(watch, run_id, step, depth, call);
        if let Some(refused) = mask_gate(mask, call, watch, run_id, step, depth) {
            out.push(Some(refused));
            continue;
        }
        if let Some(refused) = tool_gate(hooks, call, watch, run_id, step, depth) {
            out.push(Some(refused));
            continue;
        }
        match prepare_read(
            ws, call, approver, store, run_id, step, custom, watch, depth, goal, sandbox,
        )
        .await?
        {
            Prepared::Work(work) => {
                queued.push_back((out.len(), work.attempt(), work.spawns(), work));
                out.push(None);
            }
            Prepared::Done(done) => out.push(Some(done)),
            Prepared::Stop(stop) => {
                out.push(Some(stop));
                break;
            }
        }
    }

    let owned = ws.clone();
    let mut set: tokio::task::JoinSet<(usize, Option<i64>, bool, Dispatched)> =
        tokio::task::JoinSet::new();
    let fill =
        |set: &mut tokio::task::JoinSet<(usize, Option<i64>, bool, Dispatched)>,
         queued: &mut std::collections::VecDeque<(usize, Option<i64>, bool, ReadWork)>| {
            while set.len() < max_parallel {
                let Some((at, attempt, spawns, work)) = queued.pop_front() else {
                    break;
                };
                let ws = owned.clone();
                set.spawn(async move {
                    (
                        at,
                        attempt,
                        spawns,
                        work.run(&ws, cap, max_read, run_id, step).await,
                    )
                });
            }
        };
    fill(&mut set, &mut queued);
    while let Some(joined) = set.join_next().await {
        let (at, attempt, spawns, done) = match joined {
            Ok(quad) => quad,
            // A tool that panics panicked before this release too, and the run
            // died with it. Carrying the unwind on rather than turning it into an
            // observation keeps that true.
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => {
                return Err(Error::Config(format!(
                    "a read-only tool call was cancelled: {e}"
                )))
            }
        };
        // 0.65.0 — the call returned, so the attempt is no longer indeterminate.
        // Closed here rather than inside the task: `run` holds no store, and a
        // `Store` cannot cross into a spawned task.
        if let Some(id) = attempt {
            store.close_attempt(id)?;
        }
        // 0.75.0 — and the containment this call spawned under is gone, for the
        // same reason and in the same place: `run` holds no store, so the row
        // that says the sandbox was torn down is written by whoever joined it.
        if spawns {
            record_sandbox_step(
                store,
                watch,
                depth,
                &crate::state::SandboxEvent::destroy(run_id, step),
            );
        }
        out[at] = Some(done);
        fill(&mut set, &mut queued);
    }

    Ok(out
        .into_iter()
        .map(|d| d.expect("every prepared call was either finished or joined"))
        .collect())
}

/// Tell a watcher what the run is about to do.
///
/// The subject is whichever of the conventional argument names this tool uses; a
/// tool that names none of them is its own subject, which is what an MCP or
/// registered tool call is.
pub(super) fn announce(watch: &Watch<'_>, run_id: i64, step: u32, depth: u32, call: &ToolCall) {
    let s = |k: &str| call.arguments.get(k).and_then(|v| v.as_str());
    watch.emit(RunEvent::at_depth(
        run_id,
        step,
        depth,
        EventKind::ToolCall {
            name: call.name.clone(),
            target: ["path", "pattern", "name_glob", "glob", "key", "name"]
                .into_iter()
                .find_map(s)
                .unwrap_or(&call.name)
                .to_string(),
        },
    ));
}

/// The caller's own per-turn mask, applied before anything is started (0.76.0).
///
/// **Called from both places a tool call can begin** — the head of [`dispatch`]
/// and [`read_batch`]'s per-call loop — for the same reason [`tool_gate`] is: a
/// rule that must see every call has to live at both, and `read_batch` does not
/// route through `dispatch`. A mask enforced only where the catalogue is built
/// would be advisory, because `dispatch` matches on the tool's name and answers
/// anything it does not recognise with an unknown-tool message rather than a
/// refusal — so a model that names a withheld tool anyway would be obeyed.
///
/// Refused with [`Error::Refused`](crate::Error::Refused)'s existing shape rather
/// than a new variant, and before the operator's `before_tool` hook: the mask is
/// the caller's own instruction about this turn, and a call the caller withheld is
/// not a call anyone else needs to rule on.
pub(super) fn mask_gate(
    mask: &crate::ToolMask,
    call: &ToolCall,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    depth: u32,
) -> Option<Dispatched> {
    if !mask.withholds(&call.name) {
        return None;
    }
    watch.emit(RunEvent::at_depth(
        run_id,
        step,
        depth,
        EventKind::Refused {
            act: "tool".into(),
            target: call.name.clone(),
            rule: Some(call.name.clone()),
            layer: Some("turn tool mask".into()),
        },
    ));
    Some(Dispatched::go(
        format!("{} refused: withheld from this turn", call.name),
        format!(
            "\n[{} refused] this turn withholds that tool, so nothing was started. \
             Use one of the tools that is available.\n",
            call.name
        ),
    ))
}

/// Ask the operator's `before_tool` hooks whether this call may happen (0.42.0).
///
/// One definition, two call sites: the head of [`dispatch`], which every
/// non-batched call passes through, and [`read_batch`]'s per-call loop, which is
/// where 0.41.0's concurrent reads are prepared. Both are serial and on the
/// loop's own thread, so a hook runs in the model's call order and the read work
/// it approves still runs concurrently. `None` means nothing objected.
///
/// A refusal is reported through [`EventKind::Refused`] with the hook's program
/// where a rule's pattern would be: a refusal that did not come from the policy
/// is still a refusal, and an observer already routing on them should see it.
pub(super) fn tool_gate(
    hooks: Option<&crate::hooks::Hooks>,
    call: &ToolCall,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    depth: u32,
) -> Option<Dispatched> {
    let hooks = hooks?;
    if !hooks.gates_tools() {
        return None;
    }
    let payload = serde_json::json!({
        "at": "before_tool",
        "run_id": run_id,
        "step": step,
        "depth": depth,
        "tool": call.name,
        "arguments": call.arguments,
    })
    .to_string();

    let (argv0, why, cancel) = match hooks.before_tool(&call.name, &payload) {
        crate::hooks::ToolGate::Go => return None,
        crate::hooks::ToolGate::Refused { argv0, why } => (argv0, why, false),
        crate::hooks::ToolGate::Cancel { argv0 } => (
            argv0,
            "a local check stopped the run rather than this call".to_string(),
            true,
        ),
    };
    watch.emit(RunEvent::at_depth(
        run_id,
        step,
        depth,
        EventKind::Refused {
            act: "tool".into(),
            target: call.name.clone(),
            rule: Some(argv0.clone()),
            layer: Some("io.toml hook".into()),
        },
    ));
    if cancel {
        watch.cancel();
    }
    Some(Dispatched::go(
        format!("{} refused by hook {argv0}", call.name),
        format!(
            "\n[{} refused] a local check (`{argv0}`) stopped this call: {why}\n",
            call.name
        ),
    ))
}

/// Execute one tool call against the workspace, enforcing the policy and
/// consulting `approver` for anything it marks [`Effect::Ask`].
///
/// Tool-level failures (bad regex, path escape, a policy refusal) become
/// The images one request carries: the caller's, which are the task's subject and
/// ride every step, plus whatever the agent looked at last step, which rides one.
///
/// Bounded here rather than at either source, because neither can see the total.
/// Over the bound the oldest viewed images are dropped first and the model is not
/// told a lie about it — the drop is reported in the trace by the caller. The
/// caller's own images are never dropped: a task about an image that silently
/// stops carrying it is the failure this whole boundary exists to prevent, so an
/// over-budget contract is an error at the first step instead.
#[cfg(feature = "media")]
pub(super) fn attach_media(
    contract: &TaskContract,
    pending: &mut PendingMedia,
) -> Result<Vec<crate::provider::Media>> {
    use crate::provider::MAX_REQUEST_IMAGE_BYTES;
    let fixed: usize = contract.images.iter().map(|m| m.byte_len()).sum();
    if fixed > MAX_REQUEST_IMAGE_BYTES {
        return Err(Error::Config(format!(
            "the contract's images total {fixed} bytes, over the \
             {MAX_REQUEST_IMAGE_BYTES}-byte per-request bound"
        )));
    }
    let mut out = contract.images.clone();
    let mut used = fixed;
    for m in pending.drain(..) {
        if used + m.byte_len() > MAX_REQUEST_IMAGE_BYTES {
            continue;
        }
        used += m.byte_len();
        out.push(m);
    }
    Ok(out)
}

/// Read a `todo_write` argument object into a plan, or say what was wrong with it.
///
/// Strict on purpose, and the error goes back to the model as an observation rather
/// than out of the run: an item whose state the crate does not understand is an item
/// whose state nobody knows, and guessing `pending` would show an operator a plan the
/// agent did not write. The message names the three legal states, so the correction
/// costs one step and needs no documentation.
pub(super) fn parse_todo_items(
    args: &serde_json::Value,
) -> std::result::Result<Vec<TodoItem>, String> {
    let list = args
        .get("items")
        .ok_or_else(|| "`items` is required: send the whole plan as a list".to_string())?
        .as_array()
        .ok_or_else(|| {
            "`items` must be a list of {text, state} objects, and the whole plan is sent \
             every time"
                .to_string()
        })?;
    let mut out = Vec::with_capacity(list.len());
    for (i, raw) in list.iter().enumerate() {
        let text = raw
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("item {} needs a non-empty `text`", i + 1))?;
        let state = raw
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("item {} (`{text}`) needs a `state`", i + 1))?;
        let state = TodoState::parse(state).ok_or_else(|| {
            format!(
                "item {} (`{text}`) has state `{state}`; use pending, active or done",
                i + 1
            )
        })?;
        out.push(TodoItem::new(text, state));
    }
    Ok(out)
}
