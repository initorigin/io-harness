//! outcome: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// Refuse a model that would be approving for its own model (0.42.0).
///
/// The approval mirror of the review refusal below it, and the same argument: a
/// model answering for a call it just made reports what the run already believes,
/// so it is a refusal rather than a warning. Taken before the first request is
/// built, which is what makes it observable as **zero** calls to either provider.
///
/// A helper rather than another arm of the preflight because there are two loops:
/// the flat one preflights, the tree entry does not, and an unattended tree is
/// exactly where a self-approving model would do the most damage.
pub(super) fn refuse_self_approval<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    approver: &dyn Approver,
) -> Result<()> {
    let Some(approving) = approver.model() else {
        // Not a model. Nothing to compare, and nothing to refuse.
        return Ok(());
    };
    if approver.self_approval_allowed() {
        return Ok(());
    }
    // The run's own model, as this contract asks for it, and as the provider says
    // it would ask. `None` on both means the provider's own default, which this
    // crate cannot name — so it cannot compare it either, and says so in
    // `docs/CONTRACT.md` rather than guessing.
    let routed = contract.routing.as_ref().and_then(|r| {
        r.escalate_after
            .as_ref()
            .map(|(_, m)| m.as_str())
            .or(r.downshift_under.as_ref().map(|(_, m)| m.as_str()))
    });
    for writing in [routed, provider.model_hint()].into_iter().flatten() {
        if writing == approving {
            return Err(Error::Config(format!(
                "the approving model and the model making the call are both {approving}; a model \
                 answering for its own call reports what the run already believes. Use a different \
                 model, or build the approver with allow_self_approval(true) to say you meant it"
            )));
        }
    }
    Ok(())
}

/// Everything refused before the first request is billed.
///
/// Four checks, all of them things a run can be certain of up front:
///
/// 1. A [`Verification::Review`] criterion with no reviewer registered. A gate
///    that cannot run is found at run start rather than after the work.
/// 2. The reviewing model being the model that produced the change. A model
///    grading its own answer reports what the run already believes, so this is a
///    refusal rather than a warning — and it happens here, before any request is
///    built, which is what makes it observable as *zero* calls to the reviewing
///    provider.
/// 3. [`Routing::require_primary`](crate::Routing) against
///    [`Provider::reachable`]. An unattended job that starts on a fallback nobody
///    chose is the failure this exists to prevent.
/// 4. A model approving for its own model (0.42.0), through
///    [`refuse_self_approval`] — which the tree entry calls too, because a tree
///    does not come through here.
pub(super) async fn preflight_review_and_routing<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    approver: &dyn Approver,
) -> Result<()> {
    refuse_self_approval(contract, provider, approver)?;
    if let Verification::Review {
        allow_self_review, ..
    } = &contract.verify
    {
        let Some(reviewer) = contract.reviewer.as_ref() else {
            return Err(Error::Config(
                "a Verification::Review criterion needs a reviewer; register one with \
                 TaskContract::with_reviewer"
                    .into(),
            ));
        };
        // The run's own model, as this contract asks for it. `None` means the
        // provider's own default, which this crate cannot name — so it cannot
        // compare it either, and says so in `docs/CONTRACT.md` rather than
        // guessing.
        let author = contract.routing.as_ref().and_then(|r| {
            r.escalate_after
                .as_ref()
                .map(|(_, m)| m.as_str())
                .or(r.downshift_under.as_ref().map(|(_, m)| m.as_str()))
        });
        if !allow_self_review {
            if let (Some(reviewing), Some(writing)) = (reviewer.model(), author) {
                if reviewing == writing {
                    return Err(Error::Config(format!(
                        "the reviewing model and the model under review are both {reviewing}; a \
                         model grading its own work reports what the run already believes. Use a \
                         different model, or set allow_self_review: true to say you meant it"
                    )));
                }
            }
            if let (Some(reviewing), Some(writing)) = (reviewer.model(), provider.model_hint()) {
                if reviewing == writing {
                    return Err(Error::Config(format!(
                        "the reviewing model and the model under review are both {reviewing}; a \
                         model grading its own work reports what the run already believes. Use a \
                         different model, or set allow_self_review: true to say you meant it"
                    )));
                }
            }
        }
    }

    if contract.routing.as_ref().is_some_and(|r| r.require_primary) && !provider.reachable().await?
    {
        return Err(Error::Config(format!(
            "{} reports it is not reachable and this contract requires the primary provider; \
             refusing to start rather than running unattended on a fallback",
            provider.name()
        )));
    }
    Ok(())
}

/// Evaluate the contract's criterion once, and record what it decided (0.34.0).
///
/// The one place a gate outcome is produced, so `passed`, `failed` and — the
/// distinction 0.34.0 exists to make — `errored` are written by the same code for
/// every criterion and for every entry point. A gate that returns `Err` here has
/// already recorded [`GateOutcome::Errored`](crate::GateOutcome), which is what
/// makes [`retry_gate`] able to tell "the review never happened" from "the review
/// said no".
#[allow(clippy::too_many_arguments)]
pub(super) async fn evaluate_gate(
    contract: &TaskContract,
    root: &Path,
    guard: &crate::verify::ExecGuard<'_>,
    store: &Store,
    run_id: i64,
    step: u32,
    watch: &Watch<'_>,
    depth: u32,
) -> Result<bool> {
    let phase = gate_phase(&contract.verify);
    match &contract.verify {
        Verification::Review { rubric, .. } => {
            // Absent reviewer is caught at run start, so reaching here without one
            // is a bug in this crate rather than a caller's mistake — but it is
            // still reported rather than treated as a failing gate, because a
            // criterion that could not run has not judged anything.
            let Some(reviewer) = contract.reviewer.as_ref() else {
                let e = Error::Config(
                    "a Verification::Review criterion needs a reviewer; register one with \
                     TaskContract::with_reviewer"
                        .into(),
                );
                store.put_gate_attempt(
                    run_id,
                    step,
                    phase,
                    GateOutcome::Errored,
                    &e.to_string(),
                )?;
                return Err(e);
            };
            let request = crate::verify::ChangeReview {
                goal: contract.goal.clone(),
                rubric: rubric.clone(),
                changes: written_changes(store, run_id, root),
            };
            match reviewer.review_change(request).await {
                Ok(review) => {
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::Reviewed {
                            passed: review.passed,
                            reasons: review.reasons.clone(),
                        },
                    ));
                    let outcome = if review.passed {
                        GateOutcome::Passed
                    } else {
                        GateOutcome::Failed
                    };
                    store.put_gate_attempt(
                        run_id,
                        step,
                        phase,
                        outcome,
                        &review.reasons.join("; "),
                    )?;
                    Ok(review.passed)
                }
                // A review that did not happen. No `Reviewed` event, because
                // nothing was reviewed — the event stream would otherwise report a
                // verdict nobody gave.
                Err(e) => {
                    store.put_gate_attempt(
                        run_id,
                        step,
                        phase,
                        GateOutcome::Errored,
                        &e.to_string(),
                    )?;
                    Err(e)
                }
            }
        }
        _ => match contract.verify.passes_in_guarded(root, guard).await {
            Ok(passed) => {
                let outcome = if passed {
                    GateOutcome::Passed
                } else {
                    GateOutcome::Failed
                };
                store.put_gate_attempt(run_id, step, phase, outcome, "")?;
                Ok(passed)
            }
            Err(e) => {
                store.put_gate_attempt(
                    run_id,
                    step,
                    phase,
                    GateOutcome::Errored,
                    &e.to_string(),
                )?;
                Err(e)
            }
        },
    }
}

/// A criterion's short name, as it is recorded in `gate_attempts.phase`.
pub(super) fn gate_phase(verify: &Verification) -> &'static str {
    match verify {
        Verification::Command { .. } => "command",
        Verification::Review { .. } => "review",
        Verification::EachCompilesRust(_) => "compiles",
        Verification::DocumentContains { .. } => "document",
        Verification::FileContains(_)
        | Verification::FileEquals(_)
        | Verification::WorkspaceFileContains { .. } => "contains",
        Verification::None => "none",
    }
}

/// Every path the run wrote, with its contents as they now stand.
///
/// Read from `edits` — the run's own record of what it touched — rather than by
/// walking the tree, so a reviewer is handed the run's change and not the
/// repository. A path that has since been deleted is skipped: a reviewer reading
/// an empty file it was told exists would be judging an artefact of the read.
pub(super) fn written_changes(
    store: &Store,
    run_id: i64,
    root: &Path,
) -> Vec<crate::verify::FileChange> {
    let mut seen: Vec<String> = Vec::new();
    let mut changes = Vec::new();
    for edit in store.edits(run_id).unwrap_or_default() {
        if seen.contains(&edit.path) {
            continue;
        }
        seen.push(edit.path.clone());
        // The file as it stands. A path the run wrote and something then removed
        // is skipped rather than reported as empty, which is what `written_files`
        // has always done and is the honest reading: there is nothing to review.
        let Ok(after) = std::fs::read_to_string(root.join(&edit.path)) else {
            continue;
        };
        // The way it was, from the restore point the store has kept since 0.28.0.
        // One row per file per run, written at the *first* edit, which is exactly
        // "before this run touched it" — the boundary a reviewer of a change
        // wants, and not the one before the last edit.
        let (before, unkept) = match store.snapshot(run_id, &edit.path).ok().flatten() {
            Some(snap) => match snap.kept {
                crate::state::Kept::Text(text) => (Some(text), None),
                crate::state::Kept::Absent => (None, None),
                crate::state::Kept::Unkept(why) => (None, Some(why)),
            },
            // No row at all: a store written before restore points, or one this
            // version does not understand. Absent is the wrong answer — it would
            // claim the run created the file — so it is reported as not kept.
            None => (None, Some("no restore point was recorded".to_string())),
        };
        changes.push(crate::verify::FileChange {
            path: PathBuf::from(&edit.path),
            before,
            after,
            unkept,
        });
    }
    changes
}

/// How many gate attempts in a row, most recent first, ended without passing
/// (0.34.0).
///
/// What [`Routing::escalate_after`](crate::Routing) counts.
///
/// Consecutive rather than cumulative — and today the two readings agree, because
/// a gate that passes ends the run, so no run can hold a pass followed by a
/// failure. It is written this way for the case that stops being true: a
/// criterion that is evaluated without ending the run would make "failed three
/// times ever" and "failing right now" different questions, and escalating on the
/// first is escalating for history.
pub(super) fn consecutive_gate_failures(store: &Store, run_id: i64) -> u32 {
    let attempts = store.gate_attempts(run_id).unwrap_or_default();
    attempts
        .iter()
        .rev()
        .take_while(|a| a.outcome != GateOutcome::Passed)
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

///
/// What [`Routing::downshift_under`](crate::Routing) measures. On disk rather
/// than summed from the edits, because an edit that replaced a file twice is one
/// change of one size and two rows.
pub(super) fn bytes_written(store: &Store, run_id: i64, root: &Path) -> u64 {
    let mut seen: Vec<String> = Vec::new();
    let mut total = 0u64;
    for edit in store.edits(run_id).unwrap_or_default() {
        if seen.contains(&edit.path) {
            continue;
        }
        seen.push(edit.path.clone());
        if let Ok(meta) = std::fs::metadata(root.join(&edit.path)) {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Apply the contract's routing rules to one outbound request (0.34.0).
///
/// Called with the request already built, so the rule sets the model that is
/// actually sent rather than an intention recorded beside it. Emits
/// [`EventKind::Routed`](crate::EventKind) only when the model changes, which is
/// what makes "this run moved" distinguishable from "this run always used that
/// model".
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_routing(
    contract: &TaskContract,
    request: &mut CompletionRequest,
    routed: &mut Option<String>,
    store: &Store,
    run_id: i64,
    root: &Path,
    step: u32,
    watch: &Watch<'_>,
    depth: u32,
) {
    let Some(routing) = contract.routing.as_ref() else {
        return;
    };
    let failures = consecutive_gate_failures(store, run_id);
    let written = bytes_written(store, run_id, root);
    let Some(model) = routing.model_for(failures, written) else {
        return;
    };
    // The comparison is against what the RUN is on, not against this request:
    // every request is built fresh with `model: None`, so comparing here would
    // announce a transition on every step and make a run that moved
    // indistinguishable from one that always used that model.
    if routed.as_deref() == Some(model) {
        request.model = Some(model.to_string());
        return;
    }
    let why = if routing
        .escalate_after
        .as_ref()
        .is_some_and(|(after, m)| failures >= *after && m == model)
    {
        format!("{failures} consecutive gate failures")
    } else {
        format!("{written} bytes written so far")
    };
    watch.emit(RunEvent::at_depth(
        run_id,
        step,
        depth,
        EventKind::Routed {
            from: request.model.clone().unwrap_or_default(),
            to: model.to_string(),
            why,
        },
    ));
    *routed = Some(model.to_string());
    request.model = Some(model.to_string());
}

/// Did this step end the run, because there is no gate and the agent stopped
/// acting?
///
/// Both halves are required. Without [`Verification::None`] an assistant turn
/// with no tool call is an ordinary unproductive step — the agent thinking aloud,
/// or asking a question the loop cannot answer — and ending the run there would
/// silently cap every existing contract at its first quiet turn. Without the
/// empty tool-call list there is no signal at all: no `done` tool is added, so an
/// unverified run has exactly the tool surface a verified one has, and a model
/// that has finished says so by saying something.
pub(super) fn finished(contract: &TaskContract, response: &CompletionResponse) -> bool {
    matches!(contract.verify, Verification::None) && answered(response)
}

/// Did this completion end the exchange — no tool call, and not a pause?
///
/// The half of [`finished`] that is about the *response* rather than about the
/// contract, split out in 0.63.0 because the two questions had been one function
/// and a caller who wanted only the second could not ask it.
///
/// [`classify_first_completion`] is that caller. It has already decided whether
/// this turn may answer — `TurnExtras::classify`, which
/// `TaskContract::conversational` now sets — and asking `finished` re-derived
/// `Verification::None` a second time, underneath the decision, so an explicit
/// `conversational: Some(true)` on a judged turn set the flag and then had it
/// silently overruled. The inference had two homes and the release moved one of
/// them.
///
/// 0.22.0 — the pause. A provider running a long web search hands back what it
/// has so far with a *paused* stop reason and no tool call, which is
/// indistinguishable from a finished answer by the tool-call check alone. Ending
/// there would stop the turn in the middle of the search it was told to make.
pub(super) fn answered(response: &CompletionResponse) -> bool {
    response.tool_calls.is_empty() && !paused_turn(response)
}

/// Did the provider pause this turn rather than end it?
///
/// Anthropic says `pause_turn` when a server-executed tool has been running long
/// enough that it would rather hand back control than hold the connection. It is
/// a continuation signal, not a finish: the loop takes another step, and the
/// partial text is an observation like any other.
///
/// What this crate does NOT do is echo the vendor's partial assistant blocks back
/// verbatim — the request has been one flattened user turn since 0.1.0 — so a
/// paused turn resumes as a fresh request and the provider may repeat a search it
/// already charged for. `docs/CONTRACT.md` says so.
pub(super) fn paused_turn(response: &CompletionResponse) -> bool {
    response.finish_reason.as_deref() == Some("pause_turn")
}

/// The note a failed provider-executed call leaves in the observation log, if
/// this response reported one.
///
/// A vendor reports a broken search inside an otherwise successful response, so
/// without this the model sees an answer with no results and concludes the web
/// had nothing to say. Naming the failure lets it retry or proceed knowingly.
pub(super) fn web_failure_note(response: &CompletionResponse) -> Option<String> {
    let failed: Vec<String> = response
        .server_tools
        .iter()
        .filter(|c| !c.succeeded())
        .map(|c| {
            format!(
                "{} failed ({})",
                c.tool,
                c.error.as_deref().unwrap_or("no reason given")
            )
        })
        .collect();
    (!failed.is_empty()).then(|| failed.join("; "))
}

pub(super) fn record_resume_markers(store: &Store, run_id: i64) -> Result<u32> {
    let last = store.last_step(run_id)?;
    let start_step = last + 1;
    store.record_checkpoint_event(&crate::state::CheckpointEvent::resume(
        run_id,
        start_step,
        format!("resuming at step {start_step}, {last} committed step(s) skipped"),
    ))?;
    for s in 1..=last {
        store.record_checkpoint_event(&crate::state::CheckpointEvent::skipped(run_id, s))?;
    }
    // Every process handle the previous process left running is orphaned here,
    // unconditionally, before the loop restarts.
    //
    // This sits in `record_resume_markers` rather than in the resume entry
    // points because this function is the one place every resume path passes
    // through — the flat loop, the tree loop, and the decision, answer and
    // stored-policy variants all reach it. Orphaning is the kind of rule that is
    // worthless if it holds on three paths out of four, and putting it at the
    // funnel is the only version of it that cannot be forgotten by a resume
    // added later.
    //
    // It is unconditional on purpose. The only thing a checkpoint records about
    // a live process is its pid, and a pid is not an identity: between the crash
    // and this moment the operating system may have given that number to
    // something unrelated. There is no test that separates the two with enough
    // confidence to justify signalling, because every "is it still our program"
    // check races the signal that follows it. So the handle is recorded,
    // reported, and left alone — this is the one place this crate could damage
    // something outside its own workspace, and the cost of being wrong is not a
    // failed run but somebody else's process.
    let orphaned = store.orphan_live_handles(run_id, crate::tools::handles::ORPHAN_REASON)?;
    for h in &orphaned {
        store.record_checkpoint_event(&crate::state::CheckpointEvent::resume(
            run_id,
            start_step,
            format!(
                "process handle {} (`{}`) was left running by a previous process and is \
                 orphaned: {}. It is not re-attached, polled or signalled.",
                h.handle,
                h.line,
                crate::tools::handles::ORPHAN_REASON
            ),
        ))?;
    }
    Ok(start_step)
}

/// A run's ledger as the store has it, with the count already durable.
///
/// The count is the watermark [`persist_ledger`] appends from: everything below
/// it is on disk, everything above it was observed since the last committed
/// step.
pub(super) fn restore_ledger(store: &Store, run_id: i64) -> Result<(ContextLedger, usize)> {
    let mut ledger = ContextLedger::new();
    for obs in store.observations(run_id)? {
        ledger.push(obs);
    }
    // 0.43.0 — and then the folds, in the order they happened.
    //
    // The observations are the whole history and a fold is a *view* of it, so a
    // resume that stopped here would hand the model back every observation the
    // run had already paid to summarise — and would then summarise them again the
    // next time the threshold was crossed, buying the same paragraph twice.
    // Replaying the rows is what makes a resumed, branched or replayed run
    // reproduce the fold instead. Each summary covers a prefix of `kept_from`
    // entries as the ledger stood when it was written, and prefixes nest, so
    // applying them oldest-first reconstructs exactly the ledger the run had.
    //
    // A row whose prefix no longer exists — a store edited by hand, or a summary
    // from a longer history than this run has rows for — is skipped rather than
    // panicking on the slice: a fold is a compression of history, and refusing to
    // resume over an odd one would be refusing over something purely advisory.
    for summary in store.summaries(run_id)? {
        let covers = summary.folded as usize;
        if covers == 0 || covers > ledger.len() {
            continue;
        }
        ledger.fold_first(
            covers,
            Observation::new(
                summary.through_step,
                ObsKind::Message,
                Some("summary".into()),
                format!("\n[earlier work, summarised]\n{}\n", summary.text),
            ),
        );
    }
    // Everything restored is durable by definition, including the summary, which
    // is a `summaries` row rather than a `ledger_observations` one and so must sit
    // below the watermark rather than be appended to the log as an observation.
    let written = ledger.len();
    Ok((ledger, written))
}

/// A run's assistant turns as the store has it, keyed by step (0.64.0).
///
/// The other half of what [`restore_ledger`] restores. The ledger holds every
/// tool *result* and, because [`Piece::of`](crate::context::Piece) classifies by
/// kind and ordinals are counted positionally per step, a restored ledger
/// correlates every result with the call it answers — as soon as there is a call
/// to correlate it with. This is those calls.
///
/// Empty for a run written before 0.64.0 and for a run that took no step. Those
/// are the same to a reader and both mean "there is nothing to restore", which is
/// 0.63.0's behaviour: the transcript builder falls back to prose for a step it
/// has no turn for, which is exactly what every resumed run did before this
/// release. **Absent is not empty**: a step with no row falls back, and a step
/// whose row carries no calls and no text is a real turn that did nothing, and is
/// emitted as one.
pub(super) fn restore_turns(store: &Store, run_id: i64) -> Result<BTreeMap<u32, StepTurn>> {
    let mut turns = BTreeMap::new();
    for turn in store.step_turns(run_id)? {
        turns.insert(
            turn.step,
            StepTurn {
                text: turn.text,
                calls: turn.calls,
            },
        );
    }
    Ok(turns)
}

/// Append everything observed since the last committed step, and return the new
/// watermark.
///
/// Called at the step boundary that commits, so an observation belonging to a
/// step that never committed does not outlive it — the ledger stays consistent
/// with the trace rather than running ahead of it.
///
/// And once before the loop, on the seeded conversation (0.68.0). That is not an
/// exception to the rule above: the seed belongs to no step of this run, so there
/// is no step it could outlive. It is written early because a fold may only
/// replace entries the store already holds, and a conversation sitting above the
/// watermark is one that neither the threshold nor the overflow recovery could
/// fold.
pub(super) fn persist_ledger(
    store: &Store,
    run_id: i64,
    ledger: &ContextLedger,
    written: usize,
) -> Result<usize> {
    store.record_observations(run_id, &ledger.entries()[written..])?;
    Ok(ledger.len())
}

/// Report the capability bundles this run is carrying, loaded and dropped
/// (0.35.0).
///
/// Emitted at run start, beside `Started`, on step 0 — a bundle is part of a
/// run's configuration rather than something that happened on a step, which is
/// the same reason an MCP connect records step 0.
///
/// The dropped half is why this is the crate's job and not the caller's:
/// [`Plugins`](crate::Plugins) has no error path, so a bundle that failed to load
/// is visible only to someone who thought to look. Putting it in the trace means
/// an operator reading a run's events finds out that the deny rules they believed
/// in were never installed.
///
/// It reads what the contract is already holding. Nothing here touches the
/// filesystem — loading happened before the run, in the caller's own
/// [`Config::plugins`](crate::Config::plugins) call.
pub(super) fn emit_plugins(watch: &Watch<'_>, run_id: i64, contract: &TaskContract) {
    for plugin in contract.plugins.iter() {
        watch.emit(RunEvent::new(
            run_id,
            0,
            EventKind::PluginLoaded {
                plugin: plugin.id().to_string(),
                contributions: plugin
                    .contributions()
                    .into_iter()
                    .map(String::from)
                    .collect(),
            },
        ));
    }
    for dropped in contract.plugins.dropped() {
        watch.emit(RunEvent::new(
            run_id,
            0,
            EventKind::PluginDropped {
                plugin: dropped.id.clone(),
                why: dropped.error.clone(),
            },
        ));
    }
}

/// The outcome of a run that is already over, if it is over.
///
/// Idempotence for every resume entry point: a run that already finished is
/// returned as-is, so resuming twice does not re-drive the loop or re-charge the
/// budget. A run still `Running` — its process died mid-loop — reads as `None`
/// here and is resumed from its last committed step.
/// 0.65.0 — refuse to drive a run that has a call the harness cannot inspect
/// still open in its journal.
///
/// Returns the pause, or `None` when there is nothing indeterminate to decide
/// about — which is every run of built-in tools, since a replayable call is never
/// journalled at all.
///
/// **Why this is a gate at each resume root rather than a check inside the
/// loop.** What must not happen is the loop being *entered*: the first thing a
/// resumed run does is re-drive the step that died, and the model re-issues the
/// call it was making. By the time the loop can look at anything, the decision to
/// repeat the call has already been taken.
///
/// The oldest open attempt is the one reported. A run interrupted twice is
/// decided one attempt at a time, in the order the calls were made, because a
/// decision about the second says nothing about the first.
pub(super) fn recovery_pause(
    store: &Store,
    run_id: i64,
    observer: &dyn crate::Observer,
) -> Result<Option<RunOutcome>> {
    let open = store.open_attempts(run_id)?;
    let Some(first) = open.first() else {
        return Ok(None);
    };
    // Announced as well as returned. A caller driving many runs learns which
    // attempt is holding this one without opening the store, and an operator
    // watching a fleet is the party the pause exists for.
    crate::run::Watch::new(observer).emit(crate::RunEvent::new(
        run_id,
        first.step,
        crate::EventKind::RecoveryPaused {
            attempt_id: first.id,
            tool: first.tool.clone(),
        },
    ));
    Ok(Some(RunOutcome::AwaitingRecovery {
        attempt_id: first.id,
        steps: store.last_step(run_id)?,
    }))
}

pub(super) fn finished_outcome(store: &Store, run_id: i64) -> Result<Option<RunOutcome>> {
    if store.run_status(run_id)? != Some(RunStatus::Completed) {
        return Ok(None);
    }
    terminal_outcome(store, run_id)
}

pub(super) fn terminal_outcome(store: &Store, run_id: i64) -> Result<Option<RunOutcome>> {
    let last = store.last_step(run_id)?;
    Ok(store.outcome(run_id)?.and_then(|o| match o.as_str() {
        "success" => Some(RunOutcome::Success { steps: last }),
        "denied" => Some(RunOutcome::Denied { steps: last }),
        "budget_ceiling_reached" => Some(RunOutcome::BudgetCeilingReached { steps: last }),
        "stalled" => Some(RunOutcome::Stalled { steps: last }),
        // Before 0.11.0 `"escalated"` was unmapped and `finish_run` reported it as a
        // plain completion, so resuming an escalated run fell straight back into the
        // loop and re-ran it. An unattended run that escalated at 3am was silently
        // restarted by the next resume.
        "escalated_retryable" => Some(RunOutcome::Escalated {
            steps: last,
            retryable: true,
        }),
        "escalated_terminal" | "escalated" => Some(RunOutcome::Escalated {
            steps: last,
            retryable: false,
        }),
        // 0.12.0: the same defect, found by the same kind of audit. `"refused"` is
        // written when a human denies the network access the provider needs
        // (`authorize_provider`), and it was unmapped — so resuming a refused run
        // re-entered the loop and asked the human the same question again. A human's
        // no is as final as a policy's, which is why `denied` above is final too.
        "refused" => Some(RunOutcome::Refused { steps: last }),
        // Also 0.12.0: a run its observer stopped. Final for the same reason a
        // human's `denied` is — the caller asked for it — so a resume reports it
        // rather than quietly starting the run up again.
        "cancelled" => Some(RunOutcome::Cancelled { steps: last }),
        // 0.17.0: a `Verification::None` run that ended on its own terms. Final —
        // there is no criterion left to re-check, so a resume reports it rather
        // than driving the loop again to watch the agent say nothing twice.
        "finished" => Some(RunOutcome::Finished { steps: last }),
        // 0.31.0: a plan a human cancelled. Final for the reason `denied` and
        // `cancelled` are — a person said no — so a resume reports it rather than
        // asking the model to propose the same approach again.
        "plan_rejected" => Some(RunOutcome::PlanRejected { steps: last }),
        _ => None,
    }))
}
