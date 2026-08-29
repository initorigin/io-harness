//! mailbox: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// The directory every per-agent worktree is made under, relative to the tree
/// root (0.36.0).
///
/// One component, never a literal holding a separator, so the path is joined the
/// way every other path in this crate is and a Windows checkout gets a Windows
/// path rather than a string that happens to work.
pub(super) const WORKTREE_DIR: &str = ".worktrees";

/// How much of `git`'s own output is kept when a worktree cannot be made.
///
/// Its own bound rather than the run's per-observation cap, because this text
/// never becomes a tool result: it reaches the model as the reason one spawn did
/// not happen, and a page of it would say nothing the first lines do not.
pub(super) const WORKTREE_ERR_CAP: usize = 4_000;

/// One agent name as one path component and one branch name.
///
/// An allowlist, matching `check_branch_name`'s: a definition's name is the
/// operator's free text and reaches both a directory and a ref. Truncated
/// because it is only half of the slug — the run id and step that follow are what
/// make it unique.
pub(super) fn slugify(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed: String = mapped.trim_matches('-').chars().take(40).collect();
    match trimmed.is_empty() {
        true => "agent".into(),
        false => trimmed,
    }
}

/// A stable 32-bit digest of one goal, for the worktree slug (0.36.0).
///
/// FNV-1a written out rather than `std::hash::DefaultHasher`, which is documented
/// as unstable across releases: a slug that changed when the crate was rebuilt on
/// a newer toolchain would send a resumed spawn to a worktree that does not
/// exist, and surviving a rebuild is the one property this derivation exists for.
pub(super) fn goal_digest(goal: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in goal.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// How often a blocked read looks again (0.60.0).
///
/// A poll rather than a notification, and the reason is that the mailbox is a
/// table: a tree may span processes, and an in-process notifier would wake only
/// the agents this process happens to be running. Two hundred milliseconds is
/// below what a step costs by orders of magnitude, so the latency it adds is not
/// measurable against a provider call, and it costs one indexed seek per tick.
pub(super) const WAIT_POLL: Duration = Duration::from_millis(200);

/// The character a *derived* address uses and an assigned one may not (0.60.0).
///
/// It is what keeps the two namespaces from meeting. A child the parent did not
/// name is called `<role>#<run id>`, which is unique because run ids are — but
/// only if no parent can assign that same string, and a parent that guessed a
/// future run id could. Forbidding one character in an assigned name closes the
/// whole class with one rule rather than with a collision check that would have to
/// be right about a number nobody has allocated yet.
pub(super) const DERIVED_MARK: char = '#';

/// The longest address a parent may assign. Long enough for any name a model will
/// think of, short enough that the refusal listing stays readable.
pub(super) const ADDRESS_MAX: usize = 64;

/// Whether a parent may assign `name` as a child's address, or why not (0.60.0).
///
/// The message is what the model reads, so each refusal names the rule it broke
/// rather than saying the name is invalid. Deliberately strict: an address is
/// typed back by another agent from a goal string, and a name carrying a space, a
/// quote or a newline is one that will be retyped wrong.
pub(super) fn address_is_assignable(name: &str) -> std::result::Result<(), String> {
    if name == ROOT_ADDRESS {
        return Err(format!(
            "`{ROOT_ADDRESS}` is the address of the agent at the top of this tree and cannot be \
             taken. Pick another name."
        ));
    }
    if name.chars().count() > ADDRESS_MAX {
        return Err(format!(
            "an address may be at most {ADDRESS_MAX} characters and `{name}` is longer"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(format!(
            "an address may contain only letters, digits, `-` and `_`, and `{name}` contains \
             `{bad}`. A name another agent has to retype is one it will retype wrong."
        ));
    }
    Ok(())
}

/// Who this agent is and who it may reach, resolved once per mailbox call
/// (0.60.0).
///
/// Deliberately computed inside the call rather than at the top of the agent loop.
/// A run whose agents never message costs nothing for the mailbox existing — no
/// query per step, no field on the loop's state — which is the property N7 asserts
/// on the assembled prompt.
pub(super) struct Addressing {
    /// This agent's own address, as its siblings would write it.
    pub(super) me: String,
    /// Every addressable agent in the tree, sorted by name.
    pub(super) tree: Vec<(String, i64)>,
}

impl Addressing {
    pub(super) fn resolve(store: &Store, run_id: i64) -> Result<Self> {
        let root = store.run_root(run_id)?;
        let tree = store.tree_addresses(root)?;
        // An agent whose `spawns` row predates 0.60.0 has no recorded address —
        // only reachable by resuming a tree a previous release spawned. It is
        // given a derived one so what it sends is still attributed, and it stays
        // out of `tree`, so nobody can address it. Stated rather than papered
        // over: the alternative is a sender rendered as an empty name.
        let me = tree
            .iter()
            .find(|(_, id)| *id == run_id)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| format!("agent{DERIVED_MARK}{run_id}"));
        Ok(Self { me, tree })
    }

    /// The run behind an address, or the refusal that lists what is reachable.
    ///
    /// The refusal names the alternatives because a model that mistyped an
    /// address recovers in one step when it is told the right ones and burns a
    /// step guessing when it is not. It is the same shape the unknown-definition
    /// refusal has used since 0.21.0.
    pub(super) fn resolve_to(&self, name: &str) -> std::result::Result<i64, String> {
        match self.tree.iter().find(|(n, _)| n == name) {
            Some((_, id)) => Ok(*id),
            None => Err(format!(
                "no agent in this tree is addressed `{name}`. Reachable from here: {}",
                self.tree
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Render one delivered message as the line its recipient reads.
pub(super) fn render_message(m: &crate::state::AgentMessage) -> String {
    format!("[{} @step {}] {}\n", m.from_name, m.step, m.body)
}

/// Handle one [`SEND_MESSAGE_TOOL`] or [`READ_MESSAGES_TOOL`] call (0.60.0).
///
/// Returns the `(decision, observation)` pair the tree loop records, exactly as
/// [`dispatch`] does for every other tool. Every failure here is a typed
/// observation the agent can adapt to and never an error that ends its run — a
/// mistyped address is the ordinary case, not a fault.
///
/// It is handled in the tree loop rather than in `dispatch` because it needs three
/// things `dispatch` is not given and should not be: the tree this run belongs to,
/// the addresses in it, and the fact that a flat run has neither tool.
pub(super) async fn mailbox_call(
    store: &Store,
    call: &ToolCall,
    run_id: i64,
    step: u32,
    max_wait: Duration,
) -> Result<(String, String)> {
    let a = &call.arguments;
    let who = Addressing::resolve(store, run_id)?;

    if call.name == SEND_MESSAGE_TOOL {
        let to = a.get("to").and_then(|v| v.as_str()).unwrap_or_default();
        let body = a.get("body").and_then(|v| v.as_str()).unwrap_or_default();
        if to.is_empty() || body.is_empty() {
            return Ok((
                "send missing fields".into(),
                format!("\n[{SEND_MESSAGE_TOOL} error] needs \"to\" and \"body\"\n"),
            ));
        }
        if to == who.me {
            return Ok((
                "send to self".into(),
                format!(
                    "\n[{SEND_MESSAGE_TOOL} error] `{to}` is you. A message to yourself is a note; \
                     write it down instead.\n"
                ),
            ));
        }
        let to_run = match who.resolve_to(to) {
            Ok(id) => id,
            Err(why) => {
                return Ok((
                    format!("unknown address {to}"),
                    format!("\n[{SEND_MESSAGE_TOOL} error] {why}\n"),
                ))
            }
        };
        store.send_message(run_id, to_run, &who.me, step, body)?;
        store.record_agent_event(&AgentEvent::message_sent(
            run_id,
            step,
            to_run,
            to,
            body.chars().count(),
        ))?;
        return Ok((
            format!("messaged {to}"),
            format!(
                "\n[sent to {to}] {} characters. It reads this when it next checks its \
                 messages.\n",
                body.chars().count()
            ),
        ));
    }

    // READ_MESSAGES_TOOL.
    let from = a
        .get("from")
        .and_then(|v| v.as_str())
        .filter(|f| !f.is_empty());
    if let Some(f) = from {
        if let Err(why) = who.resolve_to(f) {
            return Ok((
                format!("unknown address {f}"),
                format!("\n[{READ_MESSAGES_TOOL} error] {why}\n"),
            ));
        }
    }
    // 0.60.0 — the wall clock, narrowed by the operator's ceiling. A request the
    // cap cut is said once, at the front of whatever the read comes back as, so
    // the model reads it beside the result: an agent that believes it waited a
    // minute and waited five seconds draws the wrong conclusion from an empty
    // mailbox. The same shape 0.50.0 uses for a narrowed detachment.
    let asked = a
        .get("wait_secs")
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
        .unwrap_or_default();
    let wait = asked.min(max_wait);
    let narrowed = (asked > wait).then(|| {
        format!(
            "\n[wait narrowed] this run allows a wait of at most {}s, so that is what was \
             waited\n",
            wait.as_secs()
        )
    });

    let mut delivered = store.read_messages(run_id, from)?;
    let mut waited_out = false;
    if delivered.is_empty() && !wait.is_zero() {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Nothing can answer, so nothing is worth waiting for. A named sender
            // that has already finished without sending is the case a bounded
            // wait still gets wrong — thirty seconds spent on an agent that ended
            // a minute ago — and it costs one lookup to close.
            if let Some(f) = from {
                let sender = who.resolve_to(f).ok();
                if let Some(id) = sender {
                    if terminal_outcome(store, id)?.is_some() {
                        return Ok((
                            format!("{f} finished without sending"),
                            format!(
                                "\n[messages] {f} has finished and sent you nothing. Waiting for \
                                 it again will not help.\n"
                            ),
                        ));
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                waited_out = true;
                break;
            }
            tokio::time::sleep(WAIT_POLL.min(deadline - tokio::time::Instant::now())).await;
            delivered = store.read_messages(run_id, from)?;
            if !delivered.is_empty() {
                break;
            }
        }
    }

    store.record_agent_event(&AgentEvent::message_read(
        run_id,
        step,
        delivered.len(),
        from,
    ))?;
    let note = |obs: String| match &narrowed {
        Some(why) => format!("{why}{}", obs.trim_start_matches('\n')),
        None => obs,
    };
    if delivered.is_empty() {
        // "Nothing was sent" and "nothing was sent YET and I stopped waiting" are
        // different facts and a model that cannot tell them apart cannot decide
        // whether to wait again. F7 is this distinction.
        return Ok((
            if waited_out {
                "waited, nothing arrived".into()
            } else {
                "no messages".into()
            },
            note(match (waited_out, from) {
                (true, Some(f)) => format!(
                    "\n[messages] nothing from {f} after {}s; it is still running and may yet \
                     send\n",
                    wait.as_secs()
                ),
                (true, None) => format!(
                    "\n[messages] nothing arrived in {}s; the agents you are waiting on are still \
                     running\n",
                    wait.as_secs()
                ),
                (false, Some(f)) => format!("\n[messages] nothing from {f}\n"),
                (false, None) => "\n[messages] nothing waiting\n".into(),
            }),
        ));
    }
    let mut obs = format!("\n[messages] {} waiting\n", delivered.len());
    for m in &delivered {
        obs.push_str(&render_message(m));
    }
    Ok((format!("read {} messages", delivered.len()), note(obs)))
}

/// The worktree one agent of one spawn works in, made if it is not there already
/// (0.36.0).
///
/// The path is *derived* from `(agent, parent run, step, goal)` — the same key
/// `find_spawn` adopts by, plus the agent's name — rather than allocated fresh,
/// and an existing directory is reused rather than re-created. That is the whole
/// of the resume story: a parent replaying a spawn after a crash finds the
/// worktree it made last time, with the files the child had already written still
/// in it. Creating unconditionally would fail on the branch that already exists,
/// and deleting first would throw away the work the resume exists to keep.
///
/// The goal is in the slug as a digest and it is not decoration: two children of
/// the *same* definition spawned in the *same* step — which is the ordinary shape
/// of a fan-out — differ in nothing else, and would otherwise be handed one
/// worktree between them. That is the collision this field exists to remove,
/// reappearing one level down.
///
/// The path is checked against the parent's policy before `git` is asked,
/// because the crate is writing somewhere the model did not name and an
/// unchecked write is a claim this crate does not get to make. A policy denying
/// `.worktrees/**` turns the feature off loudly rather than quietly.
///
/// **0.70.0 — through `gate`, and twice.** Both checks used to be raw policy
/// reads that refused anything that was not `Effect::Allow`: the write here, and
/// the `Act::Exec` on `git` inside [`Git::run`]. `Policy::default()` sets *both*
/// of those to [`Effect::Ask`], so a definition declaring `worktree = true`
/// could never spawn out of the box — the operator saw "the policy refuses to
/// write .worktrees/…" for a policy that had asked to be asked. Both now reach
/// the approver, and both write a policy row for the trace.
///
/// What this function still cannot do is *pause*. Its result is a `PathBuf` or a
/// reason, and its caller turns a reason into a spawn that did not happen, so a
/// [`Decision::Defer`](crate::Decision::Defer) — "a human will decide later" — has
/// nowhere to go and is reported as the reason instead. Making the spawn itself
/// pausable is a change to `SpawnOutcome` and its caller, not to this function.
///
/// **What a deferral leaves behind, stated accurately.** `gate` writes the
/// pending row before it consults the approver, so a deferral leaves an
/// unresolved row on a run that then carries on and finishes — nothing surfaces
/// `AwaitingApproval` for it. And that row is *not* usefully answerable later:
/// the derived slug embeds the spawning `step`, so a retry at a later step asks
/// about a different path and writes its own row rather than meeting the
/// standing one. Each deferred attempt leaves one. An approver that defers on a
/// worktree spawn is therefore choosing "not this run", not "ask me again in a
/// moment" — the honest reading, and the reason this is a limitation rather than
/// a feature waiting to be finished.
pub(super) async fn worktree_for<P: Provider>(
    tree: &Tree<'_, P>,
    parent_policy: &Policy,
    agent: &str,
    goal: &str,
    parent_run_id: i64,
    step: u32,
    depth: u32,
) -> std::result::Result<PathBuf, String> {
    let slug = format!(
        "{}-{parent_run_id}-{step}-{:08x}",
        slugify(agent),
        goal_digest(goal)
    );
    let rel = Path::new(WORKTREE_DIR).join(&slug);
    let abs = tree.root.join(&rel);
    if abs.exists() {
        return Ok(abs);
    }

    let target = rel.to_string_lossy().into_owned();
    // The parent's policy over the tree root: `gate` resolves a relative
    // read/write target through the workspace, which is what stops a slug from
    // ever naming somewhere outside the root. Built here rather than carried
    // because `Tree` holds the root and the policy separately and this is the
    // only place in the file that needs the pair.
    let ws = Workspace::with_policy(tree.root.clone(), parent_policy.clone());
    for (act, what) in [
        (Act::Write, target.as_str()),
        (Act::Exec, crate::tools::git::GIT),
    ] {
        match gate(
            &ws,
            tree.approver,
            tree.store,
            parent_run_id,
            step,
            act,
            what,
            None,
            tree.watch,
            depth,
            goal,
        )
        .await
        .map_err(|e| e.to_string())?
        {
            Gated::Go { target: t, .. } if t != what => {
                // The path is derived from the spawn's own identity — that
                // derivation is the whole of the resume story above — so an
                // approver that rewrites it is asking for a worktree the next
                // attempt would not find. Refused rather than silently ignored.
                return Err(format!(
                    "the approver moved {what} to {t}; a worktree path is derived, not chosen"
                ));
            }
            Gated::Go { .. } => {}
            Gated::Refused { obs, .. } => return Err(obs.trim().to_string()),
            Gated::Paused { .. } => {
                return Err(format!(
                    "approval for {what} was deferred, and a spawn cannot wait for one"
                ))
            }
        }
    }

    let cmd = GitCmd::Worktree {
        name: slug,
        path: target,
    };
    match Git::new(parent_policy, &tree.root, WORKTREE_ERR_CAP)
        .gated()
        .run(&cmd)
        .await
    {
        Ok(GitOutcome::Ran { code: Some(0), .. }) => Ok(abs),
        Ok(GitOutcome::Ran { code, stderr, .. }) => Err(format!(
            "`git worktree add` {} — {}",
            match code {
                Some(c) => format!("exited {c}"),
                None => "was killed by a signal".to_string(),
            },
            stderr.trim()
        )),
        Ok(GitOutcome::Unavailable { reason }) => Err(reason),
        Err(e) => Err(e.to_string()),
    }
}
