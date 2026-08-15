//! 0.56.0: the scope above the workspace.
//!
//! Durable memory was keyed by a workspace's canonical path and nothing else, so
//! a fact true of every repository an operator owns had to be learned again per
//! workspace. A second scope sits above it: written deliberately by a run or by
//! an operator, recalled by every run, and narrower than it sounds — the
//! workspace's own note wins a key collision, because the specific place always
//! knows better than the general one.

use std::path::Path;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, ContextBudget, MemoryKind, Policy, Provider, Store, TaskContract,
    GLOBAL_MEMORY_WORKSPACE,
};
use serde_json::json;

struct Script(Vec<Vec<ToolCall>>, std::sync::atomic::AtomicUsize);

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.0.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
    fn name(&self) -> &str {
        "script"
    }
}

/// Every prompt the model was sent, so an assertion can be made about what the
/// block actually said rather than about what the store holds.
struct Seen(std::sync::Mutex<Vec<String>>);

impl Provider for Seen {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.0.lock().unwrap().push(req.user.clone());
        Ok(CompletionResponse::default())
    }
    fn name(&self) -> &str {
        "seen"
    }
}

fn ws_key(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("work over this repository", root)
        .with_max_steps(2)
        .with_context_budget(ContextBudget {
            max_tokens: 2_000,
            share: 0.5,
        })
}

async fn run_script(root: &Path, store: &Store, script: Vec<Vec<ToolCall>>) -> i64 {
    run_with(
        &contract(root),
        &Script(script, std::sync::atomic::AtomicUsize::new(0)),
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the run itself must not error")
    .run_id
}

/// The prompt of the first step of a run that calls nothing.
async fn first_prompt(root: &Path, store: &Store) -> String {
    let seen = Seen(std::sync::Mutex::new(Vec::new()));
    run_with(
        &contract(root),
        &seen,
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the run itself must not error");
    let prompts = seen.0.lock().unwrap();
    prompts.first().cloned().expect("one step happened")
}

fn remember_global(key: &str, value: &str) -> ToolCall {
    ToolCall {
        name: "remember".into(),
        arguments: json!({ "key": key, "value": value, "scope": "global" }),
    }
}

/// F12 — a note one workspace wrote globally is recalled by a workspace that
/// never wrote it, in both directions.
#[tokio::test]
async fn a_global_note_is_recalled_from_a_workspace_that_never_wrote_it() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    run_script(
        a.path(),
        &store,
        vec![vec![remember_global("package-manager", "pnpm, never npm")]],
    )
    .await;
    assert!(
        store.memory_list(&ws_key(a.path())).unwrap().is_empty(),
        "a global note is not also the writing workspace's own"
    );

    // B has no entries of its own and has never seen A.
    let prompt = first_prompt(b.path(), &store).await;
    assert!(
        prompt.contains("pnpm, never npm"),
        "the note reached a workspace that never wrote it:\n{prompt}"
    );

    // And the other direction, so the claim is about the scope rather than
    // about the order two workspaces happened to run in.
    run_script(
        b.path(),
        &store,
        vec![vec![remember_global(
            "editor",
            "the repository's own formatter",
        )]],
    )
    .await;
    let prompt = first_prompt(a.path(), &store).await;
    assert!(
        prompt.contains("the repository's own formatter"),
        "{prompt}"
    );
}

/// F13 — the workspace wins a key collision, and the global entry is not
/// rendered beside it. Asserted on the global value being ABSENT: rendering both
/// would satisfy "the workspace value is present" and still be the failure.
#[tokio::test]
async fn the_workspace_wins_a_key_collision_and_the_global_note_is_not_carried() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run = store.start_run("seed", "/seed").unwrap();

    store
        .memory_put(
            GLOBAL_MEMORY_WORKSPACE,
            "test-command",
            "make check",
            run,
            1,
        )
        .unwrap();
    store
        .memory_put(
            &ws_key(dir.path()),
            "test-command",
            "cargo nextest run",
            run,
            1,
        )
        .unwrap();

    let prompt = first_prompt(dir.path(), &store).await;
    assert!(prompt.contains("cargo nextest run"), "{prompt}");
    assert!(
        !prompt.contains("make check"),
        "the general note is not carried at all when the specific place has an answer:\n{prompt}"
    );
}

/// F14 — the block says which notes are global. A note kept for every workspace
/// presented under "notes you recorded over this workspace" would be the block
/// telling the model something untrue about where the fact came from.
#[tokio::test]
async fn the_block_renders_the_two_scopes_under_their_own_headings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run = store.start_run("seed", "/seed").unwrap();
    store
        .memory_put(
            &ws_key(dir.path()),
            "layout",
            "the parser lives in src/syn",
            run,
            1,
        )
        .unwrap();
    store
        .memory_put(GLOBAL_MEMORY_WORKSPACE, "package-manager", "pnpm", run, 1)
        .unwrap();

    let prompt = first_prompt(dir.path(), &store).await;
    assert!(prompt.contains("[memory]"), "{prompt}");
    assert!(prompt.contains("[memory: every workspace]"), "{prompt}");
    // The global note is under the second heading, not the first.
    let first = prompt.find("[memory]").unwrap();
    let second = prompt.find("[memory: every workspace]").unwrap();
    let global_at = prompt.find("pnpm").unwrap();
    let own_at = prompt.find("the parser lives in src/syn").unwrap();
    assert!(first < own_at && own_at < second, "{prompt}");
    assert!(second < global_at, "{prompt}");
}

/// F15 — the reserved key cannot be a real workspace. Asserted on a real
/// directory actually named `<global>` where the platform allows one, so the
/// claim is about canonicalisation rather than about a string written twice.
#[test]
fn a_directory_named_like_the_reserved_key_is_still_its_own_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let awkward = dir.path().join(GLOBAL_MEMORY_WORKSPACE);
    // Windows forbids `<` and `>` in a path at all, which is itself the property
    // under test — there, no directory can carry this name.
    let Ok(()) = std::fs::create_dir(&awkward) else {
        // Only Windows may refuse the name, and there refusing it IS the
        // property under test: no directory can carry it, so nothing can
        // collide with the reserved key. `cfg!` rather than a runtime assert,
        // which clippy reads as a constant assertion because it is one.
        #[cfg(not(windows))]
        panic!("only Windows may refuse `{GLOBAL_MEMORY_WORKSPACE}` as a directory name");
        #[cfg(windows)]
        return;
    };
    let key = ws_key(&awkward);
    assert_ne!(key, GLOBAL_MEMORY_WORKSPACE);
    assert!(
        Path::new(&key).is_absolute(),
        "a canonical path is absolute, and the reserved key is not one: {key}"
    );
}

/// F16 — each scope holds its own caps, in both directions. A single shared
/// counter passes the first direction alone.
#[test]
fn each_scope_is_bounded_on_its_own() {
    let store = Store::memory().unwrap();
    let run = store.start_run("seed", "/ws").unwrap();
    let limits = io_harness::MemoryLimits {
        max_entries: 3,
        ..Default::default()
    };

    for i in 0..3 {
        store
            .memory_write_with(
                "/ws",
                &format!("w{i}"),
                "v",
                run,
                1,
                MemoryKind::Fact,
                limits,
            )
            .unwrap();
    }
    // Filling the global bucket past its cap evicts global entries and touches
    // nothing of the workspace's.
    for i in 0..5 {
        store
            .memory_write_with(
                GLOBAL_MEMORY_WORKSPACE,
                &format!("g{i}"),
                "v",
                run,
                1,
                MemoryKind::Fact,
                limits,
            )
            .unwrap();
    }
    assert_eq!(store.memory_list(GLOBAL_MEMORY_WORKSPACE).unwrap().len(), 3);
    assert_eq!(
        store.memory_list("/ws").unwrap().len(),
        3,
        "the workspace's entries are not candidates for the global bucket's cap"
    );

    // And the reverse.
    for i in 3..8 {
        store
            .memory_write_with(
                "/ws",
                &format!("w{i}"),
                "v",
                run,
                1,
                MemoryKind::Fact,
                limits,
            )
            .unwrap();
    }
    assert_eq!(store.memory_list("/ws").unwrap().len(), 3);
    assert_eq!(store.memory_list(GLOBAL_MEMORY_WORKSPACE).unwrap().len(), 3);
}

/// A run may withdraw a global note, and an unknown scope is refused by name
/// rather than quietly treated as the workspace.
#[tokio::test]
async fn a_run_withdraws_a_global_note_and_an_unknown_scope_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let run = store.start_run("seed", "/seed").unwrap();
    store
        .memory_put(GLOBAL_MEMORY_WORKSPACE, "package-manager", "pnpm", run, 1)
        .unwrap();

    run_script(
        dir.path(),
        &store,
        vec![vec![ToolCall {
            name: "forget".into(),
            arguments: json!({ "key": "package-manager", "scope": "global" }),
        }]],
    )
    .await;
    assert!(store
        .memory_list(GLOBAL_MEMORY_WORKSPACE)
        .unwrap()
        .is_empty());

    // An unrecognised scope writes nothing anywhere. A model that meant "every
    // workspace" and silently got "this one" would go on believing the fact is
    // known everywhere.
    run_script(
        dir.path(),
        &store,
        vec![vec![ToolCall {
            name: "remember".into(),
            arguments: json!({ "key": "k", "value": "v", "scope": "everywhere" }),
        }]],
    )
    .await;
    assert!(store.memory_list(&ws_key(dir.path())).unwrap().is_empty());
    assert!(store
        .memory_list(GLOBAL_MEMORY_WORKSPACE)
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// 0.57.0 — a note that restates one already held is reported at the write
// ---------------------------------------------------------------------------

/// Plays a script and keeps every prompt, so an assertion can be made about what
/// the `remember` arm told the model rather than about what the store holds.
///
/// The report lands in the run's observation log, so it is the turn *after* the
/// write whose prompt carries it — which is also exactly how the model sees it.
struct Play(Vec<Vec<ToolCall>>, std::sync::Mutex<Vec<String>>);

impl Provider for Play {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let mut seen = self.1.lock().unwrap();
        let i = seen.len();
        seen.push(req.user.clone());
        Ok(CompletionResponse {
            tool_calls: self.0.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
    fn name(&self) -> &str {
        "play"
    }
}

fn remember(key: &str, value: &str) -> ToolCall {
    ToolCall {
        name: "remember".into(),
        arguments: json!({ "key": key, "value": value }),
    }
}

fn remember_scoped(key: &str, value: &str, scope: &str) -> ToolCall {
    ToolCall {
        name: "remember".into(),
        arguments: json!({ "key": key, "value": value, "scope": scope }),
    }
}

/// Every prompt of a run that plays `calls`, one per turn.
async fn played(root: &Path, store: &Store, calls: Vec<ToolCall>) -> Vec<String> {
    let steps = calls.len() as u32 + 1;
    let play = Play(
        calls.into_iter().map(|c| vec![c]).collect(),
        std::sync::Mutex::new(Vec::new()),
    );
    run_with(
        &contract(root).with_max_steps(steps),
        &play,
        store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("the run itself must not error");
    let out = play.1.lock().unwrap().clone();
    out
}

const TEST_CMD: &str = "the test command is cargo test --all-features";
const TEST_CMD_AGAIN: &str = "the test command here is cargo test --all-features";

/// F9 — a near-duplicate write names the key already held and quotes what it
/// holds, in the tool result of the same call.
#[tokio::test]
async fn a_note_that_restates_one_already_held_names_the_key_that_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    let seen = played(
        dir.path(),
        &store,
        vec![
            remember("build-command", TEST_CMD),
            remember("how-to-test", TEST_CMD_AGAIN),
        ],
    )
    .await;
    let after = seen.last().expect("a turn followed the second write");
    assert!(
        after.contains("restates `build-command`"),
        "the report must name the key already held:\n{after}"
    );
    assert!(
        after.contains("cargo test --all-features"),
        "and quote what that key holds:\n{after}"
    );
}

/// F10 — an unrelated note is not flagged, and rewriting a key is not flagged.
///
/// Both halves asserted on the *absence* of the report, because an
/// implementation that flags everything passes any test that only checks the
/// positive case.
#[tokio::test]
async fn an_unrelated_note_and_a_rewrite_of_one_key_are_not_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    let seen = played(
        dir.path(),
        &store,
        vec![
            remember("build-command", TEST_CMD),
            remember(
                "review-day",
                "the maintainer reviews pull requests on Tuesdays",
            ),
            // The same key, with a value nearly identical to its own: a
            // replacement, which is what writing by key has meant since 0.10.0.
            remember("build-command", TEST_CMD_AGAIN),
        ],
    )
    .await;
    for prompt in &seen {
        assert!(
            !prompt.contains("restates"),
            "nothing here restates anything under another key:\n{prompt}"
        );
    }
    assert_eq!(
        store.memory_list(&ws_key(dir.path())).unwrap().len(),
        2,
        "two keys, the second write of one of them having replaced its value"
    );
}

/// F11 — the write lands, and a long held value is quoted with its cut marked.
#[tokio::test]
async fn a_flagged_write_still_lands_and_the_quote_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let key = ws_key(dir.path());

    // A held value far longer than the quote may be, padded by repeating the
    // sentence rather than by adding one — new words are new vocabulary, and a
    // note that says more is legitimately less of a restatement. Padding with
    // "and it is run from the repository root" put the pair at exactly 50%,
    // which is the measure working rather than failing.
    let long = format!("{TEST_CMD}. ").repeat(20);
    store
        .memory_put(&key, "build-command", &long, 1, 1)
        .unwrap();

    let seen = played(
        dir.path(),
        &store,
        vec![remember("how-to-test", TEST_CMD_AGAIN)],
    )
    .await;
    let after = seen.last().expect("a turn followed the write");
    assert!(
        after.contains("restates `build-command`"),
        "the long entry is still the one restated:\n{after}"
    );
    assert!(
        after.contains("…[truncated]"),
        "a quote of a 900-character note is bounded and says so:\n{after}"
    );

    let held = store.memory_list(&key).unwrap();
    assert_eq!(
        held.len(),
        2,
        "the harness reports and does not refuse: {held:?}"
    );
    assert!(
        held.iter().any(|e| e.key == "how-to-test"),
        "the new note is in the store, which is what makes this a report"
    );
}

/// F12 — the check is per scope.
///
/// A workspace note restating a **global** one is the override 0.56.0 designed;
/// a workspace note restating another workspace note is the contradiction this
/// release exists for. Both directions, and the global side too.
#[tokio::test]
async fn the_duplicate_check_is_within_the_scope_being_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    store
        .memory_put(GLOBAL_MEMORY_WORKSPACE, "build-command", TEST_CMD, 1, 1)
        .unwrap();

    // Writing the same fact into the workspace is how a run corrects a wrong
    // global note. It must not be reported as a contradiction.
    let seen = played(
        dir.path(),
        &store,
        vec![remember("how-to-test", TEST_CMD_AGAIN)],
    )
    .await;
    for prompt in &seen {
        assert!(
            !prompt.contains("restates"),
            "a workspace note restating a global one is the override, not a clash:\n{prompt}"
        );
    }

    // The same write against a note in its own scope IS reported.
    let seen = played(dir.path(), &store, vec![remember("build-here", TEST_CMD)]).await;
    let after = seen.last().unwrap();
    assert!(
        after.contains("restates `how-to-test`"),
        "two workspace notes saying one thing is the case this release is for:\n{after}"
    );

    // And the global scope checks against itself, not against a workspace.
    let seen = played(
        dir.path(),
        &store,
        vec![remember_scoped("test-command", TEST_CMD_AGAIN, "global")],
    )
    .await;
    let after = seen.last().unwrap();
    assert!(
        after.contains("restates `build-command`"),
        "a global write is compared against the global scope:\n{after}"
    );
}
