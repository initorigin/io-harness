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
    Verification,
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
        "[sandbox]\nallow_network = true\n\
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
    assert!(with.allow_network, "and so is the other section");
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
"#;

#[test]
fn f7_every_key_reaches_a_typed_field() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());
    write(project.path(), "io.toml", EVERY_KEY);

    let config = Config::discover(project.path()).unwrap();

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

    // Every one of those is different from the default it would otherwise hold.
    let plain = contract(project.path());
    assert_ne!(applied.max_steps, plain.max_steps);
    assert_ne!(applied.max_retries, plain.max_retries);
    assert_ne!(applied.retry, plain.retry);
    assert_ne!(applied.stall, plain.stall);
    assert_ne!(applied.context, plain.context);
    assert_ne!(applied.exec_timeout, plain.exec_timeout);
    assert_ne!(applied.commit_identity, plain.commit_identity);
}

#[test]
fn f7_a_key_removed_from_that_file_leaves_exactly_that_field_at_its_default() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    // The negative control for the fixture above: drop one key, and that one
    // field falls back while its neighbours do not.
    let without = EVERY_KEY.replace("max_steps = 66\n", "");
    write(project.path(), "io.toml", &without);

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
    write(project.path(), "io.toml", EVERY_KEY);
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
    TaskContract::workspace("edit the workspace", root, Verification::None)
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
