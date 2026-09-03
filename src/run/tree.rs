//! tree: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// One agent's loop, reused for the root and every child. Identical to the
/// workspace loop, plus: it may spawn children (recursively, via [`SPAWN_TOOL`]),
/// and its token spend is drawn from the tree's shared ledger rather than only
/// its own contract budget.
///
/// `depth` is 0 at the root; a child's depth is its parent's + 1. Returns the
/// agent's [`RunOutcome`]; a tree-wide budget halt propagates up as
/// [`RunOutcome::BudgetCeilingReached`].
#[allow(clippy::too_many_arguments)]
/// A child the parent is no longer waiting for, and the report it will produce.
pub(super) type ChildFuture<'f> = Pin<Box<dyn Future<Output = Result<SpawnResult>> + 'f>>;

/// The same child once the loop has taken it, tagged with the order it was
/// spawned in.
///
/// A [`FuturesUnordered`] yields in completion order, and a trace that depends on
/// which child won a race is the non-reproducibility 0.12.0 removed. The tag is
/// what lets the fold put them back in the order the model asked for them, so two
/// children finishing either way round produce the same ledger.
pub(super) type TaggedChild<'f> = Pin<Box<dyn Future<Output = Result<(u64, SpawnResult)>> + 'f>>;

/// Every child a parent detached or backgrounded, driven by the parent's own
/// loop (0.50.0).
///
/// `&Store` is `Send` and not `Sync` and [`run_agent`] borrows the whole [`Tree`],
/// so a detached child cannot become a spawned task — the type system settles
/// that, exactly as it settled 0.41.0's read batch. It is a future on the parent's
/// own task instead, polled while the parent waits for its own completion.
pub(super) type Inflight<'f> = FuturesUnordered<TaggedChild<'f>>;

/// Run one agent, then drain every child it stopped waiting for.
///
/// The drain is at this one boundary rather than at each of the loop's twelve
/// endings, and that is the whole argument for the wrapper: a child abandoned on
/// the stall path or on an error propagating is a process still running after the
/// tree returned, which is the one thing 0.48.0's "everything a run starts is
/// inside the boundary" forbids. One return, one drain, no exit to forget.
pub(super) fn run_agent<'f, P: Provider>(
    tree: &'f Tree<'_, P>,
    contract: &'f TaskContract,
    run_id: i64,
    depth: u32,
    policy: &'f Policy,
    start_step: u32,
    identity: Option<&'f AgentDef>,
) -> Pin<Box<dyn Future<Output = Result<RunOutcome>> + 'f>> {
    Box::pin(async move {
        let mut inflight: Inflight<'f> = FuturesUnordered::new();
        let outcome = agent_loop(
            tree,
            contract,
            run_id,
            depth,
            policy,
            start_step,
            identity,
            &mut inflight,
        )
        .await;
        // Before the `?`, so a run ending in an error drains too. An error is
        // exactly when a leaked child is most likely and least visible.
        drain_children(tree, run_id, depth, &mut inflight).await?;
        outcome
    })
}

/// Take back the children a previous process detached and never finished.
///
/// A detached child's step commits, so the resume starts at the step *after* it
/// and the spawn call is never replayed — which without this would leave the child
/// with a `running` run row nobody is driving, the exact orphan 0.48.0's boundary
/// rule forbids. Each one is resumed through `spawn_child` itself, from the
/// arguments the spawn recorded, so adoption, admission and the narrowed policy are
/// the same code that ran the first time rather than a second implementation of it.
pub(super) async fn readopt_children<'f, P: Provider>(
    tree: &'f Tree<'_, P>,
    run_id: i64,
    depth: u32,
    policy: &Policy,
    start_step: u32,
    inflight: &mut Inflight<'f>,
) -> Result<()> {
    if start_step <= 1 {
        return Ok(());
    }
    let mut seq = u64::MAX / 2;
    for event in tree.store.agent_events(run_id)? {
        if event.kind != "spawn_args" || event.step >= start_step {
            continue;
        }
        let Some(child) = event.child_run_id else {
            continue;
        };
        // Already finished before the process died: there is nothing to drive, and
        // the ordinary fold will read its report off the store when it is asked
        // for. Re-running it would spend a second child's worth of everything.
        if terminal_outcome(tree.store, child)?.is_some() {
            continue;
        }
        let Some(arguments) = event
            .detail
            .as_deref()
            .and_then(|d| serde_json::from_str(d).ok())
        else {
            continue;
        };
        let call = ToolCall {
            name: SPAWN_TOOL.to_string(),
            arguments,
        };
        match spawn_child(tree, &call, run_id, depth, policy, event.step).await? {
            SpawnOutcome::InFlight { fut, .. } => {
                let ord = seq;
                seq += 1;
                inflight.push(Box::pin(async move { Ok((ord, fut.await?)) }));
            }
            // It finished inside the re-adoption, which is possible for a child
            // that had one step left. Its report is on the store and the ordinary
            // fold is not the place for it — this run never saw the spawn — so it
            // is left where an operator reads it, under the child's own run id.
            SpawnOutcome::Settled(_) => {}
        }
    }
    Ok(())
}

/// Fold the reports of every child that has finished into the parent's ledger.
///
/// Sorted by the order the children were spawned in and not by the order they
/// finished, which is the same argument 0.12.0 made when it replaced
/// `buffer_unordered` with `buffered`: two children finishing either way round
/// must leave the same ledger, or a run's trace and its next prompt depend on who
/// won a race.
///
/// A detached child that stops for a human does **not** pause the tree. Its parent
/// has already moved on — there is no step to leave uncommitted and re-run — so the
/// parent is told, in the ledger, that a child is waiting on somebody, and the
/// child is resumable by id like any other. Waiting is what `wait: true` is for.
#[allow(clippy::too_many_arguments)]
pub(super) fn fold_collected<P: Provider>(
    tree: &Tree<'_, P>,
    run_id: i64,
    depth: u32,
    step: u32,
    entry_cap: usize,
    inflight: &mut Inflight<'_>,
    collected: &mut Vec<Result<(u64, SpawnResult)>>,
    ledger: &mut ContextLedger,
) -> Result<()> {
    // Everything already finished, without waiting for anything that has not.
    while let Some(Some(child)) = inflight.next().now_or_never() {
        collected.push(child);
    }
    if collected.is_empty() {
        return Ok(());
    }
    let mut ready: Vec<(u64, SpawnResult)> = Vec::with_capacity(collected.len());
    for child in collected.drain(..) {
        ready.push(child?);
    }
    ready.sort_by_key(|(ord, _)| *ord);
    for (_, result) in ready {
        let text = match result {
            SpawnResult::Composed { obs, .. } => obs,
            SpawnResult::Paused { request_id } => format!(
                "\n[child awaiting approval (request {request_id})] it stopped for a human; you \
                 did not wait for it, so this run continues without it\n"
            ),
            SpawnResult::Asked { question_id } => format!(
                "\n[child awaiting answer (question {question_id})] it asked the operator \
                 something; you did not wait for it, so this run continues without it\n"
            ),
        };
        tree.watch.emit(RunEvent::at_depth(
            run_id,
            step,
            depth,
            EventKind::ChildCollected {
                text: text.trim().to_string(),
            },
        ));
        ledger.push(Observation::new(
            step,
            ObsKind::Child,
            None,
            bound(&text, entry_cap, ObsKind::Child),
        ));
    }
    Ok(())
}

/// Await `work` while the children this agent stopped waiting for run.
///
/// This is what makes a detached child concurrent rather than merely deferred. A
/// set polled only at step boundaries would advance its children one poll per
/// parent step — the parent would not be blocked and the child would barely move,
/// which is the shape of the feature without its substance. Polled against the
/// parent's own completion, the child runs through the seconds the parent spends
/// waiting for a provider, which is where a run's wall clock actually goes.
pub(super) async fn driving<'f, T>(
    inflight: &mut Inflight<'f>,
    done: &mut Vec<Result<(u64, SpawnResult)>>,
    work: impl Future<Output = T>,
) -> T {
    let mut work = Box::pin(work);
    loop {
        if inflight.is_empty() {
            return work.await;
        }
        match select(&mut work, inflight.next()).await {
            Either::Left((finished, _)) => return finished,
            // `None` means the set drained while the parent was still waiting;
            // there is nothing left to poll, so stop racing and just wait.
            Either::Right((None, _)) => return work.await,
            Either::Right((Some(child), _)) => done.push(child),
        }
    }
}

/// Finish every child still in flight and make its report durable.
///
/// The reports land as observations on the parent's run rather than in the
/// ledger, because the loop that reads the ledger has returned: an operator finds
/// them by run id, and the agent does not see them. That is stated rather than
/// hidden — a parent that detaches a child and ends in the same breath was never
/// going to read the answer, and the alternative (granting it another step) is a
/// decision this release deliberately left open rather than guessed at.
pub(super) async fn drain_children<P: Provider>(
    tree: &Tree<'_, P>,
    run_id: i64,
    depth: u32,
    inflight: &mut Inflight<'_>,
) -> Result<()> {
    if inflight.is_empty() {
        return Ok(());
    }
    let step = tree.store.last_step(run_id)?;
    while let Some(done) = inflight.next().await {
        let text = match done?.1 {
            SpawnResult::Composed { obs, .. } => obs,
            SpawnResult::Paused { request_id } => {
                format!("\n[child awaiting approval (request {request_id}) after this run ended]\n")
            }
            SpawnResult::Asked { question_id } => {
                format!("\n[child awaiting answer (question {question_id}) after this run ended]\n")
            }
        };
        tree.watch.emit(RunEvent::at_depth(
            run_id,
            step,
            depth,
            EventKind::ChildCollected {
                text: text.trim().to_string(),
            },
        ));
        tree.store.record_observations(
            run_id,
            &[Observation::new(step, ObsKind::Child, None, text)],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn agent_loop<'f, 'i, P: Provider>(
    tree: &'f Tree<'_, P>,
    contract: &'f TaskContract,
    run_id: i64,
    depth: u32,
    policy: &'f Policy,
    start_step: u32,
    // 0.21.0 — the definition this agent was spawned from, if it was spawned from
    // one. Crate-internal rather than a `TaskContract` field: a run-level model
    // override is a separate decision about whether a conversation may change models
    // mid-thread, and 0.20.0 deliberately left that shut.
    identity: Option<&'f AgentDef>,
    // 0.50.0 — the children this agent stopped waiting for. Owned by the caller
    // so that every exit from this loop drains them at one place.
    inflight: &'i mut Inflight<'f>,
) -> Pin<Box<dyn Future<Output = Result<RunOutcome>> + 'i>>
where
    'f: 'i,
{
    // Boxed so the loop can recurse into itself when an agent spawns a child.
    Box::pin(async move {
        // 0.50.0 — the order children were spawned in, which is the order their
        // reports are folded in however they finish, and the reports that have
        // arrived since the last step boundary.
        let mut spawn_seq: u64 = 0;
        let mut collected: Vec<Result<(u64, SpawnResult)>> = Vec::new();
        // 0.39.0 — what a session turn adds, and only at the root. Every child
        // reads the empty set, so the four rules below are structurally inert for
        // anything this agent spawns rather than inert by four separate tests.
        let extras = tree.extras(depth);
        // And the turn is typed immediately, the same order — and for the same
        // reason — the flat loop writes it in: `Store::check_resumable` refuses a
        // `running` reply as work to continue, so every instruction between the
        // row's creation and this one is a window in which a killed process
        // leaves a row that says nothing about what it was and is offered as
        // resumable work. Moved here from below the ledger in 0.74.0, where the
        // egress proxy had grown in front of it.
        open_turn_kind(tree.store, run_id, extras)?;
        // 0.31.0 — the plan gate, at the root only. A child that could hold its own
        // plan open would mean a hundred pending plans from one `run_tree`, which is
        // the problem the gate exists to prevent rather than a feature of it. As in
        // the workspace loop, the phase's state is read from the store.
        let mut planning = depth == 0
            && contract.plan_gate.is_some()
            && tree.store.approved_plan(run_id)?.is_none();
        let mut effective = policy.clone();
        if planning {
            effective = effective.merge(plan_lock());
        }
        // 0.36.0 — the contract's own root, which is the tree's for every agent
        // except one given a worktree by its definition. Reading it from the
        // contract rather than from the tree is what makes a per-child root a
        // property of the contract the spawn built, so nothing else in this loop
        // has to know a worktree exists.
        // 0.45.0 — computed before the policy moves into the workspace, and twice for
        // the reason the flat loop does it twice: the plan gate narrows the boundary
        // while the phase is on. `policy` here is this agent's own — a child's is its
        // parent's narrowed by `Policy::contain` — so a child is told its boundary and
        // not the root's.
        // Children share their parent's workspace, so they share its detection too.
        let toolchain = crate::toolchain::detect(&tree.root);
        // Children share their parent's workspace, so they share its containment.
        let containment = exec_containment(&contract.exec_sandbox, toolchain.as_ref());
        // 0.48.0 — the same rule as the flat loop. A contained tree run whose
        // policy names hosts must not silently take the boolean while the flat
        // loop scopes its egress.
        let egress = start_egress_proxy(policy, containment.as_ref()).await;
        let containment = match (&containment, &egress) {
            (Some(c), Some((proxy, _, _))) => {
                #[cfg(feature = "browser")]
                tree.browser.route_through(proxy.addr());
                Some(std::sync::Arc::new(c.with_proxy(Some(proxy.addr()))))
            }
            _ => containment,
        };
        report_containment(
            tree.watch,
            run_id,
            depth,
            &contract.exec_sandbox,
            containment.as_deref(),
        );
        // 0.74.0 — the tree's measurement, taken once before the root ran, rather
        // than one probe per agent.
        //
        // **Why one is the honest number, not the cheap one.** A few lines up,
        // this loop says children share their parent's workspace and share its
        // containment, and that is the argument: the boundary a child runs under
        // *is* the one the tree resolved. Measuring it once per agent asks one
        // question N times and can only produce N answers to it — a twenty-agent
        // tree that disagrees with itself about its own boundary is worse evidence
        // than one number, quite apart from the sixty short-lived child processes
        // it spends before any agent's first prompt is composed.
        //
        // **This is not the cache the release refused.** A cache is a value kept
        // past the thing it was measured on — reused for a later run, for another
        // process, or for a different configuration — and it is refused because a
        // host's Landlock ABI, its `sandbox-exec` binary and its writable roots
        // can all move underneath it. None of that applies here. It is one
        // measurement of one containment, held for exactly the lifetime of the
        // `Tree` that has that containment and dropped with it: no `static`, no
        // `OnceLock`, nothing that outlives one tree.
        //
        // And it is never read for a boundary it did not measure. A spawned
        // child's contract carries its own `exec_sandbox`, and a contract that
        // asks for a different one is a different boundary — so that agent
        // measures its own rather than being told what another configuration
        // produced. That is the line between reusing a measurement and inventing
        // one.
        let probe = if contract.exec_sandbox == tree.probed {
            tree.probe.clone()
        } else {
            probe_boundary(
                tree.store,
                tree.watch,
                depth,
                run_id,
                &contract.exec_sandbox,
                containment.as_deref(),
            )
            .await
        };
        let after_planning = boundary_section(
            policy,
            &contract.exec_sandbox,
            will_proxy(policy, contract),
            &probe,
        );
        let while_planning = boundary_section(
            &effective,
            &contract.exec_sandbox,
            will_proxy(&effective, contract),
            &probe,
        );
        let agent_root = contract.root.as_deref().unwrap_or(&tree.root);
        let mut ws = Workspace::with_policy(agent_root, effective);
        // The tree shares one MCP session, so every agent in it — root or child —
        // is offered the same server tools beside its built-ins. Connecting a
        // session and then not offering its tools would leave the model unable to
        // call something the run had already paid to set up.
        let mut extra = tree.tools.specs();
        extra.extend(tree.mcp.tool_specs());
        extra.extend(lsp_tools(tree.lsp));
        #[cfg(feature = "browser")]
        if tree.browser.configured() {
            extra.extend(crate::tools::browser::browser_tools());
        }
        extra.extend(skill_tool(tree.skills));
        // A role is PREPENDED, never a replacement: the tree prompt is what tells an
        // agent how to use its tools and that its result composes back into its
        // parent, and a role that replaced it would produce an agent that did not
        // know how to be one. It sits ahead of the whole composed prompt, so the
        // crate's ending is still the last thing an agent with a role reads.
        let with_role = |directive: Option<String>, boundary: Option<&str>| {
            let body = compose(PromptSpec {
                base: TREE_PROMPT,
                prompt: &contract.prompt,
                extra: &extra,
                skills: tree.skills,
                directive,
                instructions: &contract.instructions,
                boundary,
                family: tree.provider.prompt_family(),
                ending: CALL_TOOLS_ENDING,
            });
            match identity.and_then(|d| d.role.as_deref()) {
                Some(role) => format!("{}\n\n{body}", role.trim()),
                None => body,
            }
        };
        let base_system = with_role(None, after_planning.as_deref());
        let mut system = match planning {
            true => with_role(
                // `false`: composed for a turn already decided to be work.
                Some(planning_directive(&contract.agents, false)),
                while_planning.as_deref(),
            ),
            false => base_system.clone(),
        };
        report_prompt(
            tree.watch,
            run_id,
            depth,
            &system,
            contract,
            tree.provider.prompt_family(),
            after_planning.is_some(),
        );
        // 0.39.0 — the opening a contained turn's first completion is made with,
        // and only the first, exactly as the flat loop builds it. `None` for every
        // agent that is not a classifying turn's root, which is every child and
        // every agent of every `run_tree`.
        let conversational = conversational_opening(
            // 0.49.0 — as the flat loop, over the tree agent's own description.
            CONVERSATION_TREE_PROMPT,
            contract,
            extras,
            &extra,
            tree.skills,
            planning,
            after_planning.as_deref(),
            while_planning.as_deref(),
            tree.provider.prompt_family(),
        );
        let mut tools = tree_tools(tree.agents);
        tools.extend(extra);
        if planning {
            tools.push(propose_plan_spec());
        }
        // The budget this agent runs under is the smaller of what its contract
        // asked for and what the tree has left — a contract cannot raise it.
        let token_cap = tree.ledger.effective_token_budget(contract.max_tokens);
        // Durable per-agent budget, restored across a restart.
        let mut tokens_used: u64 = tree.store.spent_tokens(run_id)?;
        // 0.44.0 — per agent, not per tree. Each agent in a tree assembles its own
        // prompt from its own ledger, so one agent's frozen prefix says nothing about
        // another's, and a shared one would mark a prefix this agent has never sent.
        let mut marked_prefix = PrefixGuard::default();
        // 0.49.0 — per agent in the tree, for the reason the flat loop keeps one per
        // run: a child's turns are its own and must never reach its parent's request.
        //
        // 0.64.0 — and restored through the same function the flat loop uses, keyed
        // on this agent's own run id. A resumed agent is the same agent, at whatever
        // depth it sits, and an agent whose earlier turns were driven by a process
        // that is gone would otherwise be the only one in the tree reading a
        // third-person account of its own actions.
        let mut turns: BTreeMap<u32, StepTurn> = restore_turns(tree.store, run_id)?;
        // Same ledger and same per-turn assembly as the workspace loop: a tree of
        // 100 children each re-sending its own unbounded log is the multiplied
        // version of the problem 0.10.0 exists to fix — and, since 0.13.0, the
        // same restore, keyed on this agent's own run id. A child that is resumed
        // is the same child, at whatever depth it sits.
        let (mut ledger, mut written) = restore_ledger(tree.store, run_id)?;
        // The conversation this turn continues, at the root and nowhere else: a
        // child is given its goal, not the transcript.
        seed_conversation(&mut ledger, extras);
        // 0.68.0 — durable immediately, for the reason argued at the flat loop's
        // own call: a fold may only replace what the store already holds, so a
        // conversation above the watermark is one no fold can reach. `extras` is
        // root-only here, so at every other depth this appends nothing.
        written = persist_ledger(tree.store, run_id, &ledger, written)?;
        // 0.68.0 — the caller's standing request for a fold, consumed once, and by
        // the root only. `fold_forced` is what enforces both.
        let mut fold_asked = contract.fold_now;
        // 0.70.0 — see the workspace loop, including why this is seeded from the
        // store rather than from `false`: a resume that does not raise the cap
        // runs an empty loop and would otherwise un-conclude what a previous
        // attempt judged. Per agent, like everything else here: a child that
        // failed its own criterion composes back into its parent carrying that,
        // so a parent can tell a child that ran out of room from one whose work
        // was checked and rejected.
        let mut criterion_failed = criterion_already_failed(tree.store, run_id);
        // 0.70.0 — the gate-failure section most recently appended, so a
        // criterion failing the same way every step is reported once rather than
        // once per step. Per agent, like everything else in this loop.
        let mut last_gate_feedback: Option<String> = None;
        let mut progress = Progress::new();
        // Per agent, not per tree. A child's handles are the child's: it is the
        // one that knows when they are finished with, and a shared registry
        // would let one agent kill a sibling's dev server. The cap is therefore
        // also per agent, which is the same rule the ledger's budgets follow.
        let handles = std::sync::Arc::new(crate::tools::handles::Handles::new(
            crate::tools::handles::MAX_LIVE_HANDLES,
        ));
        // See the workspace loop: an orphan is adopted so it can be answered.
        for h in tree.store.process_handles(run_id)? {
            if h.state == "orphaned" {
                handles.adopt_orphan(h.handle, &h.line);
            }
        }
        let handles = &handles;
        // Children share their parent's workspace, so they share its memory: one
        // note store per workspace, every entry attributed to the run that wrote it.
        let mem_key = memory_key(&tree.root);
        // See the workspace loop: viewed images ride one step and are dropped.
        let pending_media = &mut PendingMedia::default();

        // 0.50.0 — every child this agent stopped waiting for and did not live to
        // see finish. Only for a resume, and only for steps that already
        // committed: a step left uncommitted is replayed, and replaying it
        // re-adopts its children through the ordinary spawn path — doing both
        // would adopt one child twice.
        // 0.74.0 — `ws.policy()`, not `policy`: while the plan phase is on that is
        // `policy` narrowed by `plan_lock`, and a child re-adopted from a previous
        // process must come back under the boundary this agent is actually running
        // inside rather than the one its contract asked for. See the fan-out below,
        // which passes the same thing for the same reason.
        readopt_children(tree, run_id, depth, ws.policy(), start_step, inflight).await?;

        // 0.74.0 — the rules an approver asked to stop being asked about, kept for
        // the rest of this agent's run as the flat loop keeps them. Until now the
        // tree dropped them at the end of every step, so "approve and stop asking"
        // meant "approve again next step" inside a `run_tree` and meant what it said
        // in a flat run.
        let mut remembered: Vec<Rule> = Vec::new();

        for step in start_step..=contract.max_steps {
            // 0.48.0 — the same per-step refresh and drain the flat loop does. A
            // dial made by a contained agent inside a tree is recorded at the
            // depth it happened at, so a trace shows which agent reached out.
            if let Some((proxy, shared, at)) = &egress {
                at.store(step, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut guard) = shared.write() {
                    guard.clone_from(ws.policy());
                }
                record_dials(Some(proxy), tree.store, tree.watch, run_id, depth)?;
            }
            // The step boundary, where a cancellation is honoured (see `cancelled`).
            // One flag for the whole tree, so a cancel asked for while a sibling was
            // mid-flight stops this agent too.
            if let Some(o) = cancelled(tree.store, tree.watch, run_id, depth, step - 1)? {
                return Ok(o);
            }
            // The same boundary carries an operator's steering, and it is the one
            // point at which no child of this agent is in flight: children are
            // awaited inside the step that spawned them.
            if let Some(o) = drain_steer(
                tree.store,
                tree.watch,
                run_id,
                step,
                &mut ledger,
                extras,
                &mut fold_asked,
            )? {
                return Ok(o);
            }
            if let Some(max) = contract.max_duration {
                if tree.store.elapsed_secs(run_id)? > max.as_secs_f64() {
                    finish(
                        tree.store,
                        tree.watch,
                        run_id,
                        depth,
                        step - 1,
                        "time_budget_exceeded",
                    )?;
                    return Ok(RunOutcome::TimeBudgetExceeded { steps: step - 1 });
                }
            }

            let budget_tokens = contract
                .context
                .effective_tokens(Some(token_cap.saturating_sub(tokens_used)));
            let entry_cap = entry_cap_chars(budget_tokens);
            let max_read = contract.max_read_chars.map(|c| c as usize);
            // 0.50.0 — the reports of children this agent stopped waiting for,
            // folded before the prompt is assembled so the model reads them on this
            // step rather than the next one. After `entry_cap` because a report is
            // bounded like every other observation.
            fold_collected(
                tree,
                run_id,
                depth,
                step,
                entry_cap,
                inflight,
                &mut collected,
                &mut ledger,
            )?;
            // 0.57.0 — the tree loop's own signals, from this agent's goal and
            // this agent's ledger. A child ranks against the work IT was given,
            // not against the parent's: `contract.goal` here is the child's, and
            // the ledger is the one this depth folds.
            let signals = recall_signals(&contract.goal, &ledger);
            let (notes, global_notes) = recall_scopes(tree.store, &mem_key, &signals)?;
            // 0.43.0 — the tree loop's own call to the one fold helper, in the same
            // place for the same reason. A child folds its own ledger at its own
            // depth: the summary is of the work that agent did, not of the tree.
            // At most two attempts, for the reason the flat loop states.
            let mut fold_tokens = 0;
            let mut recovered = false;
            let (response, assembled, user) = loop {
                fold_tokens += compact_ledger(
                    tree.provider,
                    contract,
                    tree.store,
                    run_id,
                    step,
                    tree.watch,
                    depth,
                    &mut ledger,
                    &mut written,
                    budget_tokens,
                    fold_forced(recovered, depth, &mut fold_asked),
                )
                .await?;
                let assembled = assemble(
                    &ledger,
                    budget_tokens,
                    &notes,
                    &global_notes,
                    Assembly {
                        ws: Some(&ws),
                        // 0.74.0 — the policy this agent is running under, which is
                        // what the flat loop passes. A stale read refreshed against
                        // the contract's own policy would re-read through a deny the
                        // plan phase or an approver had since put in the way.
                        policy: ws.policy(),
                        store: tree.store,
                        run_id,
                        step,
                    },
                )
                .await?;
                // 0.48.0 — the same rule as the flat loop, and for the same reason
                // the system half is chosen this way here too.
                let user = match &conversational {
                    Some(_) if step == start_step => {
                        conversational_user_prompt(&contract.goal, &assembled.text, &contract.tool_mask)
                    }
                    _ => workspace_user_prompt(contract, &assembled.text, toolchain.as_ref()),
                };
                // 0.44.0 — the same rule as the flat loop, through the same helper.
                // A boundary computed in one loop and not the other would make a
                // contained run and a flat one cache differently while nothing failed.
                let cache_boundary = cache_boundary_for(
                    &user,
                    &ledger,
                    &mut marked_prefix,
                    tree.watch,
                    run_id,
                    step,
                    depth,
                );
                let messages = transcript(&user, &assembled, &turns);
                #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
                let request = CompletionRequest {
                    // 0.39.0 — a contained turn's opening is its first completion
                    // only. Every later step is the tree loop of 0.38.0, asked the way
                    // it has always been asked.
                    system: match &conversational {
                        Some(c) if step == start_step => c.clone(),
                        _ => system.clone(),
                    },
                    user: user.clone(),
                    // 0.49.0 — the same conversation the flat loop sends, built by the
                    // same helper: a transcript assembled in one loop and not the other
                    // would make a contained run and a flat one talk to the model
                    // differently while nothing failed.
                    messages: messages.clone(),
                    tools: tools.clone(),
                    // 0.21.0 — a named agent's model. `None` for the root and for any
                    // child spawned without a definition, which is what every provider
                    // reads as "the model you were built with".
                    model: identity.and_then(|d| d.model.clone()),
                    // 0.22.0 — this agent's declaration, which for a child is the
                    // root's, copied in by `spawn_child` rather than taken from the
                    // spawn arguments.
                    web: contract.web.clone(),
                    // 0.31.0 — this role's tier, falling back to the run's. The
                    // definition wins because that is where "search cheaply, think hard
                    // only where thinking is the work" is said; the contract's is the
                    // root's own, and a child spawned without a definition inherits it.
                    effort: identity.and_then(|d| d.effort).or(contract.effort),
                    cache_boundary,
                    // 0.49.0 — as the flat loop, through the same helper.
                    cache_through: cache_through_for(cache_boundary, &messages),
                    #[cfg(feature = "media")]
                    media: attach_media(contract, pending_media)?,
                    ..Default::default()
                };
                match driving(
                    inflight,
                    &mut collected,
                    complete_with_retry(
                        tree.provider,
                        &request,
                        contract,
                        tree.store,
                        run_id,
                        step,
                        tree.watch,
                        depth,
                        // Streaming is the turn's choice and reaches the root
                        // only: a child's text is composed back into its parent,
                        // not shown.
                        extras.stream,
                        !recovered && contract.compaction.enabled(),
                        // 0.54.0 — the tree loop dispatches serially and never
                        // took 0.41.0's batch path either. Widening it is its own
                        // release, not a side effect of this one.
                        None,
                    ),
                )
                .await
                {
                    Ok(response) => break (response, assembled, user),
                    // Same condition as `may_compact` above, for the reason the flat
                    // loop states.
                    Err(e)
                        if !recovered
                            && contract.compaction.enabled()
                            && is_context_overflow(&e) =>
                    {
                        recovered = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            // Same record on the tree path: a sub-agent's run is a run, and its
            // recalls belong to it rather than to whoever spawned it. Recorded once,
            // after the attempt that succeeded.
            record_recalls(
                tree.store,
                run_id,
                step,
                &mem_key,
                &global_notes,
                &assembled,
            )?;

            // Which provider answered, when that is not a foregone conclusion. A
            // `Fallback` that fell over served this step from its secondary, and a
            // trace reader has no other way to know.
            if let Some(served) = tree.provider.last_served() {
                tree.store
                    .record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
                tree.watch.emit(RunEvent::at_depth(
                    run_id,
                    step,
                    depth,
                    EventKind::FellBackTo { provider: served },
                ));
            }
            // The fold's own completion is part of what this step cost, for the
            // reason the flat loop states.
            let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0) + fold_tokens;
            tokens_used += step_tokens;
            if step_tokens > 0 {
                tree.store
                    .record_context_reported(run_id, step, step_tokens)?;
            }

            // See the workspace loop: visible to a watcher, and never on the ledger.
            if let Some(thinking) = response.reasoning.as_deref() {
                tree.watch.emit(RunEvent::at_depth(
                    run_id,
                    step,
                    depth,
                    EventKind::Reasoning {
                        text: thinking.to_string(),
                        tokens: response.usage.map(|u| u.reasoning_tokens).unwrap_or(0),
                    },
                ));
            }

            // 0.49.0 — recorded before dispatch, as the flat loop does.
            //
            // 0.64.0 — and staged durably, as the flat loop does. The write rides
            // the transaction that commits this step, so a step that never commits
            // and an agent whose lease was taken from it both leave no turn.
            turns.insert(
                step,
                StepTurn {
                    text: response.text.clone(),
                    calls: response.tool_calls.clone(),
                },
            );
            tree.store.stage_step_turn(
                run_id,
                AssistantTurn::new(step, response.text.clone(), response.tool_calls.clone()),
            );

            // 0.50.0 — and recorded as an agent event, which is a different fact
            // from the turn staged above: the last of THESE rows is what this
            // agent's parent composes as its conclusion, so a parent that adopts a
            // child a previous process left behind reads the same words a parent
            // that waited does. The turn is what the agent itself is sent back on
            // a resume; this is what its parent quotes. Same text, two readers,
            // and folding them into one row would make either the parent's
            // conclusion or the agent's own transcript the other's leftovers.
            // Here rather than in `finish` because every ending is a different
            // return and `turns` is in scope at exactly one place.
            if let Some(said) = response
                .text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                tree.store
                    .record_agent_event(&AgentEvent::said(run_id, step, said))?;
            }

            let mut decisions: Vec<String> = Vec::new();
            let mut calls_json: Vec<String> = Vec::new();
            let mut step_changed = false;
            // What an approver asked to remember during THIS step, applied once at
            // the end of it. Per-step rather than per-call for the reason the flat
            // loop gives: a policy rebuilt mid-step would decide two calls of one
            // completion under two different boundaries.
            let mut new_rules: Vec<Rule> = Vec::new();
            // The same note as the flat loop: a broken provider-executed search is
            // an observation the child can act on, not silence.
            if let Some(note) = web_failure_note(&response) {
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &format!("\n[step {step}] provider web tool: {note}\n"),
                        entry_cap,
                        ObsKind::Message,
                    ),
                ));
            }
            if response.tool_calls.is_empty() {
                let said = response.text.clone().unwrap_or_default();
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &format!("\n[step {step}] {NO_TOOL_CALL} {said}\n"),
                        entry_cap,
                        ObsKind::Message,
                    ),
                ));
                decisions.push("no tool call".into());
            }
            // 0.39.0 — the same first-completion verdict the flat loop reads, in
            // the same place and through the same helper. A contained turn that
            // was conversation ends here, before a single child exists: the
            // classification happens on the completion that would have carried the
            // spawn call, so an answered turn spawns nothing by construction
            // rather than by a cap.
            if let Some(o) = classify_first_completion(
                tree.store,
                tree.watch,
                run_id,
                contract,
                &response,
                tokens_used,
                &ledger,
                written,
                extras,
                step,
                start_step,
            )? {
                return Ok(o);
            }
            // Non-spawn tools mutate the workspace and the observation log, so
            // they run in order. Spawn calls are independent sub-agents, so they
            // fan out concurrently, bounded by the tree's `max_concurrent_agents`.
            let mut paused: Option<i64> = None;
            // 0.21.0 — the other reason a step can stop short: a question nobody here
            // would answer. Kept separate from `paused` so the two pauses cannot be
            // confused for one another in the outcome.
            let mut asked: Option<i64> = None;
            let mut plan_pending: Option<i64> = None;
            let mut plan_cancelled = false;
            let mut plan_approved = false;
            let mut paused_by_child = false;
            let mut spawn_calls: Vec<&ToolCall> = Vec::new();
            for call in &response.tool_calls {
                calls_json.push(format!("{}:{}", call.name, call.arguments));
                // 0.74.0 — the three tools this loop handles itself never reach
                // `dispatch`, which is where every other call is put to the
                // operator's `before_tool` checks. They are asked here instead,
                // ahead of the short-circuits below, so a `[[hook]]` naming
                // `spawn_agent`, `send_message` or `read_messages` fires rather than
                // loading, validating, installing and then silently approving —
                // which is the one failure a check attached to a tool cannot afford,
                // and looks identical to a check that said yes. Everything past this
                // point reaches `dispatch` and is gated there, so no call is asked
                // about twice.
                if call.name == SPAWN_TOOL
                    || call.name == SEND_MESSAGE_TOOL
                    || call.name == READ_MESSAGES_TOOL
                {
                    if let Some(refused) = tool_gate(
                        contract.tool_hooks.as_deref(),
                        call,
                        tree.watch,
                        run_id,
                        step,
                        depth,
                    ) {
                        // A hook can only refuse, and `tool_gate` renders that as a
                        // `Continue` carrying the sentence the model reads. The
                        // `continue` is outside the destructuring on purpose: any
                        // other shape is still a refusal, and a call that fell
                        // through to run would be this gate failing open.
                        if let Dispatched::Continue {
                            decision,
                            obs,
                            kind,
                            target,
                            ..
                        } = refused
                        {
                            ledger.push(Observation::new(step, kind, target, obs));
                            decisions.push(decision);
                        }
                        continue;
                    }
                }
                if call.name == SPAWN_TOOL {
                    // 0.74.0 — refused while the plan is unreviewed, for the reason
                    // `remember` and `forget` are refused in `dispatch`: a spawn is
                    // intercepted here and never resolves through `Policy::explain`,
                    // so the `plan-gate` layer cannot cover it. A child started
                    // before a human saw the plan inherits this run's whole boundary
                    // and does the work outside the gate, which is not one act
                    // slipping past the phase but the phase not existing.
                    if planning {
                        ledger.push(Observation::new(
                            step,
                            ObsKind::Error,
                            None,
                            bound(
                                &format!(
                                    "\n[{SPAWN_TOOL} refused] the plan has not been approved \
                                     yet, so no sub-agent is being started — a child would \
                                     inherit this run's permissions and work outside the \
                                     phase. Call `{PROPOSE_PLAN_TOOL}` first.\n"
                                ),
                                entry_cap,
                                ObsKind::Error,
                            ),
                        ));
                        decisions.push(format!("{SPAWN_TOOL} refused (planning)"));
                        continue;
                    }
                    spawn_calls.push(call);
                    continue;
                }
                // 0.60.0 — the mailbox, handled here rather than in `dispatch`
                // because it needs the tree this run belongs to and because a flat
                // run must not have it. In order with every other non-spawn tool:
                // a send and the write that follows it are one agent's sequence,
                // and reordering them would let a sibling read a finding about a
                // file that had not been written yet.
                if call.name == SEND_MESSAGE_TOOL || call.name == READ_MESSAGES_TOOL {
                    // 0.60.0 — DRIVEN, for the reason the provider call is, and
                    // this is the release's sharpest lesson. A detached child is
                    // a future in this agent's own `inflight` set: nothing else
                    // polls it. So a wait that merely slept would stop the very
                    // siblings whose message it was waiting for, and every wait
                    // would run to its full clock and then succeed on the step
                    // after — which is exactly what the first implementation did.
                    // The mechanism already existed; the wait had to use it.
                    let (decision, obs) = driving(
                        inflight,
                        &mut collected,
                        mailbox_call(
                            tree.store,
                            call,
                            run_id,
                            step,
                            contract
                                .max_wait_secs
                                .map(Duration::from_secs)
                                .unwrap_or(DEFAULT_MAX_WAIT),
                        ),
                    )
                    .await?;
                    ledger.push(Observation::new(
                        step,
                        ObsKind::Child,
                        None,
                        bound(&obs, entry_cap, ObsKind::Child),
                    ));
                    decisions.push(decision);
                    // Reading is not progress and sending is: an agent that only
                    // ever polls an empty mailbox is exactly the stall the loop's
                    // no-change detection exists to notice.
                    step_changed |= call.name == SEND_MESSAGE_TOOL;
                    continue;
                }
                match dispatch(
                    &ws,
                    call,
                    tree.approver,
                    tree.responder,
                    tree.store,
                    run_id,
                    step,
                    tree.mcp,
                    tree.lsp,
                    tree.browser,
                    tree.tools,
                    tree.skills,
                    entry_cap,
                    max_read,
                    &mem_key,
                    contract.memory,
                    tree.watch,
                    depth,
                    pending_media,
                    &contract.commit_identity,
                    contract.exec_timeout,
                    containment.as_ref(),
                    toolchain.as_ref(),
                    handles,
                    PlanPhase {
                        gate: contract.plan_gate.as_deref().filter(|_| depth == 0),
                        agents: &contract.agents,
                        active: planning,
                    },
                    &contract.goal,
                    contract.tool_hooks.as_deref(),
                    // A child's contract is built fresh by `spawn_child` and
                    // carries no mask, which is the same boundary `fold_now`
                    // draws: the mask is a request about the operator's own turn,
                    // and a child's work is not that turn.
                    &contract.tool_mask,
                )
                .await?
                {
                    Dispatched::Continue {
                        decision,
                        obs,
                        kind,
                        target,
                        changed,
                        remember,
                    } => {
                        step_changed |= changed;
                        ledger.push(Observation::new(step, kind, target, obs));
                        decisions.push(decision);
                        new_rules.extend(remember);
                    }
                    Dispatched::Pause { request_id } => {
                        decisions.push(format!("awaiting approval (request {request_id})"));
                        paused = Some(request_id);
                        break;
                    }
                    Dispatched::Ask { question_id } => {
                        decisions.push(format!("awaiting answer (question {question_id})"));
                        asked = Some(question_id);
                        break;
                    }
                    Dispatched::Plan { plan_id, verdict } => match verdict {
                        Some(PlanVerdict::Approve) => {
                            decisions.push(format!("plan {plan_id} approved"));
                            plan_approved = true;
                        }
                        Some(PlanVerdict::Cancel) => {
                            decisions.push(format!("plan {plan_id} cancelled"));
                            plan_cancelled = true;
                            break;
                        }
                        _ => {
                            decisions.push(format!("awaiting plan decision (plan {plan_id})"));
                            plan_pending = Some(plan_id);
                            break;
                        }
                    },
                }
            }
            // The phase ends here, and the spawn calls below are the reason it must:
            // an approved plan names the agents it hands work to, and until it is
            // approved a spawn is refused at the top of the loop above. 0.74.0
            // corrected the sentence that used to stand here — it said the
            // `plan-gate` layer refused a spawn as an `Act::Exec`, and it does not:
            // a spawn is intercepted before `dispatch` and never reaches
            // `Policy::explain` at all, which is why the refusal had to be written.
            if plan_approved {
                planning = false;
                // Rebuilt from the base rather than edited, so the `plan-gate` layer
                // goes and every rule an approver remembered stays: an approval ends
                // the phase, it does not undo a decision a human already made. The
                // flat loop states the same argument at its own rebuild.
                let mut unlocked = policy.clone();
                if !remembered.is_empty() {
                    unlocked = unlocked.merge(remembered_layer(&remembered));
                }
                ws = Workspace::with_policy(&tree.root, unlocked);
                tools.retain(|t| t.name != PROPOSE_PLAN_TOOL);
                system = base_system.clone();
                if let Some(approved) = tree.store.approved_plan(run_id)? {
                    ledger.push(Observation::new(
                        step,
                        ObsKind::Message,
                        None,
                        bound(
                            &format!(
                                "\n[plan approved]\n{}\n(This is the approach you agreed to. \
                                 Carry it out.)\n",
                                approved.render()
                            ),
                            entry_cap,
                            ObsKind::Message,
                        ),
                    ));
                }
            }
            if plan_pending.is_some() || plan_cancelled {
                // Nothing has been written and no child was spawned, so there is no
                // composition to do: commit what was read and stop.
                spawn_calls.clear();
            }
            if paused.is_none() && !spawn_calls.is_empty() {
                use futures_util::stream;
                // Every spawn call is polled, and the tree's ledger — not this
                // stream — is what decides how many actually run. Until 0.32.0
                // this width was `max_concurrent`, which made the cap a per-step
                // property of one parent: two parents fanning out at the same time
                // got twice the concurrency they asked for, and a child past the
                // width was not queued, it was simply not started until an earlier
                // sibling finished, invisibly. Now every child reaches the ledger,
                // takes a slot or a place in its tier's FIFO queue, and the queue
                // is a fact in the store an observer and a resume can both read.
                let width = spawn_calls.len().max(1);
                // `buffered`, not `buffer_unordered`: children run as slots allow,
                // but their results are collected in the order the model asked for
                // them rather than the order they happen to finish.
                //
                // Until 0.12.0 this was `buffer_unordered`, which made a tree run
                // non-reproducible: the composed child observations and the
                // `decisions` list — both of which become the `steps.result` and
                // `steps.decision` columns — came back in completion order, so the
                // same task over the same workspace produced a different trace and
                // a different next prompt depending on which child won a race.
                // Deterministic replay cannot be built on that.
                //
                // The cost is that a child which finishes early has its result held
                // until the children before it are done. That changes when a result
                // is *read*, never when the work runs.
                let results: Vec<Result<SpawnOutcome>> = stream::iter(
                    spawn_calls
                        .into_iter()
                        // 0.74.0 — `ws.policy()`, not `policy`. A child may only
                        // narrow what it is handed, so handing it the contract's own
                        // policy while this agent is running under a narrowed one
                        // gives the child MORE than its parent has: through the plan
                        // phase that is `plan_lock`'s `deny_write("*")` and
                        // `deny_exec("*")`, and after an approver has narrowed the
                        // run it is that decision. What the parent is actually
                        // running under is `ws`.
                        .map(|c| spawn_child(tree, c, run_id, depth, ws.policy(), step)),
                )
                .buffered(width)
                .collect()
                .await;
                for r in results {
                    match r? {
                        // 0.50.0 — the parent stopped waiting for this one. The
                        // observation says so, and the child comes with it: it is
                        // still running, still holding its slot, and its report
                        // will arrive at a later step.
                        SpawnOutcome::InFlight { decision, obs, fut } => {
                            ledger.push(Observation::new(
                                step,
                                ObsKind::Child,
                                None,
                                bound(&obs, entry_cap, ObsKind::Child),
                            ));
                            decisions.push(decision);
                            // Starting a child is work the parent did, whether or
                            // not it waited for the answer.
                            step_changed = true;
                            let ord = spawn_seq;
                            spawn_seq += 1;
                            inflight.push(Box::pin(async move { Ok((ord, fut.await?)) }));
                        }
                        SpawnOutcome::Settled(SpawnResult::Composed { decision, obs }) => {
                            // A child's composed result is an observation like any
                            // other, and is bounded like any other.
                            ledger.push(Observation::new(
                                step,
                                ObsKind::Child,
                                None,
                                bound(&obs, entry_cap, ObsKind::Child),
                            ));
                            decisions.push(decision);
                            // A child that ran did work the parent did not have to.
                            // Whether it changed the workspace is the child's own
                            // stall problem, tracked in the child's own loop.
                            step_changed = true;
                        }
                        // A child deferred; pause the tree with its request_id.
                        SpawnOutcome::Settled(SpawnResult::Paused { request_id }) => {
                            decisions
                                .push(format!("child awaiting approval (request {request_id})"));
                            paused = Some(request_id);
                            paused_by_child = true;
                        }
                        // A child asked the operator something; pause the tree with its
                        // question_id, the same way.
                        SpawnOutcome::Settled(SpawnResult::Asked { question_id }) => {
                            decisions
                                .push(format!("child awaiting answer (question {question_id})"));
                            asked = Some(question_id);
                            paused_by_child = true;
                        }
                    }
                }
            }

            // An agent paused because one of its CHILDREN deferred does NOT commit
            // this step: on resume it must replay it to re-adopt and resume that
            // paused child (only the parent re-entering `spawn_child` can wait on
            // the child again). An agent paused by its OWN gate commits normally —
            // it resumes from the step after, past the now-approved action.
            //
            // The condition is passed to the one boundary rather than branching
            // around it, so the uncommitted case is a stated argument at the single
            // commit point instead of a second, quieter commit path that a later
            // change could forget about. `commit_step` emits no `EventKind::Step`
            // for it either: there is no committed step to report, and a resume is
            // going to run this step again.
            // 0.21.0 — `asked` counts the same as `paused` here. A parent step whose
            // child stopped for a human must be replayed on resume so the parent
            // re-adopts that child; committing it would leave the child stranded and
            // the parent believing the spawn was done.
            let committed = !((paused.is_some() || asked.is_some()) && paused_by_child);
            commit_step(
                tree.store,
                tree.watch,
                run_id,
                depth,
                StepRecord::new(step, decisions.join("; "), ledger.text_for_step(step)).with_trace(
                    user,
                    calls_json.join(" | "),
                    step_tokens,
                ),
                step_changed,
                committed,
            )?;
            // Only when the step actually committed. A step paused by a child is
            // deliberately left uncommitted so the resume replays it (0.7.0's
            // fix for double execution); persisting its observations would mean
            // the replay observed everything twice.
            if committed {
                written = persist_ledger(tree.store, run_id, &ledger, written)?;
            }

            // 0.5.0 spawns up to a hundred of these, so an agent burning its whole
            // step budget going nowhere is the multiplied version of the problem.
            let signature = calls_json.join(" | ");
            match progress.step(contract.stall, step_changed, &signature) {
                Progressing::Fine => {}
                Progressing::Replan => {
                    tree.store.record_context_event(
                        run_id,
                        &ContextEvent::replan(
                            step,
                            format!(
                                "{} steps without progress; replanning",
                                contract.stall.window
                            ),
                        ),
                    )?;
                    ledger.push(Observation::new(
                        step,
                        ObsKind::Message,
                        None,
                        bound(
                            &progress.replan_directive(contract.stall.window, &decisions),
                            entry_cap,
                            ObsKind::Message,
                        ),
                    ));
                    info!(run_id, depth, step, "agent told to change approach");
                    tree.watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::Replan {
                            window: contract.stall.window,
                        },
                    ));
                }
                Progressing::Stalled => {
                    tree.store.record_context_event(
                        run_id,
                        &ContextEvent::stalled(step, "still no progress after replanning"),
                    )?;
                    info!(run_id, depth, step, "agent stopped: stalled");
                    tree.watch
                        .emit(RunEvent::at_depth(run_id, step, depth, EventKind::Stalled));
                    finish(tree.store, tree.watch, run_id, depth, step, "stalled")?;
                    return Ok(RunOutcome::Stalled { steps: step });
                }
            }

            if let Some(question_id) = asked {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "awaiting_answer",
                )?;
                return Ok(RunOutcome::AwaitingAnswer {
                    question_id,
                    steps: step,
                });
            }

            // 0.31.0 — the root's plan. Checked beside the two pauses above rather
            // than before them because a plan call and an approval cannot happen in
            // the same step: while the phase is on, every act that could be approved
            // is refused.
            if let Some(plan_id) = plan_pending {
                finish(tree.store, tree.watch, run_id, depth, step, "awaiting_plan")?;
                return Ok(RunOutcome::AwaitingPlan {
                    plan_id,
                    steps: step,
                });
            }
            if plan_cancelled {
                finish(tree.store, tree.watch, run_id, depth, step, "plan_rejected")?;
                return Ok(RunOutcome::PlanRejected { steps: step });
            }

            if let Some(request_id) = paused {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "awaiting_approval",
                )?;
                return Ok(RunOutcome::AwaitingApproval {
                    request_id,
                    steps: step,
                });
            }

            // 0.74.0 — rules an approver asked to remember apply as a top layer for
            // the rest of this agent's run, exactly as they do in the flat loop.
            // Until now the tree read them off `Dispatched::Continue` and threw them
            // away, so an operator who said "allow this and stop asking" was asked
            // again on the next step of the same run — and one who said "and never
            // this other path" had that decision forgotten. Merging rather than
            // editing is what keeps a remembered allow from defeating a deny
            // beneath it.
            if !new_rules.is_empty() {
                let merged = ws.policy().clone().merge(remembered_layer(&new_rules));
                ws = Workspace::with_policy(agent_root, merged);
                remembered.extend(new_rules);
            }

            // Draw this step's tokens against the tree. The draw is recorded even
            // when it crosses the ceiling — the tokens were already spent — and a
            // crossing halts the whole tree, not just this agent.
            let draw = tree.ledger.draw_tokens(step_tokens);
            let remaining = tree.ledger.remaining_tokens();
            tree.store.record_agent_event(&AgentEvent::budget_draw(
                run_id,
                step,
                step_tokens,
                remaining,
            ))?;
            tree.watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::SpendDraw {
                    tokens: step_tokens,
                    // A tree always has a ceiling, so there is always a number here;
                    // the field is optional for a future draw against no ceiling.
                    remaining: Some(remaining),
                },
            ));
            if draw == Draw::Halted {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "budget_ceiling_reached",
                )?;
                return Ok(RunOutcome::BudgetCeilingReached { steps: step });
            }
            // The tree's wall-clock ceiling, measured from the ROOT's `started_at`
            // rather than this agent's: a child spawned twenty hours in has a young
            // stamp of its own, and the ceiling is about the whole tree. Checked
            // beside the token draw because both are the same kind of limit — one a
            // contract cannot raise, crossing which halts the tree rather than the
            // agent that noticed.
            //
            // `max_total_duration` has existed on `Containment` since 0.5.0 and was
            // never read, so a caller could set a ceiling on a 24-hour tree and have
            // it silently ignored. Enforced in 0.12.0. Its sibling `max_total_cost`
            // still cannot be: there is no price telemetry to compare against.
            if let Some(max) = tree.containment.max_total_duration {
                if tree.store.elapsed_secs(tree.root_run_id)? > max.as_secs_f64() {
                    finish(
                        tree.store,
                        tree.watch,
                        run_id,
                        depth,
                        step,
                        "budget_ceiling_reached",
                    )?;
                    info!(run_id, depth, step, "tree stopped: duration ceiling");
                    return Ok(RunOutcome::BudgetCeilingReached { steps: step });
                }
            }
            // This agent's own contract budget (never looser than the tree's).
            if tokens_used > token_cap {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "cost_budget_exceeded",
                )?;
                return Ok(RunOutcome::CostBudgetExceeded { steps: step });
            }

            // As in the workspace loop: no criterion means the agent's own quiet
            // turn ends it. A child composes back into its parent carrying this
            // outcome, so a parent can tell a child that finished from one that
            // ran out of steps.
            if finished(contract, &response) {
                finish(tree.store, tree.watch, run_id, depth, step, "finished")?;
                return Ok(RunOutcome::Finished { steps: step });
            }

            if evaluate_gate(
                contract,
                &tree.root,
                &ExecGuard::new(policy)
                    .tracing(tree.store, run_id, step)
                    .watching(tree.watch, depth)
                    .with_writable_roots(gate_roots(toolchain.as_ref())),
                tree.store,
                run_id,
                step,
                tree.watch,
                depth,
            )
            .await?
            {
                finish(tree.store, tree.watch, run_id, depth, step, "success")?;
                return Ok(RunOutcome::Success { steps: step });
            }
            // See the workspace loop: `|=`, and `Verification::None` never counts.
            criterion_failed |= gate_judged(&contract.verify);
            // 0.70.0 — and the same feedback into the next step's request, through
            // the same helper. A contained agent that is told nothing about why its
            // gate failed is the one that can least afford to guess: it has a
            // narrower workspace and a smaller budget than its parent.
            // And only when the section is NEW — see the workspace loop. A gate
            // failing the same way every step would otherwise append a
            // near-identical block per step and re-send all of them, which for a
            // child is worse than for a root: it has the smaller context budget
            // of the two.
            if step < contract.max_steps {
                if let Some((key, section)) = gate_failure_feedback(tree.store, run_id, step)
                    .filter(|(key, _)| last_gate_feedback.as_deref() != Some(key.as_str()))
                {
                    tree.store.record_context_event(
                        run_id,
                        &ContextEvent::gate_feedback(
                            step + 1,
                            format!(
                                "step {step} gate failure, {} chars",
                                section.chars().count()
                            ),
                        ),
                    )?;
                    ledger.push(Observation::new(
                        step,
                        ObsKind::Error,
                        None,
                        bound(&section, entry_cap, ObsKind::Error),
                    ));
                    last_gate_feedback = Some(key);
                }
            }
        }

        let (recorded, outcome) = capped_outcome(criterion_failed, contract.max_steps);
        finish(
            tree.store,
            tree.watch,
            run_id,
            depth,
            contract.max_steps,
            recorded,
        )?;
        Ok(outcome)
    })
}

/// How a parent asked for its child to come back (0.50.0).
///
/// The default is [`Return::Wait`], which is every spawn written before this
/// release and every spawn that names neither argument: a parent that says
/// nothing gets the blocking, ordered, reproducible tree it has always had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Return {
    /// Wait for the child, fold its report into this step.
    Wait,
    /// Do not wait: the child goes into the parent's in-flight set now and its
    /// report arrives at a later step.
    Detach,
    /// Wait, but only this long. Past it the child keeps running and the parent
    /// takes its next step — the child is moved to the background, never dropped.
    WaitUntil(Duration),
}

/// Narrow what the model asked for by what the operator allowed (0.50.0).
///
/// Both directions are one-way. A contract clock replaces a spawn that named
/// none and beats a spawn that named a longer one, and never loses to it; a
/// contract that refuses detachment turns every shape back into a plain wait. The
/// second return is the line the parent reads when its request was narrowed —
/// silence would leave a model believing it had fanned out when it had not.
pub(super) fn narrowed(
    want: Return,
    background_after: Option<Duration>,
    detached_spawns: bool,
) -> (Return, Option<&'static str>) {
    if !detached_spawns {
        return match want {
            Return::Wait => (Return::Wait, None),
            _ => (
                Return::Wait,
                Some(
                    "this run does not allow a child to outlive the step that spawned it, so it \
                     was waited for",
                ),
            ),
        };
    }
    match (want, background_after) {
        // A parent that never waits is already narrower than any clock.
        (Return::Detach, _) | (_, None) => (want, None),
        (Return::Wait, Some(cap)) => (Return::WaitUntil(cap), None),
        (Return::WaitUntil(asked), Some(cap)) => (Return::WaitUntil(asked.min(cap)), None),
    }
}

/// Read the two 0.50.0 arguments off a spawn call.
///
/// `Err` is the message the parent reads. A contradiction is answered the way
/// every other malformed spawn is — a typed observation naming it, no child, and
/// a parent that carries on — rather than as a failure of the parent's run.
///
/// A zero-second wall clock is [`Return::Detach`] and not an error: "wait zero
/// seconds for it" and "do not wait for it" are the same request, and refusing
/// one spelling of a coherent instruction teaches a model nothing.
///
/// **That rule was stated in 0.50.0 and applied to only one of the two spellings,
/// which a live run found in 0.60.0.** `wait: false` beside
/// `background_after_secs: 0` was refused as a contradiction while `wait: true`
/// beside the same zero was honoured — the identical request, decided two ways.
/// It matters because filling every property of a tool schema with its zero value
/// is ordinary model behaviour, not an exotic one: the run that found this sent
/// `"agent": ""`, `"deny_write": []`, `"deny_net": []` and
/// `"background_after_secs": 0` on every call, and every other one of those was
/// already treated as "unset". The spawn tool was unusable for such a model, and
/// no fixture noticed because a fixture writes only the arguments it means.
///
/// A contradiction is a wall clock that is actually asked to elapse — a positive
/// number — beside a parent that says it is not waiting. Nothing else.
pub(super) fn spawn_return(a: &serde_json::Value) -> std::result::Result<Return, String> {
    let wait = a.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);
    let after = a.get("background_after_secs").and_then(|v| v.as_u64());
    match (wait, after) {
        (false, Some(s)) if s > 0 => Err(
            "\"wait\": false and \"background_after_secs\" cannot both be set — a child you are \
             not waiting for has no wall clock to cross. Pick one."
                .into(),
        ),
        (false, _) | (true, Some(0)) => Ok(Return::Detach),
        (true, Some(s)) => Ok(Return::WaitUntil(Duration::from_secs(s))),
        (true, None) => Ok(Return::Wait),
    }
}

/// The result of one [`SPAWN_TOOL`] call.
pub(super) enum SpawnResult {
    /// The child finished; fold its composed result into the parent's log.
    Composed { decision: String, obs: String },
    /// The child deferred a sensitive action to a human. The pending action is
    /// persisted under `request_id`; the whole tree pauses so the caller can
    /// resume it with [`resume_with_decision`], exactly as a single run does.
    Paused { request_id: i64 },
    /// (0.21.0) The child asked the operator about intent and nobody in this process
    /// answered. The question is persisted under `question_id`; the whole tree pauses
    /// so the caller can resume it with [`resume_tree_with_answer`], exactly as a
    /// deferred approval does.
    Asked { question_id: i64 },
}

/// What one [`SPAWN_TOOL`] call produced (0.50.0).
///
/// A spawn used to have exactly one shape — the parent waited and folded the
/// result — so `spawn_child` returned it directly. A parent that stops waiting
/// hands the unfinished child back instead, and the loop keeps it.
pub(super) enum SpawnOutcome<'f> {
    /// The child is finished (or paused, or refused): fold it into this step.
    Settled(SpawnResult),
    /// The parent is no longer waiting. The observation says so, and the future
    /// is the child, still running and still holding its slot.
    InFlight {
        decision: String,
        obs: String,
        fut: ChildFuture<'f>,
    },
}

/// Handle one [`SPAWN_TOOL`] call: enforce the containment caps, derive the
/// child's narrowed policy, run it, and compose its result back for the parent's
/// next turn. A refused spawn is a typed observation the parent can adapt to,
/// never a failure of the parent run; a child that defers propagates the pause
/// up so the caller can resume the child once a human decides.
pub(super) async fn spawn_child<'f, P: Provider>(
    tree: &'f Tree<'_, P>,
    call: &ToolCall,
    parent_run_id: i64,
    depth: u32,
    parent_policy: &Policy,
    step: u32,
) -> Result<SpawnOutcome<'f>> {
    let a = &call.arguments;
    let goal = a.get("goal").and_then(|v| v.as_str()).unwrap_or_default();
    let file = a
        .get("verify_file")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let needle = a
        .get("verify_contains")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if goal.is_empty() || file.is_empty() {
        return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
            decision: "spawn missing fields".into(),
            obs: "\n[spawn error] spawn_agent needs \"goal\" and \"verify_file\"\n".into(),
        }));
    }

    // 0.50.0 — how the parent asked for this child back, read before anything is
    // registered, admitted or written. A contradiction must cost no run row, no
    // slot and no queue place, and the only way to guarantee that is to answer it
    // here.
    let (want, narrowing) = match spawn_return(a) {
        Ok(w) => narrowed(w, tree.spawn_background_after, tree.detached_spawns),
        Err(why) => {
            return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                decision: "spawn arguments conflict".into(),
                obs: format!("\n[spawn error] {why}\n"),
            }));
        }
    };

    let child_depth = depth + 1;

    // 0.60.0 — the address this child will answer to, if the parent named one.
    // Its *shape* is checked here, beside the other argument contradictions and
    // for the same reason: a malformed request must cost no run row, no slot and
    // no queue place. Whether the name is already taken is a question about the
    // tree rather than about the argument, so it is asked on the fresh path below
    // — a replayed spawn adopts the child that already holds the name and must not
    // find itself a duplicate of itself.
    let asked_as = a
        .get("as")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty());
    if let Some(name) = asked_as {
        if let Err(why) = address_is_assignable(name) {
            return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                decision: format!("spawn address refused ({name})"),
                obs: format!("\n[spawn error] {why}\n"),
            }));
        }
    }

    // 0.21.0 — an optional named definition. Unknown is an error observation and no
    // child: a spawn that silently became an unnarrowed agent because its definition
    // was misspelled is exactly the failure a roster must not have.
    let named = a
        .get("agent")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty());
    let def = match named {
        Some(name) => match tree.agents.get(name) {
            Some(def) => Some(def),
            None => {
                let known = tree.agents.names().join(", ");
                return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                    decision: format!("unknown agent {name}"),
                    obs: format!(
                        "\n[spawn error] no agent named `{name}`. Available: {}\n",
                        if known.is_empty() { "none" } else { &known }
                    ),
                }));
            }
        },
        None => None,
    };

    // A child inherits the parent policy and may only narrow it. Optional
    // `deny_write` globs let the parent tighten the child further, and a definition
    // narrows it again — both through `Policy::contain`, which is why neither can
    // widen anything. There is no path here that adds an allow.
    let mut overlay = Policy::permissive().layer("child");
    if let Some(def) = def {
        if def.deny_write {
            overlay = overlay.deny_write("*");
        }
        if def.deny_net {
            overlay = overlay.deny_net("*");
        }
    }
    if let Some(denies) = a.get("deny_write").and_then(|v| v.as_array()) {
        for d in denies.iter().filter_map(|v| v.as_str()) {
            overlay = overlay.deny_write(d);
        }
    }
    if let Some(denies) = a.get("deny_net").and_then(|v| v.as_array()) {
        for d in denies.iter().filter_map(|v| v.as_str()) {
            overlay = overlay.deny_net(d);
        }
    }
    let child_policy = parent_policy.contain(&overlay);

    // 0.36.0 — a child of a `worktree = true` definition works in its own
    // checkout instead of the tree's one working directory.
    //
    // Before anything is registered, admitted or written: a worktree that cannot
    // be made is a spawn that does not happen, and doing it here means the
    // failure costs no ledger entry, no queue row and no run row. The cost of
    // that ordering is that a child refused by containment a few lines below may
    // leave an empty worktree behind — harmless, reused by the next attempt
    // because the path is derived rather than fresh, and cheaper than leaking a
    // registered agent that never ran.
    let child_root = match def.filter(|d| d.worktree) {
        Some(d) => {
            // `depth`, not `child_depth`: the approval this may raise belongs to
            // the parent run that is making the worktree, and the child whose
            // depth that would be does not exist yet (0.70.0).
            match worktree_for(
                tree,
                parent_policy,
                &d.name,
                goal,
                parent_run_id,
                step,
                depth,
            )
            .await
            {
                Ok(root) => Some(root),
                Err(why) => {
                    return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                        decision: "worktree unavailable".into(),
                        obs: format!(
                        "\n[spawn error] `{}` needs its own worktree and one could not be made: \
                         {why}\n",
                        d.name
                    ),
                    }));
                }
            }
        }
        None => None,
    };
    let child_root = child_root.unwrap_or_else(|| tree.root.clone());

    let verify = Verification::WorkspaceFileContains {
        file: file.into(),
        needle: needle.into(),
    };
    let mut child_contract = TaskContract::workspace(goal, &child_root).with_verification(verify);
    // 0.22.0 — the tree's web declaration, not one the model asked for. A child
    // inherits exactly what the root was given and has no way to widen it: the
    // spawn arguments are never read for this, so "give the sub-agent web access"
    // is unwritable in the JSON the model controls.
    child_contract.web = tree.web.clone();
    if let Some(n) = a.get("max_steps").and_then(|v| v.as_u64()) {
        child_contract = child_contract.with_max_steps(n as u32);
    }
    // A definition's cap is the operator's and outranks the model's own request.
    if let Some(n) = def.and_then(|d| d.max_steps) {
        child_contract = child_contract.with_max_steps(n);
    }

    // Spawn-or-adopt. On a fresh run this spawn has no persisted record, so a new
    // child is created. On a tree resume the parent replays the same spawn step
    // and finds the child it already spawned (keyed by parent+step+goal): it
    // adopts that child and resumes it from its OWN last committed step instead
    // of creating a duplicate or restarting it. This is what lets every agent in
    // a crashed tree continue from its own checkpoint.
    // What this spawn is, before anything is admitted or written. A child already
    // finished is composed from its recorded outcome and never takes a slot —
    // there is no work left for a slot to protect.
    let adopted = match tree.store.find_spawn(parent_run_id, step, goal)? {
        Some(row) => {
            if let Some(o) = terminal_outcome(tree.store, row.child_run_id)? {
                return Ok(SpawnOutcome::Settled(compose_child(
                    tree.store,
                    row.child_run_id,
                    goal,
                    o,
                )?));
            }
            Some(row)
        }
        None => {
            // 0.60.0 — and the address is decided before any of that, for the same
            // reason: a name already held is a spawn that does not happen, and it
            // must cost nothing. Ahead of `register_agent`, so the tree's agent
            // count, its slot tally and its queue depth are all unmoved by a
            // refusal — which is what F3 asserts as three numbers rather than as
            // the absence of a child.
            if let Some(name) = asked_as {
                let root = tree.store.run_root(parent_run_id)?;
                if tree
                    .store
                    .tree_addresses(root)?
                    .iter()
                    .any(|(n, _)| n == name)
                {
                    return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                        decision: format!("spawn address taken ({name})"),
                        obs: format!(
                            "\n[spawn error] `{name}` is already the address of an agent in this \
                             tree. An address names one agent, not a role, so pick another.\n"
                        ),
                    }));
                }
            }
            // Fresh: the containment boundary decides whether it may exist. This
            // is ahead of admission on purpose — a child the tree will never let
            // exist must not first spend time in a queue.
            if let Err(refusal) = tree.ledger.register_agent(child_depth) {
                tree.store.record_agent_event(&AgentEvent::spawn_refused(
                    parent_run_id,
                    step,
                    refusal.cap(),
                ))?;
                // The parent's event, at the parent's depth: no child exists to
                // attribute it to, which is the point of the refusal.
                tree.watch.emit(RunEvent::at_depth(
                    parent_run_id,
                    step,
                    depth,
                    EventKind::SpawnRefused {
                        cap: refusal.cap().to_string(),
                    },
                ));
                return Ok(SpawnOutcome::Settled(SpawnResult::Composed {
                    decision: format!("spawn refused ({})", refusal.cap()),
                    obs: format!(
                        "\n[spawn refused] {refusal} — adapt or finish with what you have\n"
                    ),
                }));
            }
            None
        }
    };

    // Admission. The concurrency cap throttles rather than refuses, so a child
    // past it waits here — and this is the only place the wait is durable, which
    // is why it sits between deciding the child may exist and writing anything
    // about it. A child that queues and never gets a slot has no run row, no step
    // rows and no tokens against the tree's ceiling, because nothing about it was
    // started.
    //
    // Adopted children take a slot too. Without that the throttle would be a
    // different number before and after a restart: a resumed tree would run every
    // mid-flight child at once and only queue the fresh ones.
    let slot = match tree.ledger.try_admit(child_depth) {
        Some(slot) => {
            // A slot was free, so this child never waits. It may still HAVE
            // waited, in a process that is now dead — a restored backlog
            // describes waits whose slots died with it, and the replay can admit
            // one of them straight away. Clear its row, and take it out of the
            // count only if the store says a row went, so the two move together.
            // A tree that never queued anything has nothing to clear and this
            // costs it no statement at all.
            if tree.ledger.tally(child_depth).queued > 0
                && tree.store.dequeue_agent(parent_run_id, step, goal)?
            {
                tree.ledger.drop_queued(child_depth);
            }
            slot
        }
        None => {
            let newly = tree
                .store
                .enqueue_agent(parent_run_id, step, goal, child_depth)?;
            tree.ledger.mark_queued(child_depth, newly);
            emit_fleet(tree, parent_run_id, step, depth, child_depth);
            let slot = tree.ledger.admit(child_depth).await;
            tree.store.dequeue_agent(parent_run_id, step, goal)?;
            slot
        }
    };
    emit_fleet(tree, parent_run_id, step, depth, child_depth);

    let (child_run, child_start, address) = match adopted {
        // Adopted: already counted in the reconstructed ledger, so do NOT
        // register it again. It resumes from its OWN next step.
        //
        // 0.60.0 — and it keeps the address it already had, read off the row
        // rather than derived again. Re-deriving would need the run id a replay
        // does not allocate and the roster the definition may have left, and an
        // agent whose address changed across a restart is one every sibling
        // holding that name can no longer reach.
        Some(row) => (
            row.child_run_id,
            tree.store.last_step(row.child_run_id)? + 1,
            row.as_name.clone(),
        ),
        None => {
            let child_run = tree.store.start_child_run(
                goal,
                // 0.36.0 — the child's OWN root, which is the tree's unless a
                // definition gave it a worktree. The run row is what an operator
                // reads to find where a child's files went, so recording the
                // parent's root for a child that worked elsewhere would send them
                // to the wrong directory.
                &child_root.display().to_string(),
                parent_run_id,
                child_depth,
            )?;
            // 0.60.0 — the address, derived now that there is a run id to derive it
            // from. `<role>#<run id>` for a child the parent did not name: unique
            // because run ids are, and unreachable by an assigned name because
            // `DERIVED_MARK` is the one character `address_is_assignable` forbids.
            // A parent that names nothing still gets an addressable child, which is
            // what keeps every spawn written before this release usable with the
            // mailbox rather than merely unbroken by it.
            let address = match asked_as {
                Some(n) => n.to_string(),
                None => format!(
                    "{}{DERIVED_MARK}{child_run}",
                    def.map(|d| d.name.as_str()).unwrap_or("agent")
                ),
            };
            // `detail` is free-form and documented as the child's goal; a child
            // spawned from a definition records that too, so the trace says which
            // role ran without the `spawns` table gaining a column. 0.60.0 puts the
            // ADDRESS first, because the role answers "what kind of agent was this"
            // and only the address answers "which one".
            tree.store.record_agent_event(&AgentEvent::spawn(
                parent_run_id,
                step,
                child_run,
                match def {
                    Some(d) => format!("{address} as {}: {goal}", d.name),
                    None => format!("{address}: {goal}"),
                },
            ))?;
            // Attributed to the PARENT's run and depth: the parent is what spawned
            // it, and the child's own events (which carry `child_depth`) start
            // arriving next.
            tree.watch.emit(RunEvent::at_depth(
                parent_run_id,
                step,
                depth,
                EventKind::Spawned {
                    child_run_id: child_run,
                    goal: goal.to_string(),
                },
            ));
            let deny_json = a
                .get("deny_write")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".into());
            tree.store.record_spawn(
                parent_run_id,
                step,
                child_run,
                goal,
                file,
                needle,
                a.get("max_steps")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                &deny_json,
                &address,
            )?;
            // 0.50.0 — the call itself, so a child the parent stopped waiting for
            // can be re-adopted after a restart.
            //
            // A blocking child needs none of this: its step never commits, so the
            // resume replays the spawn call and the arguments arrive with it. A
            // detached child's step DOES commit, so the call is gone and only what
            // was written survives — and `spawns` holds five of the nine
            // arguments. Rebuilding a child from those five would silently drop
            // `agent` and `deny_net`, which is to say it would resume a child under
            // a WIDER policy than the one it was spawned with. The whole call goes
            // in a row of the table the tree already writes to, so the rebuild is a
            // replay rather than a reconstruction, and no column was added to
            // record it.
            tree.store.record_agent_event(&AgentEvent::spawn_args(
                parent_run_id,
                step,
                child_run,
                a,
            ))?;
            (child_run, 1, address)
        }
    };

    // The child itself, as one future that OWNS everything it needs: its contract,
    // its policy, its goal and its slot. That ownership is what lets the parent
    // stop waiting — a future borrowing three locals of this frame cannot outlive
    // it, and every shape below except `Wait` outlives it. 0.41.0's read batch
    // reached the same answer for the same reason.
    let goal_owned = goal.to_string();
    let address_owned = address.clone();
    let child = async move {
        let outcome = run_agent(
            tree,
            &child_contract,
            child_run,
            child_depth,
            &child_policy,
            child_start,
            def,
        )
        .await;
        // Before the `?` and before either early return, so every way out of a
        // child — finished, paused on a human, or an error propagating — frees the
        // slot and reports the tier. Only one of those three is the happy path,
        // and a slot released on the happy path only is a fleet that stops
        // draining the first time something goes wrong. The slot is owned by this
        // future, so a backgrounded child holds its tier's place for exactly as
        // long as it is actually working.
        drop(slot);
        emit_fleet(tree, parent_run_id, step, depth, child_depth);
        let settled = outcome?;
        // 0.60.0 — one short row to the parent saying this agent is done, and
        // deliberately NOT its report. That is what makes "wait for a named child"
        // and "wait for a message" one mechanism: a parent blocked on `scout`
        // unblocks when the scout answers OR when the scout finishes having
        // answered nothing, and neither case needs a second tool.
        //
        // The report itself still travels 0.50.0's path unchanged. Putting it in
        // the body instead would deliver it twice — once folded as an observation
        // and once as a message — and the parent would read the same paragraph in
        // two places without being able to tell they were one event.
        //
        // Not for a pause: a child stopped for a human has not terminated, and
        // saying it has would tell a waiting sibling to stop waiting for an agent
        // that is about to carry on.
        if !matches!(
            settled,
            RunOutcome::AwaitingApproval { .. } | RunOutcome::AwaitingAnswer { .. }
        ) {
            tree.store.send_message(
                child_run,
                parent_run_id,
                &address_owned,
                step,
                // The same `{:?}` rendering `compose_child` prints, so a parent
                // reading "finished" in its mailbox and reading the composed
                // report reads one word for one event rather than two spellings.
                &format!("[finished] {settled:?}"),
            )?;
        }
        match settled {
            // A child that deferred pauses the whole tree, surfacing its
            // request_id so the caller can resume that child once a human decides.
            RunOutcome::AwaitingApproval { request_id, .. } => {
                Ok(SpawnResult::Paused { request_id })
            }
            // And a child that asked the operator something pauses it the same
            // way. Without this the child's `AwaitingAnswer` would fall through to
            // `compose_child` and read as a child that had finished, so the tree
            // would carry on having never heard the question — which is how this
            // was found.
            RunOutcome::AwaitingAnswer { question_id, .. } => {
                Ok(SpawnResult::Asked { question_id })
            }
            other => compose_child(tree.store, child_run, &goal_owned, other),
        }
    };

    // 0.50.0 — and now the only thing that differs between the three shapes: how
    // long the parent waits for that future.
    // A narrowed request is said once, at the front of whatever the child comes
    // back as, so the model reads it beside the result rather than instead of it.
    let note = |obs: String| match narrowing {
        Some(why) => format!("\n[spawn narrowed] {why}\n{}", obs.trim_start_matches('\n')),
        None => obs,
    };
    match want {
        Return::Wait => Ok(SpawnOutcome::Settled(match child.await? {
            SpawnResult::Composed { decision, obs } => SpawnResult::Composed {
                decision,
                obs: note(obs),
            },
            other => other,
        })),
        Return::Detach => {
            tree.watch.emit(RunEvent::at_depth(
                parent_run_id,
                step,
                depth,
                EventKind::ChildDetached {
                    child_run_id: child_run,
                    after: None,
                },
            ));
            Ok(SpawnOutcome::InFlight {
                decision: format!("child {address} (run {child_run}) detached"),
                // 0.60.0 — the address, because this is the shape where the parent
                // needs it: a child it stopped waiting for is one it may want to
                // message or read from before the report arrives. A waited child is
                // already finished by the time its observation is written and has
                // nothing left to be told.
                obs: format!(
                    "\n[child {address} (run {child_run}) \"{goal}\" detached] it is running now; \
                     its report reaches you at a later step, and you can reach it at `{address}`\n"
                ),
                fut: Box::pin(child),
            })
        }
        // Raced against a sleep, and the LOSER IS KEPT. `tokio::time::timeout` is
        // the obvious spelling and it is the wrong one: it drops the future, which
        // cancels the child mid-step and leaves its run row `running` forever —
        // indistinguishable from a crashed process. A parent that stops waiting is
        // not a parent that stops the work.
        Return::WaitUntil(d) => {
            let mut fut: ChildFuture<'f> = Box::pin(child);
            match select(&mut fut, Box::pin(tokio::time::sleep(d))).await {
                Either::Left((done, _)) => Ok(SpawnOutcome::Settled(done?)),
                Either::Right(_) => {
                    tree.watch.emit(RunEvent::at_depth(
                        parent_run_id,
                        step,
                        depth,
                        EventKind::ChildDetached {
                            child_run_id: child_run,
                            after: Some(d),
                        },
                    ));
                    Ok(SpawnOutcome::InFlight {
                        decision: format!(
                            "child {address} (run {child_run}) moved to the background"
                        ),
                        obs: format!(
                            "\n[child {address} (run {child_run}) \"{goal}\" moved to the \
                             background after {}s] it is still running; its report reaches you at \
                             a later step, and you can reach it at `{address}`\n",
                            d.as_secs()
                        ),
                        fut,
                    })
                }
            }
        }
    }
}

/// How many agents are waiting at each tier: `(tier, waiting)`, one entry per
/// non-empty tier. A `Vec` rather than a map because a tree is a handful of tiers
/// deep and the order the store returns them in is the order they are reported.
pub(super) type Backlog = Vec<(u32, u32)>;

/// Rebuild a tree's shared ledger from the store on resume (0.32.0), and report
/// the backlog it inherited as `(tier, waiting)` pairs.
///
/// Three things are restored, and the third is the one that is easy to miss. The
/// spend and the agent count keep the budget and the total cap continuous across
/// the crash. The *queue* is separate durable state: a child that was only ever
/// waiting has no run row, so `agent_count_tree` never counted it and nothing
/// about it was ever charged — its place in the queue is the only trace it left.
///
/// The replay that follows re-queues those children. `Store::enqueue_agent` tells
/// the ledger they are already recorded, which is what keeps the restored depth
/// this number rather than twice this number, and is the difference between a
/// queue rebuilt from the store and one silently re-derived from the spawn calls
/// the model happens to repeat.
pub(super) fn restore_tree_ledger(
    store: &Store,
    root: i64,
    containment: &Containment,
) -> Result<(Arc<Ledger>, Backlog)> {
    let ledger = Arc::new(Ledger::from_state(
        containment,
        store.spent_tokens_tree(root)?,
        store.agent_count_tree(root)?,
    ));
    let mut per_tier: Backlog = Vec::new();
    for (tier, _) in store.queued_agents(root)? {
        match per_tier.iter_mut().find(|(t, _)| *t == tier) {
            Some((_, n)) => *n += 1,
            None => per_tier.push((tier, 1)),
        }
    }
    ledger.restore_queue(&per_tier);
    Ok((ledger, per_tier))
}

/// Report an inherited backlog, before the provider is authorised and long
/// before it is called, so an application that reconnects to a restarted tree
/// sees the queue it is waiting on rather than a zero that fills in later.
pub(super) fn emit_backlog(
    watch: &Watch<'_>,
    root: i64,
    step: u32,
    ledger: &Ledger,
    per_tier: &[(u32, u32)],
) {
    for &(tier, _) in per_tier {
        // Every number here comes off the restored ledger, including the depth —
        // deliberately, and not from the rows that were just counted. Reporting
        // the row count directly would make this event true whether or not the
        // ledger had actually been seeded, and a ledger that was not seeded
        // reports a backlog of zero for the rest of the run while the store still
        // holds the rows. Reading the ledger is what makes the event fail when the
        // restoration does.
        let t = ledger.tally(tier);
        watch.emit(RunEvent::at_depth(
            root,
            step,
            0,
            EventKind::Fleet {
                tier,
                working: t.working,
                queued: t.queued,
                done: t.done,
            },
        ));
    }
}

/// Report one tier's shape to the observer (0.32.0).
///
/// Attributed to the PARENT's run and the parent's own `depth`, for the same
/// reason [`EventKind::Spawned`] is: the parent is what caused the change, and a
/// queued child has no run id to attribute anything to yet. Which tier is being
/// counted is in the payload rather than in `RunEvent::depth`, so `depth` keeps
/// meaning "who emitted this" everywhere.
pub(super) fn emit_fleet<P: Provider>(
    tree: &Tree<'_, P>,
    parent_run_id: i64,
    step: u32,
    depth: u32,
    tier: u32,
) {
    let t = tree.ledger.tally(tier);
    tree.watch.emit(RunEvent::at_depth(
        parent_run_id,
        step,
        depth,
        EventKind::Fleet {
            tier,
            working: t.working,
            queued: t.queued,
            done: t.done,
        },
    ));
}

/// Fold one child's finished result back into the parent's observation log.
///
/// 0.50.0 — what the child *concluded*, not only that it finished. Until this
/// release the parent read `[child 7 "goal" -> Success { steps: 4 }]` and nothing
/// more, because [`RunOutcome::Success`] carries no text: a parent that fanned out
/// to investigate four subsystems learned that four runs succeeded and none of
/// what they found. The only way a finding could travel was a file the parent then
/// read, which is why `verify_file` was doing double duty as a return channel.
///
/// **Read from the store rather than carried out of the child's loop, and that is
/// the point.** A child this process ran and a child it adopted from a previous
/// process both leave the same `"said"` rows, so the two paths cannot render one
/// conclusion two ways — there is one rendering, and a resume composes exactly
/// what waiting composes.
pub(super) fn compose_child(
    store: &Store,
    child_run: i64,
    goal: &str,
    outcome: RunOutcome,
) -> Result<SpawnResult> {
    // What it cost comes off the run's own row, so the number a parent reads and
    // the number an auditor reads are the same number.
    let spend = match store.run_summary(child_run)? {
        Some(s) => format!(", {} steps, {} tokens", s.steps, s.tokens),
        None => String::new(),
    };
    let said = child_conclusion(store, child_run)?;
    let body = match &said {
        Some(text) => format!("\n{text}\n"),
        // Stated, not omitted: a parent that reads nothing must be able to tell
        // "it said nothing" from "this build does not report what it said".
        None => "\n(it ended without saying anything; read its trace by run id)\n".into(),
    };
    Ok(SpawnResult::Composed {
        decision: format!("spawned child {child_run}: {outcome:?}"),
        obs: format!("\n[child {child_run} \"{goal}\" -> {outcome:?}{spend}]{body}"),
    })
}

/// The last thing an agent said, from its durable trace.
///
/// The *last* rather than a summary of all of them: an agent's closing completion
/// is its answer, and the ones before it are working notes the parent did not ask
/// for. Bounded on the way in by the same `entry_cap` every observation is bounded
/// by, so a talkative child cannot flood its parent.
pub(super) fn child_conclusion(store: &Store, child_run: i64) -> Result<Option<String>> {
    Ok(store
        .agent_events(child_run)?
        .into_iter()
        .rfind(|e| e.kind == "said")
        .and_then(|e| e.detail))
}

/// The rules an approver asked to remember, as a mergeable top layer.
///
/// A layer rather than an edit: merging is what keeps a remembered allow from
/// defeating a deny beneath it, since deny is absolute across the stack.
pub(super) fn remembered_layer(rules: &[Rule]) -> Policy {
    let mut layer = Policy::permissive().layer("remembered");
    for r in rules {
        layer = layer.rule(r.act, r.effect, r.pattern.clone());
    }
    layer
}
