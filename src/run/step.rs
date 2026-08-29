//! step: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// Await the in-process gate and the durable row together, and take whichever
/// answers first (0.33.0).
///
/// `Some(v)` is the gate's own answer; `None` means a second process wrote the
/// durable row while this run was waiting, and the caller must read it back —
/// `None` deliberately carries no value, so a caller cannot report an attached
/// answer it has not read from the store.
///
/// `biased;` so the gate is polled first on every pass. An
/// [`ApproveAll`](crate::ApproveAll) or a [`DenyAll`](crate::DenyAll) answers
/// before the first timer is ever created, which is what keeps an unattended run
/// paying nothing for a feature it is not using.
pub(super) async fn race_gate<T, F, P>(gate: F, store: &Store, answered: P) -> Result<Option<T>>
where
    F: Future<Output = T> + Unpin,
    P: Fn(&Store) -> Result<bool>,
{
    let mut gate = gate;
    loop {
        tokio::select! {
            biased;
            v = &mut gate => return Ok(Some(v)),
            _ = tokio::time::sleep(ATTACH_POLL) => {
                if answered(store)? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Announce a human's answer, from the row that records it. As [`refused`]: the
/// event's `decision` is the row's, never a second literal beside it.
pub(super) fn decided(watch: &Watch<'_>, run_id: i64, depth: u32, ev: &PolicyEvent) {
    watch.emit(RunEvent::at_depth(
        run_id,
        ev.step,
        depth,
        EventKind::ApprovalDecided {
            act: ev.act.clone(),
            target: ev.target.clone(),
            decision: ev.decision.clone().unwrap_or_default(),
        },
    ));
}

/// The one step boundary: commit the step, log it, and tell the observer.
///
/// Before 0.12.0 the single-file loop, the workspace loop and the sub-agent loop
/// each had their own copy of this — their own inline [`StepRecord`], their own
/// `checkpoint_step`, and their own differently-named `info!` ("loop step" /
/// "workspace step" / "agent step"). One boundary is what stops the three
/// drifting, and what makes [`EventKind::Step`] one fact about a committed step
/// rather than three approximations of one.
///
/// `commit` is `false` for exactly one case: the sub-agent loop's step that
/// paused because one of its CHILDREN deferred. That step is deliberately left
/// uncommitted so a resume replays it and re-adopts the paused child — only the
/// parent re-entering `spawn_child` can wait on that child again — and committing
/// it would skip the replay, which is the double-execution defect 0.7.0's
/// checkpointing exists to prevent. Nothing is committed and no
/// [`EventKind::Step`] is emitted, because there is no committed step to report:
/// what the caller hears about is the pause, through
/// [`RunOutcome::AwaitingApproval`].
pub(super) fn commit_step(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    record: StepRecord,
    changed: bool,
    commit: bool,
) -> Result<()> {
    if !commit {
        info!(
            run_id,
            depth,
            step = record.step,
            "tree paused for a child's approval (step left uncommitted for replay)"
        );
        return Ok(());
    }
    store.checkpoint_step(run_id, &record)?;
    info!(
        run_id,
        depth,
        step = record.step,
        decision = %record.decision,
        tokens = record.tokens,
        changed,
        "step"
    );
    // The record's own fields, moved rather than cloned: an event must report
    // exactly what was committed, and the unobserved path must not pay an
    // allocation per step for the privilege of being ignored.
    watch.emit(RunEvent::at_depth(
        run_id,
        record.step,
        depth,
        EventKind::Step {
            decision: record.decision,
            tool_call: record.tool_call,
            tokens: record.tokens,
            changed,
        },
    ));
    Ok(())
}

/// Stop the run if the observer asked it to, recording `"cancelled"` as the
/// outcome. `None` means carry on.
///
/// Call this at a step boundary and nowhere else — that is the contract
/// [`Flow::Cancel`](crate::Flow::Cancel) states, and the whole reason the request
/// is remembered in [`Watch`] rather than acted on where it was returned. The run
/// is *finished*, not abandoned: `runs.status` stops being `running`, a summary is
/// written, and `terminal_outcome` maps the string back so a resume reports the
/// cancellation instead of re-driving the loop.
pub(super) fn cancelled(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    steps: u32,
) -> Result<Option<RunOutcome>> {
    if !watch.cancelled() {
        return Ok(None);
    }
    finish(store, watch, run_id, depth, steps, "cancelled")?;
    info!(run_id, depth, steps, "run cancelled by its observer");
    Ok(Some(RunOutcome::Cancelled { steps }))
}

/// End a run: write the outcome and tell the observer, so no terminal path can do
/// one without the other. Every `finish_run` in this file goes through here.
///
/// `steps` is what the outcome reports, which is not always the step the loop was
/// on — a time-budget stop reports the last step that completed.
pub(super) fn finish(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    steps: u32,
    outcome: &str,
) -> Result<()> {
    store.finish_run(run_id, outcome)?;
    watch.emit(RunEvent::at_depth(
        run_id,
        steps,
        depth,
        EventKind::Finished {
            outcome: outcome.to_string(),
            steps,
            // Read back from the store rather than carried in a local: the store
            // is what an audit will read, and the two must agree.
            tokens: store.spent_tokens(run_id)?,
        },
    ));
    Ok(())
}

/// The conversation, pushed into a fresh turn's ledger before its first step.
///
/// One of the four session rules that 0.39.0 made shared. Both loops call it:
/// the flat one because a turn has always reached it, the tree one because
/// [`crate::Session::turn_contained`] reaches it now. A rule copied into the
/// second loop would work today and lapse the first time one copy was reworded —
/// which is what [`NO_TOOL_CALL`] exists to prevent one literal at a time.
///
/// Step 0, because no step of THIS run produced them; and only into an empty
/// ledger, which is a fresh turn — a resumed one already has the seed from the
/// store, and seeding again would say everything twice.
pub(super) fn seed_conversation(ledger: &mut ContextLedger, extras: &TurnExtras<'_>) {
    if !ledger.is_empty() {
        return;
    }
    for (speaker, entry) in extras.seed {
        // 0.49.0 — tagged with who was speaking, so the transcript can send it as
        // that speaker's own turn rather than as narration inside another.
        ledger.push(Observation::new(
            0,
            ObsKind::Message,
            Some((*speaker).to_string()),
            entry.clone(),
        ));
    }
}

/// Type a classifying turn as a reply before its first completion is billed.
///
/// Written in this order rather than at the close, so a process killed
/// mid-answer leaves a row that says what it was doing: `check_resumable`
/// refuses it as work to continue, because there is no committed step to
/// continue from and re-asking replaces the one completion at the same price.
pub(super) fn open_turn_kind(store: &Store, run_id: i64, extras: &TurnExtras<'_>) -> Result<()> {
    if extras.classify {
        store.set_turn_kind(run_id, TURN_KIND_REPLY)?;
    }
    Ok(())
}

/// The prompt a classifying turn's **first** completion is made with, or `None`
/// when the turn is not allowed to decide it was conversation.
///
/// `base` is the loop's own conversational opening — the two loops describe
/// different worlds (one of them has sub-agents) and must keep saying so — but
/// what is wrapped around it, and the condition under which it is used at all,
/// is one rule in one place.
///
/// Both boundaries are passed and this function chooses (0.60.3), for the same
/// reason the directive is built here: the rule "the boundary an agent reads is the
/// one that will refuse it" held in the `system` block and lapsed in this one, at
/// **both** call sites, because each site chose for itself and both chose the
/// post-plan value. A rule that each caller applies is a rule that lapses wherever
/// one of them forgets.
#[allow(clippy::too_many_arguments)]
pub(super) fn conversational_opening(
    base: &str,
    contract: &TaskContract,
    extras: &TurnExtras<'_>,
    extra: &[ToolSpec],
    skills: &Skills,
    planning: bool,
    after_planning: Option<&str>,
    while_planning: Option<&str>,
    family: PromptFamily,
) -> Option<String> {
    if !extras.classify {
        return None;
    }
    Some(compose(PromptSpec {
        base,
        prompt: &contract.prompt,
        extra,
        skills,
        // The roster the directive names is the contract's, at both call sites, so
        // it is read here rather than passed twice. `true`: this is the one block
        // composed above `CONVERSATIONAL_ENDING`, and the gate is stated to a turn
        // that may still answer.
        directive: planning.then(|| planning_directive(&contract.agents, true)),
        instructions: &contract.instructions,
        boundary: match planning {
            true => while_planning,
            false => after_planning,
        },
        family,
        // 0.45.0 — the sentence that decides what a turn is, emitted last so that
        // nothing an embedder or a repository supplied can be read after it.
        ending: CONVERSATIONAL_ENDING,
    }))
}

/// What the turn's own first completion decided, for the loop that made it.
///
/// `Ok(None)` means the turn is work and the caller carries on **from that same
/// completion**, so the run's first step is the call already paid for and
/// nothing is asked twice. `Ok(Some(outcome))` means the turn is over: it
/// answered, or it had already spent its ceiling answering.
///
/// Only the first completion. A later one that stops on text is the loop
/// finishing a run, which is what it has always been.
#[allow(clippy::too_many_arguments)]
pub(super) fn classify_first_completion(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    contract: &TaskContract,
    response: &CompletionResponse,
    tokens_used: u64,
    ledger: &ContextLedger,
    written: usize,
    extras: &TurnExtras<'_>,
    step: u32,
    start_step: u32,
) -> Result<Option<RunOutcome>> {
    if !extras.classify || step != start_step {
        return Ok(None);
    }
    // `answered` and not `tool_calls.is_empty()`: a provider that paused a long
    // server-side search hands back text with no call, which is a continuation
    // and not an ending. Reading it as an answer would stop an unverified turn in
    // the middle of the search it was told to make.
    //
    // `answered` and not `finished` (0.63.0): `finished` also asks
    // `Verification::None`, which is the *same* question `extras.classify` above
    // has already answered — and which `TaskContract::conversational` may now
    // have answered differently. Asking it twice meant an explicit
    // `conversational: Some(true)` on a judged turn set the flag and was then
    // silently overruled here.
    if !answered(response) {
        store.set_turn_kind(run_id, TURN_KIND_RUN)?;
        return Ok(None);
    }
    // A reply is billed like everything else and is bounded by the same ceiling.
    // Checked before the answer is served, so a turn whose one completion has
    // already spent the budget is refused rather than served free.
    if let Some(max) = contract.max_tokens {
        if tokens_used > max {
            finish(store, watch, run_id, 0, 0, "cost_budget_exceeded")?;
            return Ok(Some(RunOutcome::CostBudgetExceeded { steps: 0 }));
        }
    }
    // What the model said is made durable, and nothing else is: no `steps` row,
    // no gate attempt, no checkpoint, no snapshot, no spawn and no deferred
    // approval. The observation is how `Session` reads the reply back — the same
    // `(no tool call)` marker every turn's closing message is read through — so a
    // reply needs no second channel out of a loop.
    persist_ledger(store, run_id, ledger, written)?;
    info!(run_id, "turn answered without opening a run");
    finish(store, watch, run_id, 0, 0, "finished")?;
    Ok(Some(RunOutcome::Finished { steps: 0 }))
}

/// The operator's channel into a running turn, read at a step boundary.
///
/// The same boundary a cancellation is honoured at, for the same reason: the
/// points in between are not safe to stop at or to change course from — a tool
/// call is in flight and a file may be half-written. In a tree that boundary is
/// also the point at which no child is running, because children are awaited
/// inside the step that spawned them.
///
/// `Ok(Some(_))` is the interrupt: the turn is finished rather than abandoned,
/// so `runs.status` stops being `running` and a resume reports the cancellation
/// instead of re-driving the loop.
///
/// `fold_asked` is the loop's own standing request for a fold — the one
/// [`fold_forced`](super::memory::fold_forced) consumes — and a drained
/// [`Steering::fold`](crate::Steering::fold) is ORed into it rather than
/// returned, so the fold is decided by the same expression whichever trigger
/// asked. It is set here and read by the compaction attempt of *this* step, a
/// few lines further down each loop, which is what makes a fold typed mid-step
/// land on the request built after it rather than the one after that.
pub(super) fn drain_steer(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    ledger: &mut ContextLedger,
    extras: &TurnExtras<'_>,
    fold_asked: &mut bool,
) -> Result<Option<RunOutcome>> {
    let Some(inbox) = extras.steer else {
        return Ok(None);
    };
    let steered = inbox.drain();
    if steered.interrupted {
        // The cancel path, not a second one. Answered before the fold for a
        // reason a caller can predict: an operator who asked for a summary and
        // then stopped the turn stopped the turn, and a summariser call spent on
        // a turn nobody will read is money the run does not get back.
        finish(store, watch, run_id, 0, step - 1, "cancelled")?;
        info!(run_id, steps = step - 1, "turn interrupted by its operator");
        return Ok(Some(RunOutcome::Cancelled { steps: step - 1 }));
    }
    if steered.fold {
        // ORed, never assigned: a contract that already asked for a fold this
        // turn has not been served yet, and an operator asking again must not
        // clear it.
        *fold_asked = true;
        info!(run_id, step, "operator asked the turn to fold");
        store.record_context_event(
            run_id,
            &ContextEvent::steered(step, "operator asked for a fold".to_string()),
        )?;
    }
    for message in steered.messages {
        // An observation like any other: bounded by the same budget, recorded in
        // the same trace, and — carrying no target — never superseded away. It is
        // text the model reads, not permission the model has: every tool call it
        // leads to is checked against the same policy by the same code.
        info!(run_id, step, "operator steered the turn");
        store.record_context_event(
            run_id,
            &ContextEvent::steered(step, format!("operator message ({} chars)", message.len())),
        )?;
        ledger.push(Observation::new(
            step,
            ObsKind::Message,
            None,
            format!("\n[operator, mid-turn] {message}\n"),
        ));
    }
    Ok(None)
}

pub(super) async fn run_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    start_step: u32,
    watch: &Watch<'_>,
) -> Result<RunResult> {
    let fs = FsTool::new(&contract.file);
    // 0.45.0 — composed like every other prompt, with two things absent by
    // construction: no boundary section, because single-file mode enforces no
    // policy, and no ending, because there is no turn to classify here.
    let system = compose(PromptSpec {
        base: SINGLE_FILE_PROMPT,
        prompt: &contract.prompt,
        extra: &[],
        skills: &Skills::none(),
        directive: None,
        instructions: &contract.instructions,
        boundary: None,
        family: provider.prompt_family(),
        ending: "",
    });
    report_prompt(
        watch,
        run_id,
        0,
        &system,
        contract,
        provider.prompt_family(),
        false,
    );
    let tool = write_file_tool();
    // Durable budget: spend and elapsed time are restored from the store, so a
    // resume continues one continuous budget instead of restarting it at zero.
    let mut tokens_used: u64 = store.spent_tokens(run_id)?;
    // Single-file mode is not policy-enforced (0.4.0), but the verify gate is
    // still sandboxed (0.6.0). A permissive guard carries the trace so the
    // sandbox lifecycle is recorded for single-file runs too.
    let permissive = Policy::permissive();
    // 0.70.0 — has the criterion ever judged this run and said no? The loop
    // cannot ask afterwards: the gate is evaluated inside the step and its answer
    // is a `bool` that goes out of scope with the iteration, so a tail that wanted
    // to know would have to re-run the criterion — on a workspace the agent has
    // stopped editing, at whatever the gate costs, to learn something the loop
    // already had. The same shape as `marked_prefix` and `routed_model` in the
    // workspace loop: a fact about the run that only the loop can hold, because a
    // rule applied to a freshly built request cannot detect its own transition.
    //
    // Seeded from the store rather than from `false`, because `start_step..=max`
    // is EMPTY on a resume that does not raise the cap — the body never runs, and
    // a flag starting false would let the tail overwrite a durable
    // `"verification_failed"` with `"step_cap_reached"`. A fresh run has no
    // outcome and seeds false, which is the honest answer for a run that never
    // reaches its gate.
    let mut criterion_failed = criterion_already_failed(store, run_id);

    for step in start_step..=contract.max_steps {
        // A cancellation is acted on here, at the boundary between two steps, and
        // nowhere else: the points inside a step are not safe to stop at — a tool
        // call is in flight, a file may be half-written — and stopping there is
        // what dropping the future already does badly. Checked before the budgets
        // because the caller asking to stop outranks a budget saying so.
        if let Some(o) = cancelled(store, watch, run_id, 0, step - 1)? {
            return Ok(RunResult::new(o, run_id));
        }
        // Time budget: checked before doing the step's work, against real
        // wall-clock elapsed since the run started (durable across a restart).
        if let Some(max) = contract.max_duration {
            if store.elapsed_secs(run_id)? > max.as_secs_f64() {
                finish(store, watch, run_id, 0, step - 1, "time_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                ));
            }
        }

        let current = fs.read().await?;
        // The whole file goes back every turn, so it is bounded on the same terms
        // as any observation: one large file must not exhaust the request. The tail
        // is kept, because the end of a file is what a writer needs.
        let user =
            user_prompt(
                contract,
                &bound(
                    &current,
                    entry_cap_chars(contract.context.effective_tokens(
                        contract.max_tokens.map(|m| m.saturating_sub(tokens_used)),
                    )),
                    ObsKind::Read,
                ),
            );
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: vec![tool.clone()],
            // 0.22.0 — what the provider may look up, as the contract declared it.
            // `None` for every contract that declared nothing, which is what the
            // three built-in providers read as "send the 0.21.0 body".
            web: contract.web.clone(),
            // 0.31.0 — the tier the caller asked for, or `None` (every contract
            // before 0.31.0) to leave the vendor's own default in place.
            effort: contract.effort,
            // Single-file mode has no `view_image` tool, so only the caller's
            // images are in play here.
            #[cfg(feature = "media")]
            media: attach_media(contract, &mut PendingMedia::default())?,
            ..Default::default()
        };

        let response = complete_with_retry(
            provider, &request, contract, store, run_id, step, watch, 0, false,
            // The single-file loop has no ledger to fold, so an over-window
            // request there is terminal exactly as it was on 0.42.0.
            false, // Never streamed, so there is no stream to speculate off.
            None,
        )
        .await?;

        // Which provider answered, when that is not a foregone conclusion. A
        // `Fallback` that fell over served this step from its secondary, and a trace
        // reader has no other way to know.
        if let Some(served) = provider.last_served() {
            store.record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
            watch.emit(RunEvent::new(
                run_id,
                step,
                EventKind::FellBackTo { provider: served },
            ));
        }
        let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
        tokens_used += step_tokens;

        let call = response
            .tool_calls
            .iter()
            .find(|c| c.name == WRITE_FILE_TOOL);
        let tool_call_json = call.map(|c| c.arguments.to_string()).unwrap_or_default();
        let write = call.and_then(|c| c.arguments.get("content").and_then(|v| v.as_str()));

        let (decision, result_text) = match write {
            Some(content) => {
                fs.write(content).await?;
                ("wrote file", content.to_string())
            }
            None => ("no tool call", response.text.clone().unwrap_or_default()),
        };
        // The file write (if any) is already applied above, before this commit:
        // a crash between the write and the commit replays this step, and the
        // model re-observes the already-written file, so the edit lands exactly
        // once. The committed checkpoint is the step's completion marker.
        //
        // Single-file mode has no workspace-change signal of its own, so `changed`
        // is whether this step wrote the file at all — the nearest true statement
        // the mode can make.
        commit_step(
            store,
            watch,
            run_id,
            0,
            StepRecord::new(step, decision, result_text).with_trace(
                user,
                tool_call_json,
                step_tokens,
            ),
            write.is_some(),
            true,
        )?;

        // Cost budget: checked after this step's tokens are counted.
        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                finish(store, watch, run_id, 0, step, "cost_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::CostBudgetExceeded { steps: step },
                    run_id,
                ));
            }
        }

        // A run with no criterion ends when the agent stops calling tools. After
        // the budget checks, because a step that also crossed a ceiling crossed
        // it — and before the gate, which for this variant can never pass.
        if finished(contract, &response) {
            finish(store, watch, run_id, 0, step, "finished")?;
            return Ok(RunResult::new(RunOutcome::Finished { steps: step }, run_id));
        }

        let contents = fs.read().await?;
        let guard = ExecGuard::new(&permissive)
            .tracing(store, run_id, step)
            .watching(watch, 0);
        if contract
            .verify
            .passes_guarded(&contract.file, &contents, &guard)
            .await?
        {
            finish(store, watch, run_id, 0, step, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id));
        }
        // `|=` rather than `=`: the question is whether the criterion has *ever*
        // judged and refused, and a later gate that could not run must not erase
        // one that already said no.
        criterion_failed |= gate_judged(&contract.verify);
    }

    let (recorded, outcome) = capped_outcome(criterion_failed, contract.max_steps);
    finish(store, watch, run_id, 0, contract.max_steps, recorded)?;
    Ok(RunResult::new(outcome, run_id))
}

/// The workspace loop (0.3 multi-file mode): the agent greps, finds, reads, and
/// writes several files under `root`, carrying its own working memory as an
/// observation log folded into each turn's prompt. Budgets, retry, trace, and
/// resume behave as in single-file mode; verification is multi-file
/// ([`Verification::passes_in`]).
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_workspace_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    root: &Path,
    start_step: u32,
    policy: &Policy,
    approver: &dyn Approver,
    mcp: &McpSession,
    lsp: &LspSession,
    browser: &BrowserSession,
    skills: &Skills,
    watch: &Watch<'_>,
    extras: &TurnExtras<'_>,
) -> Result<RunResult> {
    // 0.34.0 — every precondition a review criterion and a routing rule bring,
    // checked before the first completion is billed. A contract that cannot be
    // honoured should cost nothing to find out about, which is why this is here
    // and not at the gate: a review gate fires at the END of a run.
    preflight_review_and_routing(contract, provider, approver).await?;

    // The effective policy grows as approvers remember rules; it is rebuilt as a
    // merge so a remembered allow can still never defeat a deny beneath it.
    let mut effective = policy.clone();
    let mut remembered: Vec<Rule> = Vec::new();
    // 0.31.0 — the plan gate. Whether the phase is still on is asked of the STORE,
    // not of a local: a run approved in one process and killed in the next must not
    // plan again, and one that was never approved must not start writing because the
    // approval died with the process that held it.
    let mut planning = contract.plan_gate.is_some() && store.approved_plan(run_id)?.is_none();
    if planning {
        effective = effective.merge(plan_lock());
    }
    let mut ws = Workspace::with_policy(root, effective.clone());
    // MCP tools sit beside the built-ins under their namespaced names, so the
    // model chooses between them the same way it chooses between grep and find.
    // Registered in-process tools and MCP tools sit beside the built-ins under
    // their own names, so the model chooses between them the same way it chooses
    // between grep and find.
    let mut extra = contract.tools.specs();
    extra.extend(mcp.tool_specs());
    extra.extend(lsp_tools(lsp));
    #[cfg(feature = "browser")]
    if browser.configured() {
        extra.extend(crate::tools::browser::browser_tools());
    }
    extra.extend(skill_tool(skills));
    // 0.45.0 — composed once, here, and reused on every step. A system prompt that
    // varied between steps would move 0.38.0's cache breakpoint every turn and bill
    // a cache write per step on both wires that honour it.
    // 0.45.0 — the boundary the agent is told about is the one that will refuse it.
    // Two of them, because the plan gate narrows the policy while the phase is on and
    // the prompt the loop falls back to when it ends is a different string already.
    // An approver's remembered rule is not reflected: it widens the boundary mid-run,
    // and a prompt composed once cannot follow it (`docs/CONTRACT.md`).
    let after_planning =
        boundary_section(policy, &contract.exec_sandbox, will_proxy(policy, contract));
    // 0.60.3 — a binding rather than an argument built inline, matching the shape the
    // tree loop has had since 0.45.0, because two blocks of one turn now read it: the
    // `system` block's planning arm and the classifying opening below.
    let while_planning = boundary_section(
        &effective,
        &contract.exec_sandbox,
        will_proxy(&effective, contract),
    );
    let base_system = compose(PromptSpec {
        base: WORKSPACE_PROMPT,
        prompt: &contract.prompt,
        extra: &extra,
        skills,
        directive: None,
        instructions: &contract.instructions,
        boundary: after_planning.as_deref(),
        family: provider.prompt_family(),
        ending: CALL_TOOLS_ENDING,
    });
    let mut system = match planning {
        true => compose(PromptSpec {
            base: WORKSPACE_PROMPT,
            prompt: &contract.prompt,
            extra: &extra,
            skills,
            // `false`: this block is composed for a turn already decided to be work,
            // so there is one reading of it and the gate binds all of it.
            directive: Some(planning_directive(&contract.agents, false)),
            instructions: &contract.instructions,
            boundary: while_planning.as_deref(),
            family: provider.prompt_family(),
            ending: CALL_TOOLS_ENDING,
        }),
        false => base_system.clone(),
    };
    report_prompt(
        watch,
        run_id,
        0,
        &system,
        contract,
        provider.prompt_family(),
        after_planning.is_some(),
    );
    // 0.37.0 — the prompt the first completion of a conversational turn is made
    // with, and only the first. Today's prompt tells the agent it is executing a
    // task, which is why a diligent model reaches for a tool to answer a question
    // about itself; a turn that is allowed to answer has to be allowed to say so.
    // Built with the same wrappers `base_system` is, planning directive included,
    // so a classifying turn under a plan gate is still told about the gate.
    //
    // Every later step of a promoted turn uses `system`, unchanged from 0.36.1:
    // permitting an answer is a decision about the turn's opening, not a licence
    // to stop at a plan in prose on step nine.
    let conversational = conversational_opening(
        // 0.49.0 — a turn that has not been decided to be work is not told it has a
        // specification to meet. Every later step is `system` above, unchanged.
        CONVERSATION_PROMPT,
        contract,
        extras,
        &extra,
        skills,
        planning,
        after_planning.as_deref(),
        while_planning.as_deref(),
        provider.prompt_family(),
    );
    let mut tools = workspace_tools();
    tools.extend(extra);
    // Offered only while the phase is on, and withdrawn the moment it ends: a tool
    // that proposes a plan on a run that already has an approved one would be a
    // second gate mid-run, which is a second way for an unattended run to stop.
    if planning {
        tools.push(propose_plan_spec());
    }
    // Durable budget: restored from the store so a resume continues the same
    // token and wall-clock budget rather than restarting it at zero.
    let mut tokens_used: u64 = store.spent_tokens(run_id)?;
    // History, append-only. What the model sees of it is decided per turn by
    // `assemble`, under the contract's context budget — the log itself is never
    // trimmed, so the trace keeps everything.
    //
    // Restored from the store, so a resumed run continues with the context it had
    // rather than re-deriving one from the workspace and asking the model a
    // different question than the process before it would have. Empty for a fresh
    // run, and empty for a run checkpointed before 0.13.0, which is the same
    // re-derivation that binary did.
    let (mut ledger, mut written) = restore_ledger(store, run_id)?;
    // A session turn continues a conversation, and the conversation enters as
    // observations rather than as a longer goal: the assembler then bounds and
    // compacts it under the contract's context budget, which is the machinery that
    // already decides what a long run's history gets to say. Step 0, because no
    // step of THIS run produced them.
    //
    // Only when the ledger is empty, which is a fresh turn. A resumed turn already
    // has them from the store — the seed was persisted with the first step — and
    // seeding again would say everything twice.
    seed_conversation(&mut ledger, extras);
    // 0.68.0 — and made durable immediately, before the first step, which is what
    // lets the conversation be folded at all.
    //
    // A fold may only replace entries the store already holds
    // (`count = (len - keep).min(written)`), and `written` is 0 until the first
    // `persist_ledger` at the END of step one. So a turn seeded with a long
    // conversation could not fold at its first step — not on the threshold, and
    // not on the overflow recovery either, which sets `forced` and then dies at
    // the same `count == 0` guard before `forced` is ever read. The turn most
    // likely to overflow the window was the one immune to both remedies.
    //
    // This is not the rule at the commit boundary below being relaxed. That rule
    // is about an observation belonging to a step: it must not outlive a step
    // that never committed, so the ledger never runs ahead of the trace. The seed
    // belongs to no step of this run — it is step 0, a copy of `session_turns`
    // rows that are already durable — so there is no step it could outlive, and
    // nothing here can be orphaned by a step that fails to commit.
    //
    // A no-op on a resumed run: `restore_ledger` already returned
    // `written == ledger.len()`, so the slice appended is empty.
    written = persist_ledger(store, run_id, &ledger, written)?;
    // 0.68.0 — the caller's standing request for a fold, held here so it can be
    // consumed once. `fold_forced` takes it rather than reading it, which is what
    // makes "fold this turn" a request about the turn and not a setting that folds
    // every step of it.
    let mut fold_asked = contract.fold_now;
    // Is the agent getting anywhere? Restored from nothing on resume by design: a
    // resumed run has just been given a fresh chance, and condemning it for the
    // window it stalled in before the crash would be a poor welcome.
    let mut progress = Progress::new();
    // 0.34.0 — which model routing has moved this run to, or `None` while it is
    // still on the provider's own. Held here rather than read off the request,
    // because each request is built fresh and would report a transition every
    // step. Not restored on resume: a resumed run re-derives it from the gate
    // history it can still read, on its first step.
    let mut routed_model: Option<String> = None;
    // 0.44.0 — the frozen prefix this run last built, held for the same reason
    // `routed_model` is: the marker is offered only when a step's candidate prefix is
    // byte-identical to the previous step's, and a comparison recomputed from a freshly
    // built request could never see that. `None` while there is no fold, after a fold
    // whose summary assembly stubbed, and on a resume — a resumed run has sent this
    // prefix zero times from where it now stands, so it earns the marker again.
    let mut marked_prefix = PrefixGuard::default();
    // 0.70.0 — has the criterion ever judged this run and said no? Held here for
    // the same reason `marked_prefix` above is: it is a fact about the run rather
    // than about a step, and the loop's tail is where it is needed and where the
    // step it came from is already gone.
    //
    // Restored from the store, and the first draft's reasoning for NOT restoring
    // it — "the run is about to evaluate the gate again on its first step" — is
    // false for the one case that matters. `start_step..=max_steps` is empty on a
    // resume that does not raise the cap, so there is no first step to earn the
    // answer on, and a flag starting false would let the tail overwrite a durable
    // `"verification_failed"` with `"step_cap_reached"`.
    let mut criterion_failed = criterion_already_failed(store, run_id);
    // 0.70.0 — the gate-failure section most recently appended to the ledger, so
    // a criterion failing the same way every step is reported once rather than
    // once per step. Run-scoped for the same reason `criterion_failed` is: the
    // comparison is against what an earlier step appended, and that step is gone
    // by the time the next one asks.
    let mut last_gate_feedback: Option<String> = None;
    // 0.49.0 — what each step of THIS run asked for, so the next step can send it
    // back as an assistant turn.
    //
    // 0.64.0 — restored from the store, for the same reason the ledger above is:
    // a resumed run that could not read these sent the model a third-person
    // account of its own past actions, and everything before the crash arrived as
    // one block of user prose. The two halves are restored together because they
    // are two halves of one request — the ledger carries the results, this carries
    // the calls they answer. Empty for a run written before 0.64.0, which falls
    // back exactly as every resumed run did then.
    let mut turns: BTreeMap<u32, StepTurn> = restore_turns(store, run_id)?;
    // The run's live process handles, created before the first turn and killed
    // when the run ends however it ends. `Arc` because the reaping task for each
    // handle outlives the dispatch that started it and has to be able to record
    // the exit; `Handles` also kills whatever is still live when it drops, which
    // is the backstop for the paths that leave this function by `?`.
    let handles = std::sync::Arc::new(crate::tools::handles::Handles::new(
        crate::tools::handles::MAX_LIVE_HANDLES,
    ));
    // Seeded from the store, so a handle the previous process left behind is
    // answerable rather than merely absent. Without this a poll of an id from
    // before the crash would report "no such handle", which is true of this
    // registry and misleading about the run: the model would reasonably conclude
    // it had mistyped and try again. Adopted already-terminal — nothing here
    // attaches, polls or signals.
    for h in store.process_handles(run_id)? {
        if h.state == "orphaned" {
            handles.adopt_orphan(h.handle, &h.line);
        }
    }
    let handles = &handles;
    // Detected once, before the first turn. The marker files do not change under
    // a run often enough to be worth a filesystem walk every step, and a run that
    // creates its own `package.json` is creating a project rather than working in
    // one.
    let toolchain = crate::toolchain::detect(root);
    // 0.46.0 — resolved once per run, beside the detection it reads. The writable
    // roots depend on the toolchain, and `select` probes the host, so neither
    // belongs on a per-call path.
    let containment = exec_containment(&contract.exec_sandbox, toolchain.as_ref());
    // 0.48.0 — the run owns its proxy, and the containment carries the address so
    // every spawn site scopes the sandbox to it without asking a second question.
    let egress = start_egress_proxy(policy, containment.as_ref()).await;
    let containment = match (&containment, &egress) {
        (Some(c), Some((proxy, _, _))) => {
            #[cfg(feature = "browser")]
            browser.route_through(proxy.addr());
            Some(std::sync::Arc::new(c.with_proxy(Some(proxy.addr()))))
        }
        _ => containment,
    };
    report_containment(
        watch,
        run_id,
        0,
        &contract.exec_sandbox,
        containment.as_deref(),
    );
    let mem_key = memory_key(root);
    // Images the agent looked at last step, carried into this one's request and
    // dropped once shown. A viewed image is a tool result, not a permanent part
    // of the conversation: the model that wants it again asks again, and the
    // request stays bounded by what one step actually needed.
    let pending_media = &mut PendingMedia::default();
    // 0.37.0 — a turn that may answer is typed as a reply before its first
    // completion is billed, and corrected to a run the moment that completion
    // reaches for a tool. Written in this order rather than at the close, so a
    // process killed mid-answer leaves a row that says what it was doing:
    // `Store::check_resumable` refuses it as work to continue, because there is no
    // committed step to continue from and re-asking replaces the one completion at
    // the same price.
    open_turn_kind(store, run_id, extras)?;

    for step in start_step..=contract.max_steps {
        // The store's copy of each live handle's processes, refreshed each step.
        //
        // Kept current here rather than swept at the end because this loop has
        // eleven exits, and a rule applied at ten of them is the failure mode
        // the orphaning comment above warns about. A handle's pids are only
        // known once its stages have actually spawned, which is after the call
        // that started it returned, so this is the first place that can record
        // them — and recording them every step means whichever exit the run
        // takes, the trace already has what it needs.
        //
        // Killing the live ones is NOT done here: `Handles` kills on drop, which
        // covers every exit including a panic, and is the property that actually
        // matters for the operator's machine.
        for (id, pids) in handles.live_handles() {
            store.record_handle_pids(run_id, id, &pids)?;
        }
        // Carry any ending the reaping tasks noticed to disk.
        //
        // A handle that exits on its own is seen by its task, which cannot write
        // to the store — a `rusqlite` connection is not `Sync` — so the ending
        // lives only in memory until something on this thread records it. The
        // write is guarded in SQL on the row still being `running`, so replaying
        // it costs nothing and cannot overwrite a kill that already landed.
        for (id, state) in handles.states() {
            if let crate::tools::handles::HandleState::Exited(code) = state {
                store.record_handle_ended(run_id, id, "exited", code, None)?;
            }
        }
        // 0.48.0 — the proxy is told which step it is on, so a dial is attributed
        // to the step that made it rather than to the boundary that observed it,
        // and it is handed the policy as it now stands: a plan gate narrows the
        // effective policy mid-run, and a proxy deciding against the policy the
        // run *started* with would permit what the run had since stopped
        // permitting. Then the step's decisions are carried to disk, beside the
        // handle endings above and for the same reason — neither the proxy's
        // tasks nor the reapers can reach the store.
        if let Some((proxy, shared, at)) = &egress {
            at.store(step, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut guard) = shared.write() {
                guard.clone_from(ws.policy());
            }
            record_dials(Some(proxy), store, watch, run_id, 0)?;
        }
        // 0.48.0 — and a contained handle's containment ends when its processes
        // do. The `create` and `exec` rows were written where the handle was
        // started; this is the only thread that can write the `destroy` one,
        // because the reaping task cannot reach the store. Once per handle rather
        // than once per step: the sweep above may be replayed harmlessly and a
        // trace row may not, so the once-ness lives in the registry.
        //
        // A run that ends while a handle is still live writes no destroy row —
        // the registry kills it on drop and there is no step left to record on,
        // which is the same position `record_handle_ended` is in and is stated in
        // the release record rather than papered over.
        if containment.is_some() {
            for id in handles.take_unreported_endings() {
                let mut ended = crate::state::SandboxEvent::destroy(run_id, step);
                ended.detail = Some(format!("shell_start handle {id}"));
                record_sandbox_step(store, watch, 0, &ended);
            }
        }

        // The step boundary, where a cancellation is honoured (see `cancelled`).
        if let Some(o) = cancelled(store, watch, run_id, 0, step - 1)? {
            return Ok(RunResult::new(o, run_id).with_remembered(remembered));
        }
        // The same boundary is where an operator's steering lands, for the same
        // reason: the points in between are not safe to stop at or to change course
        // from — a tool call is in flight and a file may be half-written.
        if let Some(o) = drain_steer(
            store,
            watch,
            run_id,
            step,
            &mut ledger,
            extras,
            &mut fold_asked,
        )? {
            return Ok(RunResult::new(o, run_id).with_remembered(remembered));
        }
        if let Some(max) = contract.max_duration {
            if store.elapsed_secs(run_id)? > max.as_secs_f64() {
                finish(store, watch, run_id, 0, step - 1, "time_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                )
                .with_remembered(remembered));
            }
        }

        // One budget, derived once per turn: it sets both this request's ceiling
        // and the per-observation cap the results of this step enter under.
        let budget_tokens = contract
            .context
            .effective_tokens(contract.max_tokens.map(|m| m.saturating_sub(tokens_used)));
        let entry_cap = entry_cap_chars(budget_tokens);
        // 0.55.0 — the operator's own read ceiling, when they set one. Resolved
        // here so it travels with `entry_cap`: a read is measured against both.
        let max_read = contract.max_read_chars.map(|c| c as usize);
        // Re-read each turn rather than once at the start, so the notes the model
        // sees are the notes the store holds — including one written this run, and
        // not one the operator has since cleared.
        //
        // 0.57.0 — and ranked by what this turn is about, which grows as the run
        // reads: the signals are rebuilt here each turn for the same reason the
        // notes are.
        let signals = recall_signals(&contract.goal, &ledger);
        let (notes, global_notes) = recall_scopes(store, &mem_key, &signals)?;
        // 0.43.0 — before assembly, never inside it. Over the threshold, the older
        // observations become one written paragraph and the assembler bounds a
        // shorter ledger; under it, nothing happens and no provider is called.
        // 0.43.0 — at most two attempts at this step's completion, and the second
        // only when the first came back "this request did not fit". The threshold
        // was guessing at what the vendor has now stated, so the recovery fold is
        // unconditional; the bound is one per step, so a request that cannot be
        // made to fit escalates rather than looping.
        let mut fold_tokens = 0;
        let mut recovered = false;
        // 0.54.0 — read-only calls started off this step's stream. Declared out
        // here rather than inside the loop so a compaction retry keeps the
        // counters: the reads the abandoned attempt did were still done, and a
        // discard rate that forgot them would flatter the feature.
        //
        // `max_parallel_reads > 1` is the whole switch, deliberately: the setting
        // that turns 0.41.0's overlapping off turns starting early off with it,
        // so `with_max_parallel_reads(1)` is one escape hatch rather than two.
        let mut spec = (contract.max_parallel_reads > 1
            && extras.stream
            // A tool hook can refuse a call outright and runs serially. Asking it
            // early would hand it a call that may never settle — the same
            // objection that keeps an approver out — so a run with hooks
            // configured does not speculate at all.
            && contract.tool_hooks.is_none())
        .then(|| {
            Speculation::new(
                ws.clone(),
                &contract.tools,
                containment.clone(),
                entry_cap,
                max_read,
                contract.max_parallel_reads,
                run_id,
                step,
            )
        });
        let (response, assembled, user) = loop {
            fold_tokens += compact_ledger(
                provider,
                contract,
                store,
                run_id,
                step,
                watch,
                0,
                &mut ledger,
                &mut written,
                budget_tokens,
                fold_forced(recovered, 0, &mut fold_asked),
            )
            .await?;
            let assembled = assemble(
                &ledger,
                budget_tokens,
                &notes,
                &global_notes,
                Assembly {
                    ws: Some(&ws),
                    policy: &effective,
                    store,
                    run_id,
                    step,
                },
            )
            .await?;
            // 0.48.0 — asked by the same condition that already chooses the system
            // half below, so the two halves of one completion cannot disagree.
            let user = match &conversational {
                Some(_) if step == start_step => {
                    conversational_user_prompt(&contract.goal, &assembled.text)
                }
                _ => workspace_user_prompt(contract, &assembled.text, toolchain.as_ref()),
            };
            // 0.44.0 — the second cache breakpoint, at the end of what compaction
            // froze, and only once that prefix has already gone out once.
            let cache_boundary =
                cache_boundary_for(&user, &ledger, &mut marked_prefix, watch, run_id, step, 0);
            let messages = transcript(&user, &assembled, &turns);
            #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
            let request = CompletionRequest {
                // 0.37.0 — the conversational prompt is this turn's opening only. Every
                // later step is the loop of 0.36.1, asked the way it has always been
                // asked: permitting an answer is a decision about a turn's first
                // completion, not a licence to stop at a plan in prose on step nine.
                system: match &conversational {
                    Some(c) if step == start_step => c.clone(),
                    _ => system.clone(),
                },
                user: user.clone(),
                // 0.49.0 — the same emission as a conversation. Empty until this run
                // has driven a step of its own, which is what keeps a first step and a
                // resumed run byte-identical on the wire to what 0.48.0 sent.
                messages: messages.clone(),
                tools: tools.clone(),
                // 0.22.0 — the run's web declaration, unchanged per step.
                web: contract.web.clone(),
                // 0.31.0 — the root's tier, unchanged per step.
                effort: contract.effort,
                cache_boundary,
                // 0.49.0 — the same breakpoint the line above names, counted in
                // messages because that is what this request sends.
                cache_through: cache_through_for(cache_boundary, &messages),
                #[cfg(feature = "media")]
                media: attach_media(contract, pending_media)?,
                ..Default::default()
            };
            // 0.34.0 — the rule that changes which model answers, applied to the
            // request that is actually sent rather than recorded beside it.
            let mut request = request;
            apply_routing(
                contract,
                &mut request,
                &mut routed_model,
                store,
                run_id,
                root,
                step,
                watch,
                0,
            );

            match complete_with_retry(
                provider,
                &request,
                contract,
                store,
                run_id,
                step,
                watch,
                0,
                extras.stream,
                !recovered && contract.compaction.enabled(),
                spec.as_mut(),
            )
            .await
            {
                Ok(response) => break (response, assembled, user),
                // The same condition `may_compact` was passed under, so the loop and
                // `complete_with_retry` cannot disagree about whether this run is
                // allowed to recover — with folding off, the call above has already
                // finished the run as escalated and retrying here would drive a run
                // that has ended.
                Err(e)
                    if !recovered && contract.compaction.enabled() && is_context_overflow(&e) =>
                {
                    recovered = true;
                    continue;
                }
                Err(e) => return Err(e),
            }
        };
        // 0.30.0: which notes this turn actually leaned on, recorded per run. The
        // trace already said how many were carried; it could not say which, and a
        // count cannot tell a load-bearing entry from a passenger. Recorded once,
        // after the attempt that succeeded, so a recovered step does not write the
        // recall twice.
        record_recalls(store, run_id, step, &mem_key, &global_notes, &assembled)?;

        // Which provider answered, when that is not a foregone conclusion. A
        // `Fallback` that fell over served this step from its secondary, and a trace
        // reader has no other way to know.
        if let Some(served) = provider.last_served() {
            store.record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
            watch.emit(RunEvent::new(
                run_id,
                step,
                EventKind::FellBackTo { provider: served },
            ));
        }
        // The fold's own completion is part of what this step cost. `steps.tokens`
        // is what `spent_tokens` sums and what the token budget is measured
        // against, so a fold left out of it would be spend the run's own ceiling
        // never saw — which is the one defect this could ship without noticing.
        let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0) + fold_tokens;
        tokens_used += step_tokens;
        // The provider's own number for the request `assemble` just built, beside
        // the estimate: the pair is what makes the estimator's drift auditable. A
        // silent provider leaves it null rather than recording a zero.
        if step_tokens > 0 {
            store.record_context_reported(run_id, step, step_tokens)?;
        }

        // 0.31.0 — the thinking, to whoever is watching and to nowhere else. It is
        // deliberately NOT pushed onto `ledger`: the vendor charged for it once as
        // output, and folding it into the ledger would put it in every prompt this
        // run assembles from here on and be charged for it again as input, every
        // turn. `Usage::reasoning_tokens` is already persisted and is the durable
        // record of what it cost.
        if let Some(thinking) = response.reasoning.as_deref() {
            watch.emit(RunEvent::new(
                run_id,
                step,
                EventKind::Reasoning {
                    text: thinking.to_string(),
                    tokens: response.usage.map(|u| u.reasoning_tokens).unwrap_or(0),
                },
            ));
        }

        // 0.49.0 — record what this step asked for before anything is dispatched,
        // so the next step sends the model its own turn back instead of a
        // third-person account of it.
        //
        // 0.64.0 — and stage it durably, so a resumed run sends it back too. The
        // write itself rides the transaction that commits this step, which is why
        // this is a stage and not a record: a step that never commits must leave
        // no turn, and a driver whose lease was taken from it must write none.
        turns.insert(
            step,
            StepTurn {
                text: response.text.clone(),
                calls: response.tool_calls.clone(),
            },
        );
        store.stage_step_turn(
            run_id,
            AssistantTurn::new(step, response.text.clone(), response.tool_calls.clone()),
        );

        // Dispatch every tool call the model made this step, in order, folding
        // each result into the observation log the next turn will see.
        let mut decisions: Vec<String> = Vec::new();
        let mut calls_json: Vec<String> = Vec::new();
        // Did this step move the workspace? Only a write that wrote something
        // different can, and it is the half of the stall signal that says the agent
        // is not merely repeating itself but achieving nothing.
        let mut step_changed = false;
        // 0.22.0 — a provider-executed search that broke reaches the model as an
        // observation, so it can retry or answer knowingly. Without it the model
        // sees an answer with no sources and concludes the web had nothing.
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

        // 0.37.0 — the turn's own first completion decides what the turn was, and
        // it decides for free: this is the completion the loop was going to make
        // anyway, read rather than assumed.
        //
        // Stopped on text with nothing to do → the turn is an answer, and it ends
        // here, before anything that would report work having happened. Carrying a
        // tool call → the turn is work, the row is corrected, and the loop carries
        // on **from this same completion**, so the run's first step is the call
        // that was already paid for and nothing is asked twice.
        //
        // 0.54.0 — what starting early bought this step and what it cost, emitted
        // as soon as the completion has settled and the counts are final.
        //
        // Here rather than beside the step's commit because a step does not always
        // reach its commit: the classification just below ends the run on a
        // completion that stopped on text, and the reads that completion caused
        // were still done. An event that appeared only on the paths that finished
        // would under-report exactly the discards worth seeing.
        //
        // Only when something was started, so a step that speculated nothing — and
        // every run whose provider does not report finished calls — leaves the
        // event stream exactly as 0.53.0 left it.
        if let Some((started, used, discarded)) = spec.as_ref().map(|s| s.counts()) {
            if started > 0 {
                watch.emit(RunEvent::new(
                    run_id,
                    step,
                    EventKind::Speculated {
                        started,
                        used,
                        discarded,
                    },
                ));
            }
        }

        // Only the first completion. A later one that stops on text is the loop
        // finishing a run, which is what it has always been and is left alone
        // below.
        if let Some(o) = classify_first_completion(
            store,
            watch,
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
            return Ok(RunResult::new(o, run_id).with_remembered(remembered));
        }

        let mut paused: Option<i64> = None;
        // 0.21.0 — the other reason a step can stop short: a question nobody here
        // would answer. Kept separate from `paused` so the two pauses cannot be
        // confused for one another in the outcome.
        let mut asked: Option<i64> = None;
        // 0.31.0 — the third: a plan nobody here would decide on, and a plan somebody
        // cancelled. Separate from the two above for the same reason they are
        // separate from each other.
        let mut plan_pending: Option<i64> = None;
        let mut plan_cancelled = false;
        let mut plan_approved = false;
        let mut new_rules: Vec<Rule> = Vec::new();
        // 0.41.0 — the completion's calls are partitioned by whether they can
        // change anything, and each maximal run of read-only ones is dispatched
        // together. Everything below this line then folds a `Dispatched` exactly
        // as it always has, in the order the model asked, whether that result
        // came back from a batch or from a lone call: concurrency is where the
        // work happened, not what the run recorded.
        //
        // A cap of 1 never enters the batch path at all, which is what makes the
        // change bisectable — `with_max_parallel_reads(1)` is 0.40.0's loop.
        let effects: Vec<ToolEffect> = response
            .tool_calls
            .iter()
            .map(|c| tool_effect(&c.name, &contract.tools))
            .collect();
        let mut batched: std::collections::VecDeque<Dispatched> = std::collections::VecDeque::new();
        let mut at = 0usize;
        while at < response.tool_calls.len() {
            if batched.is_empty()
                && contract.max_parallel_reads > 1
                && effects[at] == ToolEffect::ReadOnly
                // 0.54.0 — a call already run off the stream is not batched
                // again. What survives speculation is a contiguous run from
                // position zero, so the first call without a result is where
                // batching starts and the two never overlap.
                && spec.as_ref().is_none_or(|s| !s.has(at))
            {
                let end = at
                    + effects[at..]
                        .iter()
                        .take_while(|e| **e == ToolEffect::ReadOnly)
                        .count();
                if end - at > 1 {
                    batched = read_batch(
                        &ws,
                        &response.tool_calls[at..end],
                        approver,
                        store,
                        run_id,
                        step,
                        &contract.tools,
                        entry_cap,
                        max_read,
                        watch,
                        0,
                        contract.max_parallel_reads,
                        &contract.goal,
                        contract.tool_hooks.as_deref(),
                    )
                    .await?;
                }
            }
            let call = &response.tool_calls[at];
            let position = at;
            at += 1;
            calls_json.push(format!("{}:{}", call.name, call.arguments));
            // 0.54.0 — a call whose read already happened, off the stream. The
            // work is the only thing that moved: the announcement is made here,
            // in call order, at exactly the point `read_batch` makes it, so an
            // observer sees the same events in the same order whether the read
            // started early or not.
            let speculated = spec.as_mut().and_then(|s| s.take(position));
            if speculated.is_some() {
                announce(watch, run_id, step, 0, call);
            }
            let dispatched = match speculated.or_else(|| batched.pop_front()) {
                Some(done) => done,
                None => {
                    dispatch(
                        &ws,
                        call,
                        approver,
                        responder_of(contract),
                        store,
                        run_id,
                        step,
                        mcp,
                        lsp,
                        browser,
                        &contract.tools,
                        skills,
                        entry_cap,
                        max_read,
                        &mem_key,
                        contract.memory,
                        watch,
                        0,
                        pending_media,
                        &contract.commit_identity,
                        contract.exec_timeout,
                        containment.as_ref(),
                        toolchain.as_ref(),
                        handles,
                        PlanPhase {
                            gate: contract.plan_gate.as_deref(),
                            agents: &contract.agents,
                            active: planning,
                        },
                        &contract.goal,
                        contract.tool_hooks.as_deref(),
                    )
                    .await?
                }
            };
            match dispatched {
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
                    // A `Revise` never reaches here; see the dispatch arm.
                    _ => {
                        decisions.push(format!("awaiting plan decision (plan {plan_id})"));
                        plan_pending = Some(plan_id);
                        break;
                    }
                },
            }
        }

        // 0.31.0 — the phase ends here, mid-step, and the observation that carries
        // the approved plan is what puts it in the next assembled prompt. The policy
        // is rebuilt from the base rather than edited, so the `plan-gate` layer goes
        // and every rule an approver remembered stays.
        if plan_approved {
            planning = false;
            effective = policy.clone();
            if !remembered.is_empty() {
                effective = effective.merge(remembered_layer(&remembered));
            }
            ws = Workspace::with_policy(root, effective.clone());
            tools.retain(|t| t.name != PROPOSE_PLAN_TOOL);
            system = base_system.clone();
            if let Some(approved) = store.approved_plan(run_id)? {
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

        // The trace gets this step's observations unelided, so concatenating the
        // rows in step order reproduces the whole log: bounding what the model
        // sees must not bound what an operator can audit. A delta rather than the
        // whole log per row, so the trace is linear in the step count and a
        // 24-hour run does not write the same text hundreds of times.
        // ponytail: each row repeats the whole log, so the column grows with the
        // square of the step count. Bounded in practice by the step budget times
        // the entry cap; write per-step deltas if a long run's store size matters.
        //
        // The assembly stats this line used to log (`carried`, `stubbed`,
        // `est_tokens`) are not lost with the loop's own `info!`: `assemble`
        // records them as the step's `"assembled"` context event, which is where a
        // reader could already find them.
        commit_step(
            store,
            watch,
            run_id,
            0,
            StepRecord::new(step, decisions.join("; "), ledger.text_for_step(step)).with_trace(
                user,
                calls_json.join(" | "),
                step_tokens,
            ),
            step_changed,
            true,
        )?;
        // The step is committed, so the observations behind it are safe to make
        // durable. After the commit rather than before: a ledger that ran ahead of
        // the trace would restore observations for a step the run never took.
        written = persist_ledger(store, run_id, &ledger, written)?;

        // Did that step get anywhere? A stall needs both halves — nothing changed
        // in the workspace AND a tool call this window already saw — because a
        // legitimate exploration phase changes nothing either, and flagging that
        // would degrade healthy runs to add resilience.
        let signature = calls_json.join(" | ");
        match progress.step(contract.stall, step_changed, &signature) {
            Progressing::Fine => {}
            Progressing::Replan => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::replan(
                        step,
                        format!(
                            "{} steps without progress; replanning",
                            contract.stall.window
                        ),
                    ),
                )?;
                // The directive is an observation like any other, so it is bounded
                // by the same budget and, carrying no target, can never be
                // superseded away.
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
                info!(run_id, step, "agent told to change approach");
                watch.emit(RunEvent::new(
                    run_id,
                    step,
                    EventKind::Replan {
                        window: contract.stall.window,
                    },
                ));
            }
            Progressing::Stalled => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::stalled(step, "still no progress after replanning"),
                )?;
                info!(run_id, step, "run stopped: stalled");
                watch.emit(RunEvent::new(run_id, step, EventKind::Stalled));
                finish(store, watch, run_id, 0, step, "stalled")?;
                return Ok(RunResult::new(RunOutcome::Stalled { steps: step }, run_id)
                    .with_remembered(remembered));
            }
        }

        // 0.31.0 — the plan stops. Checked before the two below because a run that
        // reached either of these has written nothing at all, which is a stronger
        // statement than "it stopped", and the outcome should say so.
        if let Some(plan_id) = plan_pending {
            finish(store, watch, run_id, 0, step, "awaiting_plan")?;
            return Ok(RunResult::new(
                RunOutcome::AwaitingPlan {
                    plan_id,
                    steps: step,
                },
                run_id,
            )
            .with_remembered(remembered));
        }
        if plan_cancelled {
            finish(store, watch, run_id, 0, step, "plan_rejected")?;
            return Ok(
                RunResult::new(RunOutcome::PlanRejected { steps: step }, run_id)
                    .with_remembered(remembered),
            );
        }

        // An approver deferred: persist nothing further, stop, and let the
        // caller resume once a human has decided.
        if let Some(question_id) = asked {
            finish(store, watch, run_id, 0, step, "awaiting_answer")?;
            return Ok(RunResult::new(
                RunOutcome::AwaitingAnswer {
                    question_id,
                    steps: step,
                },
                run_id,
            )
            .with_remembered(remembered));
        }

        if let Some(request_id) = paused {
            finish(store, watch, run_id, 0, step, "awaiting_approval")?;
            return Ok(RunResult::new(
                RunOutcome::AwaitingApproval {
                    request_id,
                    steps: step,
                },
                run_id,
            )
            .with_remembered(remembered));
        }

        // Rules an approver asked to remember apply as a top layer for the rest
        // of the run. Merging (rather than editing) is what keeps a remembered
        // allow from overriding a deny beneath it.
        if !new_rules.is_empty() {
            let mut layer = Policy::permissive().layer("remembered");
            for r in &new_rules {
                layer = layer.rule(r.act, r.effect, r.pattern.clone());
            }
            effective = effective.merge(layer);
            ws = Workspace::with_policy(root, effective.clone());
            remembered.extend(new_rules);
        }

        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                finish(store, watch, run_id, 0, step, "cost_budget_exceeded")?;
                return Ok(
                    RunResult::new(RunOutcome::CostBudgetExceeded { steps: step }, run_id)
                        .with_remembered(remembered),
                );
            }
        }

        // A run with no criterion ends when the agent stops calling tools. Checked
        // after the budgets, so a step that also crossed a ceiling reports the
        // ceiling, and before the gate, which for this variant can never pass.
        if finished(contract, &response) {
            finish(store, watch, run_id, 0, step, "finished")?;
            return Ok(RunResult::new(RunOutcome::Finished { steps: step }, run_id)
                .with_remembered(remembered));
        }

        if evaluate_gate(
            contract,
            root,
            &ExecGuard::new(&effective)
                .tracing(store, run_id, step)
                .watching(watch, 0)
                .with_writable_roots(gate_roots(toolchain.as_ref())),
            store,
            run_id,
            step,
            watch,
            0,
        )
        .await?
        {
            finish(store, watch, run_id, 0, step, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id)
                .with_remembered(remembered));
        }
        // See the single-file loop: `|=`, and `Verification::None` never counts.
        criterion_failed |= gate_judged(&contract.verify);
        // 0.70.0 — and the failure's own words go into the next step's request.
        // An observation rather than a new prompt section, which is what keeps
        // 0.44.0's `cache_boundary_for` looking at the same string shape it has
        // always looked at: this arrives where every tool result arrives, bounded
        // by the same `entry_cap`, foldable by the same compaction.
        //
        // Guarded on there being a next step: the last step of a run has no
        // request to inform, and a context event for a step that never ran would
        // say a blind attempt was told.
        // And guarded on the section being NEW. The ledger accumulates for the
        // whole run, so a gate failing the same way at every step would append a
        // near-identical block per step and re-send all of them thereafter —
        // the context leak with a plausible-looking cause this release's own
        // contract names as a risk. Comparing against the last one appended
        // costs one `String` and collapses the common case to a single block,
        // while a failure that CHANGES is still reported. The ledger is never
        // shortened to achieve this: it is tracked by a watermark index and
        // anything that shortens it in place corrupts the store.
        if step < contract.max_steps {
            if let Some((key, section)) = gate_failure_feedback(store, run_id, step)
                .filter(|(key, _)| last_gate_feedback.as_deref() != Some(key.as_str()))
            {
                store.record_context_event(
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
    finish(store, watch, run_id, 0, contract.max_steps, recorded)?;
    Ok(RunResult::new(outcome, run_id))
}

/// The server half of a diagnostics answer, attributed to the server that gave it.
///
/// `asked` is the same distinction `check` already draws against the automatic
/// post-edit path. A model that ASKED is told everything, including that a server
/// had nothing to add and why — an empty answer to a direct question reads as
/// "your project is clean". Nobody asked after an edit, so only findings are
/// spoken there; a line per edit saying a server still cannot answer is noise the
/// model pays for on every write.
pub(super) fn lsp_diagnostics_text(
    reports: &[(String, crate::tools::diagnostics::Outcome)],
    asked: bool,
) -> String {
    use crate::tools::diagnostics::Outcome;
    let mut out = String::new();
    for (server, outcome) in reports {
        match outcome {
            Outcome::Found(text) => {
                out.push_str(&format!("\n[language server {server}]\n{text}\n"));
            }
            Outcome::Clean if asked => {
                out.push_str(&format!("\n[language server {server}] found nothing\n"));
            }
            Outcome::Failed(why) | Outcome::Skipped(why) if asked => {
                out.push_str(&format!(
                    "\n[language server {server} did not answer] {why}\n"
                ));
            }
            _ => {}
        }
    }
    out
}

/// A 1-based position from a tool call, or the observation that says why not.
///
/// Line 0 is not a line any file has, and a model that sends one is off by one
/// rather than pointing at the top of the file — so it is refused by name rather
/// than clamped into an answer about the wrong line.
pub(super) fn at(a: &serde_json::Value) -> std::result::Result<(u32, u32), String> {
    let read = |key: &str| {
        a.get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    match (read("line"), read("column").or_else(|| read("character"))) {
        (Some(line), Some(column)) if line >= 1 && column >= 1 => Ok((line, column)),
        (Some(_), Some(_)) => Err(
            "\n[lsp error] \"line\" and \"column\" are 1-based, the way \
                                   read_file shows them and a compiler reports them; 0 is not a \
                                   position a file has\n"
                .to_string(),
        ),
        _ => Err(
            "\n[lsp error] this tool needs \"line\" and \"column\" as 1-based numbers\n"
                .to_string(),
        ),
    }
}

/// Turn a navigation answer, or the reason there is none, into one observation.
///
/// A failure here is an observation and never a failed run: a server that has not
/// finished indexing, or that does not answer this question, is something the
/// model adapts to, the way it adapts to a refused path or a bad regex. What it
/// must never be is empty — an empty answer to "who calls this" reads as "nobody
/// does".
/// Turn a screenshot's base64 into the media the next request carries.
///
/// The browser hands back base64 and the media path takes bytes, so this is the
/// one place the two meet. A screenshot that will not decode is dropped with its
/// reason rather than failing the action: the text half of the answer is still
/// worth having.
#[cfg(feature = "browser")]
pub(super) fn decode_screenshot(
    encoded: &str,
) -> std::result::Result<crate::provider::Media, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("the browser sent an unreadable screenshot: {e}"))?;
    crate::provider::Media::image("image/png", &bytes).map_err(|e| e.to_string())
}

pub(super) fn navigated(name: &str, answer: Result<String>, cap: usize) -> Dispatched {
    let obs = match answer {
        Ok(text) => format!("\n[{name}]\n{text}\n"),
        Err(e) => format!("\n[{name} unavailable] {e}\n"),
    };
    Dispatched::Continue {
        decision: format!("asked the language server: {name}"),
        obs: bound(&obs, cap, ObsKind::Tool),
        kind: ObsKind::Tool,
        target: None,
        // A question changes nothing, so it is not progress for the stall signal —
        // the same reasoning `check` is not.
        changed: false,
        remember: Vec::new(),
    }
}

/// The five navigation schemas, offered only to a run that configured a server.
///
/// Conditional on purpose, and it is the release's negative control: a run with
/// no `[[lsp]]` table gets an empty vector here, so its composed system prompt is
/// byte-identical to the one 0.51.0 composed. Under 0.38.0's cacheable prefix a
/// schema is paid for on every request of every run, so "free for a consumer who
/// does not want it" has to be true on bytes rather than in spirit.
pub(super) fn lsp_tools(lsp: &LspSession) -> Vec<ToolSpec> {
    if lsp.is_empty() {
        return Vec::new();
    }
    let position = |what: &str| {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the workspace root." },
                "line": { "type": "integer", "description": format!("1-based line of {what}, as read_file shows it.") },
                "column": { "type": "integer", "description": format!("1-based column of {what}.") }
            },
            "required": ["path", "line", "column"]
        })
    };
    vec![
        ToolSpec {
            name: LSP_DEFINITION_TOOL.to_string(),
            description: "Where the symbol at this position is defined, resolved by the language \
                          server rather than guessed from a text search."
                .to_string(),
            parameters: position("the symbol"),
        },
        ToolSpec {
            name: LSP_REFERENCES_TOOL.to_string(),
            description: "Every place the symbol at this position is used, resolved by the \
                          language server — not the lines a text search would match."
                .to_string(),
            parameters: position("the symbol"),
        },
        ToolSpec {
            name: LSP_SYMBOLS_TOOL.to_string(),
            description: "The symbols in one file, or — with \"query\" — where a symbol with that \
                          name is in the workspace."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "One file's symbols, path relative to the workspace root." },
                    "query": { "type": "string", "description": "Search the whole workspace for symbols with this name instead." }
                }
            }),
        },
        ToolSpec {
            name: LSP_HOVER_TOOL.to_string(),
            description: "What the symbol at this position is: its type, signature and \
                          documentation, as an editor would show on hover."
                .to_string(),
            parameters: position("the symbol"),
        },
        ToolSpec {
            name: LSP_RENAME_TOOL.to_string(),
            description: "Rename the symbol at this position everywhere it is used. Writes \
                          NOTHING: it answers with a patch series, which you apply per file with \
                          patch_file."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the workspace root." },
                    "line": { "type": "integer", "description": "1-based line of the symbol." },
                    "column": { "type": "integer", "description": "1-based column of the symbol." },
                    "new_name": { "type": "string", "description": "What to rename it to." }
                },
                "required": ["path", "line", "column", "new_name"]
            }),
        },
    ]
}

/// Start every language server the contract configured, rooted at its workspace.
///
/// One helper rather than the same three lines at eight call sites: the root a
/// server is told to index is the run's own workspace root, and a site that spelled
/// it differently would point one server somewhere else.
pub(super) async fn lsp_for(
    contract: &TaskContract,
    policy: &Policy,
    store: &Store,
    run_id: i64,
    watch: &Watch<'_>,
) -> Result<LspSession> {
    let root = contract
        .root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    LspSession::connect(&contract.lsp, policy, &root, store, run_id, watch).await
}

/// The browser session for one run, which starts no process.
///
/// Lazy by design, unlike `lsp_for`: a language server has an index worth warming
/// while the model composes, and a browser has nothing to warm. A run that
/// configures one and never browses pays for no process at all.
#[cfg(feature = "browser")]
pub(super) fn browser_for(contract: &TaskContract, _policy: &Policy) -> BrowserSession {
    BrowserSession::new(contract.browser.clone())
}

#[cfg(not(feature = "browser"))]
pub(super) fn browser_for(_contract: &TaskContract, _policy: &Policy) -> BrowserSession {
    BrowserSession
}
