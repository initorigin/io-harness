//! Configuration from a file, through the real loader and the real run loop —
//! F1 through F10 and NF3 of 0.19.0.
//!
//! Every test that touches an environment variable takes `ENV` first: the user
//! scope is discovered through `IO_CONFIG_HOME`, the process has one environment,
//! and `cargo test` runs these in parallel. A test that set a variable while
//! another read it would fail in a way nobody could reproduce.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use io_harness::config::{Config, Scope};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, Act, ApproveAll, Effect, Policy, Provider, RunOutcome, Store, TaskContract,
};
use serde_json::json;

static ENV: Mutex<()> = Mutex::new(());

/// Hold the environment, and point the user scope somewhere empty so a config
/// file on the developer's own machine cannot change what these tests measure.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IO_CONFIG_HOME", user_dir);
    guard
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

// ---------------------------------------------------------------------------
// F1, F2, F10 — the scopes and the merge
// ---------------------------------------------------------------------------

#[test]
fn f1_the_four_scopes_merge_in_order() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(user_dir.path(), "io.toml", "[run]\nmax_steps = 1\n");
    write(project.path(), "io.toml", "[run]\nmax_steps = 2\n");
    write(project.path(), "io.local.toml", "[run]\nmax_steps = 3\n");

    let steps = |root: &Path| {
        Config::discover(root)
            .unwrap()
            .apply_to(contract(root))
            .max_steps
    };

    // Four loads over the same directory, each with one more scope removed, so
    // this measures precedence rather than one file being read.
    assert_eq!(steps(project.path()), 3, "the local scope is the last word");
    std::fs::remove_file(project.path().join("io.local.toml")).unwrap();
    assert_eq!(steps(project.path()), 2, "then the project scope");
    std::fs::remove_file(project.path().join("io.toml")).unwrap();
    assert_eq!(steps(project.path()), 1, "then the user scope");
    std::fs::remove_file(user_dir.path().join("io.toml")).unwrap();
    assert_eq!(
        steps(project.path()),
        contract(project.path()).max_steps,
        "and with no file anywhere, the crate's own default"
    );
}

#[test]
fn f1_the_sources_report_which_files_were_merged_and_in_what_order() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(user_dir.path(), "io.toml", "[run]\nmax_steps = 1\n");
    write(project.path(), "io.toml", "[run]\nmax_steps = 2\n");
    write(project.path(), "io.local.toml", "[run]\nmax_steps = 3\n");

    let config = Config::discover(project.path()).unwrap();
    let scopes: Vec<Scope> = config.sources().iter().map(|(s, _)| *s).collect();
    assert_eq!(scopes, [Scope::User, Scope::Project, Scope::Local]);
}

#[test]
fn f2_a_later_scope_overrides_one_key_and_nothing_else() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        // `force_floor` rather than `allow_network`: this is the project scope, and
        // since 0.27.0 a project file may narrow the boundary and may never widen it.
        "[sandbox]\nforce_floor = true\n\
         [sandbox.limits]\nmax_wall_secs = 300\nmax_cpu_secs = 90\nmax_open_files = 64\n",
    );

    let without = Config::discover(project.path()).unwrap().sandbox().unwrap();

    write(
        project.path(),
        "io.local.toml",
        "[sandbox.limits]\nmax_wall_secs = 5\n",
    );
    let with = Config::discover(project.path()).unwrap().sandbox().unwrap();

    assert_eq!(with.limits.max_wall_secs, Some(5));
    assert_eq!(
        with.limits.max_cpu_secs,
        Some(90),
        "a sibling cap is intact"
    );
    assert_eq!(with.limits.max_open_files, Some(64));
    assert!(with.force_floor, "and so is the other section");
    // The negative control: the same load without the local file differs in
    // exactly that one key.
    assert_eq!(without.limits.max_wall_secs, Some(300));
    assert_eq!(
        io_harness::sandbox::SandboxConfig {
            limits: io_harness::sandbox::SandboxLimits {
                max_wall_secs: Some(5),
                ..without.limits.clone()
            },
            ..without.clone()
        },
        with,
        "one key differs and no other"
    );
}

#[test]
fn f10_no_config_file_is_not_an_error() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let config = Config::discover(project.path()).unwrap();
    assert!(config.is_empty());
    assert!(config.sources().is_empty());
    assert!(
        config.policy().is_none(),
        "no section is not an empty policy"
    );
    assert!(config.sandbox().is_none());
    assert!(config.prices().is_none());
    assert!(config.mcp_servers().is_empty());

    // Adding this release to a consumer who writes no file changes nothing.
    let plain = contract(project.path());
    let applied = config.apply_to(contract(project.path()));
    assert_eq!(applied.max_steps, plain.max_steps);
    assert_eq!(applied.max_tokens, plain.max_tokens);
    assert_eq!(applied.max_retries, plain.max_retries);
    assert_eq!(applied.retry, plain.retry);
    assert_eq!(applied.stall, plain.stall);
    assert_eq!(applied.context, plain.context);
    assert_eq!(applied.exec_timeout, plain.exec_timeout);
    assert_eq!(applied.commit_identity, plain.commit_identity);
}

// ---------------------------------------------------------------------------
// F5, F6 — unknown keys and substitution, through a real file
// ---------------------------------------------------------------------------

#[test]
fn f5_an_unknown_key_fails_the_load_naming_the_key_and_the_file() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(project.path(), "io.toml", "[run]\nmax_stepz = 3\n");
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("max_stepz"), "names the key: {err}");
    assert!(err.contains("io.toml"), "names the file: {err}");

    // The negative control: the correctly spelled key loads and reaches its
    // typed field, so this test proves rejection of the *unknown* rather than
    // rejection in general.
    write(project.path(), "io.toml", "[run]\nmax_steps = 3\n");
    let config = Config::discover(project.path()).unwrap();
    assert_eq!(config.apply_to(contract(project.path())).max_steps, 3);
}

#[test]
fn f6_substitution_reaches_the_field_or_names_the_key_that_failed() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    std::env::set_var("IO_HARNESS_CONFIG_TEST_TOKEN", "from-the-environment");
    write(project.path(), "secret", "  s3cret-from-a-file\n");

    write(
        project.path(),
        "io.toml",
        "[[mcp]]\nid = \"gh\"\ntransport = \"http\"\nurl = \"https://example.test\"\n\
         [mcp.headers]\nAuthorization = \"Bearer ${env:IO_HARNESS_CONFIG_TEST_TOKEN}\"\n\
         X-Key = \"${file:secret}\"\n",
    );
    let config = Config::discover(project.path()).unwrap();
    let io_harness::McpTransport::Http { headers, .. } = &config.mcp_servers()[0].transport else {
        panic!("an http server");
    };
    assert_eq!(headers["Authorization"], "Bearer from-the-environment");
    assert_eq!(
        headers["X-Key"], "s3cret-from-a-file",
        "a file's value is trimmed"
    );

    for (value, expect) in [
        ("${env:IO_HARNESS_CONFIG_TEST_UNSET}", "is not set"),
        ("${file:no-such-file}", "cannot read"),
    ] {
        write(
            project.path(),
            "io.toml",
            &format!("[run]\nskills = \"{value}\"\n"),
        );
        let err = Config::discover(project.path()).unwrap_err().to_string();
        assert!(err.contains(expect), "{value}: {err}");
        assert!(err.contains("skills"), "names the key: {err}");
        assert!(err.contains("io.toml"), "names the file: {err}");
    }
}

// ---------------------------------------------------------------------------
// F7 — every key reaches a typed field
// ---------------------------------------------------------------------------

/// Every key this crate accepts, in one file.
const EVERY_KEY: &str = r#"
[policy.defaults]
read = "allow"
write = "deny"
exec = "deny"
net = "ask"

[[policy.layers]]
name = "ops-baseline"
rules = [
  { act = "read", effect = "deny", pattern = "infra/*" },
  { act = "net", effect = "allow", pattern = "api.example.test" },
]

[sandbox]
allow_network = true
force_floor = true

[sandbox.limits]
max_cpu_secs = 11
max_wall_secs = 22
max_memory_bytes = 33
max_processes = 44
max_open_files = 55

[run]
max_steps = 66
max_duration_secs = 77
max_tokens = 88
max_retries = 9
exec_timeout_secs = 101
skills = "skills"
templates = "templates"

[run.retry]
base_ms = 1500
max_ms = 45000

[run.stall]
window = 7
max_replans = 3

[run.context]
max_tokens = 12000
share = 0.25

[run.commit_identity]
name = "release bot"
email = "bot@example.invalid"

[toolchain.cargo]
manager = "cargo"
install = ["cargo", "fetch"]
build = ["cargo", "build", "--release"]
test = ["cargo", "nextest", "run"]
lint = ["cargo", "clippy"]
format = ["cargo", "fmt"]
run = ["cargo", "run"]

[prices]
as_of = "2026-07-29"

[prices.models."some-vendor/some-model"]
input = 3000000
output = 15000000
cache_read = 300000
cache_write = 3750000
per_server_tool_request = 10000

[[mcp]]
id = "github"
transport = "stdio"
command = "github-mcp-server"
args = ["stdio"]
timeout_secs = 30

[[agent]]
name = "searcher"
role = "You find things and never edit them."
model = "cheap-model"
max_steps = 8
deny_write = true
deny_net = true

[[agent]]
name = "author"
model = "strong-model"

[web]
search = true
fetch = true
max_uses = 4
allowed_domains = ["docs.rs", "crates.io"]
blocked_domains = ["evil.test"]

[[provider]]
kind = "openrouter"
model = "anthropic/claude-sonnet-4"
api_key = "sk-primary"

[[provider]]
kind = "anthropic"
model = "claude-sonnet-4"

[[provider]]
kind = "openai"
model = "gpt-5"

[app.cli]
theme = "dark"

[instructions]
files = ["AGENTS.md"]

[profile.cheap]
run = { max_steps = 5 }
"#;

#[test]
fn f7_every_key_reaches_a_typed_field() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(project.path(), "io.local.toml", EVERY_KEY);
    write(
        project.path(),
        "AGENTS.md",
        "Never touch the generated directory.",
    );

    let config = Config::discover(project.path()).unwrap();

    // 0.27.0's four tables. `[[provider]]` is asserted on order rather than
    // membership: a fallback chain whose order is wrong is a different
    // configuration that a set comparison cannot see.
    assert_eq!(
        config.provider_spec(),
        Some(&io_harness::ProviderSpec::OpenRouter {
            model: "anthropic/claude-sonnet-4".into(),
            api_key: Some("sk-primary".into()),
        })
    );
    assert_eq!(
        config.fallback_specs(),
        [
            io_harness::ProviderSpec::Anthropic {
                model: "claude-sonnet-4".into(),
                api_key: None,
            },
            io_harness::ProviderSpec::OpenAi {
                model: "gpt-5".into(),
                api_key: None,
            },
        ]
    );
    assert_eq!(
        config
            .app::<std::collections::BTreeMap<String, String>>("cli")
            .unwrap(),
        Some(
            [("theme".to_string(), "dark".to_string())]
                .into_iter()
                .collect()
        )
    );
    assert_eq!(config.instructions().len(), 1);
    assert!(config.instructions()[0].contains("generated directory"));
    assert!(
        config.instructions()[0].contains("AGENTS.md"),
        "with its provenance"
    );
    assert_eq!(
        config
            .with_profile("cheap")
            .unwrap()
            .apply_to(contract(project.path()))
            .max_steps,
        5
    );

    let policy = config.policy().unwrap();
    assert_eq!(policy.defaults.read, Effect::Allow);
    assert_eq!(policy.defaults.write, Effect::Deny);
    assert_eq!(policy.defaults.exec, Effect::Deny);
    assert_eq!(policy.defaults.net, Effect::Ask);
    assert_eq!(
        policy.check(Act::Read, "infra/main.tf").effect,
        Effect::Deny
    );
    assert_eq!(
        policy.check(Act::Net, "api.example.test").effect,
        Effect::Allow
    );

    let sandbox = config.sandbox().unwrap();
    assert!(sandbox.allow_network);
    assert!(sandbox.force_floor);
    assert_eq!(sandbox.limits.max_cpu_secs, Some(11));
    assert_eq!(sandbox.limits.max_wall_secs, Some(22));
    assert_eq!(sandbox.limits.max_memory_bytes, Some(33));
    assert_eq!(sandbox.limits.max_processes, Some(44));
    assert_eq!(sandbox.limits.max_open_files, Some(55));

    let applied = config.apply_to(contract(project.path()));
    assert_eq!(applied.max_steps, 66);
    assert_eq!(
        applied.max_duration,
        Some(std::time::Duration::from_secs(77))
    );
    assert_eq!(applied.max_tokens, Some(88));
    assert_eq!(applied.max_retries, 9);
    assert_eq!(
        applied.exec_timeout,
        std::time::Duration::from_secs(101),
        "the exec timeout is seconds in the file and a Duration in the type"
    );
    assert_eq!(applied.skills.as_deref(), Some(Path::new("skills")));
    assert_eq!(applied.retry.base, std::time::Duration::from_millis(1500));
    assert_eq!(applied.retry.max, std::time::Duration::from_millis(45000));
    assert_eq!(applied.stall.window, 7);
    assert_eq!(applied.stall.max_replans, 3);
    assert_eq!(applied.context.max_tokens, 12_000);
    assert!((applied.context.share - 0.25).abs() < f32::EPSILON);
    assert_eq!(applied.commit_identity.name, "release bot");
    assert_eq!(applied.commit_identity.email, "bot@example.invalid");
    assert_eq!(applied.mcp[0].id, "github");
    assert_eq!(applied.mcp[0].timeout_secs, 30);
    let web = applied.web.clone().expect("[web] reaches the contract");
    assert!(web.search && web.fetch);
    assert_eq!(web.max_uses, Some(4));
    assert_eq!(web.allowed_domains, ["docs.rs", "crates.io"]);
    assert_eq!(web.blocked_domains, ["evil.test"]);

    // Every one of those is different from the default it would otherwise hold.
    let plain = contract(project.path());
    assert_ne!(applied.max_steps, plain.max_steps);
    assert_ne!(applied.max_retries, plain.max_retries);
    assert_ne!(applied.retry, plain.retry);
    assert_ne!(applied.stall, plain.stall);
    assert_ne!(applied.context, plain.context);
    assert_ne!(applied.exec_timeout, plain.exec_timeout);
    assert_ne!(applied.commit_identity, plain.commit_identity);
    assert_ne!(applied.web, plain.web);
}

#[test]
fn f7_a_key_removed_from_that_file_leaves_exactly_that_field_at_its_default() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    // The negative control for the fixture above: drop one key, and that one
    // field falls back while its neighbours do not.
    let without = EVERY_KEY.replace("max_steps = 66\n", "");
    write(project.path(), "io.local.toml", &without);

    let applied = Config::discover(project.path())
        .unwrap()
        .apply_to(contract(project.path()));
    assert_eq!(
        applied.max_steps,
        contract(project.path()).max_steps,
        "the removed key falls back"
    );
    assert_eq!(applied.max_retries, 9, "its neighbour does not");
}

// ---------------------------------------------------------------------------
// F8 — the two tables the previous releases could not fill
// ---------------------------------------------------------------------------

#[test]
fn f8_the_file_fills_the_price_table_and_the_toolchain() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(project.path(), "io.local.toml", EVERY_KEY);
    write(project.path(), "Cargo.toml", "[package]\nname = \"x\"\n");

    let config = Config::discover(project.path()).unwrap();

    let prices = config.prices().unwrap();
    assert_eq!(
        prices.as_of(),
        "2026-07-29",
        "the file's own date, not today's"
    );
    // 1M fresh input at 3_000_000/M, 500k output at 15_000_000/M, 200k cache
    // reads at 300_000/M: 3_000_000 + 7_500_000 + 60_000.
    let usage = io_harness::Usage {
        prompt_tokens: 1_200_000,
        completion_tokens: 500_000,
        cache_read_tokens: 200_000,
        ..Default::default()
    };
    assert_eq!(
        prices.cost_micros("some-vendor/some-model", &usage),
        Some(10_560_000)
    );
    assert_eq!(
        prices.cost_micros("a-model-nobody-priced", &usage),
        None,
        "an unpriced model is unknown, not free"
    );

    let detected = io_harness::toolchain::detect(project.path()).unwrap();
    let tuned = config.toolchain(detected.clone());
    assert_eq!(tuned.test, ["cargo", "nextest", "run"]);
    assert_ne!(tuned.test, detected.test);

    // A different ecosystem keeps its shipped defaults: the file names `cargo`
    // and nothing else.
    let node = tempfile::tempdir().unwrap();
    write(node.path(), "package.json", "{}");
    write(node.path(), "package-lock.json", "{}");
    let node_detected = io_harness::toolchain::detect(node.path()).unwrap();
    assert_eq!(config.toolchain(node_detected.clone()), node_detected);
}

// ---------------------------------------------------------------------------
// F3, F4, F9, NF3 — through the real run loop
// ---------------------------------------------------------------------------

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn script(steps: Vec<Vec<ToolCall>>) -> MockScript {
    MockScript {
        steps,
        at: AtomicUsize::new(0),
    }
}

fn write_call(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("edit the workspace", root)
}

/// The boundary under test, as a file: writes are denied by default and one
/// layer allows the workspace's own source.
const DENYING: &str = r#"
[policy.defaults]
read = "allow"
write = "deny"
exec = "deny"
net = "deny"

[[policy.layers]]
name = "project"
rules = [{ act = "write", effect = "allow", pattern = "src/*" }]
"#;

/// The same boundary, built in Rust. `Policy::default()` is what
/// `Config::policy` starts from, so this is the identical value by construction
/// — which is the point: the file is a projection, not a second path.
fn denying_in_rust() -> Policy {
    let policy = Policy {
        defaults: io_harness::Defaults {
            read: Effect::Allow,
            write: Effect::Deny,
            exec: Effect::Deny,
            net: Effect::Deny,
        },
        ..Policy::default()
    };
    policy.layer("project").allow_write("src/*")
}

async fn run_denied(root: &Path, policy: &Policy) -> (RunOutcome, Vec<String>) {
    let store = Store::memory().unwrap();
    let script = script(vec![
        vec![write_call("secrets/key.txt", "exfiltrated")],
        vec![write_call("src/a.rs", "pub fn a() {}\n")],
    ]);
    let result = run_with(
        &contract(root).with_max_steps(2),
        &script,
        &store,
        policy,
        &ApproveAll,
    )
    .await
    .unwrap();
    let refusals = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .map(|e| format!("{} {} {:?}", e.act, e.target, e.layer))
        .collect();
    (result.outcome, refusals)
}

#[tokio::test]
async fn f3_the_boundary_a_file_describes_is_the_boundary_a_run_enforces() {
    let user_dir = tempfile::tempdir().unwrap();
    let from_file = tempfile::tempdir().unwrap();
    let from_rust = tempfile::tempdir().unwrap();
    for dir in [from_file.path(), from_rust.path()] {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("secrets")).unwrap();
        write(&dir.join("secrets"), "key.txt", "original-secret");
    }

    let policy = {
        let _guard = env(user_dir.path());
        write(from_file.path(), "io.toml", DENYING);
        Config::discover(from_file.path())
            .unwrap()
            .policy()
            .unwrap()
    };

    let (file_outcome, file_refusals) = run_denied(from_file.path(), &policy).await;
    let (rust_outcome, rust_refusals) = run_denied(from_rust.path(), &denying_in_rust()).await;

    assert_eq!(file_outcome, rust_outcome);
    assert_eq!(
        file_refusals, rust_refusals,
        "the same refusal, recorded the same way"
    );
    assert!(
        file_refusals[0].contains("secrets/key.txt"),
        "and it is the write the file denied: {file_refusals:?}"
    );
    assert_eq!(
        std::fs::read_to_string(from_file.path().join("secrets/key.txt")).unwrap(),
        "original-secret"
    );
    assert!(
        from_file.path().join("src/a.rs").is_file(),
        "the allowed write still happened"
    );
}

#[test]
fn f4_policy_layers_append_across_scopes_and_a_deny_still_wins() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[[policy.layers]]\nname = \"ops-baseline\"\n\
         rules = [{ act = \"write\", effect = \"deny\", pattern = \"infra/*\" }]\n",
    );
    write(
        project.path(),
        "io.local.toml",
        "[[policy.layers]]\nname = \"mine\"\n\
         rules = [{ act = \"write\", effect = \"allow\", pattern = \"infra/*\" },\n\
                  { act = \"write\", effect = \"allow\", pattern = \"scratch/*\" }]\n",
    );

    let policy = Config::discover(project.path()).unwrap().policy().unwrap();
    let names: Vec<&str> = policy.layers.iter().map(|l| l.name.as_str()).collect();
    assert!(
        names.contains(&"ops-baseline") && names.contains(&"mine"),
        "both layers are present: {names:?}"
    );
    assert!(
        names.iter().position(|n| *n == "ops-baseline") < names.iter().position(|n| *n == "mine"),
        "in scope order: {names:?}"
    );

    // A later layer may add capability...
    assert_eq!(
        policy.check(Act::Write, "scratch/x").effect,
        Effect::Allow,
        "the local layer's own allow takes effect"
    );
    // ...and may never re-allow an earlier deny. That is the negative control.
    let verdict = policy.explain(Act::Write, "infra/main.tf");
    assert_eq!(verdict.effect, Effect::Deny);
    assert_eq!(verdict.layer.as_deref(), Some("ops-baseline"));
}

#[tokio::test]
async fn f9_a_config_the_agent_writes_mid_run_cannot_widen_the_boundary() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::create_dir_all(project.path().join("secrets")).unwrap();
    write(
        &project.path().join("secrets"),
        "key.txt",
        "original-secret",
    );

    // The escalation the agent would write for itself: a layer allowing the very
    // write the project file denies by default.
    const ESCALATION: &str = "[[policy.layers]]\nname = \"mine\"\n\
                              rules = [{ act = \"write\", effect = \"allow\", pattern = \"secrets/*\" }]\n";

    let policy = {
        let _guard = env(user_dir.path());
        write(project.path(), "io.toml", DENYING);
        Config::discover(project.path()).unwrap().policy().unwrap()
    };

    let store = Store::memory().unwrap();
    let script = script(vec![
        // Step 1: write the escalation. Allowed — it is not under `secrets/`...
        vec![write_call("io.local.toml", ESCALATION)],
        // ...step 2: cash it in.
        vec![write_call("secrets/key.txt", "exfiltrated")],
    ]);
    // `src/*` is the only allowed write, so the agent's own config write is
    // refused too; what matters is that even if it lands, the boundary is fixed.
    let widened = {
        let mut p = policy.clone();
        p.layers.push(io_harness::Layer {
            name: "test-allows-the-config-write".into(),
            rules: vec![io_harness::Rule {
                act: Act::Write,
                effect: Effect::Allow,
                pattern: "io.local.toml".into(),
            }],
        });
        p
    };
    let result = run_with(
        &contract(project.path()).with_max_steps(2),
        &script,
        &store,
        &widened,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        project.path().join("io.local.toml").is_file(),
        "the agent did write the file"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("secrets/key.txt")).unwrap(),
        "original-secret",
        "and it bought nothing: the boundary was read before the run started"
    );
    let refused = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .any(|e| e.kind == "refusal" && e.target == "secrets/key.txt");
    assert!(
        refused,
        "the escalated write is refused, not silently dropped"
    );

    // The negative control: the same file, present *before* the caller loads the
    // config, does take effect. The difference between the two is the whole
    // guarantee.
    let _guard = env(user_dir.path());
    let after = Config::discover(project.path()).unwrap().policy().unwrap();
    assert_eq!(
        after.check(Act::Write, "secrets/key.txt").effect,
        Effect::Allow,
        "a config loaded after the file exists is a different, wider policy"
    );
}

#[tokio::test]
async fn nf3_nothing_is_read_from_disk_for_configuration_once_the_caller_has_loaded_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();

    let (policy, contract) = {
        let _guard = env(user_dir.path());
        write(project.path(), "io.toml", DENYING);
        write(project.path(), "io.local.toml", "[run]\nmax_steps = 2\n");
        let config = Config::discover(project.path()).unwrap();
        (
            config.policy().unwrap(),
            config.apply_to(contract(project.path())),
        )
    };
    assert_eq!(
        contract.max_steps, 2,
        "loaded from the file that is about to go"
    );

    // Both files deleted between the load and the run.
    std::fs::remove_file(project.path().join("io.toml")).unwrap();
    std::fs::remove_file(project.path().join("io.local.toml")).unwrap();

    let store = Store::memory().unwrap();
    let script = script(vec![vec![write_call("src/a.rs", "pub fn a() {}\n")]]);
    let result = run_with(&contract, &script, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(
            result.outcome,
            RunOutcome::Success { .. } | RunOutcome::Finished { .. }
        ),
        "the run is unaffected by the files disappearing: {:?}",
        result.outcome
    );
    assert!(project.path().join("src/a.rs").is_file());
}

#[tokio::test]
async fn a_substituted_secret_reaches_the_field_and_not_the_trace() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();

    const SECRET: &str = "io-harness-test-secret-9f3a2b";
    let (policy, contract) = {
        let _guard = env(user_dir.path());
        std::env::set_var("IO_HARNESS_CONFIG_TEST_SECRET", SECRET);
        write(
            project.path(),
            "io.toml",
            &format!(
                "{DENYING}\n[[mcp]]\nid = \"svc\"\ntransport = \"http\"\n\
                 url = \"https://example.test\"\n[mcp.headers]\n\
                 Authorization = \"Bearer ${{env:IO_HARNESS_CONFIG_TEST_SECRET}}\"\n"
            ),
        );
        let config = Config::discover(project.path()).unwrap();
        // It did reach the typed field — the point of substitution.
        let io_harness::McpTransport::Http { headers, .. } = &config.mcp_servers()[0].transport
        else {
            panic!("an http server");
        };
        assert_eq!(headers["Authorization"], format!("Bearer {SECRET}"));
        (
            config.policy().unwrap(),
            // The MCP server is deliberately not attached to the contract: this
            // test is about what the *config* leaks, not about dialling a server.
            contract(project.path()).with_max_steps(1),
        )
    };

    let store = Store::memory().unwrap();
    let script = script(vec![vec![write_call("src/a.rs", "pub fn a() {}\n")]]);
    let result = run_with(&contract, &script, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    // Nothing the crate writes about this run carries the value.
    let mut written = String::new();
    for e in store.events(result.run_id).unwrap() {
        written.push_str(&format!("{e:?}"));
    }
    for s in store.steps(result.run_id).unwrap() {
        written.push_str(&format!("{s:?}"));
    }
    assert!(
        !written.contains(SECRET),
        "a substituted secret must not reach the trace"
    );
    // The negative control: the trace is not empty, so the assertion above is
    // measuring absence from something rather than absence of everything.
    assert!(!written.is_empty(), "the run did record something");
}

// ---------------------------------------------------------------------------
// 0.21.0 — `[[agent]]` and `[run] templates`
// ---------------------------------------------------------------------------

/// The 0.21.0 acceptance criterion: `[[agent]]` reaches a run from configuration,
/// projects onto the same roster the programmatic API builds, and accumulates across
/// scopes rather than being replaced by the narrower one.
///
/// Named for the release rather than `f9_`, which this file already uses for one of
/// 0.19.0's criteria.
#[test]
fn agent_tables_project_onto_the_same_roster_the_programmatic_api_builds() {
    use io_harness::{AgentDef, Agents};

    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        r#"
[[agent]]
name = "searcher"
role = "You find things and never edit them."
model = "cheap-model"
max_steps = 8
deny_write = true
deny_net = true

[[agent]]
name = "author"
model = "strong-model"
"#,
    );

    let from_file = Config::discover(project.path()).unwrap().agents();
    let programmatic = Agents::new()
        .with(
            AgentDef::new("searcher")
                .with_role("You find things and never edit them.")
                .with_model("cheap-model")
                .with_max_steps(8)
                .deny_write()
                .deny_net(),
        )
        .with(AgentDef::new("author").with_model("strong-model"));

    assert_eq!(
        from_file, programmatic,
        "a roster from a file and one built in Rust must be the same value"
    );
}

/// `[[agent]]` accumulates across scopes the way `policy.layers` does. A local file
/// that silently deleted the project's agents would be a roster nobody could rely on.
#[test]
fn agent_tables_accumulate_across_scopes_rather_than_being_replaced() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[[agent]]\nname = \"shared\"\nmodel = \"project-model\"\n",
    );
    write(
        project.path(),
        "io.local.toml",
        "[[agent]]\nname = \"mine\"\nmodel = \"local-model\"\n",
    );

    let agents = Config::discover(project.path()).unwrap().agents();
    assert_eq!(
        agents.names(),
        vec!["mine", "shared"],
        "both scopes' agents must survive the merge"
    );
    assert_eq!(
        agents.get("shared").unwrap().model.as_deref(),
        Some("project-model")
    );
}

/// A later scope redefining the same *name* replaces that one definition, because a
/// roster is keyed by name — accumulation is across scopes, not within a name.
#[test]
fn a_later_scope_redefining_one_agent_replaces_only_that_agent() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[[agent]]\nname = \"worker\"\nmodel = \"project-model\"\n\
         [[agent]]\nname = \"other\"\nmodel = \"untouched\"\n",
    );
    write(
        project.path(),
        "io.local.toml",
        "[[agent]]\nname = \"worker\"\nmodel = \"local-model\"\n",
    );

    let agents = Config::discover(project.path()).unwrap().agents();
    assert_eq!(
        agents.get("worker").unwrap().model.as_deref(),
        Some("local-model"),
        "the narrower scope wins for the name it redefines"
    );
    assert_eq!(
        agents.get("other").unwrap().model.as_deref(),
        Some("untouched"),
        "and leaves every other definition alone"
    );
}

/// An unknown key inside an `[[agent]]` table is an error naming the key and the file.
///
/// This matters more here than anywhere else in the file: the keys being misspelled are
/// the ones that narrow a boundary, and `deny_writes = true` silently ignored is a
/// child that can write. `[[mcp]]` cannot have this check — `#[serde(flatten)]` and
/// `deny_unknown_fields` cannot coexist — so it is worth pinning that `[[agent]]` does.
#[test]
fn an_unknown_key_inside_an_agent_table_is_rejected_naming_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        "[[agent]]\nname = \"searcher\"\ndeny_writes = true\n",
    );

    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(
        err.contains("deny_writes"),
        "the error must name the misspelled key, got: {err}"
    );
    assert!(err.contains("io.toml"), "and the file it is in, got: {err}");
}

/// A roster in a file reaches a run: `apply_to` carries it onto the contract, including
/// for a file that declares agents and nothing else.
#[test]
fn a_file_that_declares_only_agents_still_reaches_the_contract() {
    use io_harness::TaskContract;

    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        "[[agent]]\nname = \"searcher\"\ndeny_write = true\n",
    );

    let config = Config::discover(project.path()).unwrap();
    let contract = config.apply_to(TaskContract::workspace("do the thing", project.path()));

    assert_eq!(contract.agents.names(), vec!["searcher"]);
    assert!(contract.agents.get("searcher").unwrap().deny_write);
}

/// `[run] templates` reaches the typed accessor. Discovery stays the caller's, because
/// it is fallible and rendering happens before a run exists.
#[test]
fn the_run_section_carries_a_template_directory() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        "[run]\ntemplates = \"prompts\"\n",
    );

    let config = Config::discover(project.path()).unwrap();
    assert_eq!(
        config.templates(),
        Some(std::path::Path::new("prompts")),
        "the template directory must reach a typed field"
    );

    // And a file that does not mention it leaves it unset rather than guessing.
    let empty = tempfile::tempdir().unwrap();
    write(empty.path(), "io.toml", "[run]\nmax_steps = 3\n");
    assert!(Config::discover(empty.path())
        .unwrap()
        .templates()
        .is_none());
}

// ---------------------------------------------------------------------------
// 0.22.0 — `[web]`
// ---------------------------------------------------------------------------

/// The 0.22.0 acceptance criterion F7: a `[web]` table reaches a run from
/// configuration and lands on exactly the `WebAccess` the programmatic builder
/// produces.
///
/// Named for the release rather than `f7_`, which this file already uses for one of
/// 0.19.0's criteria.
///
/// Equality against the builder — rather than five field assertions — is the point:
/// it is what proves the file is a projection of the typed API and not a second way
/// of describing web access, and it fails if a later field is added to `WebAccess`
/// and the two paths fill it differently.
#[test]
fn a_web_table_projects_onto_the_same_web_access_the_programmatic_api_builds() {
    use io_harness::WebAccess;

    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        r#"
[web]
search = true
fetch = true
max_uses = 5
allowed_domains = ["docs.rs", "crates.io"]
blocked_domains = ["evil.test"]
"#,
    );

    let applied = Config::discover(project.path())
        .unwrap()
        .apply_to(contract(project.path()));

    let programmatic = WebAccess::search()
        .with_fetch()
        .max_uses(5)
        .allow("docs.rs")
        .allow("crates.io")
        .block("evil.test");
    assert_eq!(
        applied.web,
        Some(programmatic),
        "a declaration from a file and one built in Rust must be the same value"
    );

    // The negative control: a file with no `[web]` table leaves the contract with
    // no declaration at all, which is the 0.21.0 behaviour and the one that sends
    // no server tool to any vendor.
    let quiet = tempfile::tempdir().unwrap();
    write(quiet.path(), "io.toml", "[run]\nmax_steps = 3\n");
    assert_eq!(
        Config::discover(quiet.path())
            .unwrap()
            .apply_to(contract(quiet.path()))
            .web,
        None,
        "an absent [web] table is not an empty one"
    );
}

/// An unknown key inside `[web]` is an error naming the key and the file.
///
/// `WebAccess` carries its own `deny_unknown_fields`, so this is inherited rather
/// than added — but a misspelled `blocked_domain` silently ignored is a block-list
/// that does not exist, and the operator would have no way to see it, so the
/// inheritance is worth pinning here rather than assuming.
#[test]
fn an_unknown_key_inside_the_web_table_is_rejected_naming_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(
        project.path(),
        "io.toml",
        "[web]\nsearch = true\nblocked_domain = [\"evil.test\"]\n",
    );

    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(
        err.contains("blocked_domain"),
        "the error must name the misspelled key, got: {err}"
    );
    assert!(err.contains("io.toml"), "and the file it is in, got: {err}");
}

/// `[web]` obeys the same layering every other table does: a later scope wins one
/// key and leaves its siblings alone.
///
/// Asserted rather than assumed, because this is the direction that matters — an
/// individual switching search *off* over a project that turned it on. If the table
/// were replaced whole instead of merged, the local file would also silently drop
/// the project's domain lists, and the run would search a wider web than the
/// project's file described.
#[test]
fn a_local_scope_switching_search_off_overrides_a_project_scope_that_turned_it_on() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[web]\nsearch = true\nmax_uses = 5\nallowed_domains = [\"docs.rs\"]\n",
    );
    write(project.path(), "io.local.toml", "[web]\nsearch = false\n");

    let web = Config::discover(project.path())
        .unwrap()
        .apply_to(contract(project.path()))
        .web
        .expect("the table is still present, it is just switched off");

    assert!(!web.search, "the narrower scope is the last word");
    assert!(
        !web.enabled(),
        "and with fetch never turned on, nothing is declared to any vendor"
    );
    assert_eq!(
        web.max_uses,
        Some(5),
        "a sibling key the local scope never named survives the merge"
    );
    assert_eq!(
        web.allowed_domains,
        ["docs.rs"],
        "and so does the project's domain list"
    );

    // The negative control for the direction: without the local file, the project
    // scope's `search = true` is what reaches the run.
    std::fs::remove_file(project.path().join("io.local.toml")).unwrap();
    assert!(
        Config::discover(project.path())
            .unwrap()
            .apply_to(contract(project.path()))
            .web
            .unwrap()
            .search
    );
}

// ---------------------------------------------------------------------------
// 0.27.0 — F1 through F9
// ---------------------------------------------------------------------------

/// F1's negative control. `None` must mean "the file said nothing", never "the
/// crate picked one" — which matters more here than for any other accessor,
/// because a defaulted provider would be a vendor the operator never named.
#[test]
fn a_config_with_no_provider_table_has_no_spec() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(project.path(), "io.toml", "[run]\nmax_steps = 3\n");

    let config = Config::discover(project.path()).unwrap();
    assert!(config.provider_spec().is_none());
    assert!(config.fallback_specs().is_empty());
}

/// The chain is replaced by a later scope, not appended to.
///
/// `policy.layers` and `[[agent]]` accumulate because a later scope *adding* one is
/// what those types mean. A fallback chain is not that shape: appending the user's
/// two providers to the project's two would produce a four-link chain nobody wrote.
#[test]
fn a_later_scope_replaces_the_whole_provider_chain() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[[provider]]\nkind = \"openrouter\"\nmodel = \"team\"\n\
         [[provider]]\nkind = \"anthropic\"\nmodel = \"team-backup\"\n",
    );
    write(
        project.path(),
        "io.local.toml",
        "[[provider]]\nkind = \"openai\"\nmodel = \"mine\"\n",
    );

    let config = Config::discover(project.path()).unwrap();
    assert_eq!(
        config.provider_spec(),
        Some(&io_harness::ProviderSpec::OpenAi {
            model: "mine".into(),
            api_key: None,
        })
    );
    assert!(
        config.fallback_specs().is_empty(),
        "the project's chain is replaced, not appended to"
    );
}

/// Is `#[non_exhaustive]` written above `item` in `src/config.rs`?
///
/// A source read rather than a compile check. `#[non_exhaustive]` has no effect
/// inside the defining crate, so no runtime assertion can see it, and asserting a
/// compile failure would need `trybuild` — a dependency this release does not add.
/// `tests/public_api.rs` already reads `src/` for the same class of reason.
fn is_non_exhaustive(item: &str) -> bool {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config.rs"))
        .unwrap();
    let at = src
        .find(item)
        .unwrap_or_else(|| panic!("`{item}` is not in src/config.rs"));
    src[..at]
        .rsplit('\n')
        .take(6)
        .any(|line| line.trim() == "#[non_exhaustive]")
}

/// F2 — the attribute is worth a test because deleting it costs nothing today and
/// costs a major version later.
#[test]
fn provider_spec_is_non_exhaustive_because_a_later_release_adds_a_variant() {
    assert!(
        is_non_exhaustive("pub enum ProviderSpec"),
        "ProviderSpec must be #[non_exhaustive] from the first release it exists"
    );
    // The negative control for the helper: a type that is deliberately *not*
    // non-exhaustive must report absent, or a helper that always answers yes would
    // pass this file for ever.
    assert!(
        !is_non_exhaustive("pub enum Scope"),
        "the helper must be able to answer no"
    );
}

/// F3 — `[app]` is stored and never validated, and strictness is not switched off
/// to achieve it.
#[test]
fn the_app_table_takes_keys_this_crate_has_never_heard_of() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[app.cli]\nnot_a_key_this_crate_has = \"and never will\"\nwidth = 100\n\
         [app.studio]\nanything = [1, 2, 3]\n",
    );
    let config = Config::discover(project.path()).unwrap();

    #[derive(serde::Deserialize)]
    struct Cli {
        not_a_key_this_crate_has: String,
        width: u32,
    }
    let cli: Cli = config.app("cli").unwrap().expect("[app.cli] is carried");
    assert_eq!(cli.not_a_key_this_crate_has, "and never will");
    assert_eq!(cli.width, 100);

    // A sub-table the file does not carry is absent, not an error and not a
    // default-constructed value.
    assert!(config.app::<Cli>("nothing-wrote-this").unwrap().is_none());

    // The negative control, and the boundary of the hole: the *same* unknown key
    // in a section this crate does own is still refused, naming it.
    write(
        project.path(),
        "io.toml",
        "[run]\nnot_a_key_this_crate_has = \"and never will\"\n",
    );
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("not_a_key_this_crate_has"), "{err}");
}

/// F4 — through real files, since the scope is what decides and a scope only
/// exists on disk.
#[test]
fn a_command_substitution_is_refused_in_the_project_scope_and_runs_in_the_local_one() {
    #[cfg(windows)]
    let echo = "cmd /c echo s3cret";
    #[cfg(not(windows))]
    let echo = "printf s3cret";

    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let mcp = |value: &str| {
        format!("[[mcp]]\nid = \"gh\"\ntransport = \"stdio\"\ncommand = \"{value}\"\n")
    };

    write(project.path(), "io.toml", &mcp(&format!("${{cmd:{echo}}}")));
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("refused in the project scope"), "{err}");
    assert!(err.contains("io.toml"), "the error names the file: {err}");

    // The negative control: it is `cmd:` that the project scope refuses, not
    // substitution. Without this a rule that disarmed `${env:}` there — a much worse
    // feature — would pass the assertion above.
    std::env::set_var("IO_HARNESS_CONFIG_TEST_CMD", "from-the-environment");
    write(
        project.path(),
        "io.toml",
        &mcp("${env:IO_HARNESS_CONFIG_TEST_CMD}"),
    );
    let config = Config::discover(project.path()).unwrap();
    assert!(matches!(
        &config.mcp_servers()[0].transport,
        io_harness::mcp::McpTransport::Stdio { command, .. } if command == "from-the-environment"
    ));

    // And the local scope, which the operator wrote, may use it.
    write(
        project.path(),
        "io.local.toml",
        &mcp(&format!("${{cmd:{echo}}}")),
    );
    let config = Config::discover(project.path()).unwrap();
    assert!(matches!(
        &config.mcp_servers()[0].transport,
        io_harness::mcp::McpTransport::Stdio { command, .. } if command == "s3cret"
    ));
}

/// F5 — a project-scoped file may narrow the boundary and may never widen it.
///
/// Both controls, and the criterion is worthless without either: without the
/// narrowing half, a rule that refused the key outright would pass; without the
/// local half, a rule that refused the value in every scope would.
#[test]
fn a_project_scoped_file_may_narrow_the_boundary_and_may_never_widen_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let widening = [
        (
            "[policy.defaults]\nexec = \"allow\"\n",
            "policy.defaults.exec",
        ),
        (
            "[policy.defaults]\nnet = \"allow\"\n",
            "policy.defaults.net",
        ),
        ("[sandbox]\nallow_network = true\n", "sandbox.allow_network"),
        ("[sandbox]\nforce_floor = false\n", "sandbox.force_floor"),
    ];
    let narrowing = [
        "[policy.defaults]\nexec = \"deny\"\n",
        "[policy.defaults]\nnet = \"deny\"\n",
        "[sandbox]\nallow_network = false\n",
        "[sandbox]\nforce_floor = true\n",
    ];

    for (text, key) in widening {
        write(project.path(), "io.toml", text);
        let err = Config::discover(project.path()).unwrap_err().to_string();
        assert!(err.contains(key), "the error names the key: {err}");
        assert!(err.contains("widens"), "{err}");
        assert!(
            err.contains("io.local.toml"),
            "and names where to write it instead: {err}"
        );
    }

    // Control one: the same four keys, narrowing, in the same file.
    for text in narrowing {
        write(project.path(), "io.toml", text);
        Config::discover(project.path())
            .unwrap_or_else(|e| panic!("a project file may narrow: {text:?}: {e}"));
    }

    // Control two: the widening values, in the scope the operator wrote.
    std::fs::remove_file(project.path().join("io.toml")).unwrap();
    for (text, _) in widening {
        write(project.path(), "io.local.toml", text);
        Config::discover(project.path())
            .unwrap_or_else(|e| panic!("the local scope may widen: {text:?}: {e}"));
    }
}

/// A widening key cannot hide inside a profile in a project file either. The
/// profile is applied later, so a check that only looked at the base would let it
/// reach exactly the same place by a different path.
#[test]
fn the_project_scope_rule_reaches_inside_a_profile() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[profile.loose]\nsandbox = { allow_network = true }\n",
    );
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("widens"), "{err}");

    // The negative control: the same profile narrowing loads.
    write(
        project.path(),
        "io.toml",
        "[profile.tight]\nsandbox = { allow_network = false }\n",
    );
    Config::discover(project.path()).unwrap();
}

/// F6 — a profile overlays the base and touches nothing else.
#[test]
fn a_profile_overlays_the_base_and_leaves_every_key_it_did_not_name() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[run]\nmax_steps = 30\nmax_retries = 4\n\
         [profile.cheap]\nrun = { max_steps = 5 }\n",
    );
    let config = Config::discover(project.path()).unwrap();
    let cheap = config.with_profile("cheap").unwrap();

    let applied = cheap.apply_to(contract(project.path()));
    assert_eq!(applied.max_steps, 5);
    assert_eq!(
        applied.max_retries, 4,
        "a key the profile never named is untouched"
    );

    // Control one: selecting a profile does not mutate what it came from.
    assert_eq!(
        config.apply_to(contract(project.path())).max_steps,
        30,
        "the configuration the profile was taken from is unchanged"
    );

    // A name the file does not carry is an error naming it, because a `--profile`
    // argument that silently does nothing is the same failure class as a typo in a
    // key: an operator believing in a setting that is not there.
    let err = config.with_profile("careful").unwrap_err().to_string();
    assert!(err.contains("careful"), "{err}");

    // Profiles do not compose: the overlay is applied once and the result carries
    // no `[profile]` section of its own.
    assert!(cheap.with_profile("cheap").is_err());
}

/// Control two for F6: a typo inside a profile that is *never selected* is still
/// rejected at load, because a profile body deserializes as the file format rather
/// than as an opaque table.
#[test]
fn an_unknown_key_inside_a_profile_that_is_never_selected_is_rejected_naming_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(
        project.path(),
        "io.toml",
        "[run]\nmax_steps = 30\n[profile.cheap]\nrun = { max_stepz = 5 }\n",
    );
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("max_stepz"), "names the key: {err}");

    // And a profile may not contain profiles: an overlay is not a tree.
    write(
        project.path(),
        "io.toml",
        "[profile.a.profile.b]\nrun = { max_steps = 1 }\n",
    );
    let err = Config::discover(project.path()).unwrap_err().to_string();
    assert!(err.contains("may not contain profiles"), "{err}");
}

/// F7 — a repository's own instructions reach the model without a new
/// `TaskContract` field.
#[test]
fn discovered_instructions_reach_the_contract_as_constraints() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    write(project.path(), "AGENTS.md", "Never touch `generated/`.");
    write(project.path(), "io.toml", "[instructions]\n");

    let applied = Config::discover(project.path())
        .unwrap()
        .apply_to(contract(project.path()));
    assert_eq!(applied.constraints.len(), 1);
    assert!(applied.constraints[0].contains("Never touch `generated/`."));
    assert!(
        applied.constraints[0].contains("AGENTS.md"),
        "each constraint names the file it came from: {:?}",
        applied.constraints[0]
    );

    // Control one: with no `[instructions]` section, the very same `AGENTS.md` is
    // not read. Nothing is loaded implicitly, including this.
    write(project.path(), "io.toml", "[run]\nmax_steps = 3\n");
    let config = Config::discover(project.path()).unwrap();
    assert!(config.instructions().is_empty());
    assert!(config
        .apply_to(contract(project.path()))
        .constraints
        .is_empty());

    // Control two: a named file that is absent is skipped rather than failing the
    // load. This is discovery, not substitution — the one place this module's
    // "resolve or fail" rule deliberately does not apply.
    write(
        project.path(),
        "io.toml",
        "[instructions]\nfiles = [\"NOTHING-WROTE-THIS.md\", \"AGENTS.md\"]\n",
    );
    let config = Config::discover(project.path()).unwrap();
    assert_eq!(config.instructions().len(), 1);
}

/// F8 — `IO_CONFIG` names the user-scope file directly, and the scopes stay four.
#[test]
fn io_config_names_the_user_scope_file_and_does_not_bypass_the_merge() {
    let user_dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    // `IO_CONFIG_HOME` points here and would otherwise win.
    write(user_dir.path(), "io.toml", "[run]\nmax_steps = 1\n");
    let named = elsewhere.path().join("named-outright.toml");
    std::fs::write(&named, "[run]\nmax_steps = 7\nmax_retries = 9\n").unwrap();
    std::env::set_var("IO_CONFIG", &named);

    let config = Config::discover(project.path()).unwrap();
    assert_eq!(config.sources(), [(Scope::User, named.clone())]);
    assert_eq!(config.apply_to(contract(project.path())).max_steps, 7);

    // The negative control: it names a *scope*, it does not bypass the merge. A
    // project file still wins the keys it names, which is what keeps the scopes at
    // four and `Scope` free of a new variant.
    write(project.path(), "io.toml", "[run]\nmax_steps = 2\n");
    let applied = Config::discover(project.path())
        .unwrap()
        .apply_to(contract(project.path()));
    assert_eq!(applied.max_steps, 2, "the project scope still wins");
    assert_eq!(applied.max_retries, 9, "over the file IO_CONFIG named");

    std::env::remove_var("IO_CONFIG");
}

/// F9 — the strictness tests for the two new tables that have keys of their own.
/// `[app]` is excepted by design and covered by its own test above; `[profile]` by
/// the unselected-profile test above.
#[test]
fn an_unknown_key_inside_a_provider_or_instructions_table_is_rejected_naming_it() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    for (text, key) in [
        (
            "[[provider]]\nkind = \"openrouter\"\nmodel = \"x\"\nmodle = \"y\"\n",
            "modle",
        ),
        ("[instructions]\nfilez = [\"AGENTS.md\"]\n", "filez"),
        (
            "[[provider]]\nkind = \"no-such-vendor\"\nmodel = \"x\"\n",
            "no-such-vendor",
        ),
    ] {
        write(project.path(), "io.toml", text);
        let err = Config::discover(project.path()).unwrap_err().to_string();
        assert!(err.contains(key), "must name `{key}`, got: {err}");
    }

    // The negative control: the correctly spelled versions load.
    write(
        project.path(),
        "io.toml",
        "[[provider]]\nkind = \"openrouter\"\nmodel = \"x\"\n[instructions]\nfiles = []\n",
    );
    Config::discover(project.path()).unwrap();
}
