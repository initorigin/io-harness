//! dispatch: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// [`gate`](super::gate::gate), timed as the step's policy phase (0.75.0).
///
/// Every gate this module asks for goes through here, because this item shadows
/// the glob-imported one for the whole file: twenty call sites renamed by hand
/// would be twenty chances for the twenty-first to be added untimed, and a phase
/// that silently stops covering a tool is a number that reads as a tool getting
/// faster.
///
/// **The wait for a human is part of it.** A call the policy sends to an approver
/// spends the step's wall clock there, and the step's own span already contains
/// it; leaving it out would move that time into the unattributed remainder and
/// say the loop lost it.
///
/// The reading lands on the step the loop currently has open, and is dropped when
/// there is none — a sub-agent tree's dispatch, which this release does not
/// attribute.
#[allow(clippy::too_many_arguments)]
async fn gate(
    ws: &Workspace,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    act: Act,
    target: &str,
    content: Option<&str>,
    watch: &Watch<'_>,
    depth: u32,
    goal: &str,
) -> Result<Gated> {
    let gated_at = std::time::Instant::now();
    let out = super::gate::gate(
        ws, approver, store, run_id, step, act, target, content, watch, depth, goal,
    )
    .await;
    store.attribute_gate(run_id, step, gated_at.elapsed());
    out
}

/// [`gate_declared`](super::gate::gate_declared), timed the same way (0.80.0).
///
/// A sibling of [`gate`] above rather than a flag on it, for the reason that one
/// gives: the timing wrapper exists so no call site can be added untimed, and a
/// second entry point that skipped it would be exactly that. One caller — the
/// `read_skill` arm, whose target is a path the operator's configuration named
/// and not one the model chose.
#[allow(clippy::too_many_arguments)]
async fn gate_declared(
    ws: &Workspace,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    act: Act,
    target: &str,
    content: Option<&str>,
    watch: &Watch<'_>,
    depth: u32,
    goal: &str,
) -> Result<Gated> {
    let gated_at = std::time::Instant::now();
    let out = super::gate::gate_declared(
        ws, approver, store, run_id, step, act, target, content, watch, depth, goal,
    )
    .await;
    store.attribute_gate(run_id, step, gated_at.elapsed());
    out
}

/// Record one sandbox lifecycle row for a contained tool call, and tell the
/// observer.
///
/// 0.40.0. The same two writes `src/verify.rs` has made since 0.6.0, reached from
/// the tool layer so that a contained `exec` or `shell` is auditable the way a
/// contained verification gate already is. Through one helper rather than copied
/// into each tool arm: the two arms would otherwise drift, and a missing row is
/// invisible until somebody needs it.
///
/// The backend recorded is the one that **actually applied**. A run whose host
/// refused the native primitive took `PortableFloor` and confined nothing, and
/// the whole value of this row is telling that apart afterwards from a run that
/// was contained as asked.
pub(super) fn record_sandbox_step(
    store: &Store,
    watch: &Watch<'_>,
    depth: u32,
    event: &crate::state::SandboxEvent,
) {
    let _ = store.record_sandbox_event(event);
    watch.emit(RunEvent::at_depth(
        event.run_id,
        event.step,
        depth,
        EventKind::Sandbox {
            kind: event.kind.clone(),
            backend: event.backend.clone(),
        },
    ));
}

/// One `read_skill` target as text: a file's contents, or a directory's entries
/// one per line (0.73.0).
///
/// Listing a directory rather than erroring on it is the deliberate half. A skill
/// that says "see `references/`" would otherwise cost the model a turn guessing a
/// filename, and every name handed back is a file that same skill may already
/// read — the gate has already decided this path, and each entry is checked again
/// on the call that opens it. Sorted, so two runs over one bundle read alike, and
/// the count leads so a listing cut by `cap_result` still says how much was there.
///
/// `metadata` follows links, which is correct here: a symlink to a directory that
/// `resolve_under` already proved stays inside the root should list like the
/// directory it is.
fn skill_target(path: &std::path::Path) -> std::io::Result<String> {
    if !std::fs::metadata(path)?.is_dir() {
        return std::fs::read_to_string(path);
    }
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        // A trailing slash so the model can tell what it may list next without
        // spending a call to find out.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    Ok(format!("{} entries\n{}", names.len(), names.join("\n")))
}

/// What a run resolved about programs, once, before its first step (0.79.0).
///
/// Compiled in every build although only the `codeact` feature ever constructs
/// one. A parameter that appeared and disappeared with a feature flag would put a
/// `#[cfg]` on every argument of every call site of a twenty-nine-parameter
/// function, three times over — and the type is four fields with no dependency on
/// anything the feature gates.
#[cfg_attr(not(feature = "codeact"), allow(dead_code))]
pub(crate) struct CodeActReady {
    /// The interpreter discovery resolved and version-probed.
    pub(crate) interpreter: std::path::PathBuf,
    /// The tools this run offers that a program may call, already filtered by
    /// [`CODEACT_UNCALLABLE`](crate::codeact::CODEACT_UNCALLABLE) and by whether
    /// the name can be a Python binding at all.
    pub(crate) callable: Vec<String>,
    /// How many callbacks one program may make.
    pub(crate) max_callbacks: usize,
    /// How long one program may run.
    pub(crate) timeout: std::time::Duration,
}

/// How a program ended, and the only place the endings are worded (0.79.0).
#[cfg(feature = "codeact")]
enum Finish {
    /// It ran to the end. The string is everything it printed.
    Done(String),
    /// It raised. The strings are the traceback and whatever it printed first.
    Failed(String, String),
    /// It made as many calls as it is allowed.
    Bound,
    /// It ran longer than it is allowed.
    Timeout,
    /// The shim stopped speaking, or said something this crate could not read.
    Broken(String),
}

#[cfg(feature = "codeact")]
impl Finish {
    /// The stable label the observer event carries.
    fn outcome(&self) -> &'static str {
        match self {
            Self::Done(_) => "finished",
            Self::Failed(..) | Self::Broken(_) => "failed",
            Self::Bound => "bound",
            Self::Timeout => "timeout",
        }
    }

    /// One line for the event, never the program's output: the observer channel
    /// carries what happened, and what a program printed is a tool result that
    /// belongs in the ledger.
    fn detail(&self) -> String {
        match self {
            Self::Done(_) => "the program finished".to_string(),
            Self::Failed(message, _) => message.lines().last().unwrap_or("raised").to_string(),
            Self::Bound => "the callback bound was reached".to_string(),
            Self::Timeout => "the program ran out of time".to_string(),
            Self::Broken(why) => why.clone(),
        }
    }

    /// What the model reads. Every ending says what happened and what to do next,
    /// because "the program stopped" and "write a smaller program" are different
    /// instructions and an outcome label alone is neither.
    fn report(
        self,
        calls: u32,
        cap: usize,
        max_callbacks: usize,
        timeout: std::time::Duration,
    ) -> (String, String) {
        match self {
            Self::Done(output) if output.trim().is_empty() => (
                format!("program finished, {calls} calls, no output"),
                format!(
                    "\n[program finished] It made {calls} tool calls and printed nothing. Print \
                     what you need to read back, or set a variable named `result`.\n"
                ),
            ),
            Self::Done(output) => (
                format!("program finished, {calls} calls"),
                crate::tools::cap_result(
                    format!(
                        "\n[program finished] {calls} tool calls. What it printed:\n{}\n",
                        output.trim_end()
                    ),
                    cap,
                )
                .0,
            ),
            Self::Failed(message, output) => (
                format!("program raised after {calls} calls"),
                crate::tools::cap_result(
                    format!(
                        "\n[program raised] After {calls} tool calls. Anything it printed first \
                         follows the traceback; you may send a corrected program.\n{}\n{}\n",
                        message.trim_end(),
                        output.trim_end()
                    ),
                    cap,
                )
                .0,
            ),
            Self::Bound => (
                format!("program hit the {max_callbacks}-call bound"),
                format!(
                    "\n[program stopped] It reached the limit of {max_callbacks} tool calls this \
                     run allows one program, after {calls}. Nothing it printed was captured. Do \
                     less in one program, or call the tools directly.\n"
                ),
            ),
            Self::Timeout => (
                format!("program timed out after {}s", timeout.as_secs()),
                format!(
                    "\n[program stopped] It ran longer than the {}s this run allows one program \
                     and was killed after {calls} tool calls. Nothing it printed was captured. \
                     Write a narrower program.\n",
                    timeout.as_secs()
                ),
            ),
            Self::Broken(why) => (
                "program ended unexpectedly".to_string(),
                format!(
                    "\n[program stopped] {why}. It made {calls} tool calls before that, and \
                     anything those calls did has already happened.\n"
                ),
            ),
        }
    }
}

/// observations the agent can recover from rather than failing the run — only
/// the model can decide what to do about them.
#[allow(clippy::too_many_arguments)]
// `pending_media` is `()` without the feature, and nothing reads it there.
#[cfg_attr(not(feature = "media"), allow(unused_variables))]
pub(crate) async fn dispatch(
    ws: &Workspace,
    call: &ToolCall,
    approver: &dyn Approver,
    // 0.21.0 — who answers a question about intent. Separate from `approver` on
    // purpose: one decides whether an act is permitted, the other what was wanted.
    responder: &dyn Responder,
    store: &Store,
    run_id: i64,
    step: u32,
    mcp: &McpSession,
    lsp: &LspSession,
    browser: &BrowserSession,
    custom: &Toolbox,
    skills: &Skills,
    cap: usize,
    max_read: Option<usize>,
    memory_key: &str,
    // 0.56.0 — the caps this run's contract holds its workspace memory inside,
    // carried beside the key they bound rather than read from a constant, so a
    // `remember` is capped by the operator's numbers whichever door it came
    // through.
    memory_limits: MemoryLimits,
    watch: &Watch<'_>,
    depth: u32,
    pending_media: &mut PendingMedia,
    identity: &crate::tools::git::Identity,
    exec_timeout: Duration,
    // 0.40.0, reshaped in 0.46.0 — the containment this run resolved, or `None`
    // when the contract asked for `ExecMode::FullAccess`. Carried beside
    // `exec_timeout` because they bound the same tool and arrive from the same
    // place; resolved to a backend inside the tool rather than here, so a run that
    // never calls `exec` never probes the host.
    exec_sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
    // The project's ecosystem, detected once by the loop rather than per edit:
    // `toolchain::detect` reads the directory, and an edit is a hot path.
    toolchain: Option<&crate::toolchain::Toolchain>,
    // The run's live process handles. Shared rather than owned by the dispatch
    // because a handle outlives the call that started it — which is the whole
    // point of one, and the reason every guard in `handles` exists.
    handles: &std::sync::Arc<crate::tools::handles::Handles>,
    // 0.31.0 — the plan gate, in one parameter rather than three. `active` was read
    // from the store at this loop's entry, never carried from a previous process.
    plan: PlanPhase<'_>,
    // 0.42.0 — what the run is for. The approval site tells an approver why it is
    // being asked, and the goal is the half of that a `Verdict` cannot carry.
    goal: &str,
    // 0.42.0 — the operator's own `before_tool` checks, or `None`.
    hooks: Option<&crate::hooks::Hooks>,
    // 0.76.0 — the tools this turn withholds. Applied at the head, beside the
    // hook gate, because this is one of the two places a call can begin.
    mask: &crate::ToolMask,
    // 0.79.0. `None` for every caller that is not the workspace loop, and `None`
    // inside a program, which is how a program is kept from starting one.
    codeact: Option<&CodeActReady>,
) -> Result<Dispatched> {
    // The browser session is threaded to every dispatch site unconditionally, so
    // the call sites need no `#[cfg]`; without the feature the arm that reads it
    // is compiled out and the parameter is genuinely unused. Named here rather
    // than silenced with an attribute on the whole function, which would also
    // hide a real unused variable.
    #[cfg(not(feature = "browser"))]
    let _ = browser;

    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    // Announced before the call is made, so a watcher sees what the run is about
    // to do rather than only what it did.
    announce(watch, run_id, step, depth, call);
    // 0.42.0 — the operator's own check, before anything happens. Every call that
    // is not part of a read batch arrives here, and a batched one is checked in
    // `read_batch` instead, so each call is asked about exactly once.
    if let Some(refused) = mask_gate(mask, call, watch, run_id, step, depth) {
        return Ok(refused);
    }
    if let Some(refused) = tool_gate(hooks, call, watch, run_id, step, depth) {
        return Ok(refused);
    }
    let name = call.name.as_str();
    // 0.48.0 — what this call may do is decided here, before the arm that would
    // spawn for it. A tool needing more than this run grants leaves with nothing
    // started, which is what "resolved before execution rather than discovered by
    // a failure" means: no process, and therefore no permission error for the
    // model to interpret.
    let narrowed;
    let exec_sandbox = match resolve_call_mode(name, custom, exec_sandbox) {
        CallMode::Refused { needed } => {
            let granted = exec_sandbox
                .map(|c| c.config.mode)
                .unwrap_or(crate::sandbox::ExecMode::FullAccess);
            return Ok(Dispatched::go(
                format!("{name} refused: needs {}", needed.as_str()),
                format!(
                    "\n[{name} refused] this tool needs the `{}` containment mode and this run \
                     grants `{}`. Nothing was started. Do this another way, or ask for a run that \
                     grants it.\n",
                    needed.as_str(),
                    granted.as_str()
                ),
            ));
        }
        CallMode::Contained(resolved) => {
            // Reported when it differs from the run's own answer, in the shape
            // 0.44.0 used for `CacheMarked`: an event on the transition rather
            // than one per call, so an observer sees the calls that were held to
            // less than the run was granted and is not handed a copy of the run's
            // own containment on every dispatch.
            if let (Some(call_c), Some(run_c)) = (resolved.as_ref(), exec_sandbox) {
                if call_c.config.mode != run_c.config.mode {
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::Contained {
                            mode: call_c.config.mode.as_str().to_string(),
                            backend: call_c.backend().as_str().to_string(),
                            roots: call_c.roots.len() as u32,
                        },
                    ));
                }
            }
            narrowed = resolved;
            narrowed.as_ref()
        }
    };
    #[cfg(not(feature = "codeact"))]
    let _ = codeact;
    Ok(match name {
        // 0.41.0 — the three read-only built-ins go through the same two halves a
        // batched read does: the policy on this thread, then the read itself. One
        // of them run alone is the batch of size one.
        GREP_TOOL | FIND_TOOL | READ_FILE_TOOL => {
            match prepare_read(
                ws,
                call,
                approver,
                store,
                run_id,
                step,
                custom,
                watch,
                depth,
                goal,
                exec_sandbox,
            )
            .await?
            {
                Prepared::Work(work) => work.run(ws, cap, max_read, run_id, step).await,
                Prepared::Done(done) | Prepared::Stop(done) => done,
            }
        }
        // 0.79.0 — the one arm that calls this function again.
        //
        // Everything the program does arrives here as an ordinary `ToolCall` and
        // goes back through `dispatch` with the arguments this arm already holds,
        // so the policy, the gate, the `policy_events` row, the journal attempt
        // and the observer see a program's act on exactly the terms they see a
        // model's. There is deliberately no shorter path: a purpose-built one
        // would compile more easily and pass every test in this release while
        // bypassing the gate, which is the defect this shape exists to make
        // impossible rather than to test for.
        #[cfg(feature = "codeact")]
        RUN_PROGRAM_TOOL => {
            let Some(ready) = codeact else {
                // Reachable only if a model asks for a tool it was never offered.
                return Ok(Dispatched::go(
                    "run_program unavailable",
                    "\n[run_program unavailable] This run found no usable Python interpreter, so \
                     it cannot run a program. Use the individual tools instead.\n",
                ));
            };
            let Some(source) = a.get("source").and_then(|v| v.as_str()) else {
                return Ok(Dispatched::go(
                    "run_program missing source",
                    "\n[run_program error] run_program needs a \"source\" string holding the whole \
                     program\n",
                ));
            };
            // `PlanPhase` is three borrows rather than a value, and the recursive
            // call takes it by value — so it is taken apart once here and rebuilt
            // per callback, which is cheaper and clearer than making the type
            // `Copy` to hide that it is moved.
            let PlanPhase {
                gate: plan_gate,
                agents: plan_agents,
                active: plan_active,
            } = plan;

            // The plan gate is a policy *layer* denying `Write` and `Exec`, and it
            // works because every mutating path in this crate is one of those two
            // checks. Starting an interpreter is a third, so a run held still
            // waiting for an approved plan would have started programs while every
            // act inside them was denied — the tool is refused here for the same
            // reason `remember` is, rather than being filtered out of the
            // catalogue, because this crate denies tools and never hides them.
            if plan_active {
                return Ok(Dispatched::go(
                    "run_program refused: plan not approved",
                    "\n[run_program refused] This run is waiting for its plan to be approved, so \
                     it will not start a program. Propose a plan first.\n",
                ));
            }

            // **The interpreter is a program on this machine, and starting it is
            // an act.** Without this the tool was a second path around
            // `Act::Exec`: a run whose policy said `deny_exec("*")` still handed
            // arbitrary model-authored source to a host interpreter, and every
            // test still passed because they all assert on what a *callback*
            // reached. Both spellings are checked, exactly as `exec` checks them —
            // the program alone is what `deny_exec("python3")` names, and the
            // whole argv is what a narrower `allow_exec` names.
            let interpreter = ready.interpreter.display().to_string();
            let joined = format!("{interpreter} {}", crate::codeact::PROGRAM_FILE);
            for target in [interpreter.clone(), joined.clone()] {
                match gate(
                    ws,
                    approver,
                    store,
                    run_id,
                    step,
                    Act::Exec,
                    &target,
                    None,
                    watch,
                    depth,
                    goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                    Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                    Gated::Go { .. } => {}
                }
            }

            // Refused rather than degraded where this host's seam cannot apply the
            // containment the run asked for. 0.74.0's reasoning, and `shell_start`
            // already refuses for the narrower case: a boundary named in the trace
            // and not applied to the process is worse than no boundary at all.
            if let Some(why) = crate::codeact::containment_refusal(exec_sandbox) {
                return Ok(Dispatched::go(
                    "run_program refused: cannot be contained here",
                    format!("\n[run_program refused] {why}\n"),
                ));
            }

            let mut session = match crate::codeact::Session::start(
                &ready.interpreter,
                source,
                &ready.callable,
                ready.max_callbacks,
                exec_sandbox,
            )
            .await
            {
                Ok(session) => session,
                // An environmental failure — the interpreter deleted between
                // discovery and use, a temporary directory that cannot be made —
                // is an observation the agent can carry on from, which is what
                // this function's contract says it returns. Failing the whole run
                // for it would be this arm deciding the run is over.
                Err(err) => {
                    return Ok(Dispatched::go(
                        "run_program could not start",
                        format!(
                            "\n[run_program could not start] {err}. Nothing ran. Use the \
                             individual tools instead.\n"
                        ),
                    ))
                }
            };

            // Deferring is not available to an act inside a program, and the
            // wrapper is where that is decided rather than in the loop below: a
            // `Pause` returned mid-program would leave this arm before the
            // `changed` and `remember` it has accumulated were reported, and a
            // resumed run re-writes the program from scratch and re-executes the
            // acts that already landed. The caller's approver is untouched for
            // everything the model does itself.
            let approver: &dyn Approver = &crate::codeact::NoDefer(approver);

            let mut changed = false;
            let mut remembered: Vec<Rule> = Vec::new();
            let started = std::time::Instant::now();

            let mut fatal: Option<Error> = None;
            let finish = loop {
                // The wait is bounded rather than the loop, and that is the whole
                // difference between a ceiling and a hang. A program that spins
                // without ever calling back produces no frame to check a deadline
                // between, and nothing else would stop it: `SandboxLimits` is
                // `none()` on a default `TaskContract`, so there is no wall cap
                // underneath this, and an uncontained run has no rlimits at all.
                let remaining = ready.timeout.saturating_sub(started.elapsed());
                let frame = match tokio::time::timeout(remaining, session.next()).await {
                    Err(_) => break Finish::Timeout,
                    Ok(Ok(frame)) => frame,
                    Ok(Err(err)) => break Finish::Broken(err.to_string()),
                };
                let (name, args) = match frame {
                    crate::codeact::Frame::Done { output } => break Finish::Done(output),
                    crate::codeact::Frame::Failed { message, output } => {
                        break Finish::Failed(message, output)
                    }
                    crate::codeact::Frame::Call { name, args } => (name, args),
                };
                // Asked here rather than at the top of the loop, because the two
                // readings differ for a program that makes exactly its allowance
                // and then finishes: checking first meant its terminal frame was
                // never read, its output was thrown away, and the model was told
                // to do less by a run that had done exactly what it was allowed.
                if session.at_bound() {
                    break Finish::Bound;
                }
                session.count_call();
                // The exclusions are checked here as well as being absent from the
                // generated module, because the module is a convenience and this
                // is the boundary: a program that builds a call by hand must meet
                // the same list.
                if crate::codeact::CODEACT_UNCALLABLE.contains(&name.as_str()) {
                    if let Err(err) = session
                        .reply(
                            false,
                            &format!(
                                "[not callable from a program] {name} is not available inside a \
                                 program. Finish the program and call it directly."
                            ),
                        )
                        .await
                    {
                        break Finish::Broken(err.to_string());
                    }
                    continue;
                }
                let nested = ToolCall {
                    name,
                    arguments: args,
                };
                let dispatched = Box::pin(dispatch(
                    ws,
                    &nested,
                    approver,
                    responder,
                    store,
                    run_id,
                    step,
                    mcp,
                    lsp,
                    browser,
                    custom,
                    skills,
                    cap,
                    max_read,
                    memory_key,
                    memory_limits,
                    watch,
                    depth,
                    pending_media,
                    identity,
                    exec_timeout,
                    exec_sandbox,
                    toolchain,
                    handles,
                    PlanPhase {
                        gate: plan_gate,
                        agents: plan_agents,
                        active: plan_active,
                    },
                    goal,
                    hooks,
                    mask,
                    // A program may not start a program. It is on the uncallable
                    // list and refused above; `None` here is the second half of
                    // that, so a path that reached this call some other way still
                    // cannot recurse a level deeper.
                    None,
                ));
                // Not `?`. An early return here would leave the interpreter
                // running with its tree unwalked, remove the workdir underneath
                // it, and emit no `Program` event for a program that had already
                // taken gated acts. The error is carried out of the loop instead,
                // so the teardown below runs and the run still fails.
                let dispatched = match dispatched.await {
                    Ok(dispatched) => dispatched,
                    Err(err) => {
                        fatal = Some(err);
                        break Finish::Broken("the run failed while the program was open".into());
                    }
                };
                match dispatched {
                    Dispatched::Continue {
                        obs,
                        kind,
                        changed: did,
                        remember,
                        ..
                    } => {
                        changed |= did;
                        remembered.extend(remember);
                        // `ObsKind::Error` is what `Dispatched::go` marks a
                        // refusal and a tool's own failure with, and it is the
                        // only structural signal either one carries. So `.ok` in
                        // the program is that, rather than this crate reading its
                        // own refusal text back out of a string it just wrote.
                        let allowed = kind != ObsKind::Error;
                        if let Err(err) = session.reply(allowed, obs.trim()).await {
                            break Finish::Broken(err.to_string());
                        }
                    }
                    // Unreachable by construction, and handled rather than
                    // asserted. `Pause` cannot arrive because the approver above
                    // turns a deferral into a denial for a program's acts, and
                    // `Ask` and `Plan` come only from tools on the uncallable
                    // list. If one ever did, answering it here would be this arm
                    // deciding something the caller was asked — so the program is
                    // told the act needs a decision, and the loop carries on with
                    // everything it has accumulated intact.
                    _ => {
                        if let Err(err) = session
                            .reply(
                                false,
                                "[needs a decision] that act needs an approval that cannot be \
                                 taken while a program is running. Finish the program and call it \
                                 directly.",
                            )
                            .await
                        {
                            break Finish::Broken(err.to_string());
                        }
                    }
                }
            };

            let calls = session.calls() as u32;
            session.stop().await;

            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::Program {
                    interpreter: Some(ready.interpreter.display().to_string()),
                    detail: finish.detail(),
                    calls,
                    outcome: finish.outcome().to_string(),
                },
            ));

            // Raised only after the child is dead, the workdir is gone and the
            // event is on the record, so a failing run still leaves a readable
            // account of the program that was open when it failed.
            if let Some(err) = fatal {
                return Err(err);
            }

            let (decision, obs) = finish.report(calls, cap, ready.max_callbacks, ready.timeout);
            Dispatched::Continue {
                decision,
                obs,
                kind: ObsKind::Tool,
                target: None,
                origin: crate::context::Origin::Tool,
                changed,
                remember: remembered,
            }
        }
        REMEMBER_TOOL => {
            // 0.31.0 — the one write that does not pass through the policy, because
            // it lands in the harness's own store rather than in the workspace. The
            // `plan-gate` layer therefore cannot cover it and it is refused here, so
            // "nothing is written before the approval" means nothing at all rather
            // than nothing the policy happens to see.
            if plan.active {
                return Ok(Dispatched::go(
                    "remember refused (planning)",
                    format!(
                        "\n[remember refused] the plan has not been approved yet, so nothing \
                         is being written — including notes. Call `{PROPOSE_PLAN_TOOL}` \
                         first.\n"
                    ),
                ));
            }
            let key = s("key").unwrap_or_default();
            let value = s("value").unwrap_or_default();
            if key.is_empty() || value.is_empty() {
                return Ok(Dispatched::go(
                    "remember error",
                    "\n[remember error] both key and value are required\n",
                ));
            }
            let scope = match memory_scope(s("scope"), memory_key) {
                Ok(scope) => scope,
                Err(refusal) => return Ok(refusal),
            };
            // The store bounds the entry and evicts oldest-first to hold the caps;
            // it writes no trace rows of its own, so the write and every eviction
            // are recorded here, where the run_id and step are known.
            // 0.30.0: what a run writes is a fact — a decision is somebody's, and
            // a harness inferring one from a tool call would be guessing at
            // intent. A pinned entry refuses the write, and the refusal is
            // recorded and handed back to the model: an agent that believes it
            // corrected something and did not will act on the correction it
            // thinks it made.
            // 0.57.0 — asked BEFORE the write, and it has to be: afterwards the
            // new entry is itself in the scope, and an entry restating itself is
            // the one answer that is never useful.
            let restates = store.memory_similar(scope, key, value)?;
            let wrote = store.memory_write_with(
                scope,
                key,
                value,
                run_id,
                step,
                MemoryKind::Fact,
                memory_limits,
            )?;
            if wrote.refused {
                store.record_context_event(
                    run_id,
                    &ContextEvent::memory_refused(
                        step,
                        format!("{key} (pinned; the earlier value stands)"),
                    ),
                )?;
                info!(run_id, step, key, "remember refused: pinned");
                return Ok(Dispatched::go(
                    format!("remember refused {key}"),
                    format!(
                        "\n[remember refused] `{key}` is pinned by the operator and was not \
                         overwritten. The existing note stands.\n"
                    ),
                ));
            }
            let evicted = wrote.evicted;
            store.record_context_event(
                run_id,
                &ContextEvent::memory_write(
                    step,
                    format!("{key} ({} chars)", value.chars().count()),
                ),
            )?;
            // The key only: the row's detail carries a character count too, but
            // that is prose about the write, not the note's identity.
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::MemoryWrote {
                    key: key.to_string(),
                },
            ));
            for gone in &evicted {
                store.record_context_event(
                    run_id,
                    &ContextEvent::memory_evict(step, format!("{gone} (evicted to hold the cap)")),
                )?;
            }
            info!(run_id, step, key, evicted = evicted.len(), "remembered");
            // 0.57.0 — the write landed, and the model is told what it now holds
            // twice. Reported rather than refused: a harness that declined a
            // write because two strings overlapped would be guessing at intent,
            // and one that merged them would be writing a fact nobody stated.
            // Resolving it is the model's, in this turn, with `remember` or
            // `forget` — which is the whole reason to say it here rather than
            // leave it for a later run to trip over.
            //
            // The held value is quoted, because "you already know this" without
            // saying what is known is a line a model can only act on by reading
            // the store it cannot read. Bounded, because a note may be two
            // thousand characters and this text is charged to the turn.
            let restated = match &restates {
                None => String::new(),
                Some(entry) => format!(
                    "\n[remember: this restates `{}`, which holds: \"{}\"] Two notes saying \
                     the same thing are both carried and the model acts on whichever it read \
                     last. Replace one, or forget the other.\n",
                    entry.key,
                    crate::state::truncate_memory_value(&entry.value, 200),
                ),
            };
            // No target: two notes under one key are the store's business, and a
            // remember is not an observation OF anything that could go stale.
            Dispatched::seen(
                format!("remembered {key}"),
                format!("\n[remember {key}]\n{restated}"),
                ObsKind::Tool,
                None,
                // The harness restating what it just stored. No byte here came
                // from outside — `remember` writes to this crate's own memory —
                // so it is prose, and holding the call's position is `Piece`'s
                // separate business.
                Origin::Prose,
            )
        }
        FORGET_TOOL => {
            // 0.56.0 — the counterpart to `remember`, and refused in the same
            // place for the same reason: it writes into the harness's own store,
            // so the `plan-gate` layer cannot cover it and "nothing is written
            // before the approval" has to include a withdrawal.
            if plan.active {
                return Ok(Dispatched::go(
                    "forget refused (planning)",
                    format!(
                        "\n[forget refused] the plan has not been approved yet, so nothing \
                         is being changed — including notes. Call `{PROPOSE_PLAN_TOOL}` \
                         first.\n"
                    ),
                ));
            }
            let key = s("key").unwrap_or_default();
            if key.is_empty() {
                return Ok(Dispatched::go(
                    "forget error",
                    "\n[forget error] key is required\n",
                ));
            }
            let scope = match memory_scope(s("scope"), memory_key) {
                Ok(scope) => scope,
                Err(refusal) => return Ok(refusal),
            };
            match store.memory_forget(scope, key, run_id, step)? {
                MemoryForget::Pinned => {
                    store.record_context_event(
                        run_id,
                        &ContextEvent::memory_refused(
                            step,
                            format!("{key} (pinned; not withdrawn)"),
                        ),
                    )?;
                    info!(run_id, step, key, "forget refused: pinned");
                    Dispatched::go(
                        format!("forget refused {key}"),
                        format!(
                            "\n[forget refused] `{key}` is pinned by the operator and was not \
                             removed. The existing note stands.\n"
                        ),
                    )
                }
                // Not an error, and not reported as a removal either: a model
                // told it withdrew something it never wrote will believe a
                // correction happened.
                // Deliberately NOT the `[forget {key}]` prefix a real removal
                // wears. A model skimming the head of an observation would read
                // the two as the same outcome, which is the failure the three
                // answers exist to prevent — found by a sabotage that reported
                // success here and survived a test asserting only on the trace.
                MemoryForget::Absent => Dispatched::go(
                    format!("forget {key} (nothing to forget)"),
                    format!(
                        "\n[forget: nothing to forget] there was no note `{key}` over this \
                         workspace, so nothing was removed.\n"
                    ),
                ),
                MemoryForget::Removed => {
                    store.record_context_event(
                        run_id,
                        &ContextEvent::memory_forget(step, format!("{key} (withdrawn by the run)")),
                    )?;
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::MemoryForgot {
                            key: key.to_string(),
                        },
                    ));
                    info!(run_id, step, key, "forgot");
                    Dispatched::seen(
                        format!("forgot {key}"),
                        format!("\n[forget {key}]\n"),
                        ObsKind::Tool,
                        None,
                        // Harness prose, for `remember`'s reason.
                        Origin::Prose,
                    )
                }
            }
        }
        TODO_WRITE_TOOL => {
            // Not gated, and deliberately so: this writes into the harness's own
            // store, not into the workspace, the network or a binary, so there is no
            // `Act` it could be checked against. Inventing one would put a permission
            // rule in front of the agent stating its intentions. See the plan section
            // of `docs/CONTRACT.md`.
            let items = match parse_todo_items(a) {
                Ok(items) => items,
                Err(why) => {
                    return Ok(Dispatched::go(
                        "todo error",
                        format!("\n[todo error] {why}\n"),
                    ))
                }
            };
            // The store caps the list and reports what it dropped; it writes no trace
            // row of its own, so both are recorded here, where the step is known.
            let dropped = store.write_todos(run_id, &items)?;
            let done = items
                .iter()
                .filter(|i| i.state == crate::state::TodoState::Done)
                .count();
            store.record_context_event(
                run_id,
                &ContextEvent::todo_write(step, format!("{} items, {done} done", items.len())),
            )?;
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::TodoWrote {
                    items: items.clone(),
                },
            ));
            info!(run_id, step, items = items.len(), done, dropped, "plan");
            // The plan back in one line, so the model sees what was actually recorded
            // rather than assuming its write landed verbatim — the cap is visible.
            let mut obs = format!("\n[plan {} items, {done} done]\n", items.len());
            for item in &items {
                obs.push_str(&format!("- [{}] {}\n", item.state.as_str(), item.text));
            }
            if dropped > 0 {
                obs.push_str(&format!(
                    "({dropped} item(s) past the {} the plan holds were dropped)\n",
                    crate::state::TODO_MAX_ITEMS
                ));
            }
            // No target: a plan supersedes nothing and cannot go stale — the next
            // write replaces it wholesale.
            Dispatched::seen(
                format!("plan: {} items, {done} done", items.len()),
                obs,
                ObsKind::Tool,
                None,
                // The model's own plan, read back by the harness. Not external
                // and not the operator; prose, for `remember`'s reason.
                Origin::Prose,
            )
        }
        ASK_QUESTION_TOOL => {
            // Not gated, and for a sharper reason than the todo tool's: this asks a
            // human something. Putting a permission rule in front of the channel
            // whose whole purpose is to ask would be a category error, and there is
            // no `Act` that means "ask about intent" — see `docs/CONTRACT.md`.
            let question = match parse_one_question(a) {
                Ok(q) => q,
                Err(why) => {
                    return Ok(Dispatched::go(
                        "question error",
                        format!("\n[question error] {why}\n"),
                    ))
                }
            };
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::QuestionAsked {
                    question: question.question.clone(),
                    // Labels, because this variant has carried labels since 0.21.0 and
                    // changing its field type would break every observer matching on it
                    // for a fact `QuestionsAsked` already carries in full.
                    choices: question.choices.iter().map(|c| c.label.clone()).collect(),
                },
            ));
            store.record_context_event(
                run_id,
                &ContextEvent::question_asked(step, question.question.clone()),
            )?;

            // 0.33.0 — persisted BEFORE the responder is consulted, so a run
            // blocked in a `Responder` that nobody is sitting in front of can be
            // answered by a second process instead of killed.
            //
            // A resume still delivers its answer as an observation rather than
            // through here: the step that asks is committed before the run pauses,
            // so a resume starts at the step *after* it and this call is never
            // replayed. That is unchanged. What is new is the run that never
            // paused, because it is still sitting in `answer`.
            let question_id = store.put_question(run_id, step, &question)?;
            let raced = race_gate(responder.answer(&question), store, |s| {
                Ok(s.question(question_id)?.is_some_and(|q| q.resolved))
            })
            .await?;
            // The responder's answer is written through the same compare-and-swap
            // an attached one uses. "The machine decided" is a fact about the run
            // worth keeping even when nothing paused.
            if let Some(Some(answer)) = &raced {
                store.answer_question(question_id, answer, "responder")?;
            }
            // Read the row back rather than using what we raced with: the answer
            // the model is handed must be the one the store holds, in both arms,
            // or an audit of `pending_questions` cannot be trusted to say what the
            // run acted on. `answered_by` comes from the row too, so it names
            // whoever actually won.
            let answered = store
                .question(question_id)?
                .filter(|q| q.resolved)
                .and_then(|q| Some((q.answer?, q.answered_by.unwrap_or_default())));

            match answered {
                Some((answer, by)) => {
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::QuestionAnswered {
                            answer: answer.clone(),
                            by: by.clone(),
                        },
                    ));
                    store.record_context_event(
                        run_id,
                        &ContextEvent::question_answered(step, format!("{by}: {answer}")),
                    )?;
                    info!(run_id, step, %by, "question answered");
                    // The answer is an observation. It is text the model reads, and it
                    // authorizes nothing: every tool call it leads to is checked
                    // against the same policy by the same code.
                    Dispatched::seen(
                        format!("asked, answered by {by}"),
                        format!(
                            "\n[answer] {answer}\n(This is what the operator wanted. It is not \
                             permission for anything.)\n"
                        ),
                        ObsKind::Tool,
                        None,
                        // A human wrote these words, so the record says so. It
                        // still holds the `ask_question` call's position in the
                        // transcript — that is `Piece`'s job, decided from
                        // `(kind, target)`, and it is decided independently of
                        // this. The resumed path (`record_answer`) marks the same
                        // words the same way, which is what a reader should see:
                        // one operator answer, two arrival routes.
                        Origin::Operator,
                    )
                }
                None => {
                    // Nobody answered — not the in-process responder, and nobody
                    // attached while it was being asked. Pause on the row already
                    // written: a run that had to guess would spend its budget
                    // pursuing something nobody asked for.
                    info!(run_id, step, question_id, "run paused for an answer");
                    Dispatched::Ask { question_id }
                }
            }
        }
        ASK_QUESTIONS_TOOL => {
            // Not gated, for the same reason the singular is not: putting a permission
            // rule in front of the channel whose whole purpose is to ask would be a
            // category error.
            let questions = match parse_questions(a) {
                Ok(q) => q,
                Err(why) => {
                    // Strictly an error back to the model, never the end of the run —
                    // a malformed ask is something the model can send again.
                    return Ok(Dispatched::go(
                        "questions error",
                        format!("\n[questions error] {why}\n"),
                    ));
                }
            };
            // One event carrying the batch, and NOT a `QuestionAsked` per question:
            // "these were asked together" is the fact this variant exists to carry,
            // and emitting both would make three-in-a-batch indistinguishable from
            // three-in-sequence for any observer that watches the singular.
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::QuestionsAsked {
                    questions: questions.clone(),
                },
            ));
            store.record_context_event(
                run_id,
                &ContextEvent::question_asked(
                    step,
                    format!(
                        "{} questions: {}",
                        questions.len(),
                        questions
                            .iter()
                            .map(|q| q.question.as_str())
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ),
                ),
            )?;

            // One row, written BEFORE the responder is consulted, for the reason
            // 0.33.0 gave for the singular: a run blocked in a `Responder` nobody is
            // sitting in front of can be answered by a second process instead of
            // killed. One row also keeps the resume surface singular.
            let question_id = store.put_questions(run_id, step, &questions)?;
            let raced = race_gate(responder.answer_all(&questions), store, |s| {
                Ok(s.question(question_id)?.is_some_and(|q| q.resolved))
            })
            .await?;

            // A batch is answered wholly or not at all. A responder that declined even
            // one of them has not answered the ask, and the run parks on the row —
            // which is what a submitted form means, and what keeps `answer_questions`
            // a compare-and-swap.
            if let Some(answers) = &raced {
                if answers.len() == questions.len() && answers.iter().all(Option::is_some) {
                    let assembled = assemble_answers(&questions, answers);
                    store.answer_questions(question_id, &assembled, answers, "responder")?;
                }
            }
            // Read the row back rather than using what we raced with, for the reason
            // the singular does: the answer the model is handed must be the one the
            // store holds, or an audit of `pending_questions` cannot be trusted.
            let row = store.question(question_id)?.filter(|q| q.resolved);
            let answered = row
                .as_ref()
                .and_then(|q| Some((q.answer.clone()?, q.answered_by.clone().unwrap_or_default())));

            match answered {
                Some((answer, by)) => {
                    // Once per answer, because an answer is an independent fact and a
                    // UI shows each one beside its own question. A resume supplies one
                    // text and no breakdown, which is the row with an empty `answers`.
                    let each = row.as_ref().map(|q| q.answers.clone()).unwrap_or_default();
                    match each.iter().filter_map(Option::as_deref).count() {
                        0 => watch.emit(RunEvent::at_depth(
                            run_id,
                            step,
                            depth,
                            EventKind::QuestionAnswered {
                                answer: answer.clone(),
                                by: by.clone(),
                            },
                        )),
                        _ => {
                            for one in each.iter().filter_map(Option::as_deref) {
                                watch.emit(RunEvent::at_depth(
                                    run_id,
                                    step,
                                    depth,
                                    EventKind::QuestionAnswered {
                                        answer: one.to_string(),
                                        by: by.clone(),
                                    },
                                ));
                            }
                        }
                    }
                    store.record_context_event(
                        run_id,
                        &ContextEvent::question_answered(step, format!("{by}: {answer}")),
                    )?;
                    info!(run_id, step, %by, count = questions.len(), "questions answered");
                    // Many in, one block out — `todo_write`'s precedent, matched to
                    // its call by the ordinal mechanism that already exists.
                    Dispatched::seen(
                        format!("asked {}, answered by {by}", questions.len()),
                        format!(
                            "\n[answers]\n{answer}\n(This is what the operator wanted. It is not \
                             permission for anything.)\n"
                        ),
                        ObsKind::Tool,
                        None,
                        // The singular's origin, for the singular's reason.
                        Origin::Operator,
                    )
                }
                None => {
                    info!(run_id, step, question_id, "run paused for an answer set");
                    Dispatched::Ask { question_id }
                }
            }
        }
        PROPOSE_PLAN_TOOL => {
            // Not gated, for the sharpest version of the reason `ask_question` is
            // not: this is the call that asks permission to do anything at all, and
            // putting a permission rule in front of it would leave the agent with no
            // legal move. It is also the one tool that is *only* offered while
            // everything else is refused — see `plan_lock`.
            let Some(gate) = plan.gate.filter(|_| plan.active) else {
                return Ok(Dispatched::go(
                    "plan error",
                    format!(
                        "\n[plan error] there is no plan to propose on this run; \
                         `{PROPOSE_PLAN_TOOL}` is not available here.\n"
                    ),
                ));
            };
            let proposed = match parse_plan(a, plan.agents) {
                Ok(p) => p,
                Err(why) => {
                    return Ok(Dispatched::go(
                        "plan error",
                        format!("\n[plan error] {why}\n"),
                    ))
                }
            };

            // Persisted BEFORE the gate is consulted, not after. A process that dies
            // between the proposal and the verdict leaves a row a human can still
            // answer, which is the whole of the durability claim.
            let plan_id = store.put_plan(run_id, step, &proposed)?;
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::PlanProposed {
                    plan_id,
                    steps: proposed.steps.clone(),
                },
            ));
            store.record_context_event(
                run_id,
                &ContextEvent::plan_proposed(step, format!("{} steps", proposed.steps.len())),
            )?;

            // Only the in-process gate is consulted here. A human's verdict does NOT
            // arrive through this path, for the reason an answer does not: the step
            // that proposes is committed before the run pauses, so a resume starts
            // after it and this call is never replayed.
            // `resume_with_plan_decision` delivers the verdict as an observation.
            // 0.33.0 — the row was already written first, so this only had to gain
            // the race: a run held in a `PlanGate` nobody is watching is now
            // answerable by a second process, the third of the three things a live
            // run can be holding.
            let raced = race_gate(gate.review(&proposed), store, |s| {
                Ok(s.plan(plan_id)?.is_some_and(|p| p.resolved))
            })
            .await?;
            if let Some(Some(v)) = &raced {
                store.decide_plan(plan_id, v, "gate")?;
            }
            // The verdict the run acts on is the row's, in both arms — including
            // `decided_by`, so the event names whoever actually won rather than
            // assuming it was the gate.
            let decided_row = store.plan(plan_id)?.filter(|p| p.resolved);
            let by = decided_row
                .as_ref()
                .and_then(|p| p.decided_by.clone())
                .unwrap_or_default();
            let verdict = decided_row.and_then(|p| p.verdict);
            if let Some(v) = &verdict {
                watch.emit(RunEvent::at_depth(
                    run_id,
                    step,
                    depth,
                    EventKind::PlanDecided {
                        plan_id,
                        verdict: v.as_str().to_string(),
                        by: by.clone(),
                    },
                ));
                store.record_context_event(
                    run_id,
                    &ContextEvent::plan_decided(step, format!("{by}: {}", v.as_str())),
                )?;
            }
            info!(
                run_id,
                step,
                plan_id,
                verdict = verdict
                    .as_ref()
                    .map(PlanVerdict::as_str)
                    .unwrap_or("pending"),
                "plan proposed"
            );

            match verdict {
                // A correction is text the model reads and re-plans from. The run
                // stays in its planning phase and still writes nothing, so this is an
                // ordinary observation rather than a control-flow event.
                // 0.80.0 (issue #246) — `go`, not `seen`, because this answers
                // the `propose_plan` call that produced it.
                //
                // It was `ObsKind::Message` with no target, which `Piece::of`
                // reads as `Piece::Prose` — so the step emitted a `tool_use` for
                // `propose_plan` with no `tool_result` to match it. A step
                // carrying `propose_plan` beside any refused call therefore went
                // out malformed: `provider/anthropic.rs` drops an uncorrelated
                // `tool_result` and has nothing that drops an orphaned
                // `tool_use`.
                //
                // A plan sent back for revision *is* a refusal of that call, and
                // `Dispatched::go` is how every other refusal in this file
                // answers one — same kind, same absent target, same origin, and
                // `Piece::Result` because of the kind rather than the origin.
                Some(PlanVerdict::Revise { correction }) => Dispatched::go(
                    "plan sent back",
                    format!(
                        "\n[plan not approved] {correction}\n(Propose a different plan with \
                         `{PROPOSE_PLAN_TOOL}`. Nothing has been done yet and nothing will be \
                         until a plan is approved.)\n"
                    ),
                ),
                other => Dispatched::Plan {
                    plan_id,
                    verdict: other,
                },
            }
        }
        LIST_DIR_TOOL => {
            // No path means the workspace root, which is the listing an agent
            // opening an unfamiliar repository wants first. `resolve` turns the
            // empty string into the root and the policy sees it as such, so this
            // is a default rather than a special case.
            let path = s("path").unwrap_or_default();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                // The same act, the same check, the same code as a `read_file` on
                // this path: enumerating a directory the operator denied reading
                // is that read, done one level up.
                Act::Read,
                path,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match ws.list_dir(&target) {
                    Ok(entries) => {
                        let shown: Vec<String> = entries
                            .iter()
                            .take(OBS_LIST_DIR_CAP)
                            .map(Entry::to_string)
                            .collect();
                        // Said in the listing, not only in the count the trace
                        // keeps: the model reads the text and nothing else, and
                        // what it does about a truncated directory — narrow to a
                        // subdirectory, or glob it with `find` — is a decision it
                        // can only make if it is told.
                        let elided = entries.len() - shown.len();
                        let note = match elided {
                            0 => String::new(),
                            n => format!(
                                "\n[showing {} of {} entries; {n} not listed — list a \
                                 subdirectory or use find to narrow]",
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
                            // A listing is a filename answer about a path, like
                            // `find`'s: a later listing of the same directory is
                            // the same question asked again and supersedes this
                            // one. It is not its own kind because nothing in the
                            // context layer would treat it differently.
                            kind: ObsKind::Find,
                            target: Some(target.clone()),
                            origin: Origin::File,
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => Dispatched::go("list_dir error", format!("\n[list_dir error] {e}\n")),
                },
            }
        }
        #[cfg(feature = "media")]
        VIEW_IMAGE_TOOL => {
            let path = s("path").unwrap_or_default();
            // The extension decides the source media type, and an unknown one is
            // reported rather than guessed. Checked before the gate only because
            // it costs nothing and reads nothing: the gate still runs for every
            // path that could actually be read, so this cannot be used to probe
            // for a file's existence outside the policy — and 0.55.0's decode,
            // which does look at bytes, is inside the gated branch below.
            //
            // 0.55.0 widens this from the four wire types to every format the
            // crate recognises. A format it cannot decode is refused by name
            // further down, by `Media::attach`, rather than here with a list of
            // four types that is a fact about vendors instead of about the file.
            let Some(media_type) = crate::provider::Media::source_type_for(path) else {
                return Ok(Dispatched::go(
                    "view_image unsupported type",
                    format!(
                        "\n[view_image error] {path} is not an image this crate recognises. \
                         It sends {}, and converts BMP, TIFF, ICO, TGA and PNM to PNG on the \
                         way.\n",
                        crate::provider::IMAGE_MEDIA_TYPES.join(", ")
                    ),
                ));
            };
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
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match ws
                    .read_bytes(&target)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        crate::provider::Media::attach(media_type, &bytes)
                            .map_err(|e| e.to_string())
                    }) {
                    Ok(media) => {
                        // The observation records what was sent, not the image:
                        // a digest, a size and a type. A trace that held the
                        // bytes would grow by megabytes a step in exactly the
                        // long unattended runs this crate exists for.
                        let obs = format!(
                            "\n[view_image {target}] attached to the next request \
                             ({}, {} bytes, digest {})\n",
                            // 0.55.0 — a transcode says so, so a trace shows that
                            // the bytes on the wire are not the bytes on disk. A
                            // pass-through says nothing new, because nothing
                            // happened to it.
                            if media.media_type == media_type {
                                media_type.to_string()
                            } else {
                                format!("{media_type} converted to {}", media.media_type)
                            },
                            media.byte_len(),
                            media.digest()
                        );
                        pending_media.push(media);
                        Dispatched::Continue {
                            decision: format!("viewed {target}"),
                            obs,
                            kind: ObsKind::Read,
                            target: Some(target.clone()),
                            // The bytes are a file's, even though what the model
                            // reads here is the digest rather than the image.
                            origin: Origin::File,
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => {
                        Dispatched::go("view_image error", format!("\n[view_image error] {e}\n"))
                    }
                },
            }
        }
        WRITE_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let content = s("content").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "write missing path",
                    "\n[write error] write_file needs a \"path\" in workspace mode\n",
                ));
            }
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(content),
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target,
                    content,
                    remember,
                } => {
                    let body = content.unwrap_or_default();
                    // What is there now, read before the write so the line counts
                    // below have something to compare against and so the run has
                    // a restore point. The write gate has already passed at this
                    // point; this is measurement, and the content never reaches
                    // the model.
                    let (before, kept) = read_before(ws, &target);
                    // A `Kept::Unkept` before text is `""` and is not what was
                    // there, so it is not something to diff against. Read here
                    // rather than from `kept`, which is moved below.
                    let diffable = !matches!(kept, Kept::Unkept(_));
                    match ws.write_file(&target, &body) {
                        Ok(wrote) => {
                            record_edit(
                                store,
                                run_id,
                                step,
                                WRITE_FILE_TOOL,
                                &target,
                                &before,
                                &body,
                                // A whole-file write measures the whole file
                                // either way, so the hunk's texts and the
                                // counts' texts happen to be the same pair here.
                                diffable.then_some((before.as_str(), body.as_str())),
                            );
                            record_snapshot(store, run_id, step, &target, kept);
                            // The same check `edit_file` runs, for the same
                            // reason: a write is how most new code arrives, and
                            // a type error in a file the model just created is
                            // worth exactly as much to know about as one it
                            // edited into an existing file.
                            let diagnostics = diagnostics_after_write(
                                ws,
                                toolchain,
                                exec_timeout,
                                cap,
                                lsp,
                                run_id,
                                watch,
                                exec_sandbox,
                            )
                            .await;
                            Dispatched::Continue {
                                decision: format!("wrote {target}"),
                                // A write that changed nothing says so, to the model as
                                // well as to the trace: an agent rewriting a file with
                                // what it already held is the shape of a stall, and it
                                // cannot correct for what it is not told.
                                obs: bound(
                                    &format!(
                                        "\n[wrote {target}] ({} chars{})\n{}",
                                        body.chars().count(),
                                        if wrote.moved_the_workspace() {
                                            ""
                                        } else {
                                            ", identical to what was already there — the \
                                         workspace did not change"
                                        },
                                        diagnostics
                                    ),
                                    cap,
                                    ObsKind::Write,
                                ),
                                kind: ObsKind::Write,
                                target: Some(target.clone()),
                                // A write's confirmation is a fact about the
                                // filesystem, and the diagnostics folded into it
                                // are about the file that was just written.
                                origin: Origin::File,
                                changed: wrote.moved_the_workspace(),
                                remember,
                            }
                        }
                        Err(e) => Dispatched::go("write error", format!("\n[write error] {e}\n")),
                    }
                }
            }
        }
        EDIT_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let search = s("search").unwrap_or_default();
            let replacement = s("replace").unwrap_or_default();
            if path.is_empty() || search.is_empty() {
                return Ok(Dispatched::go(
                    "edit missing arguments",
                    "\n[edit error] edit_file needs a \"path\" and a non-empty \"search\"\n",
                ));
            }
            // The same act as `write_file`, so the same gate on the same path — a
            // partial edit is not a lesser write, and a policy that refuses one
            // refuses the other. The replacement text is offered as the content so
            // a human deciding an `Ask` sees what is going in rather than only
            // where.
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(replacement),
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target,
                    content,
                    remember,
                } => {
                    let replacement = content.unwrap_or_default();
                    // The restore point, and — since 0.51.0 — the hunk's "before".
                    // `Workspace::edit_file` does its own read internally and does
                    // not hand back what it found, so this is the only place the
                    // file's previous text exists on this path.
                    //
                    // It is still NOT what the counts are measured from. Those
                    // compare `search` against `replacement`, which is what has
                    // made them "the size of the replacement" rather than "the
                    // size of the file" since 0.18.0; folding the two together is
                    // the tidy-up that would silently renumber every trace.
                    let (before, kept) = read_before(ws, &target);
                    let diffable = !matches!(kept, Kept::Unkept(_));
                    match ws.edit_file(&target, search, &replacement) {
                        Ok(wrote) => {
                            // The file as it now stands, through the same reader,
                            // so the after text is what is actually on disk rather
                            // than this arm's reconstruction of what the workspace
                            // was asked to do.
                            let (after, _) = read_before(ws, &target);
                            // The replaced text against the text that replaced it.
                            // Everything outside the match is byte-identical by
                            // construction, so this is the same answer comparing the
                            // whole file would give, without reading it twice.
                            record_edit(
                                store,
                                run_id,
                                step,
                                EDIT_FILE_TOOL,
                                &target,
                                search,
                                &replacement,
                                diffable.then_some((before.as_str(), after.as_str())),
                            );
                            record_snapshot(store, run_id, step, &target, kept);
                            // The project's own checker, run against the edit
                            // that just happened. It cannot fail the edit: the
                            // write is already on disk by the time this runs, so
                            // a checker that is missing, slow or broken costs
                            // the model a note and nothing else.
                            let diagnostics = diagnostics_after_write(
                                ws,
                                toolchain,
                                exec_timeout,
                                cap,
                                lsp,
                                run_id,
                                watch,
                                exec_sandbox,
                            )
                            .await;
                            Dispatched::Continue {
                                decision: format!("edited {target}"),
                                obs: bound(
                                    &format!(
                                        "\n[edited {target}] replaced {} chars with {}{}\n{}",
                                        search.chars().count(),
                                        replacement.chars().count(),
                                        if wrote.moved_the_workspace() {
                                            ""
                                        } else {
                                            " — the replacement is identical to what was there, so \
                                         the workspace did not change"
                                        },
                                        diagnostics
                                    ),
                                    cap,
                                    ObsKind::Write,
                                ),
                                kind: ObsKind::Write,
                                target: Some(target.clone()),
                                origin: Origin::File,
                                changed: wrote.moved_the_workspace(),
                                remember,
                            }
                        }
                        // A miss or an ambiguity is the model's to fix and says how:
                        // an edit that guessed which of three occurrences was meant
                        // is the failure this tool exists to make impossible.
                        Err(e) => Dispatched::go("edit error", format!("\n[edit error] {e}\n")),
                    }
                }
            }
        }
        PATCH_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let patch = s("patch").unwrap_or_default();
            if path.is_empty() || patch.trim().is_empty() {
                return Ok(Dispatched::go(
                    "patch missing arguments",
                    "\n[patch error] patch_file needs a \"path\" and a non-empty \"patch\" \
                     holding a unified diff\n",
                ));
            }
            // The same act as `write_file` and `edit_file`, so the same gate on
            // the same path. The patch body is offered as the content so a human
            // answering an `Ask` sees the change rather than only where it lands.
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(patch),
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target,
                    content,
                    remember,
                } => {
                    let patch = content.unwrap_or_default();
                    let (before, kept) = read_before(ws, &target);
                    let diffable = !matches!(kept, Kept::Unkept(_));
                    match ws.patch_file(&target, &patch) {
                        Ok(wrote) => {
                            let (after, _) = read_before(ws, &target);
                            // Measured over the whole file, because a patch *is*
                            // a whole-file change — there is no fragment here for
                            // the counts to be about, which is the one way this
                            // arm differs from `edit_file`'s.
                            record_edit(
                                store,
                                run_id,
                                step,
                                PATCH_FILE_TOOL,
                                &target,
                                &before,
                                &after,
                                diffable.then_some((before.as_str(), after.as_str())),
                            );
                            record_snapshot(store, run_id, step, &target, kept);
                            let diagnostics = diagnostics_after_write(
                                ws,
                                toolchain,
                                exec_timeout,
                                cap,
                                lsp,
                                run_id,
                                watch,
                                exec_sandbox,
                            )
                            .await;
                            Dispatched::Continue {
                                decision: format!("patched {target}"),
                                obs: bound(
                                    &format!(
                                        "\n[patched {target}] applied {} hunk{}{}\n{}",
                                        patch.matches("\n@@ ").count()
                                            + usize::from(patch.starts_with("@@ ")),
                                        if patch.matches("@@ ").count() == 1 {
                                            ""
                                        } else {
                                            "s"
                                        },
                                        if wrote.moved_the_workspace() {
                                            ""
                                        } else {
                                            " — the patch reproduced what was already there, so \
                                             the workspace did not change"
                                        },
                                        diagnostics
                                    ),
                                    cap,
                                    ObsKind::Write,
                                ),
                                kind: ObsKind::Write,
                                target: Some(target.clone()),
                                origin: Origin::File,
                                changed: wrote.moved_the_workspace(),
                                remember,
                            }
                        }
                        // A patch that does not fit is the model's to fix and the
                        // message says which hunk and what it expected. Nothing
                        // was written, so reading the file again and rewriting the
                        // patch is a complete recovery.
                        Err(e) => Dispatched::go("patch error", format!("\n[patch error] {e}\n")),
                    }
                }
            }
        }
        CHECK_TOOL => {
            // Resolved before it is spawned, because the policy has to be asked
            // about the command and not about the word "check".
            let checker = match crate::tools::diagnostics::checker(toolchain) {
                Ok(c) => c,
                // A model that ASKED is told there is no checker. The automatic
                // post-edit path stays silent on the same answer, and that is the
                // difference between the two: silence costs nothing when nobody
                // asked, and reads as "your project is clean" when somebody did.
                Err(why) => {
                    return Ok(Dispatched::go(
                        "check skipped",
                        format!("\n[check skipped] {why}\n"),
                    ))
                }
            };
            // The same two targets `exec` checks, for the same reason: the program
            // alone is what `deny_exec("cargo")` names, and the whole argv is what
            // a rule like `deny_exec("cargo check*")` names.
            let program = checker.argv[0].clone();
            let joined = checker.argv.join(" ");
            let mut targets = vec![program];
            if joined != targets[0] {
                targets.push(joined);
            }
            let mut remembered: Vec<Rule> = Vec::new();
            for target in targets {
                match gate(
                    ws,
                    approver,
                    store,
                    run_id,
                    step,
                    Act::Exec,
                    &target,
                    None,
                    watch,
                    depth,
                    goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                    Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }
            // 0.74.0 — the `check` tool gated its checker and then spawned it
            // outside the run's containment, so a policy allowing `cargo` handed
            // the host the workspace's own `build.rs`, its proc macros and any
            // `rustc-wrapper`, while `exec cargo check` on the same run was
            // contained. Found while closing C2; the same spawn, the same fix.
            let contained = exec_sandbox
                .map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress())));
            let obs = match checker
                .run(ws.root(), exec_timeout, cap, contained.as_ref())
                .await
            {
                crate::tools::diagnostics::Outcome::Clean => {
                    "\n[check] the project's own check found nothing\n".to_string()
                }
                crate::tools::diagnostics::Outcome::Found(text) => text,
                // Both of these are silent on the post-edit path and neither is
                // here. An empty answer to a direct question is read as approval.
                crate::tools::diagnostics::Outcome::Skipped(why) => {
                    format!("\n[check skipped] {why}\n")
                }
                crate::tools::diagnostics::Outcome::Failed(why) => {
                    format!("\n[check did not run] {why}\n")
                }
            };
            // The compiler's stream is never replaced and never filtered: what a
            // server sees is added to it. `src/tools/diagnostics.rs` says why —
            // a server's own analysis omits borrow-check errors, monomorphisation
            // errors and every lint, which are the errors a model writes.
            let obs = format!(
                "{obs}{}",
                lsp_diagnostics_text(&lsp.diagnose(ws, None, run_id, watch).await, true)
            );
            Dispatched::Continue {
                decision: "checked the project".to_string(),
                obs: bound(&obs, cap, ObsKind::Tool),
                kind: ObsKind::Tool,
                target: None,
                // Two sources in one observation — a checker process's stream and
                // a language server's diagnostics — so neither `Shell` nor `Lsp`
                // is true of the whole of it. `Tool` is the origin for exactly
                // that: external, and no finer attribution this crate can make
                // without splitting one answer into two.
                origin: Origin::Tool,
                // Deliberately not `changed`, for the reason `exec` is not: the
                // stall signal asks whether the agent is getting anywhere, and
                // running the same check a fourth time without editing anything
                // in between is precisely the shape of an agent that is not.
                changed: false,
                remember: remembered,
            }
        }
        LSP_DEFINITION_TOOL | LSP_REFERENCES_TOOL | LSP_HOVER_TOOL => {
            let path = s("path").unwrap_or_default();
            let (line, column) = match at(a) {
                Ok(pair) => pair,
                Err(why) => return Ok(Dispatched::go("lsp bad position", why)),
            };
            let ask = match call.name.as_str() {
                LSP_DEFINITION_TOOL => crate::lsp::Nav::Definition { path, line, column },
                LSP_REFERENCES_TOOL => crate::lsp::Nav::References { path, line, column },
                _ => crate::lsp::Nav::Hover { path, line, column },
            };
            navigate_gated(
                &call.name,
                Some(path),
                ask,
                ws,
                approver,
                store,
                run_id,
                step,
                lsp,
                watch,
                depth,
                goal,
                cap,
            )
            .await?
        }
        LSP_RENAME_TOOL => {
            let path = s("path").unwrap_or_default();
            let new_name = s("new_name").unwrap_or_default();
            if new_name.is_empty() {
                return Ok(Dispatched::go(
                    "lsp rename missing name",
                    "\n[lsp error] lsp_rename needs \"new_name\"\n".to_string(),
                ));
            }
            let (line, column) = match at(a) {
                Ok(pair) => pair,
                Err(why) => return Ok(Dispatched::go("lsp bad position", why)),
            };
            // No `Act::Write` check here, and that is the design rather than an
            // omission: this call writes nothing. Each file's section is applied
            // by `patch_file`, which is gated on that path like any other write.
            let ask = crate::lsp::Nav::Rename {
                path,
                line,
                column,
                new_name,
            };
            navigate_gated(
                &call.name,
                Some(path),
                ask,
                ws,
                approver,
                store,
                run_id,
                step,
                lsp,
                watch,
                depth,
                goal,
                cap,
            )
            .await?
        }
        LSP_SYMBOLS_TOOL => {
            let path = s("path");
            let query = s("query");
            if path.is_none() && query.is_none() {
                return Ok(Dispatched::go(
                    "lsp symbols missing target",
                    "\n[lsp error] lsp_symbols needs either \"path\", for one file's symbols, or \
                     \"query\", to search the workspace\n"
                        .to_string(),
                ));
            }
            let ask = crate::lsp::Nav::Symbols { path, query };
            navigate_gated(
                &call.name, path, ask, ws, approver, store, run_id, step, lsp, watch, depth, goal,
                cap,
            )
            .await?
        }
        #[cfg(feature = "browser")]
        name if crate::tools::browser::is_browser_tool(name) => {
            use crate::tools::browser::Action;
            let need = |what: &str, why: &str| {
                Dispatched::go(
                    format!("{name} missing {what}"),
                    format!("\n[{name} error] {why}\n"),
                )
            };
            let action =
                match name {
                    crate::tools::BROWSER_NAVIGATE_TOOL => {
                        match s("url").filter(|u| !u.trim().is_empty()) {
                            Some(url) => Action::Navigate {
                                url: url.to_string(),
                            },
                            None => return Ok(need("url", "browser_navigate needs a \"url\"")),
                        }
                    }
                    crate::tools::BROWSER_READ_TOOL => Action::Read {
                        selector: s("selector")
                            .filter(|v| !v.trim().is_empty())
                            .map(str::to_string),
                    },
                    crate::tools::BROWSER_SCREENSHOT_TOOL => Action::Screenshot,
                    crate::tools::BROWSER_CLICK_TOOL => {
                        match s("selector").filter(|v| !v.trim().is_empty()) {
                            Some(selector) => Action::Click {
                                selector: selector.to_string(),
                            },
                            None => return Ok(need(
                                "selector",
                                "browser_click needs a CSS \"selector\" for the element to click",
                            )),
                        }
                    }
                    crate::tools::BROWSER_TYPE_TOOL => {
                        let Some(selector) = s("selector").filter(|v| !v.trim().is_empty()) else {
                            return Ok(need("selector", "browser_type needs a CSS \"selector\""));
                        };
                        match s("text") {
                            Some(text) => Action::Type {
                                selector: selector.to_string(),
                                text: text.to_string(),
                            },
                            None => return Ok(need("text", "browser_type needs \"text\" to type")),
                        }
                    }
                    _ => Action::Scroll {
                        dy: a.get("dy").and_then(serde_json::Value::as_i64).unwrap_or(0),
                    },
                };

            let acted = match browser.act(action, ws.policy(), store, run_id, watch).await {
                Ok(acted) => acted,
                // The browser could not be started at all — a configuration
                // failure, not an action failure.
                Err(e) => {
                    return Ok(Dispatched::go(
                        "browser unavailable",
                        format!("\n[{name} unavailable] {e}\n"),
                    ))
                }
            };

            if let Some(started) = acted.started {
                watch.emit(crate::observe::RunEvent::new(
                    run_id,
                    step,
                    crate::observe::EventKind::BrowserStarted {
                        binary: started.binary,
                        headless: started.headless,
                        ready_ms: u64::try_from(started.ready_ms).unwrap_or(u64::MAX),
                    },
                ));
            }
            // One row per navigation the browser attempted, including the ones
            // this call never named.
            let refused: Vec<String> = acted
                .decisions
                .iter()
                .filter(|d| !d.permitted)
                .map(|d| match &d.rule {
                    Some(rule) => format!("{} (rule {rule})", d.target),
                    None => d.target.clone(),
                })
                .collect();
            for decision in &acted.decisions {
                watch.emit(crate::observe::RunEvent::new(
                    run_id,
                    step,
                    crate::observe::EventKind::BrowserNavigated {
                        host: decision.target.clone(),
                        permitted: decision.permitted,
                    },
                ));
            }

            // A refusal is reported as a refusal, whatever the browser called it.
            // The blocked load surfaces from the protocol as an opaque network
            // error, and handing the model that instead of the boundary's own
            // words would tell it nothing about what to change.
            if !refused.is_empty() {
                return Ok(Dispatched::go(
                    format!("browser navigation refused: {}", refused.join(", ")),
                    format!(
                        "\n[{name} refused] net to {} is not permitted by this run's policy. \
                         This applies to every navigation the page attempts, including ones a \
                         click or a redirect causes.\n",
                        refused.join(", ")
                    ),
                ));
            }

            match acted.outcome {
                Ok(outcome) => {
                    let mut obs = format!("\n[{name}]\n{}\n", outcome.text);
                    if let Some(encoded) = outcome.image {
                        match decode_screenshot(&encoded) {
                            Ok(media) => {
                                obs = format!(
                                    "\n[{name}]\n{}\nattached to the next request \
                                     ({} bytes, digest {})\n",
                                    outcome.text,
                                    media.byte_len(),
                                    media.digest()
                                );
                                pending_media.push(media);
                            }
                            Err(e) => {
                                obs = format!(
                                    "\n[{name}]\n{}\n[screenshot dropped] {e}\n",
                                    outcome.text
                                )
                            }
                        }
                    }
                    Dispatched::Continue {
                        decision: format!("drove the browser: {name}"),
                        obs: bound(&obs, cap, ObsKind::Tool),
                        kind: ObsKind::Tool,
                        target: None,
                        // A page's own text, whoever put the page there. The
                        // `ObsKind` cannot say this — it is `Tool` here as it is
                        // for `exec` — which is the pair the recorded origin
                        // exists to separate.
                        origin: Origin::Web,
                        // Looking at a page changes nothing in the workspace, so
                        // it is not progress for the stall signal — the same
                        // reasoning `check` and the navigation tools follow.
                        changed: false,
                        remember: Vec::new(),
                    }
                }
                Err(e) => Dispatched::go(
                    format!("browser action failed: {name}"),
                    format!("\n[{name} error] {e}\n"),
                ),
            }
        }
        SHELL_TOOL => {
            let line_src = s("line").unwrap_or_default();
            if line_src.trim().is_empty() {
                return Ok(Dispatched::go(
                    "shell missing line",
                    "\n[shell error] shell needs a non-empty \"line\" string holding the command \
                     line to run\n",
                ));
            }
            // Refusing is an observation, not an error. The model wrote something
            // this tool does not admit, is told which construct and why, and gets
            // to write something else — the same shape as a policy refusal, and
            // for the same reason: an `Err` here would end the run over a
            // recoverable mistake.
            let parsed = match crate::tools::shell::parse(line_src) {
                Ok(l) => l,
                Err(r) => {
                    return Ok(Dispatched::go(
                        format!("shell refused: {}", r.construct),
                        format!("\n[shell refused] {r}\n"),
                    ))
                }
            };
            let plan = match crate::tools::shell::plan(&parsed, ws.root()) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(Dispatched::go(
                        "shell refused: a path outside the workspace",
                        format!("\n[shell refused] {e}\n"),
                    ))
                }
            };

            // Every check for the whole line happens here, before the first
            // spawn. This is the criterion the tool exists to satisfy: a line
            // whose second stage is denied must not run its first, so a loop that
            // checked-then-ran each stage in turn would be wrong however careful
            // it looked.
            let remembered = match check_shell_line(
                ws, approver, store, run_id, step, watch, depth, &parsed, &plan, goal,
            )
            .await?
            {
                ShellCheck::Go(remember) => remember,
                ShellCheck::Stop(d) => return Ok(d),
            };

            let contained = exec_sandbox
                .map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress())));
            if let Some(containment) = &contained {
                let backend = containment.backend();
                for event in [
                    sandbox_create(run_id, step, containment),
                    crate::state::SandboxEvent::exec(run_id, step, backend.as_str(), line_src),
                ] {
                    record_sandbox_step(store, watch, depth, &event);
                }
            }
            let outcome =
                Shell::new(exec_timeout, cap)
                    .contained(contained.clone().map(|containment| {
                        crate::tools::shell::ShellSandbox {
                            containment,
                            workdir: ws.root().to_path_buf(),
                        }
                    }))
                    .run(&parsed, &plan)
                    .await?;
            if contained.is_some() {
                record_sandbox_step(
                    store,
                    watch,
                    depth,
                    &crate::state::SandboxEvent::destroy(run_id, step),
                );
            }
            let (decision, obs) = match &outcome {
                ShellOutcome::Unavailable { reason } => (
                    "shell command unavailable".to_string(),
                    format!(
                        "\n[shell unavailable] {reason}. This machine cannot run that command; \
                         try another, or carry on without it.\n"
                    ),
                ),
                ShellOutcome::TimedOut { after } => (
                    "shell timed out".to_string(),
                    format!(
                        "\n[shell timed out] `{line_src}` was killed after {}s without \
                         finishing. Nothing it printed was captured. Run something narrower, or \
                         expect this to need longer than this run allows.\n",
                        after.as_secs()
                    ),
                ),
                ShellOutcome::Ran {
                    code,
                    stdout,
                    stderr,
                    elided,
                    ran,
                } => {
                    let body = crate::verify::joined_streams(stdout, stderr);
                    let total = parsed.commands().count();
                    (
                        format!(
                            "shell {}",
                            code.map_or("(signal)".to_string(), |c| format!("exit {c}"))
                        ),
                        format!(
                            "\n[shell `{line_src}` {}{}]{}\n{}\n",
                            code.map_or("killed by a signal".to_string(), |c| format!("exit {c}")),
                            if *ran < total {
                                // `&&` and `||` skipping is the difference between
                                // a command that failed and one that never ran,
                                // and a model that cannot tell them apart will
                                // debug the wrong stage.
                                format!(
                                    ", {ran} of {total} sub-commands ran; the rest were skipped \
                                     by `&&` or `||`"
                                )
                            } else {
                                String::new()
                            },
                            if *elided > 0 {
                                format!(
                                    " ({elided} characters of output elided from the middle; the \
                                     start and the end are both here)"
                                )
                            } else {
                                String::new()
                            },
                            if body.trim().is_empty() {
                                "(no output)"
                            } else {
                                body.trim_end()
                            }
                        ),
                    )
                }
            };
            Dispatched::Continue {
                decision,
                obs: bound(&obs, cap, ObsKind::Tool),
                // `Tool`, matching `exec`, because the target is the name of the
                // thing that answered rather than the subject of the answer: two
                // different command lines gave two different results, and letting
                // the later one supersede the earlier would discard one of them.
                kind: ObsKind::Tool,
                target: Some(line_src.to_string()),
                // A process wrote this, under an argv the model composed — the
                // least attributable content a run handles, and the reason
                // `Origin::Shell` exists rather than folding it into `Tool`.
                origin: Origin::Shell,
                changed: false,
                remember: remembered,
            }
        }
        SHELL_START_TOOL => {
            let line_src = s("line").unwrap_or_default();
            if line_src.trim().is_empty() {
                return Ok(Dispatched::go(
                    "shell_start missing line",
                    "\n[shell_start error] shell_start needs a non-empty \"line\" string holding \
                     the command line to start\n",
                ));
            }
            // Parsed and refused by the same functions the foreground tool uses.
            // The refusal wording names this tool so the model knows which call
            // was rejected, but the decision is the same decision.
            let parsed = match crate::tools::shell::parse(line_src) {
                Ok(l) => l,
                Err(r) => {
                    return Ok(Dispatched::go(
                        format!("shell_start refused: {}", r.construct),
                        format!("\n[shell_start refused] {r}\n"),
                    ))
                }
            };
            let plan = match crate::tools::shell::plan(&parsed, ws.root()) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(Dispatched::go(
                        "shell_start refused: a path outside the workspace",
                        format!("\n[shell_start refused] {e}\n"),
                    ))
                }
            };
            let remembered = match check_shell_line(
                ws, approver, store, run_id, step, watch, depth, &parsed, &plan, goal,
            )
            .await?
            {
                ShellCheck::Go(remember) => remember,
                ShellCheck::Stop(d) => return Ok(d),
            };

            // Reserved only after the whole line has cleared. A refused line
            // must not consume a slot, and a reservation is the first thing that
            // could leak if it did.
            let (id, capture) = match handles.reserve(line_src) {
                Ok(pair) => pair,
                Err(reason) => {
                    return Ok(Dispatched::go(
                        "shell_start refused: the handle cap",
                        format!("\n[shell_start refused] {reason}\n"),
                    ))
                }
            };

            // Recorded before the spawn, so a crash between here and the first
            // poll still leaves a row saying something was started — which is
            // exactly the row a resume must find and orphan. A handle that
            // exists only in memory is a handle a resume cannot warn about.
            store.record_handle_started(run_id, step, id, line_src)?;
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::HandleStarted {
                    handle: id,
                    line: line_src.to_string(),
                },
            ));

            // WEAK, not strong, and this is load-bearing rather than tidy.
            //
            // The registry kills whatever is still live when it drops, and that
            // backstop is what covers the paths this loop leaves by `?` or by a
            // panic. A strong reference held by the reaping task below defeats
            // it completely: the task lives exactly as long as the process it is
            // waiting on, so a handle that never exits keeps the registry's
            // refcount above zero forever, `Drop` never runs, and the process
            // outlives the run that started it. That is the leak the whole
            // module exists to prevent, reintroduced by the thing meant to
            // observe it.
            //
            // With a weak reference the registry's only owner is the run loop.
            // When the loop returns, it drops, it kills, and these tasks find
            // nothing to upgrade to — which is correct, because by then there is
            // no run left to record anything for.
            let registry = std::sync::Arc::downgrade(handles);
            let on_spawn = {
                let registry = registry.clone();
                std::sync::Arc::new(move |child: &tokio::process::Child| {
                    // A registry that can no longer be upgraded means the run
                    // has already ended and dropped it. Nothing is recorded and
                    // nothing is contained, and the caller kills the stage —
                    // which is right, because there is no longer a run that owns
                    // it.
                    let Some(r) = registry.upgrade() else {
                        return Err(crate::error::Error::Sandbox {
                            reason: "the run ended while this line was still starting".into(),
                        });
                    };
                    if let Some(pid) = child.id() {
                        r.add_pid(id, pid);
                    }
                    // Windows only, and only for a handle: the stage is frozen
                    // at this instant, so this is where it joins the job object
                    // that will later take its whole tree down, and where it is
                    // let go. On unix the containment was applied in the child
                    // before `exec` and there is nothing left to do here.
                    #[cfg(windows)]
                    r.contain(id, child)?;
                    Ok(())
                })
                    as std::sync::Arc<
                        dyn Fn(&tokio::process::Child) -> crate::error::Result<()> + Send + Sync,
                    >
            };
            // 0.48.0 — the same containment the foreground line gets, per stage,
            // through the same shared runner. Until this release a handle was the
            // one execution path left at full privilege, so an agent that could
            // not write outside the workspace with `shell` could start the same
            // line with `shell_start` and write wherever it liked.
            //
            // There is no cross-run lifetime to manage and that is the answer to
            // the question `docs/CONTRACT.md` left open: the restriction lives
            // with the processes — a `pre_exec` rule set or a wrapper argv on
            // unix, the Job Object this handle already owns on Windows — so
            // nothing is torn down and nothing is re-entered. A resumed run finds
            // the previous run's handle rows and orphans them, exactly as before.
            let contained = exec_sandbox
                .map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress())));
            if let Some(containment) = &contained {
                let backend = containment.backend();
                for event in [
                    sandbox_create(run_id, step, containment),
                    crate::state::SandboxEvent::exec(run_id, step, backend.as_str(), line_src),
                ] {
                    record_sandbox_step(store, watch, depth, &event);
                }
            }
            let runner = Shell::detached(
                cap,
                crate::tools::shell::Capture {
                    path: capture,
                    on_spawn,
                },
            )
            .contained(contained.clone().map(|containment| {
                crate::tools::shell::ShellSandbox {
                    containment,
                    workdir: ws.root().to_path_buf(),
                }
            }));
            // Detached on purpose: this is the one tool whose work outlives its
            // dispatch. The task reaps, so a process that ends on its own is
            // recorded as ended rather than left looking live to every later
            // poll — one of the four ways a handle could otherwise leak.
            let reaper = registry.clone();
            tokio::spawn(async move {
                let outcome = runner.run(&parsed, &plan).await;
                // Upgraded only to record. If the run has already ended, the
                // registry is gone, it killed this process on its way out, and
                // there is nothing left to tell.
                let Some(reaper) = reaper.upgrade() else {
                    return;
                };
                match outcome {
                    Ok(crate::tools::shell::ShellOutcome::Ran { code, .. }) => {
                        reaper.finished(id, code)
                    }
                    // `Unavailable` is a line whose program is not on this
                    // machine, and a timeout cannot happen without a ceiling.
                    // Either way the handle is over and must not read as live.
                    _ => reaper.finished(id, None),
                }
            });

            // Wait briefly for the first process to appear, then record what
            // was spawned.
            //
            // The pids are only knowable after the spawn, which happens in the
            // task above, so there is a short window in which the trace knows a
            // handle exists and not what it owns. Closing it matters because a
            // run that starts a handle and then ends — the model never polls,
            // never kills — would otherwise record a handle with no processes,
            // and "what did that run leave running" is exactly the question the
            // row exists to answer.
            //
            // Bounded and best-effort on purpose: this is a trace concern, not a
            // safety one. Whatever is not recorded here is recorded by the next
            // poll, by the kill, or by the per-step refresh, and the killing
            // itself never depended on this — `Handles` kills what it knows on
            // drop regardless of what reached the store.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            let pids = loop {
                let pids = handles.pids(id);
                if !pids.is_empty() || std::time::Instant::now() >= deadline {
                    break pids;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };
            store.record_handle_pids(run_id, id, &pids)?;

            Dispatched::Continue {
                decision: format!("shell_start handle {id}"),
                obs: bound(
                    &format!(
                        "\n[shell_start handle {id}] `{line_src}` is running. Read what it \
                         prints with shell_poll {id}, and end it with shell_kill {id}. It is \
                         killed automatically when this run ends.\n"
                    ),
                    cap,
                    ObsKind::Tool,
                ),
                kind: ObsKind::Tool,
                target: Some(line_src.to_string()),
                // The handle's receipt rather than its output, but it is the
                // shell family's answer and the polls that follow carry the
                // process's own bytes under the same origin.
                origin: Origin::Shell,
                changed: false,
                remember: remembered,
            }
        }
        SHELL_POLL_TOOL => {
            let Some(id) = a.get("handle").and_then(|v| v.as_u64()) else {
                return Ok(Dispatched::go(
                    "shell_poll missing handle",
                    "\n[shell_poll error] shell_poll needs a \"handle\" number, the id \
                     shell_start returned\n",
                ));
            };
            let Some(state) = handles.state(id) else {
                return Ok(Dispatched::go(
                    "shell_poll unknown handle",
                    format!(
                        "\n[shell_poll error] no process handle {id} in this run; shell_start \
                         returns the id to poll\n"
                    ),
                ));
            };
            // An orphan is answered from what was recorded, never by touching
            // anything. There is no process here to read from and the pid that
            // was recorded may belong to something else entirely.
            if let crate::tools::handles::HandleState::Orphaned(reason) = &state {
                return Ok(Dispatched::seen(
                    format!("shell_poll handle {id} orphaned"),
                    format!(
                        "\n[shell_poll handle {id}] orphaned: {reason}. It was started before \
                         this run was resumed and cannot be read or signalled. Start what you \
                         need again.\n"
                    ),
                    ObsKind::Tool,
                    None,
                    Origin::Shell,
                ));
            }
            let (text, skipped) = handles.poll(id)?;
            // Every byte a poll *reads* goes to the store as well as to the model,
            // and the qualifier is load-bearing. The capture file does not outlive
            // the run, so this is the only durable record of what the process
            // printed — "what did that dev server do" is a question asked after it
            // is gone.
            //
            // What it is NOT is a complete transcript. A poll that arrives to find
            // more than one window waiting keeps the newest and advances the cursor
            // past the rest, so bytes no poll ever read reach no store and are lost
            // with the capture file. That is the honest consequence of bounding the
            // window, the poll says so to the model rather than implying otherwise,
            // and it is recorded as a known limitation of this release. Fixing it
            // means streaming the skipped region to the store in bounded chunks,
            // which is a change to how a poll reads rather than to what it returns.
            store.record_handle_output(run_id, step, id, &text)?;
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                // The count, not the text. The channel is for watching a run,
                // not for carrying its payload — the output is in the store.
                EventKind::HandlePolled {
                    handle: id,
                    bytes: text.len(),
                },
            ));
            let line_src = handles.line(id).unwrap_or_default();
            Dispatched::seen(
                format!("shell_poll handle {id} ({} bytes)", text.len()),
                bound(
                    &format!(
                        "\n[shell_poll handle {id} `{line_src}` {}]{}\n{}\n",
                        match &state {
                            crate::tools::handles::HandleState::Running => "running".to_string(),
                            crate::tools::handles::HandleState::Exited(Some(c)) =>
                                format!("exited {c}"),
                            crate::tools::handles::HandleState::Exited(None) =>
                                "killed by a signal".to_string(),
                            other => other.as_str().to_string(),
                        },
                        if skipped > 0 {
                            format!(
                                " ({skipped} bytes of older output skipped; the newest is here, \
                                 and the skipped bytes are gone — poll more often to see \
                                 everything a noisy process prints)"
                            )
                        } else {
                            String::new()
                        },
                        if text.trim().is_empty() {
                            "(nothing new)"
                        } else {
                            text.trim_end()
                        }
                    ),
                    cap,
                    ObsKind::Tool,
                ),
                ObsKind::Tool,
                None,
                // What the process printed, verbatim inside this crate's framing
                // of it.
                Origin::Shell,
            )
        }
        SHELL_KILL_TOOL => {
            let Some(id) = a.get("handle").and_then(|v| v.as_u64()) else {
                return Ok(Dispatched::go(
                    "shell_kill missing handle",
                    "\n[shell_kill error] shell_kill needs a \"handle\" number, the id \
                     shell_start returned\n",
                ));
            };
            let line_src = handles.line(id).unwrap_or_default();
            // Written before the kill, because after it the registry is the only
            // thing that still knows which processes this handle owned and a
            // failure mid-kill would take that knowledge with it.
            store.record_handle_pids(run_id, id, &handles.pids(id))?;
            // Everything the process wrote and nobody asked for.
            //
            // The store's copy of a handle's output is written by the polls, so
            // output produced between the last poll and the kill — or by a
            // handle the model never polled at all — would otherwise never be
            // recorded anywhere that outlives the run. The capture file does not
            // survive the registry, so this is the last moment it can be read.
            let (tail, _) = handles.poll(id).unwrap_or_default();
            store.record_handle_output(run_id, step, id, &tail)?;
            match handles.kill(id) {
                // Killing something already over is not a mistake worth failing
                // a step for: a model that lost track of a handle should be told
                // how it ended and carry on.
                Ok(was) => {
                    store.record_handle_ended(run_id, id, "killed", None, None)?;
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::HandleKilled { handle: id },
                    ));
                    Dispatched::seen(
                        format!("shell_kill handle {id}"),
                        bound(
                            &format!(
                                "\n[shell_kill handle {id} `{line_src}`] {}\n",
                                match was {
                                    crate::tools::handles::HandleState::Running =>
                                        "killed, with every process it spawned".to_string(),
                                    crate::tools::handles::HandleState::Exited(Some(c)) =>
                                        format!("had already exited {c}; nothing to kill"),
                                    crate::tools::handles::HandleState::Exited(None) =>
                                        "had already been killed by a signal; nothing to kill"
                                            .to_string(),
                                    crate::tools::handles::HandleState::Killed =>
                                        "had already been killed; nothing to kill".to_string(),
                                    crate::tools::handles::HandleState::Orphaned(r) => r,
                                }
                            ),
                            cap,
                            ObsKind::Tool,
                        ),
                        ObsKind::Tool,
                        None,
                        Origin::Shell,
                    )
                }
                Err(reason) => Dispatched::go(
                    format!("shell_kill refused handle {id}"),
                    format!("\n[shell_kill error] {reason}\n"),
                ),
            }
        }
        EXEC_TOOL => {
            let argv: Vec<String> = a
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let Some(program) = argv.first().filter(|p| !p.trim().is_empty()).cloned() else {
                return Ok(Dispatched::go(
                    "exec missing argv",
                    "\n[exec error] exec needs a non-empty \"argv\" array whose first element is \
                     the program\n",
                ));
            };
            // Two checks, and the second is the one that makes a useful rule
            // writable. The program alone is what `deny_exec(\"rm\")` names, and it
            // holds whatever the arguments are. The whole argv is what
            // `allow_exec(\"git log*\")` and `deny_exec(\"git push*\")` name — a
            // check on the program could not tell those two apart, which is the
            // weakness the git built-ins were built to route around and the reason
            // they still exist.
            //
            // The argv check is a way of narrowing an allowlist and not a
            // blocklist: a joined argv has more spellings than a pattern can
            // enumerate — `git -c x push`, `env rm`, `busybox rm` — so a
            // `deny_exec` over a permissive `defaults.exec` is a boundary with an
            // unbounded number of ways around it. Documented on `EXEC_TOOL` and in
            // `docs/CONTRACT.md` rather than fixed, because the fix is the shape of
            // the policy a caller writes, not another pattern here.
            let joined = argv.join(" ");
            let mut remembered: Vec<Rule> = Vec::new();
            let mut targets = vec![program.clone()];
            if joined != program {
                targets.push(joined.clone());
            }
            for target in targets {
                match gate(
                    ws,
                    approver,
                    store,
                    run_id,
                    step,
                    Act::Exec,
                    &target,
                    None,
                    watch,
                    depth,
                    goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                    Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }

            // Egress on a contained command follows this run's policy, not the
            // config's flag: the config is shared with the verification gate,
            // which has no policy to consult, while a run has one and it is the
            // run's own statement about reaching the network. One authority per
            // path, rather than two that can disagree.
            let contained = exec_sandbox
                .map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress())));
            if let Some(containment) = &contained {
                let backend = containment.backend();
                for event in [
                    sandbox_create(run_id, step, containment),
                    crate::state::SandboxEvent::exec(run_id, step, backend.as_str(), &joined),
                ] {
                    record_sandbox_step(store, watch, depth, &event);
                }
            }
            let outcome = Exec::new(ws.root(), exec_timeout, cap)
                .contained(contained.clone())
                .run(&argv)
                .await?;
            if contained.is_some() {
                if let ExecOutcome::Capped { cap: hit, .. } = &outcome {
                    record_sandbox_step(
                        store,
                        watch,
                        depth,
                        &crate::state::SandboxEvent::cap_hit(run_id, step, hit.as_str()),
                    );
                }
                record_sandbox_step(
                    store,
                    watch,
                    depth,
                    &crate::state::SandboxEvent::destroy(run_id, step),
                );
            }
            let (decision, obs) = match &outcome {
                ExecOutcome::Unavailable { reason } => (
                    format!("{program} unavailable"),
                    format!(
                        "\n[exec unavailable] {reason}. This machine cannot run that command; \
                         try another, or carry on without it.\n"
                    ),
                ),
                ExecOutcome::TimedOut { after } => (
                    format!("{program} timed out"),
                    format!(
                        "\n[exec timed out] `{joined}` was killed after {}s without finishing. \
                         Nothing it printed was captured. Run something narrower, or expect this \
                         command to need longer than this run allows.\n",
                        after.as_secs()
                    ),
                ),
                // 0.40.0 — the sandbox's own ceiling, named. The model is told
                // which resource ran out, because "run something narrower" and
                // "this needs more memory than it was given" are different next
                // moves, and an exit status alone distinguishes neither.
                ExecOutcome::Capped {
                    cap,
                    stdout,
                    stderr,
                    ..
                } => {
                    let body = crate::verify::joined_streams(stdout, stderr);
                    (
                        format!("exec {program} hit the {} cap", cap.as_str()),
                        format!(
                            "\n[exec `{joined}` killed by the {} cap] The sandbox stopped it \
                             because it crossed the {} limit this run set, not because the \
                             command failed. Anything it printed first is below.\n{}\n",
                            cap.as_str(),
                            cap.as_str(),
                            if body.trim().is_empty() {
                                "(no output)"
                            } else {
                                body.trim_end()
                            }
                        ),
                    )
                }
                ExecOutcome::Ran {
                    code,
                    stdout,
                    stderr,
                    elided,
                } => {
                    let body = crate::verify::joined_streams(stdout, stderr);
                    (
                        format!(
                            "exec {program} {}",
                            code.map_or("(signal)".to_string(), |c| format!("exit {c}"))
                        ),
                        format!(
                            "\n[exec `{joined}` {}]{}\n{}\n",
                            code.map_or("killed by a signal".to_string(), |c| format!("exit {c}")),
                            if *elided > 0 {
                                format!(
                                    " ({elided} characters of output elided from the middle; the \
                                     start and the end are both here)"
                                )
                            } else {
                                String::new()
                            },
                            if body.trim().is_empty() {
                                "(no output)"
                            } else {
                                body.trim_end()
                            }
                        ),
                    )
                }
            };
            Dispatched::Continue {
                decision,
                obs,
                kind: ObsKind::Tool,
                target: None,
                // `shell`'s origin, for `shell`'s reason: the argv is the model's
                // and a process wrote the answer.
                origin: Origin::Shell,
                // Deliberately not `changed`, even for a command that plainly
                // wrote files. The stall signal asks whether the agent is getting
                // anywhere, and running the same build a fourth time without
                // editing anything in between is the shape of an agent that is
                // not — the argv is part of the call signature, so a *different*
                // command is never mistaken for a repeat.
                changed: false,
                remember: remembered,
            }
        }
        // Loading one skill's body. Offered only when skills are configured, so
        // the name is not special otherwise: a run without skills falls through
        // to the unknown-tool arm like any other name.
        //
        // The body is read through the policy at the moment it is asked for,
        // against the skill file's ABSOLUTE path — a skills directory usually
        // sits outside the workspace root, so this is a policy check, not a
        // workspace-relative one (see `gate`).
        READ_SKILL_TOOL if !skills.is_empty() => {
            let name = s("name").unwrap_or_default();
            let Some(skill) = skills.get(name) else {
                // Not an error and not a failed run: the model asked for
                // something that does not exist, so it is told what does.
                return Ok(Dispatched::go(
                    format!("unknown skill {name}"),
                    format!(
                        "\n[read_skill] there is no skill named {name:?}. Available: {}\n",
                        skills.names().join(", ")
                    ),
                ));
            };
            // 0.73.0 — an optional `path` reads a companion file, or lists a
            // directory, from inside the skill's own root instead of reading the
            // skill body. Resolution is `skills::resolve_companion`'s and nothing
            // here second-guesses it: it tries the skill's own directory before
            // the bundle root, so `references/tools.md` beside the skill and
            // `shared/state-model.md` beside the whole `skills/` tree both
            // resolve; an absolute path, a `..`, or a symlink whose target
            // canonicalises out of the root never reaches the filesystem; and a
            // path that is merely *not there* is told apart from one that is out
            // of bounds — a skill pointing at a file it no longer ships is a
            // typo, and reporting it as a refusal would send the operator
            // hunting for a breach.
            let asked = s("path");
            let path = match asked {
                None => skill.path.display().to_string(),
                Some(rel) => match crate::skills::resolve_companion(skill, rel) {
                    crate::skills::SkillFile::Resolved(abs) => abs.display().to_string(),
                    // What was asked for, never where it resolved to: naming the
                    // resolved location would disclose the very thing the refusal
                    // exists to withhold. No gate call, because nothing is read.
                    crate::skills::SkillFile::Outside(rel) => {
                        return Ok(Dispatched::go(
                            format!("skill {name} path {rel} refused"),
                            format!(
                                "\n[skill {name} refused] {rel} — that path leaves the skill's own \
                                 directory; ask for a path inside it\n"
                            ),
                        ));
                    }
                    crate::skills::SkillFile::Missing(rel) => {
                        return Ok(Dispatched::go(
                            format!("skill {name} has no {rel}"),
                            format!(
                                "\n[skill {name} not found] {rel} — the skill's directory has no \
                                 such file. Ask for the skill without a path to re-read what it \
                                 points at\n"
                            ),
                        ));
                    }
                },
            };
            // The observation's header, the trace's decision and the target a
            // later observation supersedes are all this one string. With no
            // `path` it is the skill's name and nothing else, which is what keeps
            // a body read byte for byte what it was before 0.73.0; with one it
            // carries the companion path, so two files from one skill are two
            // observations rather than one replacing the other.
            //
            // An empty `path` resolves to the root itself and so lists it, which
            // is the right answer to "what is in this bundle?" — written `.` here
            // so the trace cannot mistake that listing for the body read.
            let label = match asked {
                Some("") => format!("{name} ."),
                Some(rel) => format!("{name} {rel}"),
                None => name.to_string(),
            };
            // 0.80.0 — `gate_declared`, and this is the only call site in the
            // crate that takes it. A skill bundle lives outside the workspace
            // root by design, so the path here is absolute and `check_path`
            // would refuse it for leaving a root it was never inside. Until
            // 0.80.0 `gate` relaxed that for *every* absolute read and write,
            // which is the residual the audit left open at `policy_verdict`;
            // asking for the allowance here means one consumer holds it rather
            // than all of them inheriting it.
            match gate_declared(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                &path,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                // The gate's target, not the path that went in: an approver may
                // rewrite it, and whether it is a file or a directory is a
                // question about what will actually be opened.
                } => match skill_target(std::path::Path::new(&target)) {
                    Ok(body) => {
                        // Capped where it enters the context, like every other
                        // tool result, under the same budget-derived cap.
                        let (body, truncated) = crate::tools::cap_result(body, cap);
                        info!(
                            run_id,
                            step,
                            skill = name,
                            path = asked.unwrap_or_default(),
                            truncated,
                            "skill read"
                        );
                        Dispatched::Continue {
                            decision: format!("read skill {label}"),
                            obs: format!("\n[skill {label}]\n{body}\n"),
                            kind: ObsKind::Skill,
                            target: Some(label),
                            // A file on disk, but its own origin rather than
                            // `File`: a skill body is instructions somebody wrote
                            // for the agent, which is a different thing to trust
                            // than a source file the task is about.
                            origin: Origin::Skill,
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => Dispatched::go(
                        format!("skill {label} read error"),
                        format!("\n[skill {label} error] {e}\n"),
                    ),
                },
            }
        }
        // Spreadsheet built-ins (0.14.0). Each one gates on the path the model
        // named, with `Act::Read` or `Act::Write`, through the same `gate` the
        // file built-ins use — so a refusal names the workbook rather than the
        // tool, a child's narrowed policy applies to documents exactly as it
        // applies to source, and the underlying module reaches the file only
        // through `Workspace`'s policy-checked byte IO.
        //
        // This is why they are built-ins and not registered `Tool`s: a registered
        // tool is authorised once by an exec check on its name and then does
        // whatever it likes to the filesystem, which for a capability whose whole
        // job is reading and writing the user's files is the wrong boundary.
        #[cfg(feature = "xlsx")]
        XLSX_SHEETS_TOOL | XLSX_READ_TOOL => {
            let path = s("path").unwrap_or_default();
            let sheet = s("sheet");
            let listing = name == XLSX_SHEETS_TOOL;
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
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => {
                    let read = if listing {
                        crate::tools::documents::xlsx::sheet_names(ws, &target)
                            .map(|names| names.join("\n"))
                    } else {
                        crate::tools::documents::xlsx::read_sheet(ws, &target, sheet)
                    };
                    match read {
                        Ok(text) => Dispatched::Continue {
                            decision: format!("read {target}"),
                            obs: format!(
                                "\n[{name} {target}]\n{}\n",
                                bound(&text, cap, ObsKind::Read)
                            ),
                            kind: ObsKind::Read,
                            target: Some(target.clone()),
                            origin: Origin::File,
                            changed: false,
                            remember,
                        },
                        // A corrupt or non-xlsx file is the model's problem to
                        // route around, not the run's to die on.
                        Err(e) => Dispatched::go(
                            "spreadsheet read error",
                            format!("\n[{name} error] {e}\n"),
                        ),
                    }
                }
            }
        }
        #[cfg(feature = "xlsx")]
        XLSX_WRITE_TOOL | XLSX_SET_CELL_TOOL => {
            let path = s("path").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "spreadsheet missing path",
                    format!("\n[{name} error] needs a \"path\" relative to the workspace root\n"),
                ));
            }
            let sheet = s("sheet").unwrap_or_default().to_string();
            let cell = s("cell").unwrap_or_default().to_string();
            let value = s("value").unwrap_or_default().to_string();
            let rows: Vec<Vec<String>> = call
                .arguments
                .get("rows")
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_default();
            let creating = name == XLSX_WRITE_TOOL;
            // The approval preview is the change being asked for, not the file's
            // bytes: a human deciding on a spreadsheet write needs to see what it
            // does, and a workbook's raw bytes tell them nothing.
            let preview = if creating {
                format!(
                    "create workbook {path} sheet {sheet} with {} row(s)",
                    rows.len()
                )
            } else {
                format!("set {sheet}!{cell} to {value:?} in {path}")
            };
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(&preview),
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => {
                    let wrote = if creating {
                        crate::tools::documents::xlsx::write_new(ws, &target, &sheet, &rows)
                    } else {
                        crate::tools::documents::xlsx::set_cell(ws, &target, &sheet, &cell, &value)
                    };
                    match wrote {
                        Ok(w) => Dispatched::Continue {
                            decision: format!("wrote {target}"),
                            obs: format!(
                                "\n[{name} {target}] {}{}\n",
                                preview,
                                if w.moved_the_workspace() {
                                    ""
                                } else {
                                    " — identical to what was already there, the \
                                     workspace did not change"
                                }
                            ),
                            kind: ObsKind::Write,
                            target: Some(target.clone()),
                            origin: Origin::File,
                            changed: w.moved_the_workspace(),
                            remember,
                        },
                        Err(e) => Dispatched::go(
                            "spreadsheet write error",
                            format!("\n[{name} error] {e}\n"),
                        ),
                    }
                }
            }
        }
        // The remaining document readers. Same gate, same reason as the
        // spreadsheet arms above: `Act::Read` on the path the model named.
        #[cfg(any(
            feature = "docx",
            feature = "pptx",
            feature = "pdf",
            feature = "barcode"
        ))]
        n if is_document_read(n) => {
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
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match read_document(ws, name, &target) {
                    Ok(text) => Dispatched::Continue {
                        decision: format!("read {target}"),
                        obs: format!(
                            "\n[{name} {target}]\n{}\n",
                            bound(&text, cap, ObsKind::Read)
                        ),
                        kind: ObsKind::Read,
                        target: Some(target.clone()),
                        origin: Origin::File,
                        changed: false,
                        remember,
                    },
                    Err(e) => {
                        Dispatched::go("document read error", format!("\n[{name} error] {e}\n"))
                    }
                },
            }
        }
        // The remaining document writers.
        #[cfg(any(feature = "docx", feature = "pdf"))]
        n if is_document_write(n) => {
            let path = s("path").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "document missing path",
                    format!("\n[{name} error] needs a \"path\" relative to the workspace root\n"),
                ));
            }
            let preview = describe_document_write(name, &call.arguments);
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(&preview),
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match write_document(ws, name, &target, &call.arguments) {
                    Ok(w) => Dispatched::Continue {
                        decision: format!("wrote {target}"),
                        obs: format!(
                            "\n[{name} {target}] {preview}{}\n",
                            if w.moved_the_workspace() {
                                ""
                            } else {
                                " — identical to what was already there, the workspace \
                                 did not change"
                            }
                        ),
                        kind: ObsKind::Write,
                        target: Some(target.clone()),
                        origin: Origin::File,
                        changed: w.moved_the_workspace(),
                        remember,
                    },
                    Err(e) => {
                        Dispatched::go("document write error", format!("\n[{name} error] {e}\n"))
                    }
                },
            }
        }
        // A tool the embedding program registered. Registration made it
        // available; this check is what authorizes the call, on exactly the terms
        // an MCP tool gets — an exec check on the name the model used. Deciding
        // it here rather than at registration is what lets one policy layer hand
        // over a toolbox and another refuse a single tool in it.
        //
        // Registered tools are matched before the MCP arm and after the
        // built-ins, and `Toolbox::validate` has already guaranteed the three
        // sets are disjoint, so the order is documentation rather than a
        // tie-break.
        GIT_LOG_TOOL | GIT_STATUS_TOOL | GIT_DIFF_TOOL | GIT_ADD_TOOL | GIT_COMMIT_TOOL
        | GIT_BRANCH_TOOL | GIT_WORKTREE_TOOL => {
            // Paths the model named, if any. Every one of them is data: `argv`
            // puts them after `--` and refuses a leading `-`.
            let mut paths: Vec<String> = a
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // 0.36.0 — `git_worktree` names one path rather than a list, and it
            // is the location a directory is created at. Folding it into `paths`
            // here rather than carrying a second variable is what puts it
            // through the same gate loop, the same `check_path` and the same
            // `--` separator as every other model-supplied path in this crate.
            if name == GIT_WORKTREE_TOOL {
                paths = s("path")
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default();

                // 0.74.0, audit H3 — the worktree path is asked of
                // `Workspace::check_path` itself, not only of the gate below.
                // `gate` grades a *relative* read or write target through
                // `check_path`, but hands an *absolute* one to the policy
                // directly: that relaxation exists so `read_skill` can reach a
                // bundle outside the root, and it is exactly wrong for the one
                // git built-in that CREATES the path the model named.
                // `Git::argv`'s own check refuses a leading `-` and nothing
                // else, so under a policy whose writes are broad
                // `{"path":"/tmp/escaped"}` put a full checkout outside the
                // workspace and wrote an *allow* row to match. Asked here, an
                // absolute path and a `..` that climbs out are the same
                // refusal, with the same row, as every other path in this
                // crate.
                if let Some(path) = paths.first() {
                    let verdict = ws.check_path(Act::Write, path);
                    if verdict.effect == Effect::Deny {
                        let mut ev = PolicyEvent::refusal(step, "write", path.as_str());
                        ev.rule.clone_from(&verdict.rule);
                        ev.layer.clone_from(&verdict.layer);
                        store.record_event(run_id, &ev)?;
                        crate::run::refused(watch, run_id, depth, &ev);
                        let why = verdict
                            .rule
                            .as_deref()
                            .map(|r| format!(" (rule {r})"))
                            .unwrap_or_default();
                        return Ok(Dispatched::go(
                            "write refused",
                            format!(
                                "\n[write refused] {path}{why} — the policy forbids this; try \
                                 another path\n"
                            ),
                        ));
                    }
                }
            }

            // What the policy is asked, per tool. Reading history reads `.git`.
            // Staging copies a file's bytes into the object store, so it needs
            // `Act::Read` on that file — which is what stops a path the policy
            // denies from reaching a commit. Committing writes `.git`.
            //
            // 0.36.0: creating a branch writes a ref, so it writes `.git` and
            // names no path. A worktree writes `.git` *and* creates a directory
            // at the path the model chose, so that path is an `Act::Write` — the
            // only git built-in whose path is checked for writing rather than
            // reading, because it is the only one that makes a file the model
            // named rather than reading one.
            let (repo_act, path_act) = match name {
                GIT_ADD_TOOL => (Act::Write, Some(Act::Read)),
                GIT_COMMIT_TOOL | GIT_BRANCH_TOOL => (Act::Write, None),
                GIT_WORKTREE_TOOL => (Act::Write, Some(Act::Write)),
                _ => (Act::Read, Some(Act::Read)),
            };

            let mut remembered: Vec<Rule> = Vec::new();
            // 0.70.0 — spawning `git` is the first question, and until this
            // release it was the one question that never reached the approver.
            // `Git::run` asked the policy directly and refused anything that was
            // not `Allow`, so the default policy's `exec: Ask` refused every git
            // built-in with a message naming the program — which reads as "there
            // is no git here" rather than "nobody was asked". Put through the
            // same gate as the paths, it becomes an approval request and, if the
            // approver defers, a pause the run can be resumed from.
            //
            // First rather than last because it is the coarsest: a run that may
            // not spawn `git` at all should not be asked about the individual
            // files it wanted to stage. The target is `GIT` itself, the string
            // `Git::run` checks, so the approver is asked about what actually
            // runs.
            //
            // **The exec target is the program and only the program** (0.74.0,
            // audit L8). `deny_exec("git")` refuses every built-in in this arm;
            // `deny_exec("git commit*")` refuses none of them, because no
            // built-in ever presents a joined argv the way `exec` does. It could
            // not: the argv these tools build carries the `-c` hardening flags
            // between the program and the sub-command, so the string an operator
            // would have to write is one this crate composes rather than one
            // they chose. A sub-command is denied by naming what it touches —
            // `deny_write(".git")` stops `git_commit` and `git_branch`, a
            // `deny_read` on a path stops `git_add` staging it — which is the
            // check the paths below perform.
            let mut targets: Vec<(Act, String)> = vec![
                (Act::Exec, crate::tools::git::GIT.to_string()),
                (repo_act, GIT_DIR.to_string()),
            ];
            if let Some(act) = path_act {
                targets.extend(paths.iter().map(|p| (act, p.clone())));
            }
            let mut refused: Option<Dispatched> = None;
            for (act, target) in targets {
                match gate(
                    ws, approver, store, run_id, step, act, &target, None, watch, depth, goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => {
                        refused = Some(Dispatched::go(decision, obs));
                        break;
                    }
                    Gated::Paused { request_id } => {
                        refused = Some(Dispatched::Pause { request_id });
                        break;
                    }
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }
            if let Some(d) = refused {
                return Ok(d);
            }

            let cmd = match name {
                GIT_STATUS_TOOL => GitCmd::Status { paths },
                GIT_DIFF_TOOL => GitCmd::Diff {
                    staged: a.get("staged").and_then(serde_json::Value::as_bool) == Some(true),
                    paths,
                },
                GIT_LOG_TOOL => GitCmd::Log {
                    // Clamped rather than trusted: a model asking for the whole
                    // history of a large repository would blow the observation
                    // cap and learn nothing the first twenty commits do not say.
                    max_count: a
                        .get("max_count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(20)
                        .clamp(1, 200) as u32,
                    paths,
                },
                GIT_ADD_TOOL => {
                    if paths.is_empty() {
                        return Ok(Dispatched::go(
                            "git_add missing paths",
                            "\n[git error] git_add needs a non-empty \"paths\" array\n".to_string(),
                        ));
                    }
                    GitCmd::Add { paths }
                }
                GIT_BRANCH_TOOL => {
                    let Some(branch) = s("name").filter(|n| !n.trim().is_empty()) else {
                        return Ok(Dispatched::go(
                            "git_branch missing name",
                            "\n[git error] git_branch needs a non-empty \"name\"\n".to_string(),
                        ));
                    };
                    GitCmd::Branch {
                        name: branch.to_string(),
                    }
                }
                GIT_WORKTREE_TOOL => {
                    let Some(branch) = s("name").filter(|n| !n.trim().is_empty()) else {
                        return Ok(Dispatched::go(
                            "git_worktree missing name",
                            "\n[git error] git_worktree needs a non-empty \"name\"\n".to_string(),
                        ));
                    };
                    // `paths` holds exactly the `path` argument, or nothing.
                    let Some(path) = paths.into_iter().next() else {
                        return Ok(Dispatched::go(
                            "git_worktree missing path",
                            "\n[git error] git_worktree needs a non-empty \"path\"\n".to_string(),
                        ));
                    };
                    GitCmd::Worktree {
                        name: branch.to_string(),
                        path,
                    }
                }
                _ => {
                    let Some(message) = s("message").filter(|m| !m.trim().is_empty()) else {
                        return Ok(Dispatched::go(
                            "git_commit missing message",
                            "\n[git error] git_commit needs a non-empty \"message\"\n".to_string(),
                        ));
                    };
                    GitCmd::Commit {
                        message: message.to_string(),
                        identity: identity.clone(),
                    }
                }
            };

            // 0.48.0 — `exec_sandbox` here is this call's own containment, already
            // narrowed to what the tool declared: `read-only` for the three
            // readers, `workspace-write` for the four that touch `.git`. A run
            // granting `FullAccess` hands `None` and this spawn is what it always
            // was.
            let contained = exec_sandbox
                .map(|c| std::sync::Arc::new(c.with_egress(ws.policy().permits_any_egress())));
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
            // `.gated()` because the loop above already put `Act::Exec` on `GIT`
            // through `gate`: the policy allowed it, or an approver said yes. A
            // second raw policy check inside `Git::run` would read the same `Ask`
            // and refuse the call a human had just approved.
            let git = Git::new(ws.policy(), ws.root(), cap)
                .contained(contained.clone())
                .gated();
            // 0.21.0 — a refused git built-in costs a step, not the run.
            //
            // Until here, `Git::run`'s refusal left the loop as `Error::Refused`, so
            // one speculative `git status` under a policy denying `Act::Exec` for
            // `git` escalated the whole run. Found while running the 0.20.0 live
            // session and recorded in `docs/CONTRACT.md`; not fixed there because
            // that release touched no tool. Every other refusal in this crate is an
            // observation the model reads and adapts to, and this is now one too.
            //
            // Both of `Git::run`'s refusals land here — the policy denying the `git`
            // program, and a path that would be read as an option — and the row is
            // written exactly as `gate` writes one, so a reader cannot tell a git
            // refusal from any other and does not have to.
            let outcome = match git.run(&cmd).await {
                Ok(out) => out,
                Err(Error::Refused {
                    act,
                    target,
                    rule,
                    layer,
                }) => {
                    let mut ev = PolicyEvent::refusal(step, act.clone(), target.clone());
                    ev.rule = rule.clone();
                    ev.layer = layer.clone();
                    store.record_event(run_id, &ev)?;
                    // Qualified: this arm has a local `refused` holding the gate's
                    // verdict for the paths, which shadows the function.
                    crate::run::refused(watch, run_id, depth, &ev);
                    let why = rule
                        .as_deref()
                        .map(|r| format!(" (rule {r})"))
                        .unwrap_or_default();
                    return Ok(Dispatched::go(
                        format!("{name} refused"),
                        format!(
                            "\n[{act} refused] {target}{why} — the policy forbids this; carry on \
                             without git\n"
                        ),
                    ));
                }
                Err(e) => return Err(e),
            };
            if contained.is_some() {
                record_sandbox_step(
                    store,
                    watch,
                    depth,
                    &crate::state::SandboxEvent::destroy(run_id, step),
                );
            }
            match outcome {
                GitOutcome::Unavailable { reason } => Dispatched::go(
                    "git unavailable",
                    format!(
                        "\n[git unavailable] {reason}. This workspace cannot be worked as a git \
                         repository; carry on without it.\n"
                    ),
                ),
                out @ GitOutcome::Ran { .. } => {
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
                    // failure — the same treatment a malformed regex gets from
                    // `grep`. The model reads the message and adapts.
                    Dispatched::Continue {
                        decision: format!(
                            "{name} {}",
                            if ok {
                                "ok".to_string()
                            } else {
                                format!("exit {}", code.map_or("signal".into(), |c| c.to_string()))
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
                        // The overlapping half's origin, stated the same way and
                        // for the same reason — see `ReadWork::Git` in
                        // `src/run/read.rs`.
                        origin: Origin::Tool,
                        changed: matches!(cmd, GitCmd::Add { .. } | GitCmd::Commit { .. }) && ok,
                        remember: remembered,
                    }
                }
            }
        }
        // A registered tool. Gated as an exec check on its name, then invoked —
        // the same two halves as a built-in read, and through the same code, so a
        // tool that declares itself read-only behaves identically whether it ran
        // beside another call or on its own.
        name if custom.owns(name) => {
            match prepare_read(
                ws,
                call,
                approver,
                store,
                run_id,
                step,
                custom,
                watch,
                depth,
                goal,
                exec_sandbox,
            )
            .await?
            {
                Prepared::Work(work) => {
                    // 0.65.0 — `prepare_read` opened the journal row; this is the
                    // one place that knows the call returned. Read before the
                    // work is consumed.
                    let attempt = work.attempt();
                    let done = work.run(ws, cap, max_read, run_id, step).await;
                    if let Some(id) = attempt {
                        store.close_attempt(id)?;
                    }
                    done
                }
                Prepared::Done(done) | Prepared::Stop(done) => done,
            }
        }
        // An MCP tool. Invoking it is an exec check on its namespaced name, so a
        // policy can allow a server generally and still deny one of its tools.
        name if mcp.owns(name) => {
            // 0.70.0 — through `gate`, not a bare policy check. This arm used to
            // refuse anything that was not `Allow`, which turned `Effect::Ask`
            // into a silent deny for every MCP tool a policy had not named
            // outright: the sibling of the git defect, on the surface an operator
            // is most likely to want a prompt for. A deferral now pauses the run
            // with a pending row an attached process can answer, exactly as a
            // write does.
            let remember = match gate(
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
                Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                // No rewritten form arrives here. An approver may narrow or
                // deny, but "call a different MCP tool than the model asked for"
                // is a substitution this arm has never made and the model's own
                // arguments would not follow it — so since 0.74.0 (audit M4)
                // `gate` REFUSES a `modified` request on an `Act::Exec` rather
                // than returning it for a consumer to drop. A `Gated::Go` on this
                // path is therefore always the tool the model named. The
                // remembered rules do travel, so "allow this tool from now on"
                // sticks for the rest of the run exactly as it does for a write.
                Gated::Go { remember, .. } => remember,
            };
            // 0.65.0 — an MCP server declares nothing about whether its tools may
            // be called twice, and this crate cannot see what one does, so every
            // call is `Indeterminate`. Opened before the call and closed after it,
            // both here, because this arm is the only route to a server.
            let attempt =
                store.open_attempt(run_id, step, name, crate::ToolRecovery::Indeterminate)?;
            let out = mcp
                .call_media(
                    name,
                    &call.arguments,
                    store,
                    run_id,
                    step,
                    cap,
                    watch,
                    depth,
                    pending_media,
                )
                .await?;
            if let Some(id) = attempt {
                store.close_attempt(id)?;
            }
            Dispatched::Continue {
                decision: format!("called {name}"),
                obs: format!("\n[{name}]\n{out}\n"),
                kind: ObsKind::Mcp,
                target: Some(name.to_string()),
                // A server this crate does not run answered. `ObsKind::Mcp`
                // happens to agree here, and the funnel still must not read it
                // off the kind: the two agree for MCP and disagree for `exec`,
                // the browser and a registered tool, which all wear
                // `ObsKind::Tool`.
                origin: Origin::Mcp,
                changed: false,
                remember,
            }
        }
        other => Dispatched::go(
            format!("unknown tool {other}"),
            format!("\n[unknown tool {other}]\n"),
        ),
    })
}

/// Put a navigation tool's `path` through the gate, then ask the language
/// server.
///
/// 0.74.0, audit H7. [`crate::lsp::LspSession::navigate`] already refuses a path
/// that leaves the root, but it refuses it *silently*: it has no `Store`, no
/// `step` and no `depth`, and every persisted `policy_events` row in this crate
/// is written by [`gate`]. A read that crossed the boundary with no row is the
/// exact shape H7 is about, so the row is written here, where the run's identity
/// is in scope — and writing it through `gate` rather than by hand is what keeps
/// one spelling of "what a refusal looks like" instead of two.
///
/// **A policy whose reads are `Ask` now prompts on a navigation.** That is
/// visible to an operator who had one, and it is the same treatment `read_file`
/// gets: a question about a file the model chose is a question, not a silent
/// pass. The floor inside `navigate` still stands underneath — it refuses a
/// `Deny` on its own, so a caller reaching the session without this loop is no
/// weaker than it was.
///
/// One function for all three arms, not three copies, for the reason `navigate`
/// checks in one place: a boundary written three times is a boundary missing
/// from one of them.
#[allow(clippy::too_many_arguments)]
async fn navigate_gated(
    name: &str,
    path: Option<&str>,
    ask: crate::lsp::Nav<'_>,
    ws: &Workspace,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    lsp: &LspSession,
    watch: &Watch<'_>,
    depth: u32,
    goal: &str,
    cap: usize,
) -> Result<Dispatched> {
    // `lsp_symbols` may name a query instead of a path, and a missing `path` on
    // the other four is already an error the server answers. Nothing to gate.
    let remembered = match path.filter(|p| !p.is_empty()) {
        None => Vec::new(),
        Some(path) => {
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
                Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                // The rewritten target is not read back: `Nav` was built from the
                // model's own arguments and a redirected navigation would answer
                // a question nobody asked. An approver narrows a navigation by
                // denying it. The remembered rules travel, so an "always allow
                // this" answered at a hover prompt is not asked again.
                Gated::Go { remember, .. } => remember,
            }
        }
    };
    let mut done = navigated(name, lsp.navigate(ask, ws, run_id, watch).await, cap);
    if let Dispatched::Continue { remember, .. } = &mut done {
        *remember = remembered;
    }
    Ok(done)
}

/// What the policy and the approver together decided about one action.
pub(super) enum Gated {
    /// Perform it, possibly in the form an approver rewrote it into.
    Go {
        target: String,
        content: Option<String>,
        remember: Vec<Rule>,
    },
    /// Do not perform it; `obs` is what the model is told.
    Refused { decision: String, obs: String },
    /// An approver deferred the decision.
    Paused { request_id: i64 },
}

/// Run the project's own checker after a write and render what it found.
///
/// Shared by `edit_file` and `write_file` because they are the same event as far
/// as a compiler is concerned: a file changed. A write is in fact where most new
/// code arrives, so exempting it would leave the feature answering the easier
/// half of the question.
///
/// Renders to a string rather than returning the outcome, because every caller
/// wants the same thing — something to append to the observation — and the
/// distinction that matters to them is only whether there is anything to say.
///
/// **This can never fail a write.** The file is already on disk by the time this
/// runs. A checker that is missing, times out, or falls over for its own reasons
/// produces a note saying so and nothing else; it does not turn a successful
/// write into a failed one, and it does not return an error. An information
/// feature that can take down the tool it informs on is a worse trade than not
/// having it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn diagnostics_after_write(
    ws: &Workspace,
    toolchain: Option<&crate::toolchain::Toolchain>,
    timeout: Duration,
    cap: usize,
    lsp: &LspSession,
    run_id: i64,
    watch: &Watch<'_>,
    // 0.74.0 — the containment this call was granted. The check after a write
    // spawns the project's compiler, which runs the workspace's own build
    // script; before this release it did so ungated and uncontained (audit C2).
    exec_sandbox: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> String {
    let root = ws.root();
    // 0.52.0 — what a configured server sees, appended to what the compiler said
    // and never in place of it. Findings only here: nobody asked, so a line per
    // edit about a server that cannot answer is noise the model pays for on every
    // write. `check` reports all four states, because there somebody did ask.
    let served = lsp_diagnostics_text(&lsp.diagnose(ws, None, run_id, watch).await, false);
    match crate::tools::diagnostics::after_edit(
        root,
        toolchain,
        timeout,
        cap,
        ws.policy(),
        exec_sandbox,
    )
    .await
    {
        crate::tools::diagnostics::Outcome::Found(text) => format!("{text}{served}"),
        crate::tools::diagnostics::Outcome::Clean => served,
        // A skip is silent. There is no ecosystem here, so there is nothing the
        // model could do differently and nothing worth spending its context on.
        crate::tools::diagnostics::Outcome::Skipped(_) => served,
        // A failure is not silent, and that is the point. An absent diagnostics
        // section and a check that never ran look identical to a model, and one
        // of them means "this file is fine" while the other means nothing at
        // all.
        crate::tools::diagnostics::Outcome::Failed(why) => {
            format!("\n[check did not run] {why}\n{served}")
        }
    }
}

/// What checking a parsed shell line decided.
///
/// The check either clears the whole line — handing back the rules an approver
/// asked to remember — or it stops the call, and a stop is a finished
/// [`Dispatched`] rather than an error: a refused line is something the model can
/// recover from by writing a different one.
pub(super) enum ShellCheck {
    Go(Vec<Rule>),
    Stop(Dispatched),
}

/// Check every sub-command and every redirect target of a parsed line, before
/// anything is spawned.
///
/// Extracted so that the foreground `shell` tool and the handle-starting
/// `shell_start` cannot drift apart. That is not tidiness: the whole claim of
/// both tools is that what was checked is what runs, and two copies of this loop
/// would be two accept-sets to keep in agreement forever. `tests/shell.rs` drives
/// its refusal table through both tools for the same reason — the shared function
/// is the mechanism, the shared table is the proof.
///
/// The order matters and is the reason this is one pass rather than a check
/// folded into the runner: a line whose second stage is denied must not run its
/// first, so every decision for the whole line is taken here and the caller
/// spawns only after this returns [`ShellCheck::Go`].
#[allow(clippy::too_many_arguments)]
pub(super) async fn check_shell_line(
    ws: &Workspace,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    watch: &Watch<'_>,
    depth: u32,
    parsed: &crate::tools::shell::Line,
    plan: &[crate::tools::shell::Planned],
    goal: &str,
) -> Result<ShellCheck> {
    let mut remembered: Vec<Rule> = Vec::new();
    for (cmd, planned) in parsed.commands().zip(plan.iter()) {
        if let Some(dest) = &planned.cd_target {
            // `cd` spawns nothing, so there is no program to check against
            // `Act::Exec`. What it does do is choose where every later
            // stage runs, which is a read of that directory.
            let rel = relative_to(ws.root(), dest);
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                &rel,
                None,
                watch,
                depth,
                goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => {
                    return Ok(ShellCheck::Stop(Dispatched::go(decision, obs)))
                }
                Gated::Paused { request_id } => {
                    return Ok(ShellCheck::Stop(Dispatched::Pause { request_id }))
                }
                Gated::Go { remember, .. } => remembered.extend(remember),
            }
            record_shell_authorisation(ws, store, run_id, step, Act::Read, &rel)?;
        } else {
            // The same two targets `exec` checks, per sub-command rather
            // than per call: the program alone, which is what
            // `deny_exec("rm")` names, and this stage's own joined argv,
            // which is what `allow_exec("git log*")` names. Checking the
            // whole line as one string could not tell one stage from
            // another, which is the entire point of parsing it.
            let program = cmd.argv[0].clone();
            let joined = cmd.argv.join(" ");
            let mut targets = vec![program.clone()];
            if joined != program {
                targets.push(joined);
            }
            for target in targets {
                match gate(
                    ws,
                    approver,
                    store,
                    run_id,
                    step,
                    Act::Exec,
                    &target,
                    None,
                    watch,
                    depth,
                    goal,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => {
                        return Ok(ShellCheck::Stop(Dispatched::go(decision, obs)))
                    }
                    Gated::Paused { request_id } => {
                        return Ok(ShellCheck::Stop(Dispatched::Pause { request_id }))
                    }
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
                record_shell_authorisation(ws, store, run_id, step, Act::Exec, &target)?;
            }
        }

        // Redirect targets are paths, so they take the path-resolved check
        // `write_file` and `read_file` take — not the name match an
        // `Act::Exec` rule performs. A rule denying `secrets/*` has to
        // catch `> secrets/key` for the boundary to mean anything.
        for (kind, target) in &planned.redirects {
            let Some(path) = target else { continue };
            let act = if kind.is_write() {
                Act::Write
            } else {
                Act::Read
            };
            let rel = relative_to(ws.root(), path);
            match gate(
                ws, approver, store, run_id, step, act, &rel, None, watch, depth, goal,
            )
            .await?
            {
                Gated::Refused { decision, obs } => {
                    return Ok(ShellCheck::Stop(Dispatched::go(decision, obs)))
                }
                Gated::Paused { request_id } => {
                    return Ok(ShellCheck::Stop(Dispatched::Pause { request_id }))
                }
                // 0.80.0 — the rewrite is refused rather than discarded, which is
                // M4's decision for `Act::Exec` reaching the one read/write site
                // that has an exec's shape. `gate` honours a rewritten read or
                // write target because the two tool paths take their target off
                // the gate's own answer; a redirect does not — the command line
                // was parsed before this gate ran and the shell runs the
                // redirect it parsed. Discarding it meant an approver narrowing
                // `> secrets/key` to something harmless was overruled in silence
                // and the original still ran.
                Gated::Go {
                    target: performed,
                    remember,
                    ..
                } => {
                    if performed != rel {
                        let ev = PolicyEvent::refusal(step, act_word(act), &rel)
                            .with_performed(&performed);
                        store.record_event(run_id, &ev)?;
                        crate::run::refused(watch, run_id, depth, &ev);
                        return Ok(ShellCheck::Stop(Dispatched::go(
                            format!("{} rewrite refused", act_word(act)),
                            format!(
                                "\n[{} refused] {rel} — the approver rewrote it to {performed}, \
                                 and a rewritten redirect is not something this path can run: the \
                                 command line was parsed before the approval. Nothing ran. \
                                 Approve {rel} as asked, deny it, or narrow it with a rule.\n",
                                act_word(act)
                            ),
                        )));
                    }
                    remembered.extend(remember);
                }
            }
            record_shell_authorisation(ws, store, run_id, step, act, &rel)?;
        }
    }
    Ok(ShellCheck::Go(remembered))
}

/// Record that one sub-command or one redirect target of a `shell` line was
/// authorised, with the rule and layer that authorised it.
///
/// The crate does not otherwise record allows. A [`PolicyEvent`] is written for a
/// refusal and for an approver's decision, and a permitted read or write leaves
/// no row — which is right for a single-target tool, where the step's own
/// decision already says what happened to the one thing it touched.
///
/// A `shell` line is not a single target. `a | b > out` is three authorisations,
/// and a trace that recorded only "shell exit 0" would leave an operator unable
/// to answer which stage was allowed to run and under which rule. So this is the
/// one place allows are recorded, and it is recorded per stage rather than per
/// line because per line is the thing that carries no information.
///
/// **This is a record, not a second boundary.** [`gate`] has already decided and
/// has already refused or paused if it was going to; re-evaluating here would be
/// a check that could disagree with the one that actually held. The verdict is
/// recomputed only to name the deciding rule, which `gate` does not hand back,
/// and the match below mirrors `gate`'s own dispatch exactly so the rule named is
/// the rule that decided.
pub(super) fn record_shell_authorisation(
    ws: &Workspace,
    store: &Store,
    run_id: i64,
    step: u32,
    act: Act,
    target: &str,
) -> Result<()> {
    let verdict = match act {
        Act::Exec | Act::Net => ws.policy().check(act, target),
        Act::Read | Act::Write if Path::new(target).is_absolute() => ws.policy().check(act, target),
        Act::Read | Act::Write => ws.check_path(act, target),
    };
    // Only an allow. A refusal already has its own row from `gate`, and an
    // approver's decision already has one too; writing a second would double-count
    // the same act in a trace an operator reads as a list of what happened.
    if verdict.effect != Effect::Allow {
        return Ok(());
    }
    let mut ev = PolicyEvent::decision(
        step,
        format!("{act:?}").to_lowercase(),
        target,
        "allow",
        "policy",
    );
    ev.rule = verdict.rule;
    ev.layer = verdict.layer;
    store.record_event(run_id, &ev)
}

/// The workspace-relative form of an absolute path the shell planner resolved.
///
/// [`gate`] checks a read or a write through `Workspace::check_path`, which takes
/// a path relative to the root — the same string a `write_file` call carries. The
/// shell planner works in absolute paths, because absolute is what it must hand
/// to the operating system. This is the only conversion between the two, kept in
/// one place so that the string the policy sees and the path the process opens
/// cannot drift apart: two conversions would be two chances to disagree, and the
/// disagreement would be silent.
///
/// A path equal to the root becomes `.` rather than the empty string, which is
/// what a `cd` with no argument produces and what a policy rule for the root
/// itself is written against.
pub(super) fn relative_to(root: &std::path::Path, path: &std::path::Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.to_string_lossy().into_owned()
    }
}
