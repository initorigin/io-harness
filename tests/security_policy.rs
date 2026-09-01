//! The policy layer's own builtin denies, and the matcher underneath them.
//!
//! Every test here is named for the audit finding it closes and fails on the
//! behaviour that shipped before it. The threat model is one hostile repository
//! and one agent reading instructions out of it, so the interesting question is
//! never "does the rule exist" but "does it hold against the spelling the
//! attacker would have used".
//!
//! Each new deny is paired with the case it most resembles and must *not* catch.
//! A deny that refuses work which was always legitimate is a worse outcome than
//! the finding it closed: `.gitignore` is not `.git/`, `config.toml` is not
//! `io.toml`, `notevil.example` is not `evil.example`, and the three git
//! built-ins still write the repository they are for.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, Act, ApproveAll, Effect, Policy, Provider, Store, TaskContract};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
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

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

/// A checked-out repository: a source tree, and a `.git` that already exists so
/// a write into it would land rather than fail on a missing parent. The point of
/// each test below is that it does not land.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn a() {}\n").unwrap();
    dir
}

/// Reads and writes allowed outright, so the only thing in the stack that can
/// refuse a write is a builtin deny. Without the blanket allow these tests would
/// pass against a tier default and prove nothing about the rule.
fn wide_open() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
}

/// Content for a write that must never land. Deliberately inert: what makes the
/// repo-local git config dangerous is not any particular key but that git reads
/// the file back on every invocation and will run a program named in it, so the
/// boundary has to be the file rather than its contents.
const CONFIG_BODY: &str = "[core]\n\tquotepath = false\n";

// ---------------------------------------------------------------------------
// H1 — a write into `.git` is a write to what the next git built-in executes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn h1_a_write_into_dot_git_is_refused_by_the_builtin_layer() {
    let dir = repo();
    let store = Store::memory().unwrap();
    let script = MockScript::new(vec![vec![write(".git/config", CONFIG_BODY)]]);

    let result = run_with(
        &TaskContract::workspace("edit the repository", dir.path()).with_max_steps(1),
        &script,
        &store,
        &wide_open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !dir.path().join(".git/config").exists(),
        "the repo-local git config is not the agent's to write"
    );
    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal" && e.act == "write")
        .expect("the write is refused, not silently dropped");
    assert_eq!(refusal.target, ".git/config");
    assert_eq!(refusal.rule.as_deref(), Some(".git/*"));
    assert_eq!(refusal.layer.as_deref(), Some("builtin-config"));
}

/// The other two write tools. `edit_file` and `patch_file` are the same
/// `Act::Write` on the same path as `write_file` — one gate, three arms — and a
/// deny that only covered the tool the exploit happened to name would be a
/// boundary with two doors left open.
#[tokio::test]
async fn h1_edit_file_and_patch_file_are_refused_on_the_same_path() {
    let dir = repo();
    std::fs::write(dir.path().join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    let store = Store::memory().unwrap();
    let script = MockScript::new(vec![
        vec![call(
            "edit_file",
            json!({ "path": ".git/hooks/pre-commit", "search": "#!/bin/sh", "replace": "#!/bin/zsh" }),
        )],
        vec![call(
            "patch_file",
            json!({ "path": ".git/hooks/pre-commit", "patch": "@@ -1 +1 @@\n-#!/bin/sh\n+#!/bin/zsh\n" }),
        )],
    ]);

    let result = run_with(
        &TaskContract::workspace("edit the repository", dir.path()).with_max_steps(2),
        &script,
        &store,
        &wide_open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join(".git/hooks/pre-commit")).unwrap(),
        "#!/bin/sh\n",
        "neither partial-write tool got in either"
    );
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal" && e.act == "write")
        .collect();
    assert_eq!(refusals.len(), 2, "one refusal per tool: {refusals:?}");
    for refusal in refusals {
        assert_eq!(refusal.rule.as_deref(), Some(".git/*"));
        assert_eq!(refusal.layer.as_deref(), Some("builtin-config"));
    }
}

/// The companion, and the reason the rule is `.git/*` rather than `.git*`.
///
/// `git_add`, `git_commit` and `git_branch` each put an `Act::Write` on the
/// string `.git` before they run, because each of them writes the repository.
/// Every legitimate reason an agent has to change `.git` goes through one of
/// those three, so the deny has to stop short of the directory itself — and a
/// pattern one character wider would have taken all three away.
#[test]
fn h1_the_git_built_ins_still_write_the_repository_they_are_for() {
    let policy = wide_open();
    assert_eq!(
        policy.check(Act::Write, ".git").effect,
        Effect::Allow,
        "git_add, git_commit and git_branch check exactly this target"
    );
    assert_eq!(policy.check(Act::Write, ".git/config").effect, Effect::Deny);
}

/// The near misses. Each of these is a file an agent edits on an ordinary day
/// and none of them is inside `.git`.
#[test]
fn h1_a_file_whose_name_merely_starts_with_git_is_not_the_git_directory() {
    let policy = wide_open();
    for path in [
        ".gitignore",
        ".gitmodules",
        ".gitattributes",
        "src/git/mod.rs",
        "docs/.github/workflows/ci.yml",
        "gitconfig",
    ] {
        assert_eq!(
            policy.check(Act::Write, path).effect,
            Effect::Allow,
            "{path} is not inside .git and must stay writable"
        );
    }
}

/// A submodule or a nested checkout puts `.git` below the root, which the
/// leading-`.git/` form does not reach.
#[test]
fn h1_a_nested_dot_git_is_denied_as_well() {
    let policy = wide_open();
    for path in ["vendor/lib/.git/config", "sub/.git/hooks/pre-commit"] {
        assert_eq!(
            policy.check(Act::Write, path).effect,
            Effect::Deny,
            "{path}"
        );
    }
}

// ---------------------------------------------------------------------------
// H2 — a run that writes its own config writes its own next command line
// ---------------------------------------------------------------------------

/// `io.local.toml` is read back by `Config::discover`, which is where a run's
/// hooks, MCP servers and toolchain argv come from. Writing it is one ordinary
/// `Act::Write` on an unremarkable path that turns into an argv nothing in this
/// crate gates.
#[tokio::test]
async fn h2_a_write_of_io_local_toml_under_a_run_root_is_denied() {
    let dir = repo();
    let store = Store::memory().unwrap();
    let script = MockScript::new(vec![vec![write(
        "io.local.toml",
        "[[hook]]\non = [\"started\"]\nrun = [\"true\"]\n",
    )]]);

    let result = run_with(
        &TaskContract::workspace("configure the run", dir.path()).with_max_steps(1),
        &script,
        &store,
        &wide_open(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !dir.path().join("io.local.toml").exists(),
        "an agent's own config is not agent-writable"
    );
    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal" && e.act == "write")
        .expect("the write is refused");
    assert_eq!(refusal.target, "io.local.toml");
    assert_eq!(refusal.rule.as_deref(), Some("io.local.toml"));
    assert_eq!(refusal.layer.as_deref(), Some("builtin-config"));
}

/// Both scopes, at every depth — a bare filename is a recursive deny — and the
/// companion that keeps every other TOML in the tree writable.
#[test]
fn h2_the_config_files_are_denied_and_no_other_toml_is() {
    let policy = wide_open();
    for path in [
        "io.toml",
        "io.local.toml",
        "nested/io.toml",
        "nested/io.local.toml",
    ] {
        assert_eq!(
            policy.check(Act::Write, path).effect,
            Effect::Deny,
            "{path}"
        );
    }
    for path in [
        "config.toml",
        "Cargo.toml",
        ".cargo/config.toml",
        "myio.toml",
        "io.example.toml",
    ] {
        assert_eq!(
            policy.check(Act::Write, path).effect,
            Effect::Allow,
            "{path} is not the harness's own config"
        );
    }
}

/// Reads are untouched. An agent that has to know what a config or a repository
/// says can still look; only writing is refused.
#[test]
fn h2_the_config_and_repository_denies_are_writes_only() {
    let policy = wide_open();
    for path in ["io.toml", "io.local.toml", ".git/config"] {
        assert_eq!(
            policy.check(Act::Read, path).effect,
            Effect::Allow,
            "{path} stays readable"
        );
    }
}

// ---------------------------------------------------------------------------
// M2 — a host is not case-sensitive and its root dot is optional
// ---------------------------------------------------------------------------

#[test]
fn m2_a_net_deny_catches_every_spelling_of_the_host_it_names() {
    let policy = Policy::permissive().layer("ops").deny_net("evil.example");
    for target in [
        "evil.example:443",
        "EVIL.example:443",
        "Evil.Example:443",
        "evil.example.",
        "EVIL.EXAMPLE.:443",
        "evil.example",
    ] {
        assert_eq!(
            policy.check(Act::Net, target).effect,
            Effect::Deny,
            "{target} is the host the rule names"
        );
    }
}

/// The pattern side is folded too, so a rule written with a capital or a root
/// dot means the host it looks like it means.
#[test]
fn m2_a_net_rule_written_in_any_case_names_the_same_host() {
    let policy = Policy::permissive().layer("ops").deny_net("EVIL.Example.");
    assert_eq!(
        policy.check(Act::Net, "evil.example:443").effect,
        Effect::Deny
    );
}

/// The dot comes off the host, not off the end of the string, so a rule that
/// names a port still catches the rooted spelling of that host — and still means
/// that port and no other.
#[test]
fn m2_a_ported_net_rule_catches_the_rooted_host_and_no_other_port() {
    let policy = Policy::permissive()
        .layer("ops")
        .deny_net("evil.example:443");
    assert_eq!(
        policy.check(Act::Net, "EVIL.example.:443").effect,
        Effect::Deny
    );
    assert_eq!(
        policy.check(Act::Net, "evil.example:8080").effect,
        Effect::Allow,
        "a rule naming a port is honoured as written"
    );
}

/// The companion, and the one that fails if the fold turned into a substring
/// match: a host that merely contains or extends the denied one is a different
/// host and stays reachable.
#[test]
fn m2_a_net_deny_does_not_catch_a_host_that_merely_resembles_it() {
    let policy = Policy::permissive().layer("ops").deny_net("evil.example");
    for target in [
        "notevil.example:443",
        "evil.example.org:443",
        "evilexample:443",
        "evil.example.com:443",
        "example:443",
    ] {
        assert_eq!(
            policy.check(Act::Net, target).effect,
            Effect::Allow,
            "{target} is not the host the rule names"
        );
    }
}

/// The one direction the host fold adds reach: an allow rule now covers the same
/// name spelled with different capitals. DNS resolves both to one server, so the
/// rule grants the host it already named rather than a second one — and the
/// alternative is a boundary that refuses a host an operator plainly allowed.
#[test]
fn m2_a_net_allow_does_not_cover_a_spelling_it_did_not_name() {
    let policy = Policy::default().layer("ops").allow_net("api.example.com");
    // The host fold is a deny's alone. Folding an allow would grant
    // `API.Example.com`, which 0.73.0 refused, and 0.74.0 narrows only — so this
    // falls to the net default rather than being granted. The cost is real and
    // deliberate: an operator who writes one spelling and dials another meets the
    // default instead of their own rule. The deny direction, where being wrong
    // means letting something out, is covered by the three tests above.
    assert_ne!(
        policy.check(Act::Net, "API.Example.com:443").effect,
        Effect::Allow,
        "a case-folded allow would be a widening, and this release adds none"
    );
    assert_eq!(
        policy.check(Act::Net, "api.example.com:443").effect,
        Effect::Allow,
        "the spelling the rule actually names still resolves"
    );
    assert_eq!(
        policy.check(Act::Net, "other.example.com:443").effect,
        Effect::Deny,
        "and nothing else"
    );
}

/// Exec is a filesystem name, not a protocol name, so the fold is a deny's
/// alone. Nothing in this crate can tell whether the volume a given argv
/// resolves on folds case; the reading that fails closed is to let a deny catch
/// both spellings and to let an allow grant exactly what it names.
#[test]
fn m2_an_exec_deny_catches_a_case_variant_and_an_exec_allow_does_not() {
    let denied = Policy::permissive().layer("ops").deny_exec("rm");
    for target in ["rm", "RM", "Rm"] {
        assert_eq!(
            denied.check(Act::Exec, target).effect,
            Effect::Deny,
            "{target}"
        );
    }

    let allowed = Policy::default().layer("ops").allow_exec("rustc");
    assert_eq!(allowed.check(Act::Exec, "rustc").effect, Effect::Allow);
    assert_eq!(
        allowed.check(Act::Exec, "RUSTC").effect,
        Effect::Ask,
        "an allow grants the one spelling it names; the tier default decides the rest"
    );
}

// ---------------------------------------------------------------------------
// L7 — the basename retry widens a pattern, so it belongs to denies
// ---------------------------------------------------------------------------

/// The shape the finding names: a policy allowing the toolchain's `cargo` also
/// permitted a `cargo` the agent had just built into its own target directory.
#[test]
fn l7_an_exec_allow_does_not_reach_a_binary_the_agent_built() {
    let policy = Policy::default().layer("ops").allow_exec("cargo");
    assert_eq!(
        policy.check(Act::Exec, "cargo").effect,
        Effect::Allow,
        "the binary the rule names still runs"
    );
    for target in ["./target/debug/cargo", "target/debug/cargo", "/tmp/x/cargo"] {
        assert_eq!(
            policy.check(Act::Exec, target).effect,
            Effect::Ask,
            "{target} is a different binary and the tier default decides it"
        );
    }
}

/// The half that is load-bearing and stays: a deny by bare filename is still
/// recursive, for names as well as for paths. Removing the retry outright would
/// have taken `.env` down with it.
#[test]
fn l7_a_deny_by_bare_name_is_still_recursive() {
    let paths = Policy::permissive().layer("ops").deny_read(".env");
    assert_eq!(paths.check(Act::Read, "config/.env").effect, Effect::Deny);

    let names = Policy::permissive().layer("ops").deny_exec("cargo");
    assert_eq!(
        names.check(Act::Exec, "./target/debug/cargo").effect,
        Effect::Deny
    );
}

// ---------------------------------------------------------------------------
// L9 — a resolved argv on Windows carries backslashes
// ---------------------------------------------------------------------------

#[test]
fn l9_an_exec_deny_reads_a_backslash_as_a_separator_in_the_target() {
    let policy = Policy::permissive()
        .layer("ops")
        .deny_exec("kubectl.exe")
        .deny_exec("rm");
    for target in [
        r"C:\Program Files\kubernetes\kubectl.exe",
        r"tools\kubectl.exe",
        r"C:/Program Files/kubernetes/kubectl.exe",
    ] {
        assert_eq!(
            policy.check(Act::Exec, target).effect,
            Effect::Deny,
            "{target}"
        );
    }
    // Both separators, and the case fold, in the one target a hostile argv would
    // actually carry.
    assert_eq!(
        policy.check(Act::Exec, r"C:\Windows\System32\RM").effect,
        Effect::Deny
    );
}

/// The pattern side is left exactly as written: a `\` in a *rule* is the literal
/// character a rule writer meant, and folding it would make one rule cover two
/// different names.
#[test]
fn l9_the_pattern_side_keeps_its_backslash_literal() {
    let policy = Policy::permissive()
        .layer("ops")
        .deny_exec(r"tools\build.exe");
    assert_eq!(
        policy.check(Act::Exec, r"tools\build.exe").effect,
        Effect::Deny
    );
    assert_eq!(
        policy.check(Act::Exec, "tools/build.exe").effect,
        Effect::Allow,
        "a forward-slash path is a different name and the rule does not reach it"
    );
}
