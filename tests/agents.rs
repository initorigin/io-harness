//! Named agent definitions: a role, a model, and a narrower boundary (0.21.0).
//!
//! Before this release a spawned sub-agent was its parent with a different goal
//! string. One provider instance serves a whole tree, and every provider fixes its
//! model at construction, so "search with the cheap model, write with the strong one"
//! — the largest cost lever this crate has — could not be said at all.
//!
//! Two things are proven here, and the second matters more than the first.
//!
//! **The model reaches the wire.** Asserted from the `provider_calls` rows in the
//! store, not from the roster that asked for it. The mock echoes the model it was
//! *sent* back as the model that *served*, which is what makes the store's record
//! evidence that the field travelled rather than evidence that it was configured.
//!
//! **A definition cannot widen anything.** There is no `allow_write` on `AgentDef`
//! and there must never be one: "give the writer agent write access" is the natural
//! thing to reach for, and a roster in a config file that could grant would be a
//! privilege-escalation path. Narrowing composes through `Policy::contain`, the same
//! function that has bounded every child since 0.5.0, so the guarantee is structural
//! rather than a new check somebody has to trust.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_tree, AgentDef, Agents, ApproveAll, Containment, Policy, Provider, RunOutcome, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a script and keeps every request, echoing the requested model back as the
/// model that served the call.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
    /// What this provider was "constructed with", returned when a request names no
    /// model of its own.
    configured: String,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
            configured: "configured-model".to_string(),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// Every system prompt the provider was sent.
    fn systems(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.system).collect()
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // Echo what we were asked for, so the trace records the request reaching here.
        let served = req.model.clone().unwrap_or_else(|| self.configured.clone());
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            model: Some(served),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// Every model recorded against a run and its children, in call order.
fn models(store: &Store, root: i64) -> Vec<String> {
    let mut out: Vec<String> = store
        .provider_calls(root)
        .unwrap()
        .into_iter()
        .filter_map(|c| c.model)
        .collect();
    for child in store.children(root).unwrap() {
        out.extend(
            store
                .provider_calls(child)
                .unwrap()
                .into_iter()
                .filter_map(|c| c.model),
        );
    }
    out
}

fn spawn_as(agent: &str, goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({
            "agent": agent,
            "goal": goal,
            "verify_file": file,
            "verify_contains": needle,
            "max_steps": 2
        }),
    )
}

// ------------------------------------------- F7: the model reaches the wire

/// F7 — two definitions naming two models put two models on the wire, read back from
/// the store.
#[tokio::test]
async fn two_named_agents_put_their_own_two_models_on_the_wire() {
    let dir = ws();
    let contract = TaskContract::workspace("search, then write", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "done".into(),
        })
        .with_max_steps(6)
        .with_agents(
            Agents::new()
                .with(AgentDef::new("searcher").with_model("cheap-model"))
                .with(AgentDef::new("author").with_model("strong-model")),
        );

    let provider = MockScript::new(vec![
        vec![spawn_as("searcher", "find it", "found.txt", "found")],
        vec![call(
            "write_file",
            json!({ "path": "found.txt", "content": "found" }),
        )],
        vec![spawn_as("author", "write it", "done.txt", "done")],
        vec![call(
            "write_file",
            json!({ "path": "done.txt", "content": "done" }),
        )],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let seen = models(&store, result.run_id);
    assert!(
        seen.contains(&"cheap-model".to_string()),
        "the searcher's model must have reached the wire; recorded: {seen:?}"
    );
    assert!(
        seen.contains(&"strong-model".to_string()),
        "the author's model must have reached the wire; recorded: {seen:?}"
    );
    // The root itself named no definition, so it used the provider's own model.
    assert!(
        seen.contains(&"configured-model".to_string()),
        "the root must still use the provider's configured model; recorded: {seen:?}"
    );
}

/// F7's negative control: a spawn naming no definition carries the provider's own
/// model, so the assertion above is about the definition and not about the plumbing
/// setting some model on everything.
#[tokio::test]
async fn a_spawn_with_no_agent_named_uses_the_providers_configured_model() {
    let dir = ws();
    let contract = TaskContract::workspace("delegate once", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(4)
        .with_agents(Agents::new().with(AgentDef::new("unused").with_model("never-asked-for")));

    let provider = MockScript::new(vec![
        vec![call(
            "spawn_agent",
            json!({ "goal": "do it", "verify_file": "out.txt", "verify_contains": "ok", "max_steps": 2 }),
        )],
        vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "ok" }),
        )],
    ]);
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let seen = models(&store, result.run_id);
    assert!(
        !seen.iter().any(|m| m == "never-asked-for"),
        "an unnamed spawn must not pick up a registered definition; recorded: {seen:?}"
    );
    assert!(
        seen.iter().all(|m| m == "configured-model"),
        "every call should have used the provider's own model; recorded: {seen:?}"
    );
    // And every request genuinely carried `None` rather than a string that happened
    // to match.
    assert!(
        provider.requests().iter().all(|r| r.model.is_none()),
        "no request should have named a model"
    );
}

// ------------------------------- F8: a role arrives, and nothing is ever widened

/// F8, the role half — the child's system prompt carries its definition's role, and
/// carries it *in addition to* the tree prompt rather than instead of it. A role that
/// replaced the tree prompt would produce an agent that did not know how to be one.
#[tokio::test]
async fn a_named_agents_role_is_prepended_to_its_system_prompt() {
    let dir = ws();
    const ROLE: &str = "You only read. You report paths and line numbers.";
    let contract = TaskContract::workspace("delegate the search", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(4)
        .with_agents(Agents::new().with(AgentDef::new("searcher").with_role(ROLE)));

    let provider = MockScript::new(vec![
        vec![spawn_as("searcher", "look", "out.txt", "ok")],
        vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "ok" }),
        )],
    ]);
    let store = Store::memory().unwrap();
    run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let systems = provider.systems();
    let child = systems
        .iter()
        .find(|s| s.contains(ROLE))
        .expect("some request must have carried the role");
    // The tree prompt is still there: the child knows it can spawn and that its
    // result composes back.
    assert!(
        child.contains("sub-agent") || child.contains("spawn"),
        "the role must be prepended to the tree prompt, not replace it:\n{child}"
    );
    // The root's own prompt never carried it.
    assert!(
        !systems[0].contains(ROLE),
        "the root agent has no role and must not receive one:\n{}",
        systems[0]
    );
}

/// F8, the narrowing half — a definition's `deny_write` refuses the child a write its
/// parent is allowed.
#[tokio::test]
async fn a_definition_that_denies_writes_produces_a_child_that_cannot_write() {
    let dir = ws();
    let contract = TaskContract::workspace("delegate to a reader", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(4)
        .with_agents(Agents::new().with(AgentDef::new("reader").deny_write()));

    let provider = MockScript::new(vec![
        vec![spawn_as("reader", "try to write", "child.txt", "written")],
        // The child tries the write its definition forbids.
        vec![call(
            "write_file",
            json!({ "path": "child.txt", "content": "written" }),
        )],
        vec![],
        vec![],
    ]);
    // The PARENT is allowed to write, so the refusal can only come from the
    // definition.
    let parent_policy = Policy::default()
        .layer("parent")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*");
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &parent_policy,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        !dir.path().join("child.txt").exists(),
        "the child must not have been able to write"
    );
    let refused: Vec<_> = store
        .children(result.run_id)
        .unwrap()
        .into_iter()
        .flat_map(|c| store.events(c).unwrap())
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        !refused.is_empty(),
        "the child's write must be refused and recorded, got {refused:?}"
    );
    assert_eq!(refused[0].act, "write");
    assert_eq!(
        refused[0].layer.as_deref(),
        Some("child"),
        "the refusal belongs to the containment layer, got {:?}",
        refused[0]
    );
}

/// F8's most important assertion: a definition **silent** about a path its parent
/// denies still yields a child that is refused it. A definition can only narrow, so
/// saying nothing can never restore anything.
#[tokio::test]
async fn a_definition_silent_about_a_denied_path_does_not_restore_it() {
    let dir = ws();
    std::fs::write(dir.path().join("secret.txt"), "classified\n").unwrap();
    let contract = TaskContract::workspace("delegate", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(4)
        // Says nothing at all about secret.txt.
        .with_agents(Agents::new().with(AgentDef::new("worker").with_role("You do the work.")));

    let provider = MockScript::new(vec![
        vec![spawn_as("worker", "read the secret", "out.txt", "ok")],
        vec![call("read_file", json!({ "path": "secret.txt" }))],
        vec![],
        vec![],
    ]);
    let parent_policy = Policy::default()
        .layer("parent")
        .allow_read("*")
        .deny_read("secret.txt")
        .allow_write("*");
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &parent_policy,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let child_refusals: Vec<_> = store
        .children(result.run_id)
        .unwrap()
        .into_iter()
        .flat_map(|c| store.events(c).unwrap())
        .filter(|e| e.kind == "refusal" && e.target.contains("secret.txt"))
        .collect();
    assert!(
        !child_refusals.is_empty(),
        "the parent's deny must still bind a child whose definition never mentions it"
    );
}

/// F8's last part — an unknown name is an error observation and no child, not a
/// permissive default. A spawn that silently became an unnarrowed agent because its
/// definition was misspelled is the failure this prevents.
#[tokio::test]
async fn an_unknown_agent_name_yields_an_error_observation_and_no_child() {
    let dir = ws();
    let contract = TaskContract::workspace("delegate", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(3)
        .with_agents(Agents::new().with(AgentDef::new("searcher")));

    let provider = MockScript::new(vec![
        vec![spawn_as("Searcher", "wrong case", "out.txt", "ok")],
        vec![],
        vec![],
    ]);
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        store.children(result.run_id).unwrap().is_empty(),
        "no child may be created for a name nobody registered"
    );
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.result.contains("no agent named") || s.decision.contains("unknown agent")),
        "the model must be told, and told what is available; steps: {:?}",
        steps
            .iter()
            .map(|s| (&s.decision, &s.result))
            .collect::<Vec<_>>()
    );
    // It names what IS available, so the correction costs one step.
    assert!(
        steps.iter().any(|s| s.result.contains("searcher")),
        "the observation must name the available agents; steps: {:?}",
        steps.iter().map(|s| &s.result).collect::<Vec<_>>()
    );
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "an unknown name costs a step, it does not end the run; got {:?}",
        result.outcome
    );
}

/// The definition a child was spawned from is in the trace, so a run can be read back
/// and say which role ran. Recorded in the free-form `detail` of the spawn event
/// rather than a new column, because this release alters no table.
#[tokio::test]
async fn the_trace_records_which_definition_a_child_was_spawned_from() {
    let dir = ws();
    let contract = TaskContract::workspace("delegate", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(4)
        .with_agents(Agents::new().with(AgentDef::new("searcher")));

    let provider = MockScript::new(vec![
        vec![spawn_as("searcher", "look for it", "out.txt", "ok")],
        vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "ok" }),
        )],
    ]);
    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let spawns: Vec<_> = store
        .agent_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "spawn")
        .collect();
    assert_eq!(spawns.len(), 1);
    let detail = spawns[0].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("searcher"),
        "the spawn's detail must name the definition, got {detail:?}"
    );
    assert!(
        detail.contains("look for it"),
        "and must still carry the goal, got {detail:?}"
    );
}
