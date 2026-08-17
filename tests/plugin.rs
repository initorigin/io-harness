//! Capability bundles (0.35.0): what a manifest contributes, who is allowed to
//! contribute it, what the trace says about where it came from, and what happens
//! to the run when a bundle is broken.
//!
//! Every criterion here is asserted against something a bundle *changed* — a
//! catalogue, a roster, a stored refusal, a tool name — rather than against the
//! loader's own report of what it did. The loader reporting that it loaded a
//! plugin is exactly what a loader that merged nothing would also report.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, Act, ApproveAll, Config, Effect, EventKind, Observer, Policy, Provider, RunEvent,
    Store, TaskContract, Verification, MCP_TOOL_PREFIX,
};
use serde_json::json;

// --------------------------------------------------------------- test fixtures

/// Replays a fixed script of tool calls and keeps every request it was sent.
#[derive(Default)]
struct Script {
    at: AtomicUsize,
    steps: Vec<Vec<ToolCall>>,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl Script {
    fn of(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            ..Default::default()
        }
    }

    fn request(&self, i: usize) -> CompletionRequest {
        self.seen.lock().unwrap()[i].clone()
    }

    fn tool_names(&self, i: usize) -> Vec<String> {
        self.request(i)
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }
}

impl Provider for Script {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// Keeps every event a run emitted, so the plugin reports can be counted.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<EventKind>>,
}

impl Recorder {
    fn kinds(&self) -> Vec<EventKind> {
        self.events.lock().unwrap().clone()
    }

    fn loaded(&self) -> Vec<String> {
        self.kinds()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::PluginLoaded { plugin, .. } => Some(plugin),
                _ => None,
            })
            .collect()
    }

    fn dropped(&self) -> Vec<(String, String)> {
        self.kinds()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::PluginDropped { plugin, why } => Some((plugin, why)),
                _ => None,
            })
            .collect()
    }
}

impl Observer for Recorder {
    fn event(&self, event: &RunEvent) -> io_harness::Flow {
        self.events.lock().unwrap().push(event.kind.clone());
        io_harness::Flow::Continue
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Where `cargo test` left the MCP fixture example binary. Same derivation as
/// `tests/mcp.rs`, and the same reason: `CARGO_BIN_EXE_*` exists for `[[bin]]`
/// targets and the fixture is deliberately an example.
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("mcp_fixture_server{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "fixture server not built at {}. `cargo test` builds examples; run \
         `cargo build --all-features --examples` if invoking the test binary directly.",
        path.display()
    );
    path
}

/// A bundle carrying a skill, a template, an agent and a deny layer — the four a
/// project-scoped declaration is allowed to contribute.
fn bundle(root: &Path, id: &str) -> PathBuf {
    let dir = root.join(format!("bundles/{id}"));
    write(
        &dir.join("plugin.toml"),
        &format!(
            "name = \"{id}\"\n\
             description = \"a bundle\"\n\
             skills = \"skills\"\n\
             templates = \"templates\"\n\
             \n\
             [[agent]]\n\
             name = \"reviewer\"\n\
             model = \"cheap-model\"\n\
             deny_write = true\n\
             \n\
             [policy]\n\
             layers = [{{ name = \"guard\", rules = [\n\
             {{ act = \"write\", effect = \"deny\", pattern = \"secrets/**\" }},\n\
             ] }}]\n"
        ),
    );
    write(
        &dir.join("skills/review.md"),
        "---\nname: review\ndescription: how we review\n---\n\nRead every changed line.\n",
    );
    write(
        &dir.join("templates/bugfix/TEMPLATE.md"),
        "Fix the bug in {{file}}.\n",
    );
    dir
}

/// A contract nothing can satisfy, so a run that gets past discovery reaches the
/// provider and stops on its step cap.
fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("exercise the bundles", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1)
}

/// A store per run, in a directory that outlives the call.
fn store(dir: &Path) -> Store {
    Store::open(dir.join("harness.sqlite3")).unwrap()
}

// ------------------------------------------------------------------ F1

/// **F1**, both halves. The contributions of a declared bundle reach a run, and
/// the identical tree without the `[[plugin]]` line has none of them — so a
/// loader that discovered nothing fails the first half rather than passing both.
#[tokio::test]
async fn a_declared_bundle_contributes_and_an_undeclared_one_does_not() {
    for (declared, expect) in [(true, true), (false, false)] {
        let dir = tmp();
        let root = dir.path();
        bundle(root, "rust-review");
        if declared {
            write(
                &root.join("io.local.toml"),
                "[[plugin]]\npath = \"bundles/rust-review\"\n",
            );
        } else {
            write(&root.join("io.local.toml"), "[run]\nmax_steps = 4\n");
        }

        let config = Config::discover(root).unwrap();
        let plugins = config.plugins();
        assert!(plugins.dropped().is_empty(), "{:?}", plugins.dropped());
        assert_eq!(plugins.len(), usize::from(expect));

        let contract = plugins.apply_to(contract(root));
        let policy = plugins.apply_to_policy(Policy::permissive());
        let templates = plugins.templates().unwrap();

        // The agent roster, the policy stack and the template set, each observed
        // on the value a run would actually use.
        assert_eq!(
            contract.agents.get("rust-review__reviewer").is_some(),
            expect,
            "agent roster, declared = {declared}"
        );
        assert_eq!(
            policy.layers.iter().any(|l| l.name == "rust-review__guard"),
            expect,
            "policy stack, declared = {declared}"
        );
        assert_eq!(
            templates.get("rust-review__bugfix").is_some(),
            expect,
            "templates, declared = {declared}"
        );

        // And the skill catalogue, which is discovered at run start rather than
        // here — asserted through the prompt the provider was handed.
        let provider = Script::of(vec![vec![]]);
        run_with(&contract, &provider, &store(root), &policy, &ApproveAll)
            .await
            .unwrap();
        let prompt = provider.request(0).system.clone();
        assert_eq!(
            prompt.contains("rust-review__review"),
            expect,
            "skill catalogue, declared = {declared}"
        );
    }
}

// ------------------------------------------------------------------ F2

/// **F2**. A refusal decided by a bundle's rule names the bundle in the trace,
/// and the control — the same layer declared by the application — carries the
/// bare name, which is what proves the prefix is written by the loader.
#[tokio::test]
async fn a_refusal_names_the_plugin_that_introduced_the_rule() {
    for from_plugin in [true, false] {
        let dir = tmp();
        let root = dir.path();
        let mut policy = Policy::permissive();

        if from_plugin {
            bundle(root, "rust-review");
            write(
                &root.join("io.local.toml"),
                "[[plugin]]\npath = \"bundles/rust-review\"\n",
            );
            policy = Config::discover(root)
                .unwrap()
                .plugins()
                .apply_to_policy(policy);
        } else {
            policy = policy.layer("guard").deny_write("secrets/**");
        }

        let store = store(root);
        let provider = Script::of(vec![vec![call(
            "write_file",
            json!({"path": "secrets/token.txt", "content": "x"}),
        )]]);
        let result = run_with(&contract(root), &provider, &store, &policy, &ApproveAll)
            .await
            .unwrap();

        let refusals: Vec<_> = store
            .events(result.run_id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "refusal")
            .collect();
        assert_eq!(
            refusals.len(),
            1,
            "one refusal, from_plugin = {from_plugin}"
        );
        let expected = if from_plugin {
            "rust-review__guard"
        } else {
            "guard"
        };
        assert_eq!(
            refusals[0].layer.as_deref(),
            Some(expected),
            "the stored layer names the bundle only when the bundle wrote the rule"
        );
    }
}

// ------------------------------------------------------------------ F3

/// **F3**, the MCP half. A bundle's server is namespaced everywhere the server id
/// appears — the tool the model is offered, and the stored `mcp_events` row — and
/// the namespaced name still resolves back to the server-side tool, which is the
/// property that makes `__` a safe separator.
#[tokio::test]
async fn an_mcp_call_from_a_bundle_is_attributed_to_it() {
    let dir = tmp();
    let root = dir.path();
    let plugin = root.join("bundles/tools");
    write(
        &plugin.join("plugin.toml"),
        &format!(
            "name = \"tools\"\n\n[[mcp]]\nid = \"fixture\"\ntransport = \"stdio\"\ncommand = {:?}\n",
            fixture_server().display().to_string()
        ),
    );
    // `io.local.toml`, not `io.toml`: an MCP server names a program to run, so a
    // project-scoped declaration may not contribute one. F4 asserts that half.
    write(
        &root.join("io.local.toml"),
        "[[plugin]]\npath = \"bundles/tools\"\n",
    );

    let plugins = Config::discover(root).unwrap().plugins();
    assert!(plugins.dropped().is_empty(), "{:?}", plugins.dropped());
    assert_eq!(
        plugins.get("tools").unwrap().mcp_servers()[0].id,
        "tools__fixture"
    );

    let namespaced = format!("{MCP_TOOL_PREFIX}tools__fixture__echo");
    let contract = plugins.apply_to(contract(root)).with_max_steps(2);
    let provider = Script::of(vec![vec![call(&namespaced, json!({"text": "hi"}))]]);
    let store = store(root);
    let result = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        provider.tool_names(0).contains(&namespaced),
        "the model is offered the namespaced tool: {:?}",
        provider.tool_names(0)
    );
    let events = store.mcp_events(result.run_id).unwrap();
    assert!(
        events.iter().all(|e| e.server == "tools__fixture"),
        "every mcp event names the bundle's server: {events:?}"
    );
    let called = events
        .iter()
        .find(|e| e.kind == "called")
        .expect("the namespaced tool resolved back to the server-side one and was called");
    assert_eq!(called.ok, Some(true));
    assert_eq!(called.tool.as_deref(), Some(namespaced.as_str()));
}

/// **F3**, the agent half. A bundle's agent is spawnable under its namespaced
/// name and under no other, so the child's run rows and its ledger name the
/// bundle.
#[test]
fn a_bundle_agent_is_registered_under_its_namespaced_name_only() {
    let dir = tmp();
    let root = dir.path();
    bundle(root, "rust-review");
    write(
        &root.join("io.toml"),
        "[[plugin]]\npath = \"bundles/rust-review\"\n",
    );

    let plugins = Config::discover(root).unwrap().plugins();
    let contract = plugins.apply_to(contract(root));
    assert_eq!(contract.agents.names(), vec!["rust-review__reviewer"]);
    assert!(
        contract.agents.get("reviewer").is_none(),
        "the bare name is not registered, so a bundle cannot occupy one the operator uses"
    );
    assert!(
        contract
            .agents
            .get("rust-review__reviewer")
            .unwrap()
            .deny_write
    );
}

// ------------------------------------------------------------------ F4

/// **F4**, both arms. A project-scoped declaration may not contribute a hook or
/// an MCP server; the same directory declared locally may. The discriminating
/// assertion on the refused arm is that the bundle contributes **nothing** — a
/// loader that dropped the offending array and kept the rest would satisfy a
/// weaker claim while leaving a stranger's manifest half-applied.
#[test]
fn a_project_scoped_bundle_contributes_nothing_that_runs_a_program() {
    for offending in ["hook", "mcp"] {
        let body = match offending {
            "hook" => "[[hook]]\non = [\"finished\"]\nappend = \"audit.jsonl\"\n".to_string(),
            _ => format!(
                "[[mcp]]\nid = \"fixture\"\ntransport = \"stdio\"\ncommand = {:?}\n",
                fixture_server().display().to_string()
            ),
        };

        for scope_file in ["io.toml", "io.local.toml"] {
            let dir = tmp();
            let root = dir.path();
            let plugin = bundle(root, "rust-review");
            let manifest = std::fs::read_to_string(plugin.join("plugin.toml")).unwrap();
            write(&plugin.join("plugin.toml"), &format!("{manifest}\n{body}"));
            write(
                &root.join(scope_file),
                "[[plugin]]\npath = \"bundles/rust-review\"\n",
            );

            let plugins = Config::discover(root).unwrap().plugins();
            if scope_file == "io.toml" {
                assert_eq!(plugins.len(), 0, "{offending} from a project scope");
                assert_eq!(plugins.dropped().len(), 1);
                let error = &plugins.dropped()[0].error;
                assert!(
                    error.contains(offending) && error.contains("io.local.toml"),
                    "the refusal names the key and where it may live: {error}"
                );
                // Nothing else of the manifest reached anything.
                let contract = plugins.apply_to(contract(root));
                let policy = plugins.apply_to_policy(Policy::permissive());
                assert!(contract.agents.names().is_empty(), "no agent");
                assert!(contract.mcp.is_empty(), "no server");
                assert!(contract.plugins.is_empty(), "no skills");
                assert_eq!(policy.layers.len(), 0, "no layer");
            } else {
                assert_eq!(plugins.len(), 1, "{offending} from a local scope");
                assert!(plugins.dropped().is_empty(), "{:?}", plugins.dropped());
            }
        }
    }
}

/// **F4**, the hook arm's positive half taken all the way: a locally declared
/// bundle's hook is installed and fires on a real event.
#[tokio::test]
async fn a_locally_declared_bundle_hook_runs() {
    let dir = tmp();
    let root = dir.path();
    let plugin = bundle(root, "rust-review");
    let manifest = std::fs::read_to_string(plugin.join("plugin.toml")).unwrap();
    write(
        &plugin.join("plugin.toml"),
        &format!("{manifest}\n[[hook]]\non = [\"finished\"]\nappend = \"audit.jsonl\"\n"),
    );
    write(
        &root.join("io.local.toml"),
        "[[plugin]]\npath = \"bundles/rust-review\"\n",
    );

    let config = Config::discover(root).unwrap();
    let plugins = config.plugins();
    let hooks = plugins.apply_to_hooks(config.hooks(), root);
    assert!(!hooks.is_empty(), "the bundle's hook was installed");

    let contract = plugins.apply_to(contract(root));
    let policy = plugins.apply_to_policy(Policy::permissive());
    io_harness::run_with_observed(
        &contract,
        &Script::of(vec![vec![]]),
        &store(root),
        &policy,
        &ApproveAll,
        &hooks,
    )
    .await
    .unwrap();

    let log = std::fs::read_to_string(root.join("audit.jsonl")).unwrap();
    assert!(
        log.contains("finished"),
        "the hook wrote the event it asked for: {log}"
    );
}

// ------------------------------------------------------------------ F5

/// **F5**, asserted by effect. A manifest whose layer would grant something is
/// dropped, *and* the act it would have granted is still refused — which is what
/// fails if the loader validated after merging the layer.
#[tokio::test]
async fn plugin_policy_may_only_narrow() {
    for (name, needle, block) in [
        (
            "allow",
            "policy.layers",
            "[policy]\nlayers = [{ name = \"open\", rules = [\n\
             { act = \"write\", effect = \"allow\", pattern = \"secrets/**\" },\n] }]\n",
        ),
        (
            "defaults",
            "policy.defaults",
            "[policy]\ndefaults = { write = \"allow\" }\nlayers = []\n",
        ),
    ] {
        let dir = tmp();
        let root = dir.path();
        let plugin = root.join("bundles/wide");
        write(
            &plugin.join("plugin.toml"),
            &format!("name = \"wide\"\n\n{block}"),
        );
        write(
            &root.join("io.local.toml"),
            "[[plugin]]\npath = \"bundles/wide\"\n",
        );

        let plugins = Config::discover(root).unwrap().plugins();
        assert_eq!(plugins.len(), 0, "the {name} bundle is dropped");
        assert!(
            plugins.dropped()[0].error.contains(needle),
            "the refusal names the key at fault: {}",
            plugins.dropped()[0].error
        );

        // The application's own boundary still decides, unchanged.
        let base = Policy::permissive().layer("app").deny_write("secrets/**");
        let policy = plugins.apply_to_policy(base);
        assert_eq!(
            policy.check(Act::Write, "secrets/token.txt").effect,
            Effect::Deny,
            "the {name} bundle granted nothing"
        );
    }
}

// ------------------------------------------------------------------ F6

/// **F6**. Three bundles broken three ways beside one that is not: the run
/// completes, each break is reported once with its own reason, and the valid
/// bundle's contributions are all present. The control repairs every manifest and
/// asserts nothing is dropped — without it, a loader that dropped everything
/// would pass the first half.
#[tokio::test]
async fn a_broken_bundle_costs_exactly_itself() {
    for repaired in [false, true] {
        let dir = tmp();
        let root = dir.path();
        bundle(root, "good");

        // No manifest at all.
        std::fs::create_dir_all(root.join("bundles/absent")).unwrap();
        if repaired {
            write(
                &root.join("bundles/absent/plugin.toml"),
                "name = \"absent\"\n",
            );
        }
        // Unparseable TOML.
        write(
            &root.join("bundles/unparseable/plugin.toml"),
            if repaired {
                "name = \"unparseable\"\n"
            } else {
                "name = \"unparseable\n"
            },
        );
        // A key the format does not have.
        write(
            &root.join("bundles/unknown/plugin.toml"),
            if repaired {
                "name = \"unknown\"\n"
            } else {
                "name = \"unknown\"\nskils = \"skills\"\n"
            },
        );

        write(
            &root.join("io.local.toml"),
            "[[plugin]]\npath = \"bundles/good\"\n\
             [[plugin]]\npath = \"bundles/absent\"\n\
             [[plugin]]\npath = \"bundles/unparseable\"\n\
             [[plugin]]\npath = \"bundles/unknown\"\n",
        );

        let plugins = Config::discover(root).unwrap().plugins();
        let expected_drops = if repaired { 0 } else { 3 };
        assert_eq!(
            plugins.dropped().len(),
            expected_drops,
            "repaired = {repaired}"
        );
        assert_eq!(plugins.len(), 4 - expected_drops);

        if !repaired {
            let reasons: Vec<&str> = plugins.dropped().iter().map(|d| d.error.as_str()).collect();
            assert!(
                reasons.iter().any(|r| r.contains("plugin.toml")),
                "{reasons:?}"
            );
            assert!(
                reasons.iter().any(|r| r.contains("unknown field")),
                "{reasons:?}"
            );
            assert_eq!(
                reasons
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                3,
                "three different causes, not one repeated: {reasons:?}"
            );
        }

        // The valid bundle is unaffected either way, and the run is not.
        let contract = plugins.apply_to(contract(root));
        assert!(contract.agents.get("good__reviewer").is_some());

        let recorder = Recorder::default();
        let policy = plugins.apply_to_policy(Policy::permissive());
        io_harness::run_with_observed(
            &contract,
            &Script::of(vec![vec![]]),
            &store(root),
            &policy,
            &ApproveAll,
            &recorder,
        )
        .await
        .unwrap();
        assert_eq!(
            recorder.dropped().len(),
            expected_drops,
            "reported to the observer"
        );
        assert_eq!(recorder.loaded().len(), 4 - expected_drops);
    }
}

/// **F6**, the id rules. A malformed id and a duplicate id each drop their own
/// bundle and nothing else.
#[test]
fn a_bad_or_duplicate_id_drops_only_its_own_bundle() {
    let dir = tmp();
    let root = dir.path();
    bundle(root, "good");
    for (name, id) in [
        ("upper", "Rust_Review"),
        ("separator", "a__b"),
        ("long", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), // 33
    ] {
        write(
            &root.join(format!("bundles/{name}/plugin.toml")),
            &format!("name = \"{id}\"\n"),
        );
    }
    // A second directory claiming an id that is already loaded.
    write(&root.join("bundles/twin/plugin.toml"), "name = \"good\"\n");

    write(
        &root.join("io.local.toml"),
        "[[plugin]]\npath = \"bundles/good\"\n\
         [[plugin]]\npath = \"bundles/upper\"\n\
         [[plugin]]\npath = \"bundles/separator\"\n\
         [[plugin]]\npath = \"bundles/long\"\n\
         [[plugin]]\npath = \"bundles/twin\"\n",
    );

    let plugins = Config::discover(root).unwrap().plugins();
    assert_eq!(plugins.names(), vec!["good"]);
    assert_eq!(plugins.dropped().len(), 4);
    assert!(plugins
        .dropped()
        .iter()
        .any(|d| d.error.contains("already loaded")));
}

// ------------------------------------------------------------------ F7

/// **F7**. Two bundles and the contract's own directory each carrying a `review`
/// skill: all three are offered, under three names. The control removes the
/// namespacing by pointing the contract at a bundle's own skills directory
/// directly, under which the duplicate refusal fires — the failure that shows the
/// property is load-bearing rather than incidental.
#[tokio::test]
async fn namespacing_makes_a_collision_impossible() {
    let dir = tmp();
    let root = dir.path();
    bundle(root, "alpha");
    bundle(root, "beta");
    write(
        &root.join("own/review.md"),
        "---\nname: review\ndescription: ours\n---\n\nOur own.\n",
    );
    write(
        &root.join("io.local.toml"),
        "[[plugin]]\npath = \"bundles/alpha\"\n[[plugin]]\npath = \"bundles/beta\"\n",
    );

    let plugins = Config::discover(root).unwrap().plugins();
    let contract = plugins
        .apply_to(contract(root))
        .with_skills(root.join("own"));

    let provider = Script::of(vec![vec![]]);
    run_with(
        &contract,
        &provider,
        &store(root),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    let prompt = provider.request(0).system.clone();
    for name in ["alpha__review", "beta__review"] {
        assert!(prompt.contains(name), "{name} is offered");
    }
    assert!(
        prompt.contains("- review:"),
        "and the operator's own keeps its bare name"
    );

    // The control: the same two skills with no namespace between them — one
    // directory holding both — under which discovery refuses the catalogue
    // outright. Without namespacing that is what two bundles would produce.
    write(
        &root.join("flat/review.md"),
        "---\nname: review\ndescription: alpha\n---\n\nA.\n",
    );
    write(
        &root.join("flat/other/SKILL.md"),
        "---\nname: review\ndescription: beta\n---\n\nB.\n",
    );
    let err = io_harness::skills::Skills::discover(root.join("flat")).unwrap_err();
    assert!(
        err.to_string().contains("review"),
        "an un-namespaced collision is a refused catalogue: {err}"
    );
}

// ------------------------------------------------------------------ F8

/// **F8**. `${cmd:}` is refused inside a manifest from every scope, because a
/// bundle is a third party's directory wherever the file naming it lives.
#[test]
fn a_manifest_may_not_run_a_command_in_any_scope() {
    for scope_file in ["io.toml", "io.local.toml"] {
        let dir = tmp();
        let root = dir.path();
        write(
            &root.join("bundles/sneaky/plugin.toml"),
            "name = \"sneaky\"\ndescription = \"${cmd:echo hello}\"\n",
        );
        write(
            &root.join(scope_file),
            "[[plugin]]\npath = \"bundles/sneaky\"\n",
        );

        let plugins = Config::discover(root).unwrap().plugins();
        assert_eq!(plugins.len(), 0, "dropped from {scope_file}");
        assert!(
            plugins.dropped()[0].error.contains("cmd"),
            "the refusal names the substitution: {}",
            plugins.dropped()[0].error
        );
    }
}

// ------------------------------------------------------------------ N3

/// **N3**. Loading is the caller's, once, before the run: no function in the
/// run's per-step loop reaches into the plugin module. Asserted by reading
/// `src/run.rs`, with a splice-in control that must fail — a source-reading test
/// that cannot fail is a comment.
#[test]
fn the_step_loop_never_touches_a_plugin() {
    let source = run_subsystem_source();
    let uses: Vec<&str> = source
        .lines()
        .filter(|l| l.contains("crate::plugin") || l.contains("plugin::"))
        .collect();
    assert!(
        uses.iter().all(|l| l.contains("///") || l.contains("//!")),
        "the run module names the plugin module only in documentation: {uses:?}"
    );

    // The control: the same check over a source that does call into it.
    let spliced = source.replace(
        "fn emit_plugins(",
        "fn spliced() { let _ = crate::plugin::Plugins::none(); }\nfn emit_plugins(",
    );
    let control: Vec<&str> = spliced
        .lines()
        .filter(|l| l.contains("crate::plugin") || l.contains("plugin::"))
        .filter(|l| !l.contains("///") && !l.contains("//!"))
        .collect();
    assert_eq!(
        control.len(),
        1,
        "the control finds the call this test exists to catch"
    );
}

/// `src/run.rs` and every `src/run/<subject>.rs`, concatenated.
///
/// 0.63.0 moved the run subsystem's private machinery into submodules, so a
/// source-reading checker pointed at the parent alone now sees a fraction of it —
/// and a count that comes back zero reads exactly like a rule that was deleted.
/// The floor below turns "the walk went blind" into a failure instead of a pass.
fn run_subsystem_source() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = std::fs::read_to_string(root.join("src/run.rs"))
        .expect("src/run.rs")
        .replace("\r\n", "\n");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("src/run"))
        .expect("src/run/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 5,
        "src/run/ holds only {} modules — the split has been undone or this walk is blind, \
         and either way every count taken from it is meaningless",
        paths.len()
    );
    for path in paths {
        all.push('\n');
        all.push_str(
            &std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n"),
        );
    }
    all
}
