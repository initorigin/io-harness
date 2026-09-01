//! gate: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// The outcome of authorizing the provider's own endpoint, before a run makes
/// its first outbound call.
pub(super) enum ProviderAccess {
    /// Cleared to run, under the policy the provider layer has been merged into.
    Granted(Policy),
    /// An approver deferred; the pending decision is persisted under this id and
    /// the run stops before it ever dials.
    Pending(i64),
}

/// Authorize the provider's endpoint once, before the first completion.
///
/// Once per run rather than once per step: a provider's endpoint is fixed for
/// the life of the provider, so re-asking each step would be the same question
/// with the same answer — and asking a human it repeatedly would train them to
/// wave it through.
///
/// The provider layer is merged *before* the check for an endpoint of a trusted
/// origin, and only for one (0.74.0, audit finding C3). That ordering is what
/// makes a network-deny base usable: the `net` default denies, the provider
/// layer's allow rule beats a default, and a caller's explicit `deny_net` still
/// beats the allow because deny is absolute across layers. So "deny everything
/// but the model" needs no host list from the caller, while "deny even the model"
/// remains expressible — and fails fast as a refusal rather than hanging on a
/// call that is never made.
///
/// An endpoint of an untrusted origin is put to the caller's own policy first,
/// where a deny answers before the overlay can widen anything. The two origins
/// that stay exempt are the user-scope `io.toml` and a provider the embedder
/// built in its own Rust — see [`net::ProviderOrigin`] for how the distinction
/// travels, and why an unmarked policy means the second of those.
pub(super) async fn authorize_provider<P: Provider>(
    provider: &P,
    policy: &Policy,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
    watch: &Watch<'_>,
    goal: &str,
) -> Result<ProviderAccess> {
    // A provider that opens no connection (the mock providers tests drive the
    // loop with) has no endpoint to authorize.
    // Every host in the chain, not just the first: a `Fallback` can dial its
    // secondary, and a host the policy never checked would be a hole in an egress
    // model that is deny-by-default everywhere else.
    let urls = provider.endpoints();
    if urls.is_empty() {
        // A provider that opens no connection (the mock providers the tests drive
        // the loop with) has no endpoint to authorize.
        return Ok(ProviderAccess::Granted(policy.clone()));
    }

    let mut effective = policy.clone();
    let mut ask: Option<(String, crate::policy::Verdict)> = None;
    for url in urls {
        let Some(target) = net::target(url) else {
            return Err(crate::error::Error::Refused {
                act: "net".into(),
                target: url.to_string(),
                rule: None,
                layer: None,
            });
        };
        // C3 — the overlay below is an *allow* for this host. Until 0.74.0 it was
        // merged before the host was checked, so a caller whose `net` default is
        // Deny was never asked about its own provider's endpoint. It is asked
        // first now, unless the endpoint came from an origin the operator owns.
        //
        // Against `policy`, never `effective`: `effective` already carries the
        // overlay of every endpoint checked before this one, and a fallback
        // chain's first host must not authorize its second.
        //
        // Only a Deny is intercepted. An `Ask` is still the overlay's to answer
        // below, so this narrows the ordering and cannot turn an allowance into a
        // question the caller never asked for.
        if net::provider_origin(policy, &target) == net::ProviderOrigin::Untrusted
            && policy.check(Act::Net, &target).effect == Effect::Deny
        {
            // Through the guard rather than as a bare `Error::Refused`, so the
            // refusal is recorded and announced the way every other one is. It
            // re-reads the same policy and reaches the same Deny.
            NetGuard::new(policy)
                .tracing(store, run_id, 0)
                .watching(watch, 0)
                .check_target(&target)?;
        }
        effective = effective.merge(net::provider_layer(&target));
        // Step 0: the authorization happens before the run's first step.
        let verdict = NetGuard::new(&effective)
            .tracing(store, run_id, 0)
            .watching(watch, 0)
            .check_target(&target)?;
        if verdict.effect == Effect::Ask {
            // One human decision covers the run; the first host that needs asking
            // is the one asked about. The verdict rides along because the approver
            // is told which rule and which layer asked (0.42.0), and only the
            // asking host's verdict is the answer to that.
            ask = Some((target.clone(), verdict));
        }
    }

    let Some((target, verdict)) = ask else {
        return Ok(ProviderAccess::Granted(effective));
    };

    // The run is now waiting on a human, before its first step. Step 0 for the
    // same reason the rows below are: the authorization precedes step 1.
    watch.emit(RunEvent::new(
        run_id,
        0,
        EventKind::ApprovalRequested {
            act: "net".into(),
            target: target.clone(),
        },
    ));
    // 0.33.0 — durable before the gate, so a run parked here before its first step
    // is answerable by a second process rather than only by killing it. See
    // `gate_path` for the same ordering and the same reason.
    let request_id = store.put_pending(run_id, 0, "net", &target, None)?;
    let request = Request::new(Act::Net, &target);
    let context = approval_context(goal, &verdict);
    let raced = race_gate(approver.decide_in_context(&request, &context), store, |s| {
        Ok(s.pending(request_id)?.is_some_and(|p| p.resolved.is_some()))
    })
    .await?;

    if matches!(raced, Some(Decision::Defer)) {
        let ev = PolicyEvent::decision(0, "net", &target, "defer", "approver");
        store.record_event(run_id, &ev)?;
        decided(watch, run_id, 0, &ev);
        finish(store, watch, run_id, 0, 0, "awaiting_approval")?;
        return Ok(ProviderAccess::Pending(request_id));
    }

    let reason = match &raced {
        Some(Decision::Deny { reason }) => reason.clone(),
        _ => "answered by an attached process".to_string(),
    };
    let mine = match &raced {
        Some(Decision::Approve { .. }) => store.resolve_pending(request_id, "approve")?,
        Some(Decision::Deny { .. }) => store.resolve_pending(request_id, "deny")?,
        _ => false,
    };
    // The row decides, not the value we raced with. Read back in both arms.
    let landed = store
        .pending(request_id)?
        .and_then(|p| p.resolved)
        .unwrap_or_else(|| "deny".to_string());
    let source = if mine { "approver" } else { "attached" };

    let ev = PolicyEvent::decision(0, "net", &target, &landed, source);
    store.record_event(run_id, &ev)?;
    decided(watch, run_id, 0, &ev);
    if landed == "approve" {
        return Ok(ProviderAccess::Granted(effective));
    }
    // Step 0: the run never started, so it finished having taken no steps.
    finish(store, watch, run_id, 0, 0, "refused")?;
    Err(crate::error::Error::Refused {
        act: "net".into(),
        target: format!("{target} — {reason}"),
        // A human denied it, so the refusal is theirs, not a rule's: there is no
        // rule to name.
        rule: None,
        layer: None,
    })
}

/// The responder a contract carries, or one that answers nothing.
///
/// A `static` rather than a local, so the "nobody answers" case is a reference to one
/// value instead of a temporary every call site has to keep alive. Answering nothing
/// is the default and the honest one for unattended work: the question persists and
/// the run pauses, so waiting costs nothing.
pub(super) static NO_RESPONDER: ResponderNone = ResponderNone;

pub(super) fn responder_of(contract: &TaskContract) -> &dyn Responder {
    match &contract.responder {
        Some(r) => r.as_ref(),
        None => &NO_RESPONDER,
    }
}

/// The result of dispatching one tool call.
/// What the dispatch needs to know about the plan gate, in one parameter rather
/// than three.
///
/// `active` is read from the store at every loop entry rather than carried in a
/// local, which is the whole of the durability claim: a run approved in one
/// process and killed in the next does not plan again, and one that was never
/// approved does not start writing because the approval died with the process
/// that held it.
#[derive(Clone, Copy)]
pub(crate) struct PlanPhase<'a> {
    /// Who reviews a proposal, or `None` when no gate is registered.
    pub(super) gate: Option<&'a dyn PlanGate>,
    /// The roster a proposed step's owner must be on.
    pub(super) agents: &'a crate::agent::Agents,
    /// Whether the run is still waiting for an approved plan.
    pub(super) active: bool,
}

/// The policy layer that holds a run still while its plan is unreviewed.
///
/// Denying rather than filtering the tool list, because [`Policy::explain`]
/// resolves deny-first across every layer and every mutating path in the crate
/// already goes through it: `write_file`, `edit_file`, `exec`, the shell tools and
/// `git` are `Write` or `Exec` checks, and a registered [`Tool`](crate::Tool) and
/// an MCP tool are `Exec` checks on their own names. A list of tool names would be
/// complete on the day it was written and wrong the next time one was added.
///
/// `Read` and `Net` are untouched. Reads stay open because a plan written without
/// looking at the workspace is not worth gating, and the provider is reached over
/// the network — denying `Net` here would stop the run from asking the model for
/// the plan in the first place.
pub(super) fn plan_lock() -> Policy {
    Policy::permissive()
        .layer("plan-gate")
        .deny_write("*")
        .deny_exec("*")
}

/// The system prompt while a plan is unreviewed.
///
/// `classifying` is the one path that composes this block above
/// [`CONVERSATIONAL_ENDING`] (0.60.3). A turn that has not been decided to be work
/// was being ordered to plan before it was permitted to answer — the directive said
/// "before you do anything else", the ending said "if a plain answer is the whole of
/// what is wanted, call no tool", and an operator who typed a greeting into a gated
/// session got a plan proposed for it and a human asked to approve one. So the gate
/// binds the *work* reading there and says so. It is not weakened: the sentence about
/// what is refused is the same in both forms, and [`plan_lock`] is what enforces it
/// either way — what changes is that a turn allowed to answer is not told the answer
/// must wait for a plan.
///
/// One function with a flag rather than two strings, for the reason
/// [`CONVERSATIONAL_ENDING`] is a `const`: a rule reworded in one of them and not the
/// other is a gate that reads differently depending on which block composed it.
pub(super) fn planning_directive(agents: &crate::agent::Agents, classifying: bool) -> String {
    let roster = match agents.len() {
        0 => {
            "There are no sub-agents on this run, so leave `agent` unset on every step.".to_string()
        }
        _ => format!(
            "The agents you may hand a step to are: {}. Leave `agent` unset for a step you \
             will do yourself; naming one that is not on that list is refused.",
            agents.names().join(", ")
        ),
    };
    let order = match classifying {
        true => format!(
            "If any part of this needs the repository written to or a command run, then before \
             you do that or anything else you must call `{PROPOSE_PLAN_TOOL}` with the ordered \
             steps you intend to take, and wait."
        ),
        false => format!(
            "Before you do anything else you must call `{PROPOSE_PLAN_TOOL}` with the ordered \
             steps you intend to take, and wait."
        ),
    };
    format!(
        " {order} Until that plan is approved you may read, search \
         and think, and every attempt to write a file, run a command or call any other tool \
         WILL be refused — so read what you need first, then propose. {roster}"
    )
}

/// The `propose_plan` tool, offered only while the phase is active.
pub(super) fn propose_plan_spec() -> ToolSpec {
    ToolSpec {
        name: PROPOSE_PLAN_TOOL.to_string(),
        description: "Propose the ordered steps you intend to take, then wait for a human to \
                      approve them. Nothing you propose here is done by proposing it, and \
                      nothing else you can do will work until this is approved — writes, \
                      commands and every other tool are refused while a plan is outstanding. \
                      Read enough of the workspace first that the plan is worth reviewing."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "The plan, in the order you intend to carry it out.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "intent": { "type": "string", "description": "What this step does, in a short phrase." },
                            "agent": { "type": "string", "description": "Optional: the sub-agent that will own this step. Omit for a step you will do yourself." }
                        },
                        "required": ["intent"]
                    }
                }
            },
            "required": ["steps"]
        }),
    }
}

/// Read a `propose_plan` argument object into a [`Plan`], or say what was wrong.
///
/// Strict for the reason [`parse_todo_items`] is, and more so: this is the object
/// a human is about to be shown and asked to approve, and a step whose owner the
/// crate silently dropped would be approved on false terms.
pub(super) fn parse_plan(
    args: &serde_json::Value,
    agents: &crate::agent::Agents,
) -> std::result::Result<Plan, String> {
    let list = args
        .get("steps")
        .ok_or_else(|| "`steps` is required: send the whole plan as a list".to_string())?
        .as_array()
        .ok_or_else(|| "`steps` must be a list of {intent, agent} objects".to_string())?;
    if list.is_empty() {
        return Err("a plan with no steps is not a plan; say what you intend to do".to_string());
    }
    let mut steps = Vec::with_capacity(list.len());
    for (i, raw) in list.iter().enumerate() {
        let intent = raw
            .get("intent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("step {} needs a non-empty `intent`", i + 1))?;
        let mut step = PlanStep::new(intent);
        if let Some(agent) = raw
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            // The check the whole `agent` field exists for. A plan naming an owner
            // that cannot be spawned is a plan that cannot be carried out, and
            // finding that out at approval time costs one step instead of a run.
            if agents.get(agent).is_none() {
                let known: Vec<&str> = agents.names();
                return Err(match known.is_empty() {
                    true => format!(
                        "step {} names agent `{agent}`, and this run has no agents at all; \
                         leave `agent` unset",
                        i + 1
                    ),
                    false => format!(
                        "step {} names agent `{agent}`, which is not on this run's roster; \
                         the agents are: {}",
                        i + 1,
                        known.join(", ")
                    ),
                });
            }
            step = step.by(agent);
        }
        steps.push(step);
    }
    Ok(Plan::new(steps))
}

/// Read one offered choice, in either spelling, or say what was wrong with it.
///
/// A JSON string is a bare label; an object is read by field. Strict for the reason
/// [`parse_plan`] is: this is what an operator is about to be shown, and a malformed
/// offer the crate silently dropped would be answered on false terms.
fn parse_choice(raw: &serde_json::Value, at: &str) -> std::result::Result<Choice, String> {
    if let Some(label) = raw.as_str() {
        return match label.trim().is_empty() {
            // The rule `parse_plan` already holds for an empty `intent`, rather than a
            // second rule invented for the same shape.
            true => Err(format!("{at} has an empty label")),
            false => Ok(Choice::new(label.trim())),
        };
    }
    let object = raw
        .as_object()
        .ok_or_else(|| format!("{at} must be a string or a {{label, description?}} object"))?;
    let label = object
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| format!("{at} needs a non-empty `label`"))?;
    let mut choice = Choice::new(label);
    if let Some(description) = object
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        choice = choice.describe(description);
    }
    if let Some(preview) = object
        .get("preview")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    {
        choice = choice.preview(bound_preview(preview));
    }
    Ok(choice)
}

/// Cut an over-long preview at a line boundary, and say so in the text itself.
///
/// `todo_write`'s cap-and-tell idiom rather than a silent trim: the model wrote this
/// and gets to see that it was cut. Never mid-word, because every consumer draws it
/// into a terminal viewport. Control characters and escape sequences go too — this
/// value is written by a model and rendered by a terminal, which is the pair that
/// makes an unfiltered escape a real problem rather than a cosmetic one.
fn bound_preview(preview: &str) -> String {
    let clean: String = preview
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control() && *c != '\u{9b}')
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut kept = String::new();
    let mut cut = false;
    for (i, line) in clean.lines().enumerate() {
        if i >= PREVIEW_MAX_LINES || kept.len() + line.len() + 1 > PREVIEW_MAX_BYTES {
            cut = true;
            break;
        }
        if i > 0 {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    if cut {
        kept.push_str(&format!(
            "\n[preview cut: at most {PREVIEW_MAX_LINES} lines or {PREVIEW_MAX_BYTES} bytes]"
        ));
    }
    kept
}

/// Read one question object — the shape both question tools share.
fn parse_question(raw: &serde_json::Value, at: &str) -> std::result::Result<Question, String> {
    let text = raw
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| format!("{at} needs a non-empty `question`"))?;
    let mut question = Question::new(text);
    if let Some(context) = raw
        .get("context")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        question = question.with_context(context);
    }
    if let Some(list) = raw.get("choices") {
        let list = list
            .as_array()
            .ok_or_else(|| format!("{at}: `choices` must be a list"))?;
        let mut choices = Vec::with_capacity(list.len());
        for (i, choice) in list.iter().enumerate() {
            choices.push(parse_choice(choice, &format!("{at}, choice {}", i + 1))?);
        }
        question = question.with_choices(choices);
    }
    if raw.get("multiple").and_then(|v| v.as_bool()) == Some(true) {
        // There is nothing to take more than one of. Refused rather than accepted
        // and ignored, because a UI that trusts the flag would draw checkboxes over
        // an empty list.
        if question.choices.is_empty() {
            return Err(format!("{at} says `multiple` but offers no `choices`"));
        }
        question = question.multiple();
    }
    Ok(question)
}

/// Read one question out of an `ask_question` argument object (0.72.0).
///
/// The singular tool gained described choices and `multiple` in 0.72.0 and nothing
/// else. It shares [`parse_question`] with the batch so the two tools cannot disagree
/// about what an offer is.
pub(super) fn parse_one_question(
    args: &serde_json::Value,
) -> std::result::Result<Question, String> {
    parse_question(args, "the question")
}

/// Read an `ask_questions` argument object into the batch, or say what was wrong.
///
/// Strict **per index**, with the failing index named, exactly as [`parse_plan`] is
/// for steps: an operator about to answer five questions must be answering the five
/// that were asked.
pub(super) fn parse_questions(
    args: &serde_json::Value,
) -> std::result::Result<Vec<Question>, String> {
    let list = args
        .get("questions")
        .ok_or_else(|| "`questions` is required: send the whole ask as a list".to_string())?
        .as_array()
        .ok_or_else(|| "`questions` must be a list of question objects".to_string())?;
    if list.is_empty() {
        return Err(
            "an ask with no questions is not an ask; say what you need to know".to_string(),
        );
    }
    if list.len() > QUESTIONS_MAX {
        return Err(format!(
            "{} questions is more than the {QUESTIONS_MAX} one ask may carry; \
             send the ones you need to start with and ask the rest after",
            list.len()
        ));
    }
    let mut questions = Vec::with_capacity(list.len());
    for (i, raw) in list.iter().enumerate() {
        questions.push(parse_question(raw, &format!("question {}", i + 1))?);
    }
    Ok(questions)
}

/// The batch's answers as the one block the model reads.
///
/// Many in, one block out — `todo_write`'s precedent. Each answer is drawn beside the
/// question it answers, because a bare list of five sentences is not readable as an
/// answer set and the model would have to re-derive the pairing.
pub(super) fn assemble_answers(questions: &[Question], answers: &[Option<String>]) -> String {
    let mut out = String::new();
    for (i, question) in questions.iter().enumerate() {
        let answer = answers
            .get(i)
            .and_then(Option::as_deref)
            .unwrap_or("(not answered)");
        out.push_str(&format!("{}. {}\n   {answer}\n", i + 1, question.question));
    }
    out.trim_end().to_string()
}

pub(super) enum Dispatched {
    /// The call resolved; fold `obs` into the observation log and carry any
    /// rules an approver asked to remember.
    ///
    /// `kind` and `target` travel with `obs` because assembly reasons about them:
    /// what a later observation supersedes, and which read a write invalidates,
    /// is a question about the tool and its subject — not something to recover by
    /// re-parsing the text afterwards.
    Continue {
        decision: String,
        obs: String,
        kind: ObsKind,
        target: Option<String>,
        /// Whether this call moved the workspace. Only a write can, and only a
        /// write that wrote something different — the signal stall detection reads.
        changed: bool,
        remember: Vec<Rule>,
    },
    /// An approver deferred; the action is persisted under `request_id` and the
    /// run stops until a human decides.
    Pause { request_id: i64 },
    /// (0.21.0) The agent asked the operator about intent and no `Responder` in this
    /// process would answer. The question is persisted under `question_id` and the
    /// run stops until a human answers it.
    Ask { question_id: i64 },
    /// (0.31.0) The agent proposed a plan and the gate answered — or did not.
    ///
    /// `verdict` is `None` when no [`PlanGate`](crate::PlanGate) in this process
    /// would decide, which stops the run with
    /// [`RunOutcome::AwaitingPlan`](RunOutcome::AwaitingPlan). A
    /// [`PlanVerdict::Revise`] never reaches here: the correction is text the model
    /// reads and the run stays in its planning phase, so it comes back as an
    /// ordinary `Continue`.
    Plan {
        plan_id: i64,
        verdict: Option<PlanVerdict>,
    },
}

impl Dispatched {
    /// A tool result: what it was, and the subject it names (if any).
    pub(super) fn seen(
        decision: impl Into<String>,
        obs: impl Into<String>,
        kind: ObsKind,
        target: Option<String>,
    ) -> Self {
        Dispatched::Continue {
            decision: decision.into(),
            obs: obs.into(),
            kind,
            target,
            changed: false,
            remember: Vec::new(),
        }
    }

    /// A failure or a refusal. Kept subject-less on purpose: an error about a
    /// path is not an observation *of* that path, so it must never supersede one.
    pub(super) fn go(decision: impl Into<String>, obs: impl Into<String>) -> Self {
        Self::seen(decision, obs, ObsKind::Error, None)
    }
}

/// Whether a call in this completion can change anything (0.41.0).
///
/// Three built-ins observe and change nothing: `grep`, `find` and `read_file`.
/// **Everything else built in is [`ToolEffect::Mutating`]**, including tools that
/// only read the world but reach it through a process — the git readers, `exec`,
/// `shell` — because a spawn under a sandbox backend is not something this
/// release makes concurrent, and including `list_dir` and `view_image`, which
/// read the workspace but are outside the set the contract named. Widening the
/// set is a later release's decision, taken with its own evidence.
///
/// A registered tool answers for itself through [`Tool::effect`](crate::Tool),
/// which is defaulted to `Mutating`, so a toolbox assembled before 0.41.0 keeps
/// running exactly as it did. An MCP tool is `Mutating`: honouring a server's
/// `readOnlyHint` means overlapping requests on one [`McpSession`], which is a
/// question about the client rather than about this loop.
pub(super) fn tool_effect(name: &str, custom: &Toolbox) -> ToolEffect {
    match name {
        GREP_TOOL | FIND_TOOL | READ_FILE_TOOL => ToolEffect::ReadOnly,
        _ => custom
            .get(name)
            .map_or(ToolEffect::Mutating, |tool| tool.effect()),
    }
}

/// The mode a call needs, before anything is spawned for it (0.48.0).
///
/// `None` means *whatever this run was granted*, which is the answer for `exec`,
/// `shell` and `shell_start`: they are the tools
/// [`TaskContract::exec_sandbox`](crate::TaskContract::exec_sandbox) was written
/// for, and narrowing them would be the contract disagreeing with itself.
///
/// **The git built-ins are classified the way this crate already classifies
/// them.** `dispatch` decides a git call's `Act` on `.git` at one place — writers
/// are `git_add`, `git_commit`, `git_branch` and `git_worktree`, readers are
/// `git_log`, `git_status` and `git_diff` — and a second, hand-maintained opinion
/// about which of them writes is a fact in two files waiting to disagree. The
/// modes here are that table read as grants.
///
/// The three read-only built-ins declare `ReadOnly` and it is **inert**: they
/// spawn nothing, so no backend ever wraps them and no mode is ever applied. It
/// is written down anyway because the alternative is a reader of this function
/// wondering whether their absence meant "needs everything".
///
/// A registered tool answers for itself through
/// [`Tool::exec_mode`](crate::tools::Tool), defaulted to `None`, so a toolbox
/// assembled before 0.48.0 keeps running exactly as it did. An MCP tool declares
/// nothing: the server owns that process and this crate does not model it.
pub(super) fn tool_mode(name: &str, custom: &Toolbox) -> Option<crate::sandbox::ExecMode> {
    use crate::sandbox::ExecMode;
    match name {
        GREP_TOOL | FIND_TOOL | READ_FILE_TOOL => Some(ExecMode::ReadOnly),
        GIT_LOG_TOOL | GIT_STATUS_TOOL | GIT_DIFF_TOOL => Some(ExecMode::ReadOnly),
        GIT_ADD_TOOL | GIT_COMMIT_TOOL | GIT_BRANCH_TOOL | GIT_WORKTREE_TOOL => {
            Some(ExecMode::WorkspaceWrite)
        }
        EXEC_TOOL | SHELL_TOOL | SHELL_START_TOOL => None,
        _ => custom.get(name).and_then(|tool| tool.exec_mode()),
    }
}

/// Whether this run will route its contained commands through a proxy (0.48.0).
///
/// The same two questions [`start_egress_proxy`] asks, as a pure predicate, so the
/// prompt's boundary section can say what the run is about to do without the
/// listener having been started yet — and so the two can never disagree about
/// whether a run is proxied.
pub(super) fn will_proxy(policy: &Policy, contract: &TaskContract) -> bool {
    contract.exec_sandbox.mode.is_contained()
        && policy.names_hosts()
        // 0.59.0 — and the backend has to be one a proxy can be reached from. A
        // process inside a Windows AppContainer cannot reach a loopback listener
        // under any capability set, so a run there is given no proxy rather than
        // one that would swallow every request it makes.
        && crate::sandbox::select(&contract.exec_sandbox)
            .backend()
            .reaches_loopback_proxy()
}

/// The run's egress proxy, when it needs one (0.48.0).
///
/// Started only when the run's commands are contained **and** its policy names
/// hosts. A run whose only statement about the network is its default — everything
/// or nothing — is served exactly as well by the boolean a backend takes, and
/// starting a listener for it would buy a component with a lifetime for nothing.
///
/// The returned proxy owns its listener and its accept loop, and both end when it
/// is dropped, which is when the run ends however it ends.
pub(super) async fn start_egress_proxy(
    policy: &Policy,
    containment: Option<&std::sync::Arc<crate::sandbox::ExecContainment>>,
) -> Option<(
    crate::sandbox::proxy::EgressProxy,
    std::sync::Arc<std::sync::RwLock<Policy>>,
    std::sync::Arc<std::sync::atomic::AtomicU32>,
)> {
    let containment = containment?;
    if !policy.names_hosts() {
        return None;
    }
    // 0.59.0 — and the backend must be one a contained command can reach a
    // loopback listener from. `will_proxy` asks the same question as a pure
    // predicate for the prompt, and the two must not disagree about whether this
    // run is proxied.
    if !crate::sandbox::select(&containment.config)
        .backend()
        .reaches_loopback_proxy()
    {
        return None;
    }
    let shared = std::sync::Arc::new(std::sync::RwLock::new(policy.clone()));
    let step = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    match crate::sandbox::proxy::EgressProxy::start(
        std::sync::Arc::clone(&shared),
        std::sync::Arc::clone(&step),
    )
    .await
    {
        Ok(proxy) => Some((proxy, shared, step)),
        // A listener that will not bind is not a reason to fail the run. The run
        // keeps the boolean it had before this release and `EventKind::Contained`
        // reports the backend that actually applied, which is the same honesty
        // rule every degradation in this crate follows.
        Err(e) => {
            tracing::warn!("sandbox: the egress proxy could not start ({e}); keeping the boolean");
            None
        }
    }
}

/// Carry a step's dial decisions into the trace (0.48.0).
///
/// The proxy cannot write them itself — a `rusqlite::Connection` is `Send` and not
/// `Sync` — so it queues them and this drains at the step boundary, beside the
/// handle registry's endings, which are on this thread for the same reason.
///
/// Two rows and one event each, all in tables that already exist: the decision
/// goes to `policy_events` with `act = "net"`, where the crate's own network
/// decisions already live, and a `SandboxEvent` of kind `"dial"` names
/// `host:port` at command scope.
pub(super) fn record_dials(
    proxy: Option<&crate::sandbox::proxy::EgressProxy>,
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
) -> Result<()> {
    let Some(proxy) = proxy else {
        return Ok(());
    };
    for dial in proxy.drain() {
        // A permitted dial is a `decision` row and a refused one is a `refusal`
        // row — the same two shapes the crate's own network calls already write,
        // so a reader learns nothing new to read these.
        let mut ev = if dial.allowed {
            PolicyEvent::decision(
                dial.step,
                "net".to_string(),
                dial.target(),
                "allow".to_string(),
                "policy".to_string(),
            )
        } else {
            PolicyEvent::refusal(dial.step, "net".to_string(), dial.target())
        };
        ev.rule.clone_from(&dial.rule);
        ev.layer.clone_from(&dial.layer);
        store.record_event(run_id, &ev)?;
        let mut sandbox = crate::state::SandboxEvent::destroy(run_id, dial.step);
        sandbox.kind = "dial".to_string();
        sandbox.detail = Some(dial.target());
        record_sandbox_step(store, watch, depth, &sandbox);
        watch.emit(RunEvent::at_depth(
            run_id,
            dial.step,
            depth,
            EventKind::Dialed {
                host: dial.host.clone(),
                port: dial.port,
                allowed: dial.allowed,
            },
        ));
    }
    Ok(())
}

/// The `create` row for one contained call, naming the mode that call resolved
/// to (0.48.0).
///
/// The mode goes in `detail`, which was unused on a `create` row, because the
/// mode is now a **per-call** fact and not a per-run one: a git reader declares
/// `read-only` inside a run granting `workspace-write`, and a trace that recorded
/// only the run's grant would say the opposite of what was enforced. `detail` is
/// a text column, so this costs no schema change — and `SandboxEvent::create` is
/// public, so its signature is left exactly as it is.
pub(super) fn sandbox_create(
    run_id: i64,
    step: u32,
    containment: &crate::sandbox::ExecContainment,
) -> crate::state::SandboxEvent {
    let mut created =
        crate::state::SandboxEvent::create(run_id, step, containment.backend().as_str());
    created.detail = Some(containment.config.mode.as_str().to_string());
    created
}

/// What the approver is told about the question, beyond the action itself
/// (0.42.0).
///
/// One definition, called by both approval sites — the tool path in [`gate`] and
/// the provider authorization in [`authorize_provider`]. The two are in different
/// loops and would otherwise each grow their own copy of "which parts of the
/// verdict an approver gets", which is exactly the drift `NO_TOOL_CALL`'s doc
/// comment and `tests/session_fanout.rs` exist to prevent.
pub(super) fn approval_context(goal: &str, verdict: &crate::policy::Verdict) -> ApprovalContext {
    ApprovalContext::new(goal).flagged_by(verdict.rule.clone(), verdict.layer.clone())
}

/// What the policy says about one act on one target, and nothing else.
///
/// Read and write targets are workspace paths, and are resolved so a symlink
/// cannot smuggle one outside the root. Exec and net targets are *names* — a
/// binary, an MCP tool, a registered tool, a host — and must not be resolved
/// against the root, or a file that happens to share a tool's name would change
/// what the policy said about calling it.
///
/// An ABSOLUTE read/write target is not a workspace path at all — a skill file
/// normally lives outside the root — so it is decided by the policy directly.
/// `check_path` would resolve it against the root and deny it unconditionally,
/// which would make `read_skill` refusable only by accident. This relaxes what
/// the *gate* says, not what the workspace does: `Workspace::resolve` rejects
/// absolute paths outright and both `read_file` and `write_file` go through it,
/// so an absolute path still cannot leave the root (asserted in tests/skills.rs).
///
/// **A free function rather than a closure inside [`gate`] since 0.54.0**, because
/// speculation asks this same question without answering it — a call the policy
/// does not allow outright is never started early. Two copies of this expression
/// would be two boundaries, and the speculative one would be the copy nobody
/// noticed drifting wider.
pub(super) fn policy_verdict(ws: &Workspace, act: Act, target: &str) -> crate::policy::Verdict {
    match act {
        Act::Exec | Act::Net => ws.policy().check(act, target),
        Act::Read | Act::Write if Path::new(target).is_absolute() => ws.policy().check(act, target),
        Act::Read | Act::Write => ws.check_path(act, target),
    }
}

/// The word the store spells an act with — in a pending row's `act` column and
/// in every `policy_events` row this module writes.
///
/// A total match over [`Act`] rather than `format!("{act:?}").to_lowercase()`,
/// because the *reader* of that column has to reconstruct the same spelling and a
/// derivation cannot be matched against. [`crate::resume_with_decision`] reads it
/// back to decide what a resumed approval replays, and mapping a word it does not
/// know onto a write is how an approved `exec` became a file named after the
/// program (0.74.0, audit M1). Exhaustive here so a fifth act is a compile error
/// in this crate rather than a silent write in a caller's workspace.
pub(super) fn act_word(act: Act) -> &'static str {
    match act {
        Act::Read => "read",
        Act::Write => "write",
        Act::Exec => "exec",
        Act::Net => "net",
    }
}

/// What a refused action's observation tells the model to do instead.
///
/// A path is one of many: a file it may not read usually has a sibling it may,
/// so "try another path" is advice it can act on. A *program* or a *host* is
/// not — there is no second `git`, and an MCP tool the policy refuses has no
/// alternative spelling. Telling a model to try another path there is how one
/// refusal becomes a retry loop that spends the run's steps on the same answer.
///
/// 0.70.0. Before this, every refusal gave the path advice, including the
/// `Act::Exec` refusals the git and MCP arms now route through here.
fn advice(act: Act) -> &'static str {
    match act {
        Act::Read | Act::Write => "try another path",
        Act::Exec | Act::Net => "carry on without it",
    }
}

/// Evaluate one action against the policy, consulting `approver` only for the
/// sensitive-but-permitted tier.
///
/// A denied action never reaches the approver — refusal and approval are
/// different things. An approver that rewrites a *read* or a *write* has the
/// rewritten form re-evaluated here, so it can narrow or redirect within the
/// policy but cannot move an action across a deny.
///
/// **A rewritten `Act::Exec` is refused rather than performed** (0.74.0, audit
/// M4). No exec consumer reads `target` back off the returned [`Gated::Go`] — the
/// argv was parsed before this gate ran — so honouring the rewrite is not on
/// offer and discarding it silently would run the original while recording the
/// rewrite. The approver's `remember` rules still travel; only the substitution
/// is refused.
#[allow(clippy::too_many_arguments)]
pub(super) async fn gate(
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
    let kind = act_word(act);
    let verdict = policy_verdict(ws, act, target);

    match verdict.effect {
        Effect::Deny => {
            let mut ev = PolicyEvent::refusal(step, kind, target);
            if let (Some(rule), layer) = (verdict.rule.clone(), verdict.layer.clone()) {
                ev.rule = Some(rule);
                ev.layer = layer;
            }
            store.record_event(run_id, &ev)?;
            refused(watch, run_id, depth, &ev);
            let why = verdict
                .rule
                .as_deref()
                .map(|r| format!(" (rule {r})"))
                .unwrap_or_default();
            Ok(Gated::Refused {
                decision: format!("{kind} refused"),
                obs: format!(
                    "\n[{kind} refused] {target}{why} — the policy forbids this; {}\n",
                    advice(act)
                ),
            })
        }
        Effect::Allow => Ok(Gated::Go {
            target: target.to_string(),
            content: content.map(str::to_string),
            remember: Vec::new(),
        }),
        Effect::Ask => {
            let mut request = Request::new(act, target);
            if let Some(c) = content {
                request = request.with_content(c);
            }
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::ApprovalRequested {
                    act: kind.to_string(),
                    target: target.to_string(),
                },
            ));
            // 0.33.0 — the row is durable BEFORE the gate is consulted, the
            // ordering `put_plan` has had since 0.31.0. A row that only appears
            // once the approver has deferred is a row no second process can answer
            // while the run is still holding the question, which is exactly the
            // gap this release closes.
            let request_id = store.put_pending(run_id, step, kind, target, content)?;
            let context = approval_context(goal, &verdict);
            let raced = race_gate(approver.decide_in_context(&request, &context), store, |s| {
                Ok(s.pending(request_id)?.is_some_and(|p| p.resolved.is_some()))
            })
            .await?;

            // Deferring is the one answer that writes nothing: it leaves the row
            // unresolved so the run pauses with something a resume — or an
            // attached process — can still answer.
            if matches!(raced, Some(Decision::Defer)) {
                let ev = PolicyEvent::decision(step, kind, target, "defer", "approver");
                store.record_event(run_id, &ev)?;
                decided(watch, run_id, depth, &ev);
                return Ok(Gated::Paused { request_id });
            }

            // The gate's answer goes through the same compare-and-swap an attached
            // process uses, so the store arbitrates instead of the last writer.
            let mine = match &raced {
                Some(Decision::Approve { .. }) => store.resolve_pending(request_id, "approve")?,
                Some(Decision::Deny { .. }) => store.resolve_pending(request_id, "deny")?,
                // The attached arm: the row was written by whoever raced us.
                _ => false,
            };
            let (modified, remember, reason) = match raced {
                Some(Decision::Approve { modified, remember }) => {
                    (modified, remember, String::new())
                }
                Some(Decision::Deny { reason }) => (None, Vec::new(), reason),
                _ => (
                    None,
                    Vec::new(),
                    "answered by an attached process".to_string(),
                ),
            };

            // The row is the authority, in BOTH arms. A decision reported from the
            // value we raced with rather than from the store would be true whether
            // or not the durable write landed — and would still be reported if a
            // second process had won the swap a microsecond earlier, which is the
            // silent double-answer this release exists to prevent. `mine` is the
            // store's own answer to "was it me", so the source is a fact rather
            // than an assumption.
            let landed = store
                .pending(request_id)?
                .and_then(|p| p.resolved)
                .unwrap_or_else(|| "deny".to_string());
            let source = if mine { "approver" } else { "attached" };

            if landed != "approve" {
                let ev = PolicyEvent::decision(step, kind, target, &landed, source);
                store.record_event(run_id, &ev)?;
                decided(watch, run_id, depth, &ev);
                return Ok(Gated::Refused {
                    decision: format!("{kind} denied"),
                    obs: format!("\n[{kind} denied] {target} — {reason}\n"),
                });
            }

            // 0.74.0 (audit M4) — a rewrite is honoured only where the performing
            // side reads it back. The read and write paths take `target` and
            // `content` off the `Gated::Go` below, so an approver can redirect one.
            // Every `Act::Exec` consumer cannot: `exec`, `shell`, the git
            // built-ins, a registered tool and an MCP tool all dispatch the argv
            // they parsed *before* this gate was consulted and read only
            // `remember`. Discarding the rewrite silently means a human approves
            // one command while another runs and the trace records the one that
            // did not — and, in the direction that matters more, an approver
            // *narrowing* an argv is overruled without ever being told. Honouring
            // it would mean re-splitting a command line inside the gate, which is
            // the tool layer's job and not this one's, so the rewrite is refused
            // and the refusal names both forms.
            if let Some(rewrite) = modified
                .as_ref()
                .filter(|m| act == Act::Exec && m.target != target)
            {
                let ev = PolicyEvent::refusal(step, kind, target).with_performed(&rewrite.target);
                store.record_event(run_id, &ev)?;
                refused(watch, run_id, depth, &ev);
                return Ok(Gated::Refused {
                    decision: format!("{kind} rewrite refused"),
                    obs: format!(
                        "\n[{kind} refused] {target} — the approver rewrote it to {}, and a \
                         rewritten command is not something this path can run: the argv was \
                         parsed before the approval. Nothing ran. Approve {target} as asked, \
                         deny it, or narrow it with an exec rule.\n",
                        rewrite.target
                    ),
                });
            }

            let performed = modified.unwrap_or_else(|| request.clone());
            // The rewritten action gets the same scrutiny as the original.
            let recheck = policy_verdict(ws, act, &performed.target);
            if recheck.effect == Effect::Deny {
                let mut ev = PolicyEvent::refusal(step, kind, &performed.target);
                ev.rule = recheck.rule.clone();
                ev.layer = recheck.layer.clone();
                store.record_event(run_id, &ev)?;
                // A refusal, not a decision: the row is a refusal too, and the
                // approval it overrode never took effect.
                refused(watch, run_id, depth, &ev);
                return Ok(Gated::Refused {
                    decision: format!("{kind} refused after approval"),
                    obs: format!(
                        "\n[{kind} refused] {} — an approved change may not cross a deny\n",
                        performed.target
                    ),
                });
            }
            let mut ev = PolicyEvent::decision(step, kind, target, "approve", source);
            if performed.target != target {
                ev = ev.with_performed(&performed.target);
            }
            store.record_event(run_id, &ev)?;
            decided(watch, run_id, depth, &ev);
            Ok(Gated::Go {
                target: performed.target,
                content: performed.content,
                remember,
            })
        }
    }
}
